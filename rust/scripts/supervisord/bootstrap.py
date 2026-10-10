"""Bootstrap a fresh 3-node data directory layout for the supervisord harness.

Wipes ./data and ./logs (unless --keep is given), builds the release binary if
missing, and runs `cb-node init` for each of the three nodes against the repo
genesis.jsonl. Idempotent when run with --keep on an already-initialised cluster
(skips the cargo build and the init step that would otherwise refuse to clobber
the existing rocksdb).
"""

from __future__ import annotations

import argparse
import logging
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
NODE_BIN = REPO_ROOT / "target" / "release" / "cb-node"
GENESIS = REPO_ROOT / "genesis.jsonl"
DATA_DIR = HERE / "data"
LOGS_DIR = HERE / "logs"
NODE_NAMES = ("node0", "node1", "node2")

log = logging.getLogger("bootstrap")


def build_binary() -> None:
    if NODE_BIN.exists():
        log.info("cb-node binary already built at %s", NODE_BIN)
        return
    log.info("building cb-node (release) — this can take a couple of minutes")
    subprocess.run(
        ["cargo", "build", "--release", "-p", "cb-node"],
        cwd=REPO_ROOT,
        check=True,
    )


def wipe_dirs() -> None:
    for d in (DATA_DIR, LOGS_DIR):
        if d.exists():
            log.info("wiping %s", d.relative_to(HERE))
            shutil.rmtree(d)


def init_node(name: str) -> None:
    data_dir = DATA_DIR / name
    if (data_dir / "CURRENT").exists():
        log.info("%s already initialised; skipping", name)
        return
    data_dir.mkdir(parents=True, exist_ok=True)
    log.info("initialising %s with genesis %s", name, GENESIS.name)
    subprocess.run(
        [
            str(NODE_BIN),
            "--data-dir",
            str(data_dir),
            "init",
            "--genesis",
            str(GENESIS),
        ],
        check=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--keep",
        action="store_true",
        help="don't wipe ./data and ./logs (resume an existing cluster)",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="[bootstrap] %(message)s")

    if not GENESIS.exists():
        log.error("genesis.jsonl not found at %s", GENESIS)
        return 2

    if not args.keep:
        wipe_dirs()

    build_binary()
    LOGS_DIR.mkdir(parents=True, exist_ok=True)
    DATA_DIR.mkdir(parents=True, exist_ok=True)

    for name in NODE_NAMES:
        init_node(name)

    log.info("bootstrap complete: 3 nodes ready under %s", DATA_DIR)
    return 0


if __name__ == "__main__":
    sys.exit(main())
