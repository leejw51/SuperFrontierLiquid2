"""End-to-end blockchain check via the human-facing `cb-client` wallet.

Sibling to `scripts/supervisord/integration_test.py`, but exercises the
operator path (login -> balance -> send -> balance) against a *single-node*
sequencer instead of the 3-node `cb-sim` synthetic-load harness. Same
underlying invariants, different driver:

  1. Sequencer's head_height advanced by >= 1 across the send window.
  2. Sender's USD balance dropped by exactly the transferred amount.
  3. Recipient's USD balance grew by exactly the transferred amount.
  4. state_root changed between pre-send and post-send (state actually moved).
  5. Per-asset total across {sender, recipient} is conserved (Transfer must
     neither mint nor burn).
  6. Sender's nonce incremented by exactly 1.

Hermetic: picks free ports, uses its own data dir (./data-test-blockchain),
writes a generated genesis whose only funded actor is the sender (a
`validator` record with `wallet=<sender_addr>` and `initial_amount`). We
chose the validator-with-wallet form because:
  - `community_wallet` is single-instance (`anyhow::ensure!(spec.community_wallet.is_none())`)
    in `crates/cb-genesis/src/spec.rs`, so it can't both seed the chain's
    canonical reserve wallet AND fund our test sender.
  - `initial_amount` is credited as USD (`crates/cb-genesis/src/build.rs`
    line 161: `s.add(AssetId::Usd, v.initial_amount)`), which is exactly the
    asset we send below.
  - It avoids depending on `.env` (which is gitignored and currently lacks
    `INITIAL_AMOUNT` on a fresh checkout).

Runs unattended: stdlib-only, single command, machine-readable exit code.
On failure leaves data + logs in place when `--keep-on-failure` is passed.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent
NODE_BIN = REPO_ROOT / "target" / "release" / "cb-node"
CLIENT_BIN = REPO_ROOT / "target" / "release" / "cb-client"
DATA_DIR = REPO_ROOT / "data-test-blockchain"
GENESIS_PATH = DATA_DIR / "genesis.jsonl"
SEQUENCER_LOG = DATA_DIR / "sequencer.log"

# Mnemonics seed deterministic test addresses; not secrets. They are real
# BIP-39 English phrases (the strict deriver in `cb-client` rejects anything
# else after the BIP-39/BIP-44 migration).
SENDER_MNEMONIC = (
    "abandon abandon abandon abandon abandon abandon abandon abandon "
    "abandon abandon abandon about"
)
RECIPIENT_MNEMONIC = (
    "legal winner thank year wave sausage worth useful legal winner thank yellow"
)
SENDER_LABEL = "sender"
SENDER_INITIAL_USD = 10_000_000  # raw u128, also the validator's initial_amount
SEND_AMOUNT = 1_000  # raw u128 USD

ASSETS = ("btc", "eth", "usd")

log = logging.getLogger("blockchain")


# ---------------------------------------------------------------- build / ports


def build_binaries(rebuild: bool) -> None:
    if not rebuild and NODE_BIN.exists() and CLIENT_BIN.exists():
        log.info("binaries already built (use --rebuild to force)")
        return
    log.info("building cb-node + cb-client (release)")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "cb-node", "-p", "cb-client"],
        cwd=REPO_ROOT,
        check=True,
    )


def pick_free_port() -> int:
    """Bind to port 0, read what the kernel assigned, release. Race-y in the
    abstract but fine here: only this script touches ./data-test-blockchain
    and we hand the port straight to the sequencer at startup."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
    finally:
        s.close()


def wait_for_http(url: str, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=0.5) as resp:
                if resp.status < 500:
                    return True
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
            time.sleep(0.1)
    return False


# ------------------------------------------------------------- cb-client driver


def derive_address(mnemonic: str) -> str:
    """Invoke `cb-client addr --mnemonic <M> --index 0`. Contract: prints
    `0x<40 hex>\\n` to stdout, exit 0. The `addr` subcommand is being added
    by a parallel agent; this script designs against that contract."""
    out = subprocess.check_output(
        [str(CLIENT_BIN), "addr", "--mnemonic", mnemonic, "--index", "0"],
        text=True,
    ).strip()
    if not (out.startswith("0x") and len(out) == 42):
        raise RuntimeError(
            f"cb-client addr returned unexpected output: {out!r} "
            "(expected `0x` + 40 hex chars)"
        )
    return out


def run_client(rpc: str, *args: str, env: dict[str, str] | None = None) -> str:
    """Invoke `cb-client --rpc <rpc> <args...>` and return stdout. Uses a
    per-test wallet store under DATA_DIR/.cb_client so the harness never
    touches the operator's real ~/.cb_client."""
    full_env = os.environ.copy()
    # cb-client resolves the store via dirs::home_dir(); HOME is the lever.
    full_env["HOME"] = str(DATA_DIR)
    if env:
        full_env.update(env)
    cmd = [str(CLIENT_BIN), "--rpc", rpc, *args]
    return subprocess.check_output(cmd, text=True, env=full_env)


