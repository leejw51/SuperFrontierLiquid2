# Operator runbook

Everything here was run against this repository before it was written down,
except where a line says it was not. Where a command could not be verified on
the writer's machine, it says so and explains why — treat those as documented
intent, not as tested procedure.

All commands run from `rust/` unless stated otherwise.

---

## 1. Before you start

You need a Rust toolchain (the workspace pins stable via `rust-toolchain.toml`),
the Cap'n Proto compiler (`capnp`), Python 3, and `curl`. For the frontend,
`cargo install trunk`. For the three-node cluster, `pip3 install supervisor`.

The first release build takes several minutes — RocksDB and the Cap'n Proto
codegen dominate. This is a cold-cache cost, not a per-run cost.

```bash
make build          # release cb-node
make build-client   # release cb-client
```

### The data directory

A node owns one directory, given by `--data-dir`. It holds the RocksDB database
and nothing else worth keeping by hand. `make` uses `./data` for the spot chain
and `./data-perps` for the perps chain, and both are gitignored.

RocksDB takes an exclusive lock on that directory. One process per data
directory, always.

### Genesis needs environment variables

`genesis.jsonl` contains `${VAR}` tokens — chain id, denomination, the four
wallet addresses, the initial credit. They are substituted before the file is
parsed. A referenced variable that is set nowhere is a hard error, deliberately:
a silently empty address would produce a chain whose funds nobody controls.

The node auto-loads a `.env` sitting next to the genesis file, and a real shell
variable always beats the file. `make env` writes `rust/.env` from the tracked
`rust/.env.example`; `make check-env` runs before `make init` and creates or
repairs it as needed. You do not normally need to run either yourself — §10
covers the case where you do.

---

## 2. A single node

Two genesis files, two chains, same ports. `make start` boots the spot-only
chain from `genesis.jsonl` into `./data`; `make startperps` boots the fuller
one from `genesis-perps.jsonl` into `./data-perps`, adding perpetuals, a dated
future, an insurance fund and pre-funded demo traders. Use the perps chain if
you want to trade by hand — §3 explains why. Run one at a time.

```bash
make start                 # builds if needed, applies genesis if needed, runs
make status
make stop
```

`make start` prints the RPC address, the dashboard URL and the log path. The
defaults are RPC on `127.0.0.1:7700`, dashboard on `http://127.0.0.1:8080`, a
100 ms block interval.

Override anything on the command line:

```bash
make start DATA_DIR=/tmp/chain1 RPC_LISTEN=127.0.0.1:7711 DASH_LISTEN=127.0.0.1:8099
```

Check it is alive:

```bash
curl -s http://127.0.0.1:8080/api/status | python3 -m json.tool | head -20
```

### The node is silent by default

`cb-node` logs through `tracing_subscriber`'s default filter, which prints
nothing unless `RUST_LOG` is set. An empty `sequencer.log` does not mean the
node is broken.

```bash
RUST_LOG=info make start
make logs                  # tail -f the log
```

### Height stays at zero

A block is only produced when there is at least one transaction. An idle chain
does not tick. Submit something and the height moves.

### Doing it by hand

```bash
./target/release/cb-node --data-dir ./data init --genesis genesis.jsonl
./target/release/cb-node --data-dir ./data sequencer \
    --listen 127.0.0.1:7700 --dashboard-listen 127.0.0.1:8080 --block-interval-ms 100
```

`--data-dir` is a global flag and must come **before** the subcommand.

### Derivative markets need the oracle running

`cb-oracle` is the price service for the derivative half of the exchange. It
discovers every perpetual and dated future the node lists, takes prices from
the same seeded simulator the rest of the project uses (`cb-feed`), and signs
and submits one `OracleUpdate` per market per second with the authority key in
`rust/.env`. **Nothing else writes an oracle price**, and the executor only
recomputes a market's mark price when an `OracleUpdate` lands — so without this
running, every derivative market reports a zero mark, no position can be
opened, funding never accrues and a dated future cannot settle.

