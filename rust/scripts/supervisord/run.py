"""Boot the 3-node SuperFrontierLiquid cluster under supervisord.

Cleans up any leftover supervisord process bound to the same socket, calls
bootstrap.py (unless --keep is given), launches `supervisord -c <conf>` in the
background, and waits for node0's dashboard to become reachable so the caller
knows it's safe to run verify.py.

Defaults to supervisord.conf (RPC sim mode); pass --conf supervisord_benchmark.conf
for the in-process-benchmark leader variant used by `make startmultibenchmark`.
"""

from __future__ import annotations

import argparse
import logging
import os
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOGS_DIR = HERE / "logs"
PID_FILE = LOGS_DIR / "supervisord.pid"
DEFAULT_CONF = HERE / "supervisord.conf"
DASHBOARDS = {
    "node0": ("127.0.0.1", 18080),
    "node1": ("127.0.0.1", 18081),
    "node2": ("127.0.0.1", 18082),
}

log = logging.getLogger("run")


def kill_existing(conf: Path) -> None:
    """Kill a previous supervisord left over from an earlier run."""
    if PID_FILE.exists():
        try:
            pid = int(PID_FILE.read_text().strip())
            os.kill(pid, signal.SIGTERM)
            log.info("sent SIGTERM to leftover supervisord pid %d", pid)
            time.sleep(1)
        except (ValueError, ProcessLookupError):
            pass
        try:
            PID_FILE.unlink()
        except FileNotFoundError:
            pass

    # Defensive sweep: any supervisord pointing at our conf file.
    subprocess.run(
        ["pkill", "-f", f"supervisord.*{conf.name}"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    # Plus any stray cb-node processes from the prior cluster.
    subprocess.run(
        ["pkill", "-f", "target/release/cb-node"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(0.5)


def wait_for_port(host: str, port: int, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--conf",
        default=str(DEFAULT_CONF),
        help="path to supervisord conf (default: supervisord.conf)",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="skip bootstrap (preserve ./data and ./logs from previous run)",
    )
    parser.add_argument(
        "--ready-timeout",
        type=float,
        default=20.0,
        help="seconds to wait for node0's dashboard before failing (default: 20)",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="[run] %(message)s")

    conf = Path(args.conf).resolve()
    if not conf.exists():
        log.error("supervisord conf not found: %s", conf)
        return 2

    kill_existing(conf)

    bootstrap_cmd = [sys.executable, str(HERE / "bootstrap.py")]
    if args.keep:
        bootstrap_cmd.append("--keep")
    subprocess.run(bootstrap_cmd, check=True)

    LOGS_DIR.mkdir(parents=True, exist_ok=True)

    log.info("starting supervisord with %s", conf.name)
    subprocess.run(
        ["supervisord", "-c", str(conf)],
        cwd=HERE,
        check=True,
    )

    host, port = DASHBOARDS["node0"]
    log.info("waiting for node0 dashboard at %s:%d", host, port)
    if not wait_for_port(host, port, args.ready_timeout):
        log.error("node0 dashboard never came up — see logs/node0.err.log")
        return 1

    log.info("cluster ready — dashboards:")
    # Only advertise dashboards for programs the conf actually has — the
    # benchmark conf is 2-node, the BFT-integration conf is 3-node.
    conf_text = conf.read_text()
    for name, (h, p) in DASHBOARDS.items():
        if f"[program:{name}]" in conf_text:
            log.info("  %s → http://%s:%d/", name, h, p)
    log.info("logs in %s", LOGS_DIR)
    log.info("teardown: python3 teardown.py [--clean]")
    return 0


if __name__ == "__main__":
    sys.exit(main())