# ------------------------------------------------------------- chain inspection


def fetch_json(url: str, timeout: float = 1.0) -> dict[str, Any] | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, ConnectionError, TimeoutError, json.JSONDecodeError):
        return None


def fetch_status(dash_url: str) -> dict[str, Any] | None:
    return fetch_json(f"{dash_url}/api/status")


def fetch_account(dash_url: str, addr: str) -> dict[str, Any] | None:
    return fetch_json(f"{dash_url}/api/account/{addr}")


def balances_of(dash_url: str, addr: str) -> dict[str, int]:
    """Return {asset: balance_int} with all known assets present (zero-fill)."""
    acct = fetch_account(dash_url, addr) or {}
    raw = acct.get("balances", {}) or {}
    out = {asset: int(raw.get(asset, 0)) for asset in ASSETS}
    return out


def nonce_of(dash_url: str, addr: str) -> int:
    acct = fetch_account(dash_url, addr) or {}
    return int(acct.get("nonce", 0))


# --------------------------------------------------------------------- genesis


def write_genesis(sender_addr: str) -> None:
    """Write a minimal genesis that funds the test sender via a `validator`
    record with `wallet=<sender_addr>` and `initial_amount`. See the module
    docstring for why this funding form was chosen."""
    GENESIS_PATH.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        '{"kind":"chain","chain_id":1,"denom":"ULD"}',
        # Validator's signing key is derived from `seed`; `wallet` redirects
        # the initial USD credit to our test sender's address.
        json.dumps(
            {
                "kind": "validator",
                "seed": "cb-validator-0",
                "wallet": sender_addr,
                "weight": 1,
                "initial_amount": str(SENDER_INITIAL_USD),
            }
        ),
    ]
    GENESIS_PATH.write_text("\n".join(lines) + "\n")


# --------------------------------------------------------------- sequencer mgmt


def init_chain() -> None:
    log.info("initializing chain at %s", DATA_DIR)
    subprocess.run(
        [
            str(NODE_BIN),
            "--data-dir",
            str(DATA_DIR),
            "init",
            "--genesis",
            str(GENESIS_PATH),
        ],
        cwd=REPO_ROOT,
        check=True,
    )


def start_sequencer(rpc_port: int, dash_port: int) -> subprocess.Popen:
    SEQUENCER_LOG.parent.mkdir(parents=True, exist_ok=True)
    log_handle = open(SEQUENCER_LOG, "wb")
    log.info(
        "starting sequencer (rpc=127.0.0.1:%d, dashboard=127.0.0.1:%d)",
        rpc_port,
        dash_port,
    )
    proc = subprocess.Popen(
        [
            str(NODE_BIN),
            "--data-dir",
            str(DATA_DIR),
            "sequencer",
            "--listen",
            f"127.0.0.1:{rpc_port}",
            "--dashboard-listen",
            f"127.0.0.1:{dash_port}",
            "--block-interval-ms",
            "100",
        ],
        cwd=REPO_ROOT,
        stdout=log_handle,
        stderr=subprocess.STDOUT,
    )
    # Hand the file handle to the proc so its lifetime tracks the process.
    proc._ul_log_handle = log_handle  # type: ignore[attr-defined]
    return proc


def stop_sequencer(proc: subprocess.Popen | None) -> None:
    if proc is None:
        return
    if proc.poll() is None:
        log.info("stopping sequencer (pid=%d)", proc.pid)
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    handle = getattr(proc, "_ul_log_handle", None)
    if handle is not None:
        handle.close()


