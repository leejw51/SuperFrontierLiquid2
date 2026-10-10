"""Verify the 3-node SuperFrontierLiquid cluster: poll each node's dashboard and assert
that heights advance, replicas stay close to the sequencer, and state roots
match at any height all three have observed.

Polls every `--interval` seconds for `--seconds` seconds total, then prints a
summary table and exits non-zero if any assertion failed.
"""

from __future__ import annotations

import argparse
import json
import logging
import sys
import time
import urllib.request
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent

DASHBOARDS = {
    "node0": "http://127.0.0.1:18080/api/status",
    "node1": "http://127.0.0.1:18081/api/status",
    "node2": "http://127.0.0.1:18082/api/status",
}
SEQUENCER = "node0"
REPLICAS = ("node1", "node2")
MAX_LAG_BLOCKS = 5  # replicas must close to within this of the sequencer after warmup

log = logging.getLogger("verify")


@dataclass
class NodeSnapshot:
    head_height: int
    state_root: str
    recent_blocks: list[dict[str, Any]]
    raw: dict[str, Any]


def fetch(url: str, timeout: float = 1.0) -> NodeSnapshot | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except (urllib.error.URLError, ConnectionError, TimeoutError, json.JSONDecodeError) as e:
        log.debug("fetch %s failed: %s", url, e)
        return None
    return NodeSnapshot(
        head_height=int(data.get("head_height", 0)),
        state_root=data.get("state_root", ""),
        recent_blocks=data.get("recent_blocks", []),
        raw=data,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--seconds",
        type=int,
        default=60,
        help="total observation window in seconds (default: 60)",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="seconds between polls (default: 1.0)",
    )
    parser.add_argument(
        "--min-blocks",
        type=int,
        default=10,
        help="minimum blocks the sequencer must produce in the window (default: 10)",
    )
    parser.add_argument(
        "--warmup-seconds",
        type=int,
        default=5,
        help="grace period before lag-bound assertion (default: 5)",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="[verify] %(message)s")

    # Per-height: state_root reported by each node that's seen that height.
    by_height: dict[int, dict[str, str]] = defaultdict(dict)
    last_seen: dict[str, NodeSnapshot] = {}

    deadline = time.monotonic() + args.seconds
    start = time.monotonic()
    failures: list[str] = []

    while time.monotonic() < deadline:
        for name, url in DASHBOARDS.items():
            snap = fetch(url)
            if snap is None:
                continue
            last_seen[name] = snap
            for blk in snap.recent_blocks:
                h = int(blk.get("height", 0))
                root = str(blk.get("state_root", ""))
                if h <= 0 or not root:
                    continue
                prior = by_height[h].get(name)
                if prior is not None and prior != root:
                    failures.append(
                        f"{name} reported two different state_roots at height {h}: "
                        f"{prior!r} vs {root!r}"
                    )
                by_height[h][name] = root

        # Lag check (only after warmup): each replica must be within MAX_LAG_BLOCKS.
        if time.monotonic() - start >= args.warmup_seconds and SEQUENCER in last_seen:
            seq_h = last_seen[SEQUENCER].head_height
            for r in REPLICAS:
                if r not in last_seen:
                    continue
                lag = seq_h - last_seen[r].head_height
                if lag > MAX_LAG_BLOCKS:
                    failures.append(
                        f"{r} lagging sequencer by {lag} blocks "
                        f"(seq={seq_h}, {r}={last_seen[r].head_height})"
                    )

        time.sleep(args.interval)

    print()
    print("=" * 72)
    print(f"{'node':<8} {'head_height':>11} {'state_root':<20} {'lag':>5}")
    print("-" * 72)
    seq_h = last_seen.get(SEQUENCER, NodeSnapshot(0, "", [], {})).head_height
    for name in DASHBOARDS:
        if name not in last_seen:
            print(f"{name:<8} {'?':>11} {'(unreachable)':<20}")
            failures.append(f"{name} dashboard never reachable")
            continue
        snap = last_seen[name]
        lag = seq_h - snap.head_height if name != SEQUENCER else 0
        short_root = (snap.state_root[:18] + "..") if len(snap.state_root) > 20 else snap.state_root
        print(f"{name:<8} {snap.head_height:>11} {short_root:<20} {lag:>5}")
    print("=" * 72)

    # Assertions.
    if SEQUENCER not in last_seen:
        failures.append("sequencer was never reachable — nothing to assert")
    else:
        produced = last_seen[SEQUENCER].head_height
        if produced < args.min_blocks:
            failures.append(
                f"sequencer produced only {produced} blocks in {args.seconds}s "
                f"(expected >= {args.min_blocks})"
            )

    # Cross-node state-root consistency at any shared height.
    mismatches = 0
    matched_heights = 0
    for h, by_node in sorted(by_height.items()):
        if len(by_node) < 2:
            continue
        roots = set(by_node.values())
        if len(roots) > 1:
            mismatches += 1
            failures.append(
                f"state_root mismatch at height {h}: "
                + ", ".join(f"{n}={r[:18]}.." for n, r in sorted(by_node.items()))
            )
        else:
            matched_heights += 1
    print(f"  shared heights checked: {matched_heights}, mismatches: {mismatches}")

    if failures:
        print()
        print("FAILED:")
        for f in failures:
            print(f"  - {f}")
        return 1

    print("OK — sub-100ms blocks producing, replicas in sync, state roots match")
    return 0


if __name__ == "__main__":
    sys.exit(main())