`make up` and `make -C rust dev` start it alongside the node. To run it on its
own, or against a node on other ports:

```bash
make oracle                                        # background, logs to ./data/oracle.log
make -C rust oracle-logs                           # tail them
make oracle RPC_LISTEN=127.0.0.1:7770 ORACLE_SCENARIO=crash ORACLE_SEED=7
```

`ORACLE_PERIOD_MS` (default 1000), `ORACLE_SEED`, `ORACLE_SCENARIO` (`calm`,
`trend_up`, `trend_down`, `crash`, `squeeze`, `chop`) and `ORACLE_CONTROL` are
all overridable. A given seed replays the same price path, which is what makes
a liquidation reproducible rather than merely reported.

The scenario is also changeable while it runs, so an experiment does not have
to restart its price paths to see a crash:

```bash
curl -s http://127.0.0.1:8171/health
curl -s http://127.0.0.1:8171/scenario
curl -s -XPOST -H 'content-type: application/json' \
     -d '{"scenario":"crash","symbol":"BTC-USD"}' http://127.0.0.1:8171/scenario
```

It survives the node going away: it reconnects with backoff, re-reads its nonce
and its height from the chain, and resumes. It also refuses to publish a price
the feed has stopped updating — a stale price presented as a fresh one is what
liquidates somebody by mistake — and says so in the log rather than silently
republishing the last number.

Check it is working:

```bash
./target/release/cb-client --rpc 127.0.0.1:7700 mark perp:0
# and, end to end, open a real position with two genesis-funded demo traders:
cargo run --release -p cb-oracle --example open_position -- 127.0.0.1:7700 1000
```

### The perpetuals chain

`genesis-perps.jsonl` is a superset of the spot genesis: the same validators and
spot markets, plus two perpetuals, a dated future, an insurance fund, pre-funded
demo margin accounts and separate oracle authorities. It gets its own data
directory so both chains can exist on disk at once — but they share the same
default ports, so only one runs at a time.

```bash
make startperps      # ./data-perps, RPC 7700, dashboard 8080
make perps-logs
make stopperps
```

`make startperps` derives its genesis environment by evaluating
`scripts/perps_env.sh`, which prints addresses derived from BIP-39 mnemonics.
That is what makes its accounts drivable from `cb-client` — see §3.

---

## 3. Trading against it

Today the working client is `cb-client`. The REST API in `docs/api-contract.md`
is not served by anything yet, so the faucet, order entry and portfolio calls
described there cannot be used; the equivalents below can.

**Use the perpetuals chain for this.** `make startperps` derives its genesis
wallets from BIP-39 mnemonics printed by `scripts/perps_env.sh`, which is what
`cb-client login` consumes, so you can control the funded accounts. The default
spot chain funds addresses derived from bare seed strings in `rust/.env`, and
`cb-client` cannot derive those — the faucet on that chain will submit a
transfer from an account it does not control, which is rejected and never
produces a block. Both chains use the same ports, so stop one before starting
the other.

```bash
make stop && make startperps
```

`cb-client` keeps its wallet store under `$HOME`. Point `HOME` at a scratch
directory if you do not want it touching your own.

```bash
export HOME=/tmp/cb-operator && mkdir -p $HOME
BIN=./target/release/cb-client
RPC=127.0.0.1:7700
```

### Create a wallet

```bash
set -a; eval "$(scripts/perps_env.sh)"; set +a     # dev mnemonics, not secrets
$BIN --rpc $RPC login --label alice --mnemonic "$MYWALLET3_MNEMONIC" --index 5 --force
```

Any valid BIP-39 phrase works; the deriver rejects anything that fails the
checksum.

### Claim from the faucet

```bash
$BIN --rpc $RPC faucet
```

The faucet signs a `Transfer` from the community wallet using
`UL_FAUCET_MNEMONIC`, falling back to `MYWALLET3_MNEMONIC` — both supplied by
`scripts/perps_env.sh`. It prints the sender, the recipient and the transaction
hash. The default grant is 1,000,000,000,000 raw units; pass an amount to
override.

Balances appear once the next block is produced:

```bash
$BIN --rpc $RPC balance
```

`uld` in that table is the chain's USD asset shown under its display
denomination. There is no cooldown and no rate limit on this path — the
cooldown described in the API contract belongs to the gateway, which does not
exist yet.

### Look at the markets and trade

```bash
$BIN --rpc $RPC markets
$BIN --rpc $RPC book spot:btc_usd 10
$BIN --rpc $RPC place spot:btc_usd buy 1000000 3000000000000
$BIN --rpc $RPC position
```

Market references are `spot:<id>`, `perp:<N>` or `future:<N>` — `markets` prints
the exact strings. Quantities and prices are raw integers at the chain's scales,
not decimals: the order above is 0.01 BTC at 30,000 USD. `place` also takes
`gtc`, `ioc`, `fok`, `post-only` and `reduce-only` as trailing flags.

### Derivatives need margin, and the client cannot move it

A perpetual or futures order is admitted against the account's margin
collateral, not its spot balance, and `cb-client` has no command that funds a
margin account — there is an `AdjustMargin` transaction, but nothing exposes it.
In practice this means trading derivatives by hand requires an account the
genesis pre-funded. `genesis-perps.jsonl` provides three:

```bash
$BIN --rpc $RPC login --label demo0 \
     --mnemonic "$DEMO_TRADER0_MNEMONIC" --index $DEMO_TRADER0_INDEX --force
$BIN --rpc $RPC margin                    # collateral, unrealized PnL, bad debt
make oraclehit MARKET=BTC PRICE=50000     # a derivative needs a fresh mark price
$BIN --rpc $RPC place perp:0 buy 100000 4990000000000
$BIN --rpc $RPC book perp:0 5
```

An order placed by an account with zero collateral is rejected at execution and
simply never appears on the book.

`make client ARGS="wallets"` runs the same binary through the Makefile, and
`make client` with no arguments drops into the REPL.

---

## 4. A three-validator cluster

**Not verified on the writer's machine: `supervisord` was not installed.** The
commands and topology below come from `scripts/supervisord/`, which is where to
look if something misbehaves.

```bash
pip3 install supervisor
make startmulti
```

Topology: node0 is the sequencer, RPC on 7700, dashboard on 18080. node1 and
node2 are replicas with no RPC listener, dashboards on 18081 and 18082. All
three are initialised from the same `genesis.jsonl`.

```bash
python3 scripts/supervisord/verify.py --seconds 60 --min-blocks 10
make testmulti          # bootstrap, run, assert sync, tear down
make stop               # tears the cluster down along with everything else
```

### Replicas do not verify by default

A replica started without `--strict-sync` runs in fast-mirror mode: it pulls
pre-encoded block bytes, writes them to a mirror column family, and votes. It
does not decode, execute or recompute the state root, and it does not update its
accounts column family. Its vote means "received", not "valid".

Use `--strict-sync` when you want a replica that independently re-executes every
block and compares state roots — which is the mode that makes a divergence
detectable at all. `--async-verify` exists but is a no-op.

---

## 5. The gateway

**Not runnable yet.** `cb-gateway` is under construction; at the time of writing
it has no binary. `make gateway` detects this and says so rather than failing
inside cargo:

```
$ make gateway
cb-gateway has no binary yet — the crate is still library-only
```

When it lands, `make gateway` runs it and `GATEWAY_ARGS` passes flags through:

```bash
make gateway GATEWAY_ARGS="--listen 127.0.0.1:8081"
```

The Makefile reserves `127.0.0.1:8081` for the gateway, because 8080 is the node
dashboard and 18080–18082 are the cluster dashboards. Its flag names are not
fixed here on purpose — this document will not invent a CLI the crate has not
declared.

---

## 6. The frontend

```bash
cargo install trunk       # once
make web-build            # release build into crates/cb-web/dist
make web-serve            # dev server with hot reload on 127.0.0.1:3000
make web-serve WEB_PORT=3111
```

Both were verified. Note that Trunk's own default port is 8080, which collides
with the node dashboard; the Makefile passes an explicit port.