# ---------------------------------------------------------------------- driver


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--rebuild",
        action="store_true",
        help="force `cargo build --release` even if binaries already exist",
    )
    parser.add_argument(
        "--keep-on-failure",
        action="store_true",
        help="leave ./data-test-blockchain intact if any assertion fails "
        "(default: always wipe)",
    )
    parser.add_argument(
        "--settle-timeout",
        type=float,
        default=5.0,
        help="seconds to poll for the send to commit (default: 5.0)",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO, format="[blockchain] %(asctime)s %(message)s"
    )

    build_binaries(args.rebuild)

    # Wipe + recreate data dir up front so the run is hermetic.
    if DATA_DIR.exists():
        log.info("wiping %s", DATA_DIR)
        shutil.rmtree(DATA_DIR)
    DATA_DIR.mkdir(parents=True)

    # Derive the sender + recipient addresses via `cb-client addr`.
    log.info("deriving test addresses via cb-client addr")
    sender_addr = derive_address(SENDER_MNEMONIC)
    recipient_addr = derive_address(RECIPIENT_MNEMONIC)
    log.info("sender    = %s", sender_addr)
    log.info("recipient = %s", recipient_addr)

    write_genesis(sender_addr)
    init_chain()

    rpc_port = pick_free_port()
    dash_port = pick_free_port()
    rpc = f"127.0.0.1:{rpc_port}"
    dash_url = f"http://127.0.0.1:{dash_port}"

    seq_proc: subprocess.Popen | None = None
    failures: list[str] = []
    pre_status: dict[str, Any] = {}
    post_status: dict[str, Any] = {}
    pre_balances: dict[str, dict[str, int]] = {}
    post_balances: dict[str, dict[str, int]] = {}
    pre_nonce = 0
    post_nonce = 0

    try:
        seq_proc = start_sequencer(rpc_port, dash_port)

        if not wait_for_http(f"{dash_url}/api/status", timeout=15.0):
            failures.append(
                f"sequencer dashboard {dash_url}/api/status never came up "
                f"(see {SEQUENCER_LOG})"
            )
            return _finish(failures, args, seq_proc, pre_status, post_status,
                           pre_balances, post_balances, pre_nonce, post_nonce,
                           sender_addr, recipient_addr)
        log.info("sequencer is up")

        # ------- stage 1: capture genesis state -------
        log.info("stage 1/4: capturing genesis state")
        pre_status = fetch_status(dash_url) or {}
        pre_balances = {
            "sender": balances_of(dash_url, sender_addr),
            "recipient": balances_of(dash_url, recipient_addr),
        }
        pre_nonce = nonce_of(dash_url, sender_addr)
        log.info(
            "genesis: head_height=%s state_root=%s sender_usd=%d recipient_usd=%d nonce=%d",
            pre_status.get("head_height"),
            pre_status.get("state_root"),
            pre_balances["sender"]["usd"],
            pre_balances["recipient"]["usd"],
            pre_nonce,
        )

        if pre_balances["sender"]["usd"] != SENDER_INITIAL_USD:
            failures.append(
                f"sender balance at genesis = {pre_balances['sender']['usd']}, "
                f"expected {SENDER_INITIAL_USD}"
            )
            return _finish(failures, args, seq_proc, pre_status, post_status,
                           pre_balances, post_balances, pre_nonce, post_nonce,
                           sender_addr, recipient_addr)

        # ------- stage 2: cb-client login + balance -------
        log.info("stage 2/4: cb-client login + balance check")
        run_client(
            rpc,
            "login",
            "--label",
            SENDER_LABEL,
            "--mnemonic",
            SENDER_MNEMONIC,
            "--index",
            "0",
        )
        bal_out = run_client(rpc, "balance")
        log.info("cb-client balance:\n%s", bal_out.rstrip())
        if str(SENDER_INITIAL_USD) not in bal_out:
            failures.append(
                f"cb-client balance output did not include expected "
                f"sender funds {SENDER_INITIAL_USD}: {bal_out!r}"
            )

        # ------- stage 3: cb-client send -------
        log.info(
            "stage 3/4: cb-client send (recipient=%s, amount=%d usd)",
            recipient_addr,
            SEND_AMOUNT,
        )
        send_out = run_client(
            rpc, "send", recipient_addr, "usd", str(SEND_AMOUNT)
        )
        log.info("cb-client send:\n%s", send_out.rstrip())

        # Poll until the chain has both advanced AND the recipient sees the
        # credited amount, or the deadline elapses.
        deadline = time.monotonic() + args.settle_timeout
        pre_height = int(pre_status.get("head_height", 0) or 0)
        while time.monotonic() < deadline:
            st = fetch_status(dash_url) or {}
            bal_recipient = balances_of(dash_url, recipient_addr)
            advanced = int(st.get("head_height", 0) or 0) > pre_height
            credited = bal_recipient["usd"] >= pre_balances["recipient"]["usd"] + SEND_AMOUNT
            if advanced and credited:
                break
            time.sleep(0.1)

        # ------- stage 4: assertions on post-send state -------
        log.info("stage 4/4: capturing post-send state + asserting")
        post_status = fetch_status(dash_url) or {}
        post_balances = {
            "sender": balances_of(dash_url, sender_addr),
            "recipient": balances_of(dash_url, recipient_addr),
        }
        post_nonce = nonce_of(dash_url, sender_addr)

        # 1) head_height advanced.
        pre_h = int(pre_status.get("head_height", 0) or 0)
        post_h = int(post_status.get("head_height", 0) or 0)
        if post_h - pre_h < 1:
            failures.append(
                f"head_height did not advance (pre={pre_h}, post={post_h})"
            )

        # 2) sender USD dropped by SEND_AMOUNT.
        sender_drop = pre_balances["sender"]["usd"] - post_balances["sender"]["usd"]
        if sender_drop != SEND_AMOUNT:
            failures.append(
                f"sender USD changed by {sender_drop}, expected -{SEND_AMOUNT} "
                f"(pre={pre_balances['sender']['usd']}, "
                f"post={post_balances['sender']['usd']})"
            )

        # 3) recipient USD rose by SEND_AMOUNT.
        recipient_rise = (
            post_balances["recipient"]["usd"] - pre_balances["recipient"]["usd"]
        )
        if recipient_rise != SEND_AMOUNT:
            failures.append(
                f"recipient USD changed by {recipient_rise}, expected "
                f"+{SEND_AMOUNT} (pre={pre_balances['recipient']['usd']}, "
                f"post={post_balances['recipient']['usd']})"
            )

        # 4) state_root changed.
        pre_root = pre_status.get("state_root")
        post_root = post_status.get("state_root")
        if pre_root and post_root and pre_root == post_root:
            failures.append(
                f"state_root unchanged across send (still {pre_root}); "
                "block was not applied"
            )

        # 5) per-asset total conservation across {sender, recipient}.
        for asset in ASSETS:
            pre_total = (
                pre_balances["sender"][asset] + pre_balances["recipient"][asset]
            )
            post_total = (
                post_balances["sender"][asset]
                + post_balances["recipient"][asset]
            )
            if pre_total != post_total:
                failures.append(
                    f"asset {asset}: total across sender+recipient changed "
                    f"({pre_total} -> {post_total}); Transfer must conserve"
                )

        # 6) sender nonce incremented by 1.
        if post_nonce - pre_nonce != 1:
            failures.append(
                f"sender nonce changed by {post_nonce - pre_nonce}, expected +1 "
                f"(pre={pre_nonce}, post={post_nonce})"
            )

        return _finish(
            failures,
            args,
            seq_proc,
            pre_status,
            post_status,
            pre_balances,
            post_balances,
            pre_nonce,
            post_nonce,
            sender_addr,
            recipient_addr,
        )

    finally:
        stop_sequencer(seq_proc)
        # Wipe data-dir unless the caller asked to keep it AND we failed.
        keep = args.keep_on_failure and bool(failures)
        if not keep and DATA_DIR.exists():
            log.info("wiping %s", DATA_DIR)
            shutil.rmtree(DATA_DIR, ignore_errors=True)
        elif keep:
            log.info("keeping %s for post-mortem (--keep-on-failure)", DATA_DIR)


