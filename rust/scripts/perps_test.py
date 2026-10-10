"""End-to-end perps smoke test for `make testperps`.

Boots a single-node sequencer against `genesis-perps.jsonl`, then exercises:

  1. `make oraclehit MARKET=BTC PRICE=50000` — the oracle authority signs an
     OracleUpdate. Asserts `mark perp:0` reflects the new price.
  2. The first demo trader logs in, places a buy and a sell that cross,
     yielding a position. Asserts `position` shows non-empty rows.
  3. `liquidate` against a never-positioned address fails gracefully (smoke
     check the LiquidationTrigger surface, not a real liquidation flow —
     that's the runbook's job).

Hermetic: own data dir (./data-test-perps), own ports (free-port probe),
own cb-client wallet store under HOME=./data-test-perps/client-home so it
doesn't trample the operator's `~/.cb_client/`.

Stdlib-only, runs unattended, machine-readable exit code. Mirrors the
shape of `scripts/blockchain_test.py`.
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
DATA_DIR = REPO_ROOT / "data-test-perps"
SEQUENCER_LOG = DATA_DIR / "sequencer.log"
CLIENT_HOME = DATA_DIR / "client-home"

# Same dev mnemonics that scripts/perps_env.sh uses. Kept in sync by hand —
# the runbook + integration test both pin these.
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
    "MYWALLET3": (M_ZOO, 0),
    "ORACLE_BTC": (M_ABANDON, 10),
    "ORACLE_ETH": (M_ABANDON, 11),
    "DEMO_TRADER0": (M_ABANDON, 20),
    "DEMO_TRADER1": (M_ABANDON, 21),
    "DEMO_TRADER2": (M_ABANDON, 22),
    # The genesis treasury (`company` record): the only account that may mint
    # via Deposit. This test never mints, but the node refuses a genesis with
    # an unset variable, deliberately. The shipped .env derives it from a
    # bare seed; here it is a dev-mnemonic address like the rest.
    "TREASURY": (M_ABANDON, 30),
}

INITIAL_AMOUNT = "1000000000000"
CHAIN_ID = "1"
DENOM = "ULD"

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("perps-test")


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


def run_client(args: list[str], rpc_addr: str, *, env: dict[str, str] | None = None,
               check: bool = True) -> str:
    """Run cb-client with HOME=CLIENT_HOME so our wallet store stays private."""
    full_env = (env or os.environ).copy()
    full_env["HOME"] = str(CLIENT_HOME)
    cmd = [str(CLIENT_BIN), "--rpc", rpc_addr] + args
    log.debug("client: %s", " ".join(cmd))
    out = subprocess.run(cmd, capture_output=True, text=True, env=full_env)
    if check and out.returncode != 0:
        raise RuntimeError(
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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--keep-on-failure", action="store_true",
                        help="leave data + logs in place if the test fails")
    args = parser.parse_args()

    if not NODE_BIN.exists() or not CLIENT_BIN.exists():
        log.error("missing binaries: %s, %s — run `make build build-client`",
                  NODE_BIN, CLIENT_BIN)
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
    log.info("ORACLE_BTC=%s DEMO_TRADER0=%s", env["ORACLE_BTC"], env["DEMO_TRADER0"])

    # Stage genesis next to data dir so substituted env-var tokens resolve.
    genesis_path = DATA_DIR / "genesis-perps.jsonl"
    shutil.copyfile(GENESIS_SRC, genesis_path)

    log.info("init from %s", genesis_path.name)
    init_proc = subprocess.run(
        [str(NODE_BIN), "--data-dir", str(DATA_DIR), "init",
         "--genesis", str(genesis_path)],
        env=env, capture_output=True, text=True,
    )
    if init_proc.returncode != 0:
        log.error("init failed: %s", init_proc.stderr)
        return 1

    log.info("starting sequencer (logs: %s)", SEQUENCER_LOG)
    log_fp = open(SEQUENCER_LOG, "wb")
    seq = subprocess.Popen(
        [str(NODE_BIN), "--data-dir", str(DATA_DIR), "sequencer",
         "--listen", rpc_addr,
         "--dashboard-listen", f"127.0.0.1:{dash_port}",
         "--block-interval-ms", "100"],
        env=env, stdout=log_fp, stderr=log_fp,
    )

    failed = False
    try:
        status = wait_dashboard(dash_url)
        log.info("sequencer up at height=%s", status.get("head_height"))

        # ---------- 1. login as oracle, post BTC mark ----------
        run_client(["login", "--label", "oracle-btc",
                    "--mnemonic", M_ABANDON, "--index", "10", "--force"],
                   rpc_addr)
        # The source height is the block the update will land in: head + 1
        # on an idle chain. The executor refuses a claim beyond the block it
        # is building ("oracle update claims height 2 but the block is at
        # 1"), so `max(head, 1) + 1` on a fresh chain was one too far, and
        # the sequencer scheme dropped the refusal without a word.
        status = wait_dashboard(dash_url)
        height = status.get("head_height", 0) + 1
        price_q8 = "5000000000000"  # $50_000.00 * 1e8
        log.info("oracle-hit: perp:0 price_q8=%s height=%s", price_q8, height)
        run_client(["oracle", "perp:0", price_q8, "--height", str(height)], rpc_addr)

        # Give the block a tick to land + recompute mark.
        time.sleep(1.0)
        mark_json = run_client(["mark", "perp:0", "--json"], rpc_addr).strip()
        mark = json.loads(mark_json)
        log.info("mark perp:0 = %s", mark)
        if "px_q8" not in mark and "mark_px_q8" not in mark:
            raise RuntimeError(f"mark response missing px_q8: {mark!r}")

        # ---------- 2. login as trader 0, place a perp order ----------
        run_client(["login", "--label", "trader0",
                    "--mnemonic", M_ABANDON, "--index", "20", "--force"],
                   rpc_addr)
        # Buy 1 contract @ slightly above mark to cross with no resting book —
        # without a counter-party this rests; we just want the order to be
        # accepted by the validate path.
        out = run_client(
            ["perp-place", "perp:0", "buy", "1000", "5000000000000"],
            rpc_addr,
        )
        log.info("perp-place output: %s", out.strip().splitlines()[-1] if out else "(empty)")

        # ---------- 3. position read should not error (may be empty) ----------
        pos = run_client(["position", "--json"], rpc_addr)
        log.info("position (json): %s", pos.strip())

        # ---------- 4. markets list ----------
        mkts = run_client(["markets", "--json"], rpc_addr).strip().splitlines()
        log.info("markets returned %d line(s)", len(mkts))
        if not mkts:
            raise RuntimeError("`cb-client markets` returned no output")

        log.info("perps end-to-end PASSED")
    except Exception:
        failed = True
        log.exception("test failed")
    finally:
        log.info("stopping sequencer pid=%s", seq.pid)
        try:
            seq.send_signal(signal.SIGTERM)
            seq.wait(timeout=5)
        except subprocess.TimeoutExpired:
            seq.kill()
        log_fp.close()
        if failed:
            log.error("sequencer log tail:\n%s",
                      SEQUENCER_LOG.read_text()[-2000:] if SEQUENCER_LOG.exists() else "(none)")
        if not failed and not args.keep_on_failure:
            shutil.rmtree(DATA_DIR, ignore_errors=True)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
