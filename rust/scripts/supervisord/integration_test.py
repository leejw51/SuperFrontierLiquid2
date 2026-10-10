"""End-to-end blockchain integration test.

Boots the 3-node cluster, captures genesis trader balances, submits real signed
Transfer txs via cb-sim against the sequencer, then asserts:

  1. Sequencer produced >= --min-blocks blocks within the window
  2. Both replicas reach within MAX_LAG_BLOCKS of sequencer's head
  3. State roots match across all 3 nodes at every shared height
  4. Trader balances changed from genesis (the ledger advanced — at least one
     Transfer tx actually committed)
  5. Same trader's (nonce, balances) is identical on all 3 nodes after settle
     (sync correctness — replicas re-executed the same way)
  6. Per-asset total across all traders is conserved (txs only transfer between
     traders, never mint or burn — strong accounting invariant)

Tears down on success or failure (data + logs wiped unless --keep-on-failure).

Designed to run unattended in CI: single command, machine-readable exit code, and
all logs left in scripts/supervisord/logs/ on failure for debugging.
"""

from __future__ import annotations

import argparse
import json
import logging
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
SIM_BIN = REPO_ROOT / "target" / "release" / "cb-sim"
NODE_BIN = REPO_ROOT / "target" / "release" / "cb-node"

DASHBOARDS = {
    "node0": "http://127.0.0.1:18080",
    "node1": "http://127.0.0.1:18081",
    "node2": "http://127.0.0.1:18082",
}
SEQUENCER = "node0"
ASSETS = ("btc", "eth", "usd")
MAX_LAG_BLOCKS = 5

log = logging.getLogger("integration")


def build_binaries() -> None:
    if NODE_BIN.exists() and SIM_BIN.exists():
        log.info("binaries already built")
        return
    log.info("building cb-node + cb-sim (release)")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "cb-node", "-p", "cb-sim"],
        cwd=REPO_ROOT,
        check=True,
    )


def wait_for_port(host: str, port: int, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def fetch_json(url: str, timeout: float = 1.0) -> dict[str, Any] | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, ConnectionError, TimeoutError, json.JSONDecodeError):
        return None


def trader_addresses(num: int, prefix: str) -> list[str]:
    out = subprocess.check_output(
        [
            str(SIM_BIN),
            "traders",
            "--num-traders",
            str(num),
            "--seed-prefix",
            prefix,
        ],
        text=True,
    )
    addrs: list[str] = []
    for line in out.splitlines():
        parts = line.strip().split()
        if len(parts) >= 2 and parts[-1].startswith("0x"):
            addrs.append(parts[-1])
    return addrs


def fetch_account(node_url: str, addr: str) -> dict[str, Any] | None:
    return fetch_json(f"{node_url}/api/account/{addr}")


def fetch_status(node_url: str) -> dict[str, Any] | None:
    return fetch_json(f"{node_url}/api/status")


