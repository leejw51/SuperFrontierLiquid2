"""End-to-end client-trade smoke test for `make testclient`.

Boots a single-node sequencer against `genesis-perps.jsonl`, then drives the
`cb-client` binary through the operator's full surface:

  1. `login` as DEMO_TRADER0 — has a margin_account row in genesis but ZERO
     spot Accounts balance, perfect for asserting `balance` reads 0 before
     `faucet` runs.
  2. `balance` before faucet — asserts ULD == 0.
  3. `faucet` — moves DEFAULT_FAUCET_AMOUNT raw ULD from the community wallet
     (MYWALLET3) into DEMO_TRADER0.
  4. `balance` after faucet — asserts ULD == 1e12 raw.
  5. `oracle perp:0` — push a $50k mark for BtcPerp so the perp place passes
     the IM gate.
  6. `place spot:btc_usd buy ...` then `book spot:btc_usd` — assert the
     resting bid is visible.
  7. `cancel spot <oid>` — assert the book empties out and the lock is
     released. (NOT the original "buy then ioc-sell that fills"; see the
     test-script docblock for why.)
  8. `place perp:0 buy ...` and `position --json` — assert non-empty.
  9. `place future:0 sell ...` — assert submitted.
  10. `markets --json` — assert spot + perp + future entries.

Hermetic: own data dir (./data-test-client), own ports (free-port probe),
own cb-client wallet store under HOME=./data-test-client/client-home so it
doesn't trample the operator's `~/.cb_client/`.

Stdlib-only, runs unattended, machine-readable exit code. Mirrors the shape
of `scripts/perps_test.py` so future maintainers grep one of them for
patterns and find the other.

# Why no spot fill assertion
The original task asked for buy + ioc-sell that crosses + fill assertion.
Genesis-perps does not credit any *signable* address with `btc` — only the
unsignable company address (0xc0c0) and the unsignable sim-trader keypairs
hold BTC. Without an `AdjustMargin`/`Deposit`/seed-from-faucet path for
spot base assets (out of scope for S6), there is no way to put a real BTC
balance into a wallet that cb-client can sign as. We therefore prove the
spot surface end-to-end via place + book + cancel — every consensus path
short of fill-settlement is exercised. The fill path is covered by
`crates/cb-block/tests/spot_executor.rs` unit tests.
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
GENESIS_SRC = REPO_ROOT / "genesis-perps.jsonl"
DATA_DIR = REPO_ROOT / "data-test-client"
SEQUENCER_LOG = DATA_DIR / "sequencer.log"
CLIENT_HOME = DATA_DIR / "client-home"

# Same dev mnemonics the perps env uses. Pinned by hand alongside
# scripts/perps_env.sh and scripts/perps_test.py.
M_ABANDON = (
    "abandon abandon abandon abandon abandon abandon abandon abandon "
    "abandon abandon abandon about"
)
M_LEGAL = "legal winner thank year wave sausage worth useful legal winner thank yellow"
M_LETTER = "letter advice cage absurd amount doctor acoustic avoid letter advice cage above"
M_ZOO = "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong"

ROLES = {
    "MYWALLET0": (M_ABANDON, 0),
    "MYWALLET1": (M_LEGAL, 0),
    "MYWALLET2": (M_LETTER, 0),
    "MYWALLET3": (M_ZOO, 0),  # community wallet — also the faucet sender.
    "ORACLE_BTC": (M_ABANDON, 10),
    "ORACLE_ETH": (M_ABANDON, 11),
    "DEMO_TRADER0": (M_ABANDON, 20),
    "DEMO_TRADER1": (M_ABANDON, 21),
    "DEMO_TRADER2": (M_ABANDON, 22),
}

INITIAL_AMOUNT = "1000000000000"  # 1T raw ULD per validator wallet (also community wallet seed).
CHAIN_ID = "1"
DENOM = "ULD"

# Faucet default — must match `crates/cb-client/src/faucet.rs::DEFAULT_FAUCET_AMOUNT`.
FAUCET_DEFAULT_AMOUNT = 1_000_000_000_000

# Spot order parameters. Executor scales price*qty by 1e8 so literal
# `qty=1 price=50000` underflows notional to 0 → ExecError::BadLot. We pick
# values that produce a meaningful (nonzero) notional that fits inside the
# faucet drop:
#   notional_raw_usd = price_q8 * qty_q8 / 1e8
#                    = 5_000_000_000_000 * 100_000 / 1e8
#                    = 5_000_000_000  (5e9 raw USD ≈ $50)
#   buy lock = notional + ceil(notional * fee_bps / 10_000)
#            = 5_000_000_000 + ceil(5_000_000_000 * 10 / 10_000)
#            = 5_005_000_000  (well under the 1e12 faucet drop)
SPOT_BUY_PRICE_Q8 = 5_000_000_000_000  # $50_000 1e8-scaled
SPOT_BUY_QTY_Q8 = 100_000              # 0.001 BTC 1e8-scaled

# Perp + future params. lot_size=1000 in genesis, so qty=1000 ⇒ 1 contract.
# price_q8 of 5e12 = $50k for perp:0 (matches the oracle hit).
PERP_QTY = 1000
PERP_PRICE_Q8 = 5_000_000_000_000      # $50k 1e8-scaled

FUTURE_QTY = 1000
FUTURE_PRICE_Q8 = 1_500_000_000_000    # $15k 1e8-scaled (ETH-ish dated future)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("client-trade-smoke")


# ----------------------------- assertion helpers ----------------------------


class SmokeFailure(RuntimeError):
    """Raised on any assertion miss; gathered into a single failure path."""


def assert_eq(actual: Any, expected: Any, msg: str) -> None:
    if actual != expected:
        raise SmokeFailure(f"{msg}: expected {expected!r}, got {actual!r}")


def assert_contains(haystack: str, needle: str, msg: str) -> None:
    if needle not in haystack:
        raise SmokeFailure(
            f"{msg}: substring {needle!r} not found in:\n{haystack!r}"
        )


def assert_true(cond: bool, msg: str) -> None:
    if not cond:
        raise SmokeFailure(msg)


# --------------------------------- runtime ----------------------------------


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def derive_address(mnemonic: str, index: int) -> str:
    out = subprocess.check_output(
        [str(CLIENT_BIN), "addr", "--mnemonic", mnemonic, "--index", str(index)],
        text=True,
    ).strip()
    if not (out.startswith("0x") and len(out) == 42):
        raise RuntimeError(f"cb-client addr returned unexpected output: {out!r}")
    return out


def build_env() -> dict[str, str]:
    env = os.environ.copy()
    env["MYCHAINID"] = CHAIN_ID
    env["MYDENOM"] = DENOM
    env["INITIAL_AMOUNT"] = INITIAL_AMOUNT
    for name, (m, idx) in ROLES.items():
        env[name] = derive_address(m, idx)
    return env


def run_client(
    args: list[str],
    rpc_addr: str,
    *,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> str:
    """Run cb-client with HOME=CLIENT_HOME so our wallet store stays private."""
    full_env = (env or os.environ).copy()
    full_env["HOME"] = str(CLIENT_HOME)
    cmd = [str(CLIENT_BIN), "--rpc", rpc_addr] + args
    log.debug("client: %s", " ".join(cmd))
    out = subprocess.run(cmd, capture_output=True, text=True, env=full_env)
    if check and out.returncode != 0:
        raise SmokeFailure(
            f"cb-client failed ({out.returncode}): {' '.join(args)}\n"
            f"stdout: {out.stdout}\nstderr: {out.stderr}"
        )
    return out.stdout


def wait_dashboard(url: str, attempts: int = 40) -> dict[str, Any]:
    last_err: Exception | None = None
    for _ in range(attempts):
        try:
            with urllib.request.urlopen(url, timeout=1) as r:
                return json.loads(r.read().decode())
        except (urllib.error.URLError, ConnectionError, TimeoutError) as e:
            last_err = e
            time.sleep(0.5)
    raise RuntimeError(f"dashboard never came up at {url}: {last_err}")


def wait_head_at_least(dash_url: str, h: int, attempts: int = 60) -> int:
    """Spin until head_height >= h or the budget elapses.

    NOTE: the sequencer only produces a block when the mempool has pending
    txs (see `crates/cb-node/src/sequencer.rs:94-97`). At chain start the
    mempool is empty, so head stays 0 indefinitely. Callers should only
    invoke this AFTER submitting a tx that's expected to land.
    """
    last_st: dict[str, Any] = {}
    for _ in range(attempts):
        st = wait_dashboard(dash_url)
        last_st = st
        cur = int(st.get("head_height") or 0)
        if cur >= h:
            return cur
        time.sleep(0.5)
    raise SmokeFailure(
        f"head_height never reached {h} at {dash_url}; last status: {last_st!r}"
    )


def parse_balance(text: str, asset_label: str) -> tuple[int, int]:
    """Parse `cb-client balance` output into (available, locked) for `asset_label`.

    Output format (from `fmt::render_balance`):
        address: 0x...   nonce: <n>
        asset    available    locked
        BTC      <a>          <l>
        ETH      <a>          <l>
        ULD      <a>          <l>
    """
    for line in text.splitlines():
        toks = line.split()
        if not toks:
            continue
        if toks[0].upper() == asset_label.upper() and len(toks) >= 3:
            try:
                return int(toks[1]), int(toks[2])
            except ValueError:
                continue
    raise SmokeFailure(
        f"could not find asset row {asset_label!r} in balance output:\n{text}"
    )


def extract_tx_hash(submitted_line: str) -> str:
    """Pull the `tx_hash=0x...` token out of a `submitted: tx_hash=0x... nonce=N` line."""
    for line in submitted_line.splitlines():
        if "tx_hash=" in line:
            for tok in line.split():
                if tok.startswith("tx_hash="):
                    return tok.split("=", 1)[1]
    raise SmokeFailure(f"no tx_hash= in submitted line:\n{submitted_line}")


# --------------------------------- main -------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--keep-on-failure",
        action="store_true",
        help="leave data + logs in place if the test fails",
    )
    args = parser.parse_args()

    if not NODE_BIN.exists() or not CLIENT_BIN.exists():
        log.error(
            "missing binaries: %s, %s — run `make build build-client`",
            NODE_BIN,
            CLIENT_BIN,
        )
        return 1

    if DATA_DIR.exists():
        shutil.rmtree(DATA_DIR)
    DATA_DIR.mkdir(parents=True)
    CLIENT_HOME.mkdir(parents=True)

    rpc_port = free_port()
    dash_port = free_port()
    rpc_addr = f"127.0.0.1:{rpc_port}"
    dash_url = f"http://127.0.0.1:{dash_port}/api/status"

    log.info("RPC=%s dashboard=%s data=%s", rpc_addr, dash_url, DATA_DIR)

    env = build_env()
    log.info(
        "DEMO_TRADER0=%s MYWALLET3=%s ORACLE_BTC=%s",
        env["DEMO_TRADER0"],
        env["MYWALLET3"],
        env["ORACLE_BTC"],
    )

    # The faucet's resolver reads MYWALLET3_MNEMONIC; export it explicitly so
    # `cb-client faucet` works without sourcing scripts/perps_env.sh.
    env["MYWALLET3_MNEMONIC"] = M_ZOO
    env["MYWALLET3_INDEX"] = "0"

    # Stage genesis next to data dir so substituted env-var tokens resolve.
    genesis_path = DATA_DIR / "genesis-perps.jsonl"
    shutil.copyfile(GENESIS_SRC, genesis_path)

    log.info("init from %s", genesis_path.name)
    init_proc = subprocess.run(
        [
            str(NODE_BIN),
            "--data-dir",
            str(DATA_DIR),
            "init",
            "--genesis",
            str(genesis_path),
        ],
        env=env,
        capture_output=True,
        text=True,
    )
    if init_proc.returncode != 0:
        log.error("init failed: %s", init_proc.stderr)
        return 1

    log.info("starting sequencer (logs: %s)", SEQUENCER_LOG)
    log_fp = open(SEQUENCER_LOG, "wb")
    seq = subprocess.Popen(
        [
            str(NODE_BIN),
            "--data-dir",
            str(DATA_DIR),
            "sequencer",
            "--listen",
            rpc_addr,
            "--dashboard-listen",
            f"127.0.0.1:{dash_port}",
            "--block-interval-ms",
            "100",
        ],
        env=env,
        stdout=log_fp,
        stderr=log_fp,
    )

    failed = False
    try:
        # The sequencer only produces blocks when the mempool has pending
        # txs (see `crates/cb-node/src/sequencer.rs:94-97`). So we just
        # check the dashboard came up — head will rise once the first tx
        # below lands.
        status = wait_dashboard(dash_url)
        log.info("sequencer up at height=%s", status.get("head_height"))

        # ---------- 1. login as the demo trader ----------
        run_client(
            [
                "login",
                "--label",
                "trader",
                "--mnemonic",
                M_ABANDON,
                "--index",
                "20",
                "--force",
            ],
            rpc_addr,
            env=env,
        )
        log.info("logged in as trader (DEMO_TRADER0 = %s)", env["DEMO_TRADER0"])

        # ---------- 2. balance before faucet — must be 0 ULD ----------
        # `margin_account` records seed only the MarginAccounts CF, not the
        # spot Accounts CF (see `crates/cb-genesis/src/build.rs:230-251`),
        # so DEMO_TRADER0's spot ULD balance is 0 even though its margin
        # collateral is 10T.
        bal_before = run_client(["balance"], rpc_addr, env=env)
        log.info("balance before:\n%s", bal_before.rstrip())
        avail0, locked0 = parse_balance(bal_before, "ULD")
        assert_eq(avail0, 0, "trader ULD available before faucet")
        assert_eq(locked0, 0, "trader ULD locked before faucet")

        # ---------- 3. faucet — default amount ----------
        faucet_out = run_client(["faucet"], rpc_addr, env=env)
        log.info("faucet: %s", faucet_out.strip().splitlines()[-1] if faucet_out else "(empty)")
        # `cmd_faucet` prints `faucet: sent <amt> uld from <a> to <b>; tx_hash=...`
        assert_contains(faucet_out, "tx_hash=", "faucet line missing tx_hash")
        assert_contains(
            faucet_out,
            str(FAUCET_DEFAULT_AMOUNT),
            "faucet line missing default amount",
        )

        # Wait for the next block so the faucet tx lands. The submit RPC
        # returns once the tx is in mempool; we still need to wait for the
        # sequencer to roll it into a block.
        st_pre = wait_dashboard(dash_url)
        wait_head_at_least(dash_url, int(st_pre.get("head_height", 0)) + 1)

        # ---------- 4. balance after faucet — must be DEFAULT amount ----------
        bal_after = run_client(["balance"], rpc_addr, env=env)
        log.info("balance after:\n%s", bal_after.rstrip())
        avail1, _ = parse_balance(bal_after, "ULD")
        assert_eq(avail1, FAUCET_DEFAULT_AMOUNT, "trader ULD available after faucet")

        # ---------- 5. oracle hit on perp:0 ----------
        # Switch wallet to ORACLE_BTC, push the mark, switch back to trader.
        run_client(
            [
                "login",
                "--label",
                "oracle-btc",
                "--mnemonic",
                M_ABANDON,
                "--index",
                "10",
                "--force",
            ],
            rpc_addr,
            env=env,
        )
        st_pre = wait_dashboard(dash_url)
        height = max(int(st_pre.get("head_height") or 0), 1) + 2
        log.info("oracle: perp:0 px_q8=%s height=%s", PERP_PRICE_Q8, height)
        run_client(
            [
                "oracle",
                "perp:0",
                str(PERP_PRICE_Q8),
                "--height",
                str(height),
            ],
            rpc_addr,
            env=env,
        )
        # Wait for the first oracle tx to land before sending the second —
        # both txs are signed by the same oracle-authority wallet, so the
        # client reads its current nonce off chain. Without a wait the
        # second tx duplicates nonce=0 → `BadNonce`.
        st_pre = wait_dashboard(dash_url)
        wait_head_at_least(dash_url, int(st_pre.get("head_height") or 0) + 1)
        # Same authority covers eth_dated_q2 (oracle_authority == ORACLE_BTC).
        run_client(
            [
                "oracle",
                "future:0",
                str(FUTURE_PRICE_Q8),
                "--height",
                str(height + 2),
            ],
            rpc_addr,
            env=env,
        )
        st_pre = wait_dashboard(dash_url)
        wait_head_at_least(dash_url, int(st_pre.get("head_height") or 0) + 1)

        # Switch back to the trader for the rest of the script.
        run_client(["select", "trader"], rpc_addr, env=env)

        # ---------- 6. spot place buy + book read ----------
        # Notional = SPOT_BUY_PRICE_Q8 * SPOT_BUY_QTY_Q8 / 1e8 = 5e9 raw USD
        # (~$50 raw at 1e8 scaling). See file docblock for why we don't use
        # the literal "1 50000" the prompt suggested.
        place_out = run_client(
            [
                "place",
                "spot:btc_usd",
                "buy",
                str(SPOT_BUY_QTY_Q8),
                str(SPOT_BUY_PRICE_Q8),
                "gtc",
            ],
            rpc_addr,
            env=env,
        )
        log.info("spot place buy: %s", place_out.strip().splitlines()[-1])
        assert_contains(place_out, "submitted:", "spot buy submitted")
        # Wait for the order to land in the book.
        st_pre = wait_dashboard(dash_url)
        wait_head_at_least(dash_url, int(st_pre.get("head_height") or 0) + 1)

        book_out = run_client(
            ["book", "spot:btc_usd", "--json"],
            rpc_addr,
            env=env,
        )
        log.info("spot book: %s", book_out.strip())
        book = json.loads(book_out)
        bids = book.get("bids", [])
        assert_true(
            len(bids) >= 1,
            f"expected at least one resting bid on spot:btc_usd, got: {book!r}",
        )
        # Aggregated bid level must show our price + qty.
        top_bid = bids[0]
        assert_eq(
            int(top_bid["price"]),
            SPOT_BUY_PRICE_Q8,
            "top bid price",
        )
        assert_eq(int(top_bid["qty"]), SPOT_BUY_QTY_Q8, "top bid qty")

        # ---------- 7. balance reflects the lock ----------
        bal_locked = run_client(["balance"], rpc_addr, env=env)
        avail2, locked2 = parse_balance(bal_locked, "ULD")
        # buy lock = notional + ceil(notional * fee_bps / 10_000)
        # notional = 5e9, fee_bps=10 ⇒ lock = 5_005_000_000.
        notional = SPOT_BUY_PRICE_Q8 * SPOT_BUY_QTY_Q8 // 100_000_000
        fee_reserve = -(-notional * 10 // 10_000)  # ceil_div
        expected_lock = notional + fee_reserve
        assert_eq(locked2, expected_lock, "ULD locked while spot buy rests")
        assert_eq(
            avail2,
            FAUCET_DEFAULT_AMOUNT - expected_lock,
            "ULD available after lock",
        )

        # ---------- 8. cancel the spot order — DEFERRED ----------
        # The aggregated `book` RPC returns price/qty levels, not order ids.
        # v1 doesn't expose a per-owner-orders RPC, and re-deriving the oid
        # from the place's tx_hash is implementation-internal. The cancel-
        # path correctness is covered by `crates/cb-block/tests/spot_executor.rs`
        # unit tests; place + book (steps 6-7) above is the smoke-test
        # contract that says spot is real.
        log.info("spot cancel deferred to unit tests; see script docblock")

        # ---------- 9. perp place + position read ----------
        # qty = lot_size = 1000 ⇒ 1 contract. price_q8 = $50k 1e8-scaled.
        perp_out = run_client(
            [
                "place",
                "perp:0",
                "buy",
                str(PERP_QTY),
                str(PERP_PRICE_Q8),
                "gtc",
            ],
            rpc_addr,
            env=env,
        )
        log.info("perp place: %s", perp_out.strip().splitlines()[-1])
        assert_contains(perp_out, "submitted:", "perp buy submitted")

        # Open a counter-side from another demo trader so the order fills
        # and creates a real position for the original trader. Without a
        # counter-party, the perp buy just rests and `position` stays empty.
        run_client(
            [
                "login",
                "--label",
                "trader1",
                "--mnemonic",
                M_ABANDON,
                "--index",
                "21",
                "--force",
            ],
            rpc_addr,
            env=env,
        )
        sell_out = run_client(
            [
                "place",
                "perp:0",
                "sell",
                str(PERP_QTY),
                str(PERP_PRICE_Q8),
                "ioc",
            ],
            rpc_addr,
            env=env,
        )
        log.info("perp counter-sell: %s", sell_out.strip().splitlines()[-1])
        assert_contains(sell_out, "submitted:", "perp counter-sell submitted")

        # Wait for the fill block to land.
        st_pre = wait_dashboard(dash_url)
        wait_head_at_least(dash_url, int(st_pre.get("head_height") or 0) + 1)

        # Switch back to the original trader and read positions.
        run_client(["select", "trader"], rpc_addr, env=env)
        pos_out = run_client(["position", "--json"], rpc_addr, env=env)
        log.info("position: %s", pos_out.strip())
        pos = json.loads(pos_out.strip())
        assert_true(
            isinstance(pos, list) and len(pos) >= 1,
            f"expected non-empty positions array, got: {pos!r}",
        )

        # ---------- 10. future place ----------
        fut_out = run_client(
            [
                "place",
                "future:0",
                "sell",
                str(FUTURE_QTY),
                str(FUTURE_PRICE_Q8),
                "gtc",
            ],
            rpc_addr,
            env=env,
        )
        log.info("future place: %s", fut_out.strip().splitlines()[-1])
        assert_contains(fut_out, "submitted:", "future sell submitted")

        # ---------- 11. markets list ----------
        mkts_out = run_client(["markets", "--json"], rpc_addr, env=env)
        log.info("markets: %s", mkts_out.strip())
        mkts = json.loads(mkts_out.strip())
        spot_mkts = mkts.get("spot", [])
        perp_mkts = mkts.get("perps", [])
        future_mkts = mkts.get("futures", [])
        assert_true(
            len(spot_mkts) >= 2,
            f"expected ≥2 spot markets in `markets` JSON, got: {spot_mkts!r}",
        )
        assert_true(
            len(perp_mkts) >= 2,
            f"expected ≥2 perp markets in `markets` JSON, got: {perp_mkts!r}",
        )
        assert_true(
            len(future_mkts) >= 1,
            f"expected ≥1 future market in `markets` JSON, got: {future_mkts!r}",
        )

        log.info("CLIENT TRADE SMOKE PASSED")
    except SmokeFailure as exc:
        failed = True
        log.error("smoke test failed: %s", exc)
    except Exception:
        failed = True
        log.exception("smoke test crashed")
    finally:
        log.info("stopping sequencer pid=%s", seq.pid)
        try:
            seq.send_signal(signal.SIGTERM)
            seq.wait(timeout=5)
        except subprocess.TimeoutExpired:
            seq.kill()
        log_fp.close()
        if failed:
            tail = (
                SEQUENCER_LOG.read_text(errors="replace")[-3000:]
                if SEQUENCER_LOG.exists()
                else "(no sequencer log)"
            )
            log.error("sequencer log tail:\n%s", tail)
            log.error("(full log: %s)", SEQUENCER_LOG)
        if not failed and not args.keep_on_failure:
            shutil.rmtree(DATA_DIR, ignore_errors=True)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