`cb-web` is currently a build-chain spike: it compiles to WebAssembly, boots,
and renders one line of text. It does not talk to anything.

### Everything at once

```bash
make dev
```

starts the node in the background, starts the gateway if it has a binary
(skipping it with a printed note if not), then runs the frontend dev server in
the foreground. Ctrl-C stops the frontend; `make stop` stops the node.

The node and frontend halves were verified. The gateway half could not be, for
the reason in §5.

---

## 7. Running the simulation

There are two different things called simulation here, and they are not the
same.

### When the leader dies

A block proposed in view `V` is valid only if it is signed by
`validators[V % N]`, checked by every node. Skipping a dead leader is therefore
arithmetic, not a negotiation: advance the view and the next validator in the
set is the one whose blocks everyone accepts.

What a follower does, in order:

1. **Notices.** No block for `--failover-after-ms`. Silence alone proves
   nothing — a chain with nothing to do produces no blocks, because the builder
   refuses to make an empty one — so the follower asks the leader directly. A
   broken block stream, or a `chainInfo` that goes unanswered, is the answer.
2. **Checks it may lead.** It must be `validators[(view + 1) % N]`, and its head
   must be at most one block above the highest certificate it holds. A
   certificate for block N travels in block N+1's header, so one uncertified
   block at the tip is the ordinary state; two is more than the rest of the
   network can be assumed to have seen, and it waits instead.
3. **Takes over in place.** It records the view it is leading and enters the
   same code the `sequencer` subcommand enters, on the same database, in the
   same process. Nothing is restarted.

Everyone else keeps following: a node that is not next in line reconnects to
whichever `--peer` is now producing.

```bash
cb-node --data-dir ./data/node1 replica \
  --sequencer 127.0.0.1:7700 --peer 127.0.0.1:7702 \
  --listen 127.0.0.1:7701 --strict-sync --failover-after-ms 5000
cb-node --data-dir ./data/node1 consensus   # who leads now, who leads next
```

This is crash-fault failover: it assumes a validator stops rather than lies.
Three validators tolerate one silent node and no Byzantine one. Not covered: a
leader whose process is alive and answering but has stopped producing — that
needs the followers to agree among themselves, and they do not. A fast-mirror
replica can never take over; it keeps block bytes and no state to lead from.
Clients and the gateway follow the leader's address themselves; nothing here
moves them.

### Load simulation over RPC — `cb-sim`

Drives real signed transactions into a running node over Cap'n Proto. With
`--orders-pct N` (Makefile: `SIM_ORDERS_PCT`) that share of submissions are
spot orders on BTC/USD and ETH/USD: even-numbered traders rest `Gtc` orders
around a fixed mid, odd-numbered traders cross them with `Ioc`, both buy and
sell at random, and makers `BatchCancel` every 256 orders so resting depth
stays bounded. The split matters: a taker that meets its own resting order is
rejected as a self-trade, the executor does not consume the nonce, and every
pipelined transaction behind it fails `BadNonce`. At the default 0 the
simulator sends paired **transfers** only and populates no book.

Each worker bounds its in-flight submissions (`--max-in-flight`, default 4,000)
and re-reads a trader's nonce after a rejection once that trader's pipeline
has drained. `SIM_WORKERS` runs several processes over disjoint trader ranges.

```bash
make startsimulation                          # auto-starts a node if none is up
make startsimulation SIM_TRADERS=64 SIM_TPS=5000
make sim-status
make sim-logs
```

It finds its target by probing the single-node dashboard first, then the cluster
dashboard on 18080. Verified against a single node. It signs with the chain id
in `.env` (`MYCHAINID`); the executor rejects a transaction for any other chain,
so a genesis built with a different id needs `.env` to say so.

