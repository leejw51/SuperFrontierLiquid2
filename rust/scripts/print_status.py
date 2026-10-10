#!/usr/bin/env python3
"""Format the JSON returned by the sequencer's /api/status into a compact human view."""

import json
import sys


def fmt_int(n) -> str:
    try:
        return f"{int(n):,}"
    except (TypeError, ValueError):
        return str(n)


def fmt_rate(v) -> str:
    try:
        v = float(v)
    except (TypeError, ValueError):
        return str(v)
    if abs(v) >= 1_000_000:
        return f"{v / 1_000_000:.2f} M"
    if abs(v) >= 1_000:
        return f"{v / 1_000:.1f} K"
    return f"{v:.1f}"


def main() -> int:
    raw = sys.stdin.read()
    if not raw.strip():
        print("  dashboard returned empty response")
        return 1
    try:
        d = json.loads(raw)
    except json.JSONDecodeError as e:
        print(f"  could not parse dashboard response: {e}")
        return 1

    print(f"  chain id:     {d['chain_id']}")
    print(f"  head height:  {fmt_int(d['head_height'])}")
    print(f"  head hash:    {d['head_hash']}")
    print(f"  state root:   {d['state_root']}")
    print(f"  mempool:      {'empty' if d['mempool_empty'] else 'non-empty'}")
    print(f"  company:      {d['company_addr']}")

    m = d.get("metrics") or {}
    if m.get("running"):
        print()
        print("## throughput")
        print(f"  recent tps:   {fmt_rate(m.get('recent_tps', 0))} (5s window)")
        print(f"  avg tps:      {fmt_rate(m.get('avg_tps', 0))}")
        print(f"  elapsed:      {m.get('elapsed_secs', 0):.1f}s")
        print()
        print("## orders / s")
        print(f"  total:        {fmt_rate(m.get('recent_orders_per_sec', 0))}")
        print(f"  buys:         {fmt_rate(m.get('recent_buys_per_sec', 0))}")
        print(f"  sells:        {fmt_rate(m.get('recent_sells_per_sec', 0))}")
        print(f"  BTC/USD:      {fmt_rate(m.get('recent_btc_per_sec', 0))}")
        print(f"  ETH/USD:      {fmt_rate(m.get('recent_eth_per_sec', 0))}")
        print()
        print("## pipeline")
        submitted = m.get("submitted", 0)
        applied = m.get("applied", 0)
        inflight = max(submitted - applied, 0)
        print(
            f"  txs:          submitted={fmt_int(submitted)} "
            f"applied={fmt_int(applied)} inflight={fmt_int(inflight)} "
            f"rejected={fmt_int(m.get('rejected', 0))}"
        )
        print(
            f"  blocks:       {fmt_int(m.get('blocks', 0))} "
            f"(avg {m.get('block_txs_avg', 0):.0f} txs · "
            f"min {fmt_int(m.get('block_txs_min', 0))} · "
            f"max {fmt_int(m.get('block_txs_max', 0))} · "
            f"last {fmt_int(m.get('block_txs_last', 0))})"
        )
        print(
            f"  buys/sells:   {fmt_int(m.get('buys', 0))} / {fmt_int(m.get('sells', 0))}"
        )
        print(
            f"  BTC/ETH:      {fmt_int(m.get('btc_orders', 0))} / {fmt_int(m.get('eth_orders', 0))}"
        )
        print(f"  channel:      {fmt_int(m.get('channel_depth', 0))}")
    print()
    print("## recent blocks")
    rows = d.get("recent_blocks", [])
    if not rows:
        print("  (no blocks yet)")
    else:
        for b in rows:
            print(
                f"  height={b['height']:>6}  txs={b['tx_count']:>5}  "
                f"ts={b['timestamp']}  hash={b['hash'][:18]}..."
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
