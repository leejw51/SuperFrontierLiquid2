# SuperFrontierLiquid

A personal experiment in building an exchange simulator and blockchain in Rust.
This prototype explores order books, spot trading, perpetuals, dated futures,
consensus, and a browser terminal using simulated assets and test funds.

The backend uses deterministic execution, Cap'n Proto RPC, RocksDB for chain
state, and SQLite for application state. The browser client is written in Rust
and compiled to WebAssembly. You can create a disposable test wallet, claim
faucet funds, and trade against simulated market makers on your own machine.

**For learning and local experiments only.** This is unfinished software with
known security and correctness limitations. It is not suitable for real funds
or production use. No real shares or external assets are held or settled.

This is an independent personal project with no company sponsorship or exchange
affiliation. Hyperliquid is an architectural reference for the experiment.

Where it stands today, what is broken, and what to do next, in order:
[`docs/STATUS.md`](docs/STATUS.md).

Licensed under MIT or Apache-2.0, at your option.

All committed mnemonic examples, signing keys, and deterministic validator seeds
are public test fixtures. Never fund their addresses. Create a fresh test wallet
for this prototype; never paste a recovery phrase from a wallet holding real
assets. Local credentials and wallet exports belong in ignored files.

> **Alpha, for running on your own machine.** Read [`SECURITY.md`](SECURITY.md)
> before exposing any part of it to a network. In short: the gateway holds
> every user's trading key, one key sets every oracle price, the validator,
> oracle and treasury keys in the shipped configuration are derived from
> seeds published in this repository, and a single peer can stall a node.
> Only the ordering of blocks is decentralised; custody, prices and the
> validator set are one operator's.

---

## The shape of it

```
        Browser (Rust -> WebAssembly)
               |  JSON + WebSocket; every number is a decimal string
               v
        +--------------------+          SQLite
        |     cb-gateway     | <------>  users, sessions, faucet ledger,
        |   custodial keys   |           candles, leaderboard, audit log
        +--------------------+
               |  Cap'n Proto RPC
               v
        +--------------------+          RocksDB, 22 column families
        |      cb-node       | <------>  accounts, orders, books, positions,
        |     sequencer      |           margin, blocks, certificates, meta
        +--------------------+
             ^            ^
             |            |  block stream, votes
        +---------+   +-----------+
        | cb-sim  |   |  replica  | x2
        +---------+   +-----------+
```

A transaction is signed, submitted over Cap'n Proto, sorted into a deterministic
order, executed by a pure matching engine on one thread, and written to
RocksDB in one atomic batch per block. The block's state root is a hash chain
over every row the block changed (ADR 0007, which replaced the merkle tries).
A replica that re-executes the same block must arrive at the same root, or it
halts.
`docs/architecture.md` walks the whole path.

---

## What works today, and what does not

The tables summarize development runs. See [`CHANGELOG.md`](CHANGELOG.md) for
later fixes and [`SECURITY.md`](SECURITY.md) for known limitations.

**Working, and you can drive it end to end:**