From the repository root there are three tiers. `make start` runs the stack
with the **simulation switch** on: the gateway itself launches one `cb-sim`
worker (eight traders, about twenty signed orders a second) so the books
move, and the operator moves the switch from the admin page — OFF (no default
buyer or seller), ON (that trickle), MAX (six workers, sixty traders, as fast
as the node takes them). `make start SIM_MODE=off` boots a quiet exchange.
The workers' logs are `data/sim-gateway-<n>.log`; `GET /admin/sim` and
`GET /chain/status` report the position. Do not run `make startsimulation`
against a stack the gateway fronts: two populations on one trader range share
nonces and strand each other.

The sequencer writes each block to RocksDB on a writer thread while it builds
the next one (`--sync-writes` turns that off). Nothing leaves the process
about a block until that write returns — block streams, `getBlock`, the p2p
announcer and the dashboard read only what is on disk — so a crash loses at
most eight blocks no peer was told about, and the restarted node continues
from the durable head. One thing to know: a balance read over RPC during
those milliseconds reflects the pending block, and a crash then reverts it.

If you raise `LOAD_TPS` or cut `LOAD_TRADERS`, watch the rejection count in
`make status`. Each worker's in-flight window is shared by its traders, and one
refused transaction strands everything pipelined behind it on `BadNonce`; too
few traders for the window turns that into a collapse rather than a blip.

```bash
make startmultisimulation                     # cluster + stress run (unverified: needs supervisord)
```

### Scenario simulations in process — also `cb-sim`

These run the engine and executor directly, with no node process and no RPC.
They are the fastest way to exercise margin, funding and liquidation.

```bash
make simperps        # 8 traders, 1 market, 200 blocks
make simshock        # 100 traders, 2 markets, 30% oracle drop at block 200
make simcollapse     # 60 traders, ten -5% epochs
make simfeestress    # 50 traders, 1000 blocks
```

`simperps` was verified; it prints per-run statistics ending in the final
accounts root. The other three write JSON metrics under `$(DATA_DIR)/sim-metrics`
and were not run to completion here.

### The simulation game — `cb-simgame`

**Not runnable yet.** The bot population, scenarios and continuous invariant
assertions described in `docs/PLAN.md` are being written. `make simgame` reports
that the crate has no binary rather than failing obscurely.

### Throughput, measured

On an Apple M4 (10 cores), single node, block interval as configured by the
Makefile:

| Path | Result |
|---|---|
| `make startsimulation` at the default 10,000 tx/s target | ~10,000 tx/s sustained at first, degrading over tens of seconds |
| `make startbenchmark` (in-process generator) | ~280,000 tx/s applied, ~9,900 transactions per block |

The second number is not an end-to-end figure. The in-process generator
pre-signs and pre-verifies transactions and bypasses the RPC and the mempool, so
it measures execution and commit throughput only. One machine, one run, no
statistical claim.

---

## 8. Reading the dashboard

`http://127.0.0.1:8080/` is an HTML page; the same data is JSON at
`/api/status`. Other endpoints: `/api/account/{addr}`, `/api/perps/funding`,
`/api/perps/positions`, `/api/perps/oracle`, `/api/perps/liquidations`,
`/api/perps/insurance`. There is no Prometheus endpoint.

What to look at, and what it tells you:

**Chain head.** `head_height`, `head_hash`, `state_root`. A height that is not
moving while transactions are being submitted is the signal that something is
wrong. A height that is not moving while nothing is being submitted is normal.

**`last_qc_height`.** How far behind the head the most recent quorum certificate
is. On a single node the leader self-votes, so this tracks the head. On a
cluster, a gap means replicas have stopped voting.

**`mempool_empty`.** True on a healthy idle chain. A mempool that is never empty
under light load means transactions are being rejected on execution — check
nonces.

**`metrics`.** `applied`, `blocks`, and a family of rates — `tps_5s`,
`tps_60s`, `avg_tps`, `last_block_tps` — plus block size statistics
(`block_txs_min`, `_max`, `_avg`, `_last`). Those are apply-side and always
populated. Every one of them counts **transactions**, not orders.

`orders_applied` and `recent_orders_applied_per_sec` are the apply-side *order*
counters: the subset of each block's transactions that placed an order
(`PlaceOrder`, `PlacePerpOrder`, `OpenPosition`, `ClosePosition`). The sequencer
classifies the block's bodies before recording it. A replica running in
fast-mirror mode never decodes a block, so it reports zero orders rather than
guessing.

