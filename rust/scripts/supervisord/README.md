# SuperFrontierLiquid supervisord harness

3-node local cluster orchestrated with [supervisord](http://supervisord.org/), used
to verify block production, replica sync, and (eventually) BFT consensus failover.

Today's topology:

| node  | role      | RPC port | dashboard port |
|-------|-----------|---------:|---------------:|
| node0 | sequencer | 7700     | 18080          |
| node1 | replica   | —        | 18081          |
| node2 | replica   | —        | 18082          |

The sequencer signs blocks with the dev key derived from the seed `cb-validator-0`
(matches the `validator` records in `genesis.jsonl`). Replicas verify the signature
via the proposer address persisted at genesis.

## Prerequisites

```bash
pip3 install supervisor
```

`cargo`, `python3 >= 3.11`. The Rust binary is built on demand.

## Quick start

```bash
# from repo root
make supervisord-test
```

That target runs `up → verify → down` and exits non-zero on failure. Equivalent
breakdown:

```bash
python3 scripts/supervisord/run.py
python3 scripts/supervisord/verify.py --seconds 60 --min-blocks 10
python3 scripts/supervisord/teardown.py --clean
```

## Files

- `supervisord.conf` — three `[program:nodeN]` blocks
- `bootstrap.py` — wipes data dirs, builds the binary if needed, runs `cb-node init`
  for each node against the repo `genesis.jsonl`
- `run.py` — kills any leftover supervisord, calls `bootstrap.py`, starts
  supervisord, waits for node0's dashboard
- `verify.py` — polls each node's `/api/status`, asserts heights advance, replicas
  stay within 5 blocks, state roots match at shared heights
- `teardown.py` — `supervisorctl shutdown` + pkill fallback; `--clean` also wipes
  data + logs

## Logs

```
scripts/supervisord/logs/
├── supervisord.log
├── node0.out.log / node0.err.log   # sequencer (tracing writes to stderr)
├── node1.out.log / node1.err.log   # replica
└── node2.out.log / node2.err.log   # replica
```

The Rust `tracing` subscriber writes structured logs to stderr — that's where you'll
see `produced block height=N txs=M` (sequencer) and `replica applied block` (replicas).

## Manual ops

```bash
# attach supervisorctl to the running cluster
supervisorctl -c scripts/supervisord/supervisord.conf

# tail one node's output
tail -f scripts/supervisord/logs/node0.err.log

# poke the sequencer dashboard
curl -s http://127.0.0.1:18080/api/status | python3 -m json.tool
```

## Troubleshooting

- **node0 dashboard never came up** — usually means the binary panicked on init.
  Check `logs/node0.err.log` first, then `logs/supervisord.log`.
- **port already in use** — another supervisord or stray `cb-node` process is alive.
  Run `python3 teardown.py --clean` to wipe everything.
- **replicas stuck at height 0** — verify the sequencer is producing blocks
  (its `head_height` should advance) and that the sequencer's RPC port (7700) is
  reachable from the replicas.