| Thing | How you see it |
|---|---|
| Single-node chain: genesis, block production, state root | `make startperps`, `make status` |
| Spot order books: place, cancel, match, fees, balance locking | `cb-client place spot:btc_usd ...` |
| Perpetuals: margin, funding, mark price, liquidation, insurance fund | `make simperps`, the dashboard's perps panels |
| Dated futures: expiry, TWAP or oracle settlement | genesis `future_market` records, `cb-client markets` |
| Faucet, transfers, balances, nonces | `cb-client faucet`, `cb-client balance` |
| Cap'n Proto RPC: 22 methods, including the pull-based block, raw-block and refusal streams (`streamMarket` is declared but unimplemented) | `cb-client`, replica sync |
| Four-validator BFT cluster (chained HotStuff, one-block finality), the exchange on top, real signed load forwarded between validators | `make startbft`, then `make loadbft` |
| Three-node cluster with votes and quorum certificates (genesis now lists four validators, so this cluster's quorum is three of three: no vote slack) | `make startmulti` |
| Leader failover: kill the sequencer, the next validator takes over in its own process and the others follow it | `make startmulti`, then kill node0 |
| Operator dashboard and JSON status API | `http://127.0.0.1:8080/api/status` |
| Load simulator over RPC, and in-process crash and squeeze scenarios | `make startsimulation`, `make simshock` |
| The replaceability traits | `rust/crates/cb-ports/` |
| Browser sign-in with a BIP-39 test mnemonic and Ethereum-compatible signatures | `make up`, then the landing page |
| Faucet paying a real on-chain deposit | `make faucet` |
| Eight-screen trading terminal in Rust and WebAssembly, in a 16-bit SNES / Genesis skin | `make trade`, `make admin`, `make user` |
| The operator's simulation switch: OFF (no default buyer or seller), ON (a trickle), MAX (the stress test), plus the oracle scenario, from the admin page | `make admin`, the Simulation panel; `make start SIM_MODE=off` |
| User page listing every screen as a menu, and the RPC facts (gateway API, WebSocket, node RPC, oracle control, chain id) | `make user` |
| Dashboard: chain, recent blocks, the last sixty transactions, and the same RPC facts | `make chain` |
| Browser end-to-end suite with Playwright over the real stack | `make start`, then `make web-e2e` |
| Two languages in the client, English (default) and Korean, with a pixel Hangul face | the language switch in the top bar, or `?lang=ko` |
| REST and WebSocket gateway | `curl http://127.0.0.1:8081/health` |
| Bot simulation asserting nine invariants every block | `make simgame` |
| Peer-to-peer gossip, discovery and catch-up | `--p2p-listen` / `--p2p-seed` |
| Order-matching benchmark that measures matches, not transfers | `make orderbench` |
| An order the executor refuses is reported `rejected` at once, from the sequencer's `streamRejected` stream or inside the placement reply, instead of sitting `pending` | `make e2e` provokes one with a self-trade |
| Spot market orders priced with the chain's own tick size, minimum quantity and fee, read over `listSpotMarkets` | `curl http://127.0.0.1:8081/api/v1/markets` |

Every row above was exercised on a developer machine while this was written,
except the three-node cluster, which needs `supervisor` installed.

The targets `startperps`, `simperps`, `simshock`, `startmulti`,
`startsimulation`, `oraclehit` and `stop-sim` live in `rust/Makefile`; run them
from `rust/`. Everything else runs from the repository root.

**Known limitations and completed fixes:**

| Thing | State |
|---|---|
| Twelve known defects in the exchange core | All fixed. `docs/PLAN.md` §8 lists every one by severity; the funding rate is now per interval, which needs a fresh genesis. |
| Seventeen findings from the core review | All fixed. `docs/core-review.md`. The largest: every chain started from the shipped genesis used to halt at height 3,600, because nothing produced the funding tick the executor demands. |
| Throughput | On the ten-core M4, single sequencer: 221,586 orders/sec applied under `make start2` (pre-verified input, the risk-engine premise, no quorum), 36,806 under `make start3` with every signature recovered across all cores and the clients on the same machine, and 66,400 matched/sec in the engine benchmark on 2026-09-21 (285,000–294,000 on 2026-09-23 under ADR 0007's hash-chain root). On the 18-core M5 Max, single sequencer with the in-process generator, balances unlimited: 458,000 orders/sec applied with the generator signing, 1,023,000 with placeholder signatures (`CB_BENCH_UNSIGNED=1 make start2`; the block-loop thread is the bound). Four BFT validators plus the load generator: 60,000 signed orders/sec committed at 5 ms p50 with no backlog, 73,000–76,000 on three of four validators when offered 150,000, about half of them applied (the generator's traders run dry) — `docs/performance.md` §0, *Re-measured 2026-09-29*. Sixteen throughput defects fixed 2026-09-21; `docs/STATUS.md` §3.4a has each one and its measurement. `docs/performance.md` §0 is the one place every figure is set against Hyperliquid's with its machine and scheme named. |
| Spot trades and candles over the API | Estimates. The node exposes no trade stream, so the gateway infers each print from the depth that vanished in a block: exact in price on derivatives, estimated in both price and quantity on spot. Candle volume inherits the estimate; nothing financial is computed from it (`rust/crates/cb-gateway/src/marketdata.rs`). |
| Gateway state across a restart | Suspensions, market halts and risk overrides are held in memory and lost. |
| Finality on a gossip replica | A peer-to-peer replica does not vote, so that topology produces blocks that are agreed but never final. Use the cluster config when finality matters. |
| Configured margin, leverage and position caps | Enforced alongside tier floors since the 2026-09-30 audit fixes; see `docs/adr/0009-audit-risk-and-block-identity.md`. |

Two more things worth saying plainly.

**A replica does not verify by default.** Without `--strict-sync` it mirrors the
sequencer's block bytes and votes that it received them. Independent
re-execution and state-root comparison is opt-in.

**The default scheme is not Byzantine fault tolerant; the BFT scheme is opt-in.**
`make start` runs one fixed sequencer. Its votes and quorum certificates exist
and are checked, but its quorum is three of the four validators in genesis:
with all four running it tolerates one crash fault and zero Byzantine faults,
and the three-node `make startmulti` cluster tolerates none. It has no view
change. `make startbft`
(ADR 0008) runs a four-validator chained-HotStuff chain that tolerates one
Byzantine validator; a chain is one or the other from genesis. Validator
decentralisation beyond that is an explicit non-goal.

---

## Prerequisites

- A Rust toolchain. `rust/rust-toolchain.toml` pins stable and adds the
  `wasm32-unknown-unknown` target.
- The Cap'n Proto compiler: `brew install capnp`, or your distribution's
  package.
- Python 3 and `curl`, for the operator scripts.
- Optional: `cargo install trunk` for the frontend, `pip3 install supervisor`
  for the three-node cluster.

The first release build takes several minutes — RocksDB and the Cap'n Proto
codegen dominate. Subsequent builds are fast.

---

## Quickstart

Clone or download this repository, then run the whole stack — chain, oracle,
gateway, web app and a trickle of bot traders — from the repository root:

```bash
make setup      # once: checks tools, writes rust/.env, builds genesis
make start      # prints the URLs and how to log in
```

Or just the chain and its dashboard, from `rust/`:

```bash
cd rust
make startperps
```

That builds the node and the client, applies `genesis-perps.jsonl` — spot
markets, two perpetuals, a dated future, an insurance fund and three pre-funded
demo traders — and runs a sequencer with its dashboard:

```
perps sequencer started (pid 79648)
  RPC:       127.0.0.1:7700
  dashboard: http://127.0.0.1:8080
```

Open the dashboard. It will say height zero, because a chain with nothing to do
produces no blocks. That is correct behaviour, not a hang.

### Trade against it

`cb-client` keeps a wallet store under `$HOME`; point it somewhere scratch so it
leaves yours alone.

```bash
export HOME=/tmp/cb-demo && mkdir -p $HOME
set -a; eval "$(scripts/perps_env.sh)"; set +a   # dev mnemonics, not secrets
BIN=./target/release/cb-client

$BIN --rpc 127.0.0.1:7700 login --label alice --mnemonic "$MYWALLET3_MNEMONIC" --index 5 --force
$BIN --rpc 127.0.0.1:7700 faucet
$BIN --rpc 127.0.0.1:7700 balance
$BIN --rpc 127.0.0.1:7700 markets
$BIN --rpc 127.0.0.1:7700 place spot:btc_usd buy 1000000 3000000000000
$BIN --rpc 127.0.0.1:7700 book spot:btc_usd 10
```

The last command shows your bid for 0.01 BTC at 30,000 USD resting on chain.
Quantities and prices are raw integers at the chain's own scales; the decimal
strings in the API contract are the gateway's HTTP representation.

Derivatives are margined against collateral in a margin account, and
`cb-client` has no command to move it there — so trade a demo account the
genesis already funded:

```bash
make oraclehit MARKET=BTC PRICE=50000              # push a mark price
$BIN --rpc 127.0.0.1:7700 login --label demo0 \
     --mnemonic "$DEMO_TRADER0_MNEMONIC" --index $DEMO_TRADER0_INDEX --force
$BIN --rpc 127.0.0.1:7700 margin
$BIN --rpc 127.0.0.1:7700 place perp:0 buy 100000 4990000000000
$BIN --rpc 127.0.0.1:7700 book perp:0 5
```

### Put load through it

```bash
make startsimulation      # synthetic load over RPC
make status               # height climbing, blocks filling, transactions applied
make stop
```

Three targets, from the repository root:

| | What runs | For |
|---|---|---|
| `make start` | the stack with the simulation switch ON: the gateway runs one `cb-sim` worker (20 orders/s over RPC), and the admin page moves the switch to OFF or MAX | using and testing; the books move |
| `make start2` | the stack and the node's own generator handing the sequencer pre-verified buyers and sellers, no RPC, no signature recovery | the pure stress test: **221,586 orders/s** applied on the ten-core M4, single sequencer, no quorum. Validation is the risk engine's job in front of this node. Not a Hyperliquid comparison: their figure has signatures and consensus in it |
| `make start3` | the stack and six `cb-sim` workers signing real orders over RPC, every signature recovered on a pool across all cores, admission kept in arrival order | the stress test with verification on: **36,806 orders/s** with the whole stack on the ten-core M4, 60,166 with the node and clients alone. Single sequencer, no quorum. Bounded by the clients' signing, not by the node |
| `make startbft` | the stack on four BFT validators (ADR 0008), each verifying every signature; `make loadbft` adds six `cb-sim` workers at `BFT_LOAD_TPS` orders/s (default 40,000; the 60,000 row needs `BFT_LOAD_TPS=60000`) | the Hyperliquid comparison, with the caveats in `docs/performance.md` §0: **60,000 orders/s committed at 5 ms p50 / 15–29 ms p99 from proposal to commit (2026-09-29); 40,000 at 5–7 ms p50 and 20,000 at 2–3 ms p50 on the 2026-09-25 code**, all four validators sharing one 18-core M5 Max with the signing workers, no network hop, zero admission refusals. "Committed" counts ordered transactions; about half of them are applied, because the generator's traders run out of balance. `grep 'consensus status' rust/data-bft/node0.log` prints the numbers every five seconds |

`STRESS_WORKERS`, `STRESS_INTERVAL`, `STRESS_BLOCK_TXS` tune `start2`;
`STRESS3_WORKERS`, `STRESS3_TPS` tune `start3`; `MINIMAL_TPS` the trickle.
`make stop-sim` quiets the load and leaves the stack up.

What the node applies is its ceiling, not the simulator's: every worker keeps a
bounded number of submissions in flight and waits for replies, so the accepted
rate is the node's. On the ten-core M4, single sequencer, the node
applies about **221,586 orders a second** with validation assumed done by the
risk engine, and about 37,000 with every signature recovered at admission over
RPC, on a pool across every core, with the clients on the same machine. That is up from 1,200 a second, decaying, before
2026-09-21: profiling the load found sixteen defects, from a spot book rebuilt by
scanning every order the chain had ever written to an RPC server sharing the
block builder's thread. `docs/STATUS.md` §3.4a lists each one with the
measurement that found it. The engine on its own, without signing or network,
ran at 118,233 orders a second applied and 59,093 matched that morning and
132,834 / 66,400 by the end of the day — `make orderbench` on the merkle-trie
root; under ADR 0007's hash chain (2026-09-23) it is 571,000–589,000 /
285,000–294,000 with `ORDERBENCH_ARGS="--producers 8 --prefill"`.

`cb-sim run` with `--orders-pct 0` (the `make startsimulation` default) is the
older transfers-only workload. ETH/USD is priced synthetically under load: the
genesis gives ETH eighteen decimals and the notional rule cannot express a
price under $10,000 per ETH, so the gateway omits that spot market from the
web app.

### The other chain

`make start` boots the chain from `genesis-full.jsonl` into `rust/data`, and
`make start2` wipes that directory and starts from `genesis-stress.jsonl`,
whose traders hold 1e32 of every asset so nothing is refused for balance.
Both are what the load and benchmark targets were built around. Their genesis funds
addresses derived from bare seed strings in `rust/.env`, which `cb-client`
cannot derive — so the faucet and hand-trading flow above will not work there.
Both chains use the same ports; run one at a time.

`make` on its own lists every target with a description.
[`docs/runbook.md`](docs/runbook.md) is the fuller operator guide, including
what to do when something is wrong.

---

## Crates

Inherited L1 core — ported, working, and what the architecture document
describes:

| Crate | Responsibility |
|---|---|
| `cb-types` | Addresses, hashes, asset and market identifiers, fixed-point scales |
| `cb-crypto` | keccak256, secp256k1 sign and recover |
| `cb-schema` | Cap'n Proto schemas and canonical encoding |
| `cb-trie` | Binary radix merkle trie over a key-value store |
| `cb-state` | RocksDB column families, per-family tuning, state root |
| `cb-tx` | Transaction types, signing, static validation |
| `cb-engine` | Pure matching, margin, funding, liquidation, settlement maths |
| `cb-block` | Deterministic ordering, block builder, executor |
| `cb-consensus` | Votes, quorum certificates |
| `cb-genesis` | Genesis loading and application |
| `cb-rpc` | Cap'n Proto RPC server, client and streams |
| `cb-net` | Replica sync and state-root verification |
| `cb-node` | The `cb-node` binary: init, sequencer, replica |
| `cb-client` | Wallet, CLI and trading REPL |
| `cb-dashboard` | Operator dashboard and JSON status API |
| `cb-sim` | Load simulator and scenario harness |
| `cb-cli` | Placeholder. Empty today; `cb-client` is the CLI. |

New in this project:

| Crate | Responsibility | State |
|---|---|---|
| `cb-ports` | Trait definitions for every replaceable dependency | Landed |
| `cb-store-sqlite` | `AppDatabase` over SQLite with versioned migrations | In progress |
| `cb-bus` | `EventBus`: in-process implementation, partitioned-log shape | In progress |
| `cb-feed` | `MarketDataFeed`: mocked crypto and equity prices | In progress |
| `cb-gateway` | Browser-facing REST and WebSocket, Cap'n Proto bridge | In progress |
| `cb-web` | Rust and WebAssembly trading terminal | Landed |
| `cb-oracle` | Oracle price publisher with scenario control | Landed |
| `cb-e2e` | End-to-end suite over the real node and gateway binaries | Landed |
| `cb-simgame` | Bot population and continuous invariant assertions | In progress |

---

## The ideas worth knowing about

**Execution is deterministic, and that is load-bearing.** Transactions are
sorted by `(class, sender, nonce)` before execution — arrival order is
discarded. Order identifiers are derived from block height, transaction index
and an intra-transaction counter, never from a counter in memory. There is no
floating point anywhere, no clock read during execution, and no iteration over
an unordered map that can reach the output. The state root is the check: a
replica re-executes and compares, and halts on a mismatch rather than
persisting a fork.

**The matching engine is pure.** `cb-engine` depends on exactly two crates,
`cb-types` and `thiserror`. It performs no I/O, reads no clock, and generates no
randomness. The matcher does not even read storage: it takes a read-only
snapshot trait and returns a description of what should happen, which the
executor then applies. Purity is not tidiness here — it is the property that
makes two machines agree.

**Two storage engines, chosen for different jobs.** RocksDB holds consensus
state, written in one batch per block and read back only at startup since
ADR 0007; the state root is a hash chain over the rows each block changed, in
column-family and key order. SQLite holds
application state — users, sessions, the faucet ledger, candles, the
leaderboard — where relational queries and migrations matter and consensus does
not. Neither is reachable by name from business logic. See ADR 0002.

**Every third-party dependency sits behind a trait.** `cb-ports` defines
`AppDatabase`, `EventBus`, `KvStore`, `MarketDataFeed`, `EquityReferenceData`,
`CustodyProvider`, `IdentityProvider` and `Clock`. Two rules keep the seam
honest: no crate above `cb-ports` may name a concrete backend, and `cb-ports`
itself may not depend on one. Swapping SQLite for PostgreSQL, or the mocked feed
for a real one, is an implementation plus one line in a composition root.

**Numbers cross the wire as decimal strings.** A JavaScript number cannot
represent one ETH in base units, and silent precision loss in a trading
interface is unacceptable. Prices, quantities, balances and PnL are strings such
as `"64250.50"`, and the gateway owns the conversion to and from the on-chain
fixed-point integers. See ADR 0006.

**The spot market registry is a closed enum, on purpose.** Market identity is
baked into transaction signing hashes, balance map keys and order-book key
prefixes. Making it dynamic would put market listing on the consensus path. The
cost — listing a spot market is a software release — is real, and ADR 0004 says
so.

`docs/adr/` records these decisions with their consequences, including the bad
ones.

---

## Documentation

| Document | What it is for |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | How it works: click to state root, purity, margin and funding, storage |
| [`docs/architecture.html`](docs/architecture.html) | The same, as one interactive page (English and Korean) |
| [`docs/crash-recovery.html`](docs/crash-recovery.html) | Interactive: one order from sign-in to replica check, and what survives a crash in each mode |
| [`SECURITY.md`](SECURITY.md) | The trust model, what not to run in public, and how to report a vulnerability |
| [`CHANGELOG.md`](CHANGELOG.md) | What each release contains and what it leaves open |
| [`docs/runbook.md`](docs/runbook.md) | Running it: single node, cluster, dashboard, flags, and what to do when it breaks |
| [`docs/adr/`](docs/adr/) | Decision records, each with honest consequences |
| [`docs/api-contract.md`](docs/api-contract.md) | The frozen gateway API, which `cb-gateway` serves under `/api/v1`; its gaps section says what is derived rather than published by the chain |
| [`docs/design-system.md`](docs/design-system.md) | The visual and interaction system for the trading terminal |
| [`docs/PLAN.md`](docs/PLAN.md) | The plan, the milestones and the non-goals |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Build, test, commit convention, and the invariants not to break |

---

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) first — particularly the invariants
section, which lists the things that do not fail loudly at the point of the
mistake.

The short version: one concern per pull request, a test that fails without the
change, documentation in the same commit, and

```bash
make format   # then
make ci       # format-check, clippy with warnings as errors, every workspace test
```

green before you open it.

Good first contributions are the honest gaps, each small and each written
down in [`SECURITY.md`](SECURITY.md) or [`CHANGELOG.md`](CHANGELOG.md): bound
the `TxBatch` messages a peer may send; restart the peer-to-peer listener
after an `accept()` error, as the RPC listener already does; put the domain
and chain id in the login challenge; refuse a mark price older than some
number of blocks; add an option to fsync the consensus safety record.

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option. Contributions are accepted under the same terms.