Neither is a *matched*-order rate: the executor does not report fills to the
metrics layer. For measured matched-order throughput use
`cb-order-bench` — see `docs/performance.md`.

The submit-side counters are only partly populated. `submitted`, `rejected` and
`channel_depth` are incremented by the in-process benchmark generator only;
transactions arriving over RPC increment `applied` and nothing else, so a busy
chain driven by `cb-sim` reports `submitted: 0`. That is the metric's scope, not
a fault.

`orders_submitted`, `buys`, `sells`, `btc_orders`, `eth_orders` and
`other_orders` move only for transactions that actually place an order. The
in-process transfer generator used to move them for every `Transfer` it signed,
so the console showed a six-figure "orders per second" for a workload that
placed no orders at all. It now calls `record_tx_submitted`, which moves the
transaction counter alone.

**`perps`.** One row per perpetual: mark, oracle and last price (all at 1e8),
funding rate (1e9), open interest, best bid and ask, and whether the market is
paused or reduce-only.

**`oracle`.** Per market: the last price, the height it was submitted at,
`blocks_since_update`, and a `stale` flag. This is the first place to look when
a derivative market stops accepting orders.

**`insurance`.** Collateral, bad debt, and a sparkline. Collateral trending down
during a liquidation cascade is the fund doing its job; bad debt climbing is the
fund losing.

---

## 9. Configuration flags

Global, and it must appear before the subcommand:

| Flag | Meaning |
|---|---|
| `--data-dir <PATH>` | Required. The RocksDB directory this node owns. |

`cb-node init`:

| Flag | Meaning |
|---|---|
| `--genesis <PATH>` | Required. The genesis JSONL to apply. Re-running on an initialised directory re-applies it rather than refusing. |

`cb-node sequencer`:

| Flag | Default | Meaning |
|---|---|---|
| `--listen` | `0.0.0.0:7700` | Cap'n Proto RPC listener. The Makefile overrides this to loopback. |
| `--dashboard-listen` | `127.0.0.1:8080` | HTTP dashboard and JSON API. |
| `--block-interval-ms` | `1000` | Sleep between build attempts. The Makefile uses 100. |
| `--max-block-txs` | `10000` | Cap on transactions drained per block. |
| `--key-file` | none | Hex-encoded 32-byte secp256k1 secret used to sign block headers. |
| `--dev-key-seed` | `cb-validator-0` | Deterministic dev key when no key file is given. Must match a `seed` in a genesis `validator` record. |
| `--benchmark` | off | Run the in-process transaction generator alongside the sequencer. |
| `--bench-traders` | `8` | Synthetic traders. Genesis must pre-fund at least this many under the seed prefix. |
| `--bench-seed-prefix` | `cb-sim-trader-` | Seed prefix for those traders. |
| `--bench-rng-seed` | `12648430` | Seed for the generator, so a run is reproducible. |
| `--bench-workers` | `0` | Producer threads; 0 auto-detects from CPU count. |
| `--bench-channel-capacity` | `50000` | In-flight pre-verified transactions. Producers block when full, which is the back-pressure mechanism. |
| `--keep-blocks` | `10` | Block bodies retained; `0` keeps every block. On by default: accounts, trie nodes, markets and the head are never pruned, and they are what answer a question about the present. Every node prunes, leader and follower alike. |
| `--failover-after-ms` | `0` (off) | On a replica: how long without a block before asking the leader whether it is still there. If it is not, and this node is `validators[(view + 1) % N]`, it takes over in the same process. Needs `--strict-sync` and `--listen`. |
| `--listen` (replica) | none | Where this node accepts transactions once it leads. A standby that starts listening only after promotion is one nothing can reach at the moment it matters. |
| `--peer` (replica, repeatable) | none | Other validators' RPC addresses. When the leader goes and this node is not next in line, it looks here for whoever is: the validator set records identities, not endpoints. |