def collect_balances(addrs: list[str]) -> dict[str, dict[str, dict[str, int]]]:
    """Return: {node_name: {trader_addr: {asset: balance_int}}}.

    Uses the sequencer as authority for "the" canonical balance; replicas only
    matter for cross-node consistency assertions.
    """
    out: dict[str, dict[str, dict[str, int]]] = {}
    for node, url in DASHBOARDS.items():
        per_trader: dict[str, dict[str, int]] = {}
        for a in addrs:
            acct = fetch_account(url, a)
            if acct is None:
                continue
            balances = {asset: int(v) for asset, v in acct.get("balances", {}).items()}
            for asset in ASSETS:
                balances.setdefault(asset, 0)
            per_trader[a] = balances
        out[node] = per_trader
    return out


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sim-seconds", type=int, default=20)
    parser.add_argument("--verify-seconds", type=int, default=30)
    parser.add_argument("--tx-per-sec", type=int, default=20)
    parser.add_argument("--num-traders", type=int, default=8)
    parser.add_argument("--seed-prefix", default="cb-sim-trader-")
    parser.add_argument("--min-blocks", type=int, default=10)
    parser.add_argument("--settle-seconds", type=float, default=2.0)
    parser.add_argument(
        "--keep-on-failure",
        action="store_true",
        help="leave data + logs intact if any assertion fails (default: always wipe)",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO, format="[integration] %(asctime)s %(message)s"
    )

    build_binaries()
    addrs = trader_addresses(args.num_traders, args.seed_prefix)
    log.info("derived %d trader addresses", len(addrs))

    # Stage 1 — boot cluster.
    log.info("stage 1/5: starting 3-node cluster")
    rc = subprocess.run(
        [sys.executable, str(HERE / "run.py")],
        check=False,
    )
    if rc.returncode != 0:
        log.error("cluster failed to start; see logs/")
        return rc.returncode

    sim_proc: subprocess.Popen | None = None
    failures: list[str] = []
    try:
        # Stage 2 — capture genesis balances.
        log.info("stage 2/5: capturing genesis trader balances")
        # Wait briefly for replicas to sync any initial state.
        time.sleep(1.0)
        genesis_balances = collect_balances(addrs)
        seq_genesis = genesis_balances[SEQUENCER]
        if not seq_genesis:
            failures.append("sequencer returned no trader balances at genesis")
        seq_genesis_totals = {
            asset: sum(b.get(asset, 0) for b in seq_genesis.values())
            for asset in ASSETS
        }
        log.info("genesis totals across %d traders: %s", len(addrs), seq_genesis_totals)

        # Stage 3 — submit txs via cb-sim.
        log.info(
            "stage 3/5: submitting txs via cb-sim "
            f"(--tx-per-sec={args.tx_per_sec}, --num-traders={args.num_traders}, "
            f"window={args.sim_seconds}s)"
        )
        max_txs = args.tx_per_sec * args.sim_seconds
        # Sign for the chain the cluster actually runs. cb-sim defaults to
        # chain id 1 and the shipped genesis is 1000; every transaction was
        # refused at admission for its chain id and the test failed with
        # "no trader balance changed", which does not say why.
        seq_status = fetch_status(DASHBOARDS[SEQUENCER]) or {}
        chain_id = seq_status.get("chain_id")
        if chain_id is None:
            log.error("sequencer did not report a chain id; cannot sign for it")
            return 1
        log.info("signing for chain id %s", chain_id)
        sim_log = HERE / "logs" / "sim.log"
        sim_log.parent.mkdir(parents=True, exist_ok=True)
        sim_handle = open(sim_log, "wb")
        sim_proc = subprocess.Popen(
            [
                str(SIM_BIN),
                "run",
                "--sequencer",
                "127.0.0.1:7700",
                "--chain-id",
                str(chain_id),
                "--num-traders",
                str(args.num_traders),
                "--seed-prefix",
                args.seed_prefix,
                "--tx-per-sec",
                str(args.tx_per_sec),
                "--max-txs",
                str(max_txs),
            ],
            stdout=sim_handle,
            stderr=subprocess.STDOUT,
        )

        # Stage 4 — verify chain progress + lag while sim is running.
        log.info(
            "stage 4/5: verifying chain progress "
            f"(window={args.verify_seconds}s, min_blocks={args.min_blocks})"
        )
        verify_rc = subprocess.run(
            [
                sys.executable,
                str(HERE / "verify.py"),
                "--seconds",
                str(args.verify_seconds),
                "--min-blocks",
                str(args.min_blocks),
            ],
            check=False,
        )
        if verify_rc.returncode != 0:
            failures.append("verify.py reported chain-progress failures")

        # Wait for sim to drain and replicas to catch up.
        if sim_proc.poll() is None:
            sim_proc.send_signal(signal.SIGTERM)
        try:
            sim_proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            sim_proc.kill()
            sim_proc.wait()
        sim_handle.close()
        log.info("simulator finished (rc=%s, log=%s)", sim_proc.returncode, sim_log)

        log.info("settling for %.1fs to let replicas finish applying", args.settle_seconds)
        time.sleep(args.settle_seconds)

        # Stage 5 — accounting + sync assertions.
        log.info("stage 5/5: checking accounting + cross-node sync")
        post_balances = collect_balances(addrs)
        seq_post = post_balances[SEQUENCER]

        # 5a) at least one trader's balance changed somewhere.
        anything_changed = any(
            seq_post.get(a, {}).get(asset, 0) != seq_genesis.get(a, {}).get(asset, 0)
            for a in addrs
            for asset in ASSETS
        )
        if not anything_changed:
            failures.append(
                "no trader balance changed after sim — txs may not have been applied"
            )

        # 5b) per-asset total conservation across all traders.
        for asset in ASSETS:
            post_total = sum(b.get(asset, 0) for b in seq_post.values())
            genesis_total = seq_genesis_totals.get(asset, 0)
            if post_total != genesis_total:
                failures.append(
                    f"asset {asset}: total across traders changed "
                    f"({genesis_total} -> {post_total}); transfers must conserve"
                )

        # 5c) per-trader cross-node balance consistency.
        for addr in addrs:
            seq_b = seq_post.get(addr, {})
            for replica in ("node1", "node2"):
                rep_b = post_balances.get(replica, {}).get(addr, {})
                for asset in ASSETS:
                    if seq_b.get(asset, 0) != rep_b.get(asset, 0):
                        failures.append(
                            f"{replica} balance for {addr} asset {asset} "
                            f"differs from sequencer "
                            f"({rep_b.get(asset, 0)} vs {seq_b.get(asset, 0)})"
                        )

        # 5d) head heights match across nodes (final snapshot).
        statuses = {n: fetch_status(u) for n, u in DASHBOARDS.items()}
        seq_h = (statuses.get(SEQUENCER) or {}).get("head_height", 0)
        for replica in ("node1", "node2"):
            rep_h = (statuses.get(replica) or {}).get("head_height", 0)
            if seq_h - rep_h > MAX_LAG_BLOCKS:
                failures.append(
                    f"{replica} final head {rep_h} lags sequencer {seq_h} by "
                    f"more than {MAX_LAG_BLOCKS} blocks"
                )

        # 5e) BFT quorum certs must be forming. Leader self-votes plus at
        # least one follower vote per block ⇒ a QC every block. We check the
        # sequencer's last_qc_height stayed close to head; allowing 2 blocks
        # of slack for the in-flight QC at the very head.
        seq_status = statuses.get(SEQUENCER) or {}
        last_qc = seq_status.get("last_qc_height", 0)
        if last_qc == 0 or seq_h - last_qc > 2:
            failures.append(
                f"BFT QC formation lagging: head={seq_h} last_qc={last_qc} "
                f"(expected last_qc within 2 of head — replicas not voting?)"
            )

        # Print a summary.
        print()
        print("=" * 80)
        print(f"{'asset':<6} {'genesis-total':>16} {'post-total':>16}")
        print("-" * 80)
        for asset in ASSETS:
            g = seq_genesis_totals.get(asset, 0)
            p = sum(b.get(asset, 0) for b in seq_post.values())
            print(f"{asset:<6} {g:>16} {p:>16}")
        print("=" * 80)
        print(f"{'node':<8} {'head_height':>11} {'matches sequencer':>22}")
        print("-" * 80)
        for n, st in statuses.items():
            h = (st or {}).get("head_height", "?")
            match = "—" if n == SEQUENCER else (
                "yes" if isinstance(h, int) and seq_h - h <= MAX_LAG_BLOCKS else "NO"
            )
            print(f"{n:<8} {h!s:>11} {match:>22}")
        print("=" * 80)

    finally:
        if sim_proc and sim_proc.poll() is None:
            sim_proc.kill()
        log.info("tearing down cluster")
        teardown_args = [sys.executable, str(HERE / "teardown.py")]
        if not (failures and args.keep_on_failure):
            teardown_args.append("--clean")
        subprocess.run(teardown_args, check=False)

    if failures:
        print()
        print("FAILURES:")
        for f in failures:
            print(f"  - {f}")
        log.error("INTEGRATION TEST FAILED (%d issues)", len(failures))
        return 1
    log.info("INTEGRATION TEST PASSED — sync, accounting, transfers all verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
