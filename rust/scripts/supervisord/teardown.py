"""Tear down the supervisord cluster started by run.py."""

from __future__ import annotations

import argparse
import logging
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOGS_DIR = HERE / "logs"
DATA_DIR = HERE / "data"
PID_FILE = LOGS_DIR / "supervisord.pid"
SOCK = Path("/tmp/cb-supervisord.sock")

log = logging.getLogger("teardown")


def shutdown_supervisord() -> None:
    # Try the unix-socket path first using both conf files (whichever supervisord
    # was started with — `make stop` doesn't know which target launched it).
    if SOCK.exists():
        for conf_name in ("supervisord.conf", "supervisord_benchmark.conf"):
            conf_path = HERE / conf_name
            if not conf_path.exists():
                continue
            log.info("shutting down via supervisorctl (%s)", conf_name)
            subprocess.run(
                ["supervisorctl", "-c", str(conf_path), "shutdown"],
                cwd=HERE,
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        time.sleep(1)

    if PID_FILE.exists():
        try:
            pid = int(PID_FILE.read_text().strip())
            os.kill(pid, signal.SIGTERM)
            log.info("sent SIGTERM to supervisord pid %d", pid)
            time.sleep(1)
        except (ValueError, ProcessLookupError):
            pass
        try:
            PID_FILE.unlink()
        except FileNotFoundError:
            pass

    # Defensive sweep — match any supervisord serving either conf.
    subprocess.run(
        ["pkill", "-f", "supervisord.*supervisord.*\\.conf"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        ["pkill", "-f", "target/release/cb-node"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--clean",
        action="store_true",
        help="also wipe ./data and ./logs",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="[teardown] %(message)s")

    shutdown_supervisord()

    if args.clean:
        for d in (DATA_DIR, LOGS_DIR):
            if d.exists():
                log.info("removing %s", d.relative_to(HERE))
                shutil.rmtree(d)

    log.info("teardown complete")
    return 0


if __name__ == "__main__":
    sys.exit(main())