`cb-node replica`:

| Flag | Default | Meaning |
|---|---|---|
| `--sequencer` | required | Address of the sequencer's RPC listener. |
| `--dashboard-listen` | none | Optional HTTP dashboard. Without it the replica exposes nothing. |
| `--key-file` | none | Key used to sign votes. |
| `--dev-key-seed` | `cb-validator-1` | Dev voter key. Must match this replica's genesis entry. |
| `--no-voting` | off | Follow without voting — a pure observer. |
| `--strict-sync` | off | Re-execute every block and compare state roots. Off means fast-mirror. |
| `--async-verify` | off | Reserved. Currently a no-op. |

Makefile variables worth knowing: `DATA_DIR`, `GENESIS`, `RPC_LISTEN`,
`DASH_LISTEN`, `INTERVAL_MS`, `SIM_TRADERS`, `SIM_TPS`, `WEB_PORT`,
`GATEWAY_ARGS`, and the `BENCH_*` family. All are overridable on the command
line.

---

## 10. When things go wrong

### The node will not start

**``Error: genesis: env var `MYCHAINID` is not set (referenced as `${MYCHAINID}`)``**
The genesis file needs its variables. Run `make env`, or export them yourself.
`make init` does this for you via `make check-env`.

**`Error: Invalid character ' ' at position 40`**
A `.env` value has a trailing `# comment` on the same line. The genesis loader's
`.env` parser does not strip inline comments, so the comment ends up inside the
address and the hex decode fails. `make check-env` repairs this automatically;
by hand it is `make env FORCE=1`, or:

```bash
sed -i.bak -E 's/[[:space:]]+#.*$//' .env
```

**`Error: rocksdb: IO error: While lock file: ./data/LOCK: Resource temporarily unavailable`**
Another process already has this data directory open. Find it and stop it:

```bash
make status
pgrep -fl cb-node
make stop
```

Do not delete `LOCK` while a process holds it. If you are certain nothing is
running — for example after a hard kill — removing the file is safe, but check
`pgrep` first.

**`Error: Address already in use (os error 48)`**
The RPC or dashboard port is taken, often by an earlier node that `make stop`
did not know about, or by Trunk (whose own default is 8080).

```bash
lsof -nP -iTCP:7700 -sTCP:LISTEN
lsof -nP -iTCP:8080 -sTCP:LISTEN
```

Then either stop the other process or pass different ports.

**`loaded key derives address 0x… but Meta::proposer_addr is 0x… — refusing to produce blocks the network would reject`**
The signing key does not match the proposer recorded at genesis. Either the
wrong `--key-file`, or a `--dev-key-seed` that is not the seed used in the
genesis `validator` record, or a data directory initialised from a different
genesis. Fix the key, or wipe and re-initialise.

**`loaded voter key derives address 0x… but it is not in the genesis validator set — refusing to start`**
Same class of problem on a replica: its `--dev-key-seed` or key file must
correspond to one of the genesis validators. `--no-voting` starts it as an
observer instead.

**Nothing in the log.** See §2: set `RUST_LOG=info`.

### The state root diverges

A strict-sync replica halts with:

```
state root mismatch at height H: expected 0x…, got 0x…
```

It stops before persisting anything, so the replica is intact and behind, not
corrupted. What to do:

1. Compare heads. `curl -s http://127.0.0.1:18080/api/status` against the
   replica's own dashboard. `expected` is what the leader signed; `got` is what
   the replica computed.
2. Confirm both nodes ran the same binary. Different builds of the executor is
   the common cause, because the state root is a hash of everything the
   executor writes.
3. Confirm both were initialised from the same genesis: the `state_root` at
   height 0 must match. Different genesis means different chains.
4. Read the divergent block. `getBlock(H)` on the leader, and look for the
   transaction kinds in it — funding ticks and futures settlements are the
   block-level transactions with the strictest rules, and the replica enforces
   required funding ticks but not required futures settlements.
5. To recover a replica, stop it, delete its data directory, re-run `init` from
   the same genesis and let it resync from height 1. There is no partial repair.