def _finish(
    failures: list[str],
    args: argparse.Namespace,
    seq_proc: subprocess.Popen | None,
    pre_status: dict[str, Any],
    post_status: dict[str, Any],
    pre_balances: dict[str, dict[str, int]],
    post_balances: dict[str, dict[str, int]],
    pre_nonce: int,
    post_nonce: int,
    sender_addr: str,
    recipient_addr: str,
) -> int:
    """Print the summary table + FAILURES block, return the exit code.
    Sequencer teardown is handled by the caller's `finally`."""
    print()
    print("=" * 80)
    print(f"{'metric':<22} {'pre-send':>26} {'post-send':>26}")
    print("-" * 80)
    print(
        f"{'head_height':<22} {pre_status.get('head_height', '?')!s:>26} "
        f"{post_status.get('head_height', '?')!s:>26}"
    )
    pre_root = str(pre_status.get("state_root", ""))[:24]
    post_root = str(post_status.get("state_root", ""))[:24]
    print(f"{'state_root (24-char)':<22} {pre_root:>26} {post_root:>26}")
    if pre_balances and post_balances:
        for who, addr in (("sender", sender_addr), ("recipient", recipient_addr)):
            pre_b = pre_balances.get(who, {})
            post_b = post_balances.get(who, {})
            for asset in ASSETS:
                p = pre_b.get(asset, 0)
                q = post_b.get(asset, 0)
                if p == 0 and q == 0:
                    continue
                print(f"{who+'/'+asset:<22} {p:>26} {q:>26}")
        print(f"{'sender_nonce':<22} {pre_nonce:>26} {post_nonce:>26}")
    print("=" * 80)
    print(f"sender    = {sender_addr}")
    print(f"recipient = {recipient_addr}")
    print("=" * 80)

    if failures:
        print()
        print("FAILURES:")
        for f in failures:
            print(f"  - {f}")
        log.error("BLOCKCHAIN TEST FAILED (%d issues)", len(failures))
        return 1

    log.info("BLOCKCHAIN TEST PASSED — login, send, accounting all verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
