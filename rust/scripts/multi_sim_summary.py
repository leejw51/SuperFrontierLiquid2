#!/usr/bin/env python3
"""Pretty-print one /api/status response for `make startmultisimulation`.

Reads the JSON body from stdin and the dashboard port from $PORT, then prints
a single line summarising the apply-side metrics this node observed during the
stress run. The interesting field is `tps_60s` — that's our "sustained" figure
to compare against Hyperliquid's published baseline.
"""

import json
import os
import sys


def main() -> int:
    try:
        d = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        print(f"  parse error: {e}", file=sys.stderr)
        return 1

    m = d.get("metrics", {}) or {}
    port = os.environ.get("PORT", "?")
    head = d.get("head_height", "?")
    last_block_txs = m.get("block_txs_last", 0)
    tps_5s = m.get("tps_5s", 0.0) or 0.0
    tps_60s = m.get("tps_60s", 0.0) or 0.0
    avg_tps = m.get("avg_tps", 0.0) or 0.0

    print(
        f"  port {port}: head={head}"
        f" last_block_txs={last_block_txs}"
        f" tps_5s={tps_5s:.0f}"
        f" tps_60s={tps_60s:.0f}"
        f" avg_tps={avg_tps:.0f}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