If both nodes are on the same binary and the same genesis, you have found a
determinism bug. That is a serious finding: capture both `state_root` values,
the height, and the block, and open an issue. `cb-block/tests/determinism.rs` is
where a regression test belongs.

A related failure with a different message — `proposer mismatch at height H` or
`malformed proposer signature at height H` — means the replica is pointed at a
node that is not the genesis proposer.

### The gateway cannot reach the node

When the gateway exists, the API contract's answer to this is
`503 NODE_UNAVAILABLE`. Work down the layers:

1. Is the node running? `make status`.
2. Is the RPC port listening where the gateway thinks?
   `lsof -nP -iTCP:7700 -sTCP:LISTEN`. Note that `--listen` defaults to
   `0.0.0.0:7700` but the Makefile binds loopback.
3. Is the dashboard answering? `curl -s http://127.0.0.1:8080/api/status`. If the
   dashboard answers and RPC does not, the node is alive and the RPC listener is
   the problem.
4. The RPC has no health endpoint of its own. The cheapest end-to-end probe is
   `./target/release/cb-client --rpc 127.0.0.1:7700 markets` — if that returns,
   the RPC path works and the problem is the gateway's configuration.

### The database is locked

Two distinct cases, and the second does not exist yet.

**RocksDB** — see "Resource temporarily unavailable" above. One process per data
directory. This is the case you will actually hit.

**SQLite** — the gateway's application database is single-writer. `database is
locked` means two writers, or a transaction held open across an await. There is
nothing to run against today because the gateway is not built; when it is, the
handling belongs here.

### A market is stuck

Work through these in order.

1. **Is the chain producing blocks at all?** `head_height` on the dashboard. If
   the whole chain is idle, this is not a market problem. Remember that an empty
   mempool means no blocks.
2. **Is the market paused?** The `perps` section of `/api/status` carries
   `paused` and `paused_reduce_only_only` per market. A paused market rejects
   new orders by design.
3. **Is the oracle stale?** `/api/perps/oracle` gives `blocks_since_update` and
   `stale` per market. A stale oracle blocks anything that needs a mark price.
   On the perps chain you can push a price by hand:

   ```bash
   make oraclehit MARKET=BTC PRICE=50000
   ```

   That target signs an `OracleUpdate` as the market's oracle authority, at the
   current head height plus one. Only that address may do it; anything else is
   rejected as not authorised. Confirm it landed with
   `curl -s http://127.0.0.1:8080/api/perps/oracle`, which shows the price, the
   source height and `blocks_since_update` per market.
4. **Has a dated future expired?** Past its expiry height a future stops
   trading and settles. `markets` in `cb-client` shows the expiry height.
5. **Is a funding tick overdue?** The sequencer signs the `FundingTick` (and
   any due `FuturesSettle`) itself before building the block, so a healthy
   node never misses one. If the log nevertheless shows
   `MissingRequiredFundingTick { height, market }`, the node's key does not
   derive the address recorded in `Meta::proposer_addr` — the injected tick
   is signed by a key the executor does not recognise as the proposer.
   `cb-node` refuses to start in that state, so the usual cause is a data
   directory initialised from a different genesis than the key was chosen for.
   The block is refused, not produced, and the chain waits rather than
   advancing without funding.
6. **Is the book simply empty?** `book perp:0 10`. No quotes is not a fault, it
   is an absence of market makers. Start the simulator.

### Transactions are accepted and never apply

Check the nonce. Execution requires a transaction's nonce to equal the account's
current nonce exactly; the sequencer drains the mempool before building, so a
block that fails to build drops the transactions in it rather than retrying
them. `cb-client balance` prints the current nonce.

---

## 11. Stopping and wiping

```bash
make stop     # single node, perps node, cluster, simulator, gateway, strays
make clean    # stop, then delete ./data, ./data-perps, test dirs, cluster
              # state and the frontend dist directory
```

`make clean` is destructive and asks nothing. A chain that has been wiped
restarts from genesis at height 0 with a fresh state root.
