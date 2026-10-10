# Changelog

## Unreleased

An order the executor refuses is now reported `rejected` at once instead of
sitting `pending`. Under the sequencer scheme a refused transaction is
dropped from the block and never recorded on chain, so nothing told the
gateway that a submission had died until a later nonce from the same account
proved it, or the hourly sweep gave it up. The sequencer now keeps its recent
refusals in memory and serves them on `streamRejected` (additive; a validator
answers `unimplemented`, because under BFT a refused transaction stays in
the block with `applied` false). The gateway follows that stream, marks the
orders behind each refusal rejected, logs the executor's reason, and drops
its cached nonce for the sender so the next order is signed against the
chain's nonce rather than refused at admission first. A refusal that lands
inside the confirm window is in the placement reply itself. `make e2e`
provokes one with a self-trade.

`make startbft` wipes `rust/data-bft` and starts the cluster from genesis,
as `make start2` wipes `rust/data`. Restarting all four validators on the
previous run's state produced no blocks at all while every submission was
refused for its nonce; a BFT chain is one scheme from genesis, and the
target's job is a fresh one. One validator can still be restarted on its
own with `rust/scripts/bft-cluster.sh restart N`.

Every spot *market* order was refused by the executor and shown as `pending`
for the rest of the session, while limit orders filled. The gateway rounded
the market order's protective price to a tick size it had guessed (one wire
unit, 0.000001 USD on BTC-USD) because the chain's spot `MarketConfig` had no
RPC in front of it; genesis gives BTC-USD a tick of 0.1, so the price failed
`price % tick_size` and the sequencer dropped the transaction from the block
without a verdict. The node now lists its spot configuration
(`listSpotMarkets`, additive) and the gateway reads tick, minimum quantity
and fee from it, on each periodic refresh and as soon as the node
connects (the first periodic attempt fires before the connection exists,
and the next was a full period later, 30 s shipped); `/markets` publishes
the chain's values and `make e2e` places a spot market order against the
real binaries at the shipped refresh period.

Security fixes for audit issue #1 (2026-09-30). **Fresh chain required**:
block IDs now hash executed content under `CB-BLOCK-ID-V2`, excluding proof and
signature encodings. BFT sync binds the timestamp to the signed proposal.
Insurance only absorbs a flat account's residual deficit, preserving liabilities
while hedge legs remain. Maker fills revalidate live margin, and all opening
exposure obeys configured IM, MM, leverage and notional caps as well as tier
floors. Trading and liquidation require marks/source heights within 600 blocks;
cancellations remain available during oracle outages. Enabled audit regressions
and ADR 0009 document the behavior and compatibility implications.

Five throughput fixes to the BFT chain, each from a profile of a loaded
validator and each measured alone (`docs/performance.md` §0, *Re-measured
2026-09-29*). Four validators plus the load generator on one 18-core
machine: 60,000 signed orders/s committed at 5 ms p50 and 15–29 ms p99
with no backlog (was 51,000 at 60–70 ms with 20% of submissions refused);
73,000–76,000 on three of four validators when offered 150,000, the fourth
falling behind. About half of every committed block is the generator's
orders refused for empty balances, so the applied rate is about half.

* A rollback drops only the order books the failed transaction changed;
  it cleared all of them, and the rebuild from RocksDB was three quarters
  of the executor's time (39 ms → 1.7 ms a block).
* A refused transaction consumes its nonce, and the block carries an
  `applied` mask that replicas verify and the gateway reads. Fresh genesis.
* The leader proposes past what is committed but not yet executed.
* Parallel jobs are chunked and each validator's threads are budgeted on a
  shared machine (`bft-cluster.sh`, `VERIFY_THREADS`).
* A transaction already held or just committed is admitted without a
  signature check.
* The executor logs a per-block histogram of refusal reasons;
  `make loadbft` takes `BFT_LOAD_INFLIGHT`.
* `make start2` starts a fresh chain from `genesis-stress.jsonl` (traders
  hold 1e32 of every asset) and `CB_BENCH_UNSIGNED=1` makes its generator
  skip signing. On the M5 Max: 458,000 orders/s applied with signing,
  1,023,000 without — the single block-loop thread is the bound.
* `PendingNonces::retire_settled` reads the chain nonce only for senders
  the ledger holds, once each; it read one per transaction (11% of the
  sequencer's block loop, for an empty ledger).

## v0.1.0-alpha — 2026-09-29

The first public release. An alpha testbed for running on your own machine;
read [`SECURITY.md`](SECURITY.md) before exposing any part of it to a
network. A chain started from an earlier commit cannot continue under this
one: funding execution changed, so start from a fresh genesis.

### What is in it

* **An on-chain exchange.** Central limit order books for spot,
  perpetuals and dated futures, eight assets and seventeen instruments in
  the shipped genesis. Price-time matching on one thread, margin with a
  worst-case gate across resting orders, funding per interval weighted by
  the mark, liquidation with an insurance fund, and a ±10% price band.
* **Two ways to order blocks.** A single sequencer with replicas that
  re-execute (`make start`), or four BFT validators running chained
  HotStuff with a two-chain commit, leader reputation and execution after
  commit (`make startbft`, ADR 0008).
* **State.** In memory on the matching thread, written to RocksDB in one
  atomic batch per block by a writer thread; the state root is a hash chain
  over each block's changes (ADR 0007).
* **Around the chain.** A REST and WebSocket gateway with EVM signature
  login, an oracle publisher over a mocked feed, a Rust and WebAssembly
  trading terminal in English and Korean, a faucet, bot traders, and an
  operator dashboard.
* **Tests.** About 2,100 workspace tests, among them property tests,
  randomised BFT fault sweeps, adversarial peers and gateway lifecycle
  tests; 52 end-to-end tests over the real binaries; a Playwright browser
  suite.
* **Docs.** Eight decision records, an architecture page and a crash
  recovery page (both interactive), the performance record with every
  figure labelled by machine and scope, and the status hand-over.

### Fixed since the audits

* A timeout certificate carried by a proposal its quorum certificate
  already justified was never verified; any validator could jump an honest
  one to a round of its choosing, fork it below its lock, and strike
  honest validators' leader reputation.
* A syncing node accepted an old block's finality proof as the next block
  and forked itself permanently.
* Funding payments left out the price: a 1 BTC long at $50,000 paid $0.05
  an interval at 0.05% instead of $25.
* A vetoed proposal that arrived before its parent lost its veto.
* The 2026-09-25 audit: forged certificates naming a known proposal, the
  safety record written after the vote it guards, the margin gate netting
  bids against asks, unbounded decoder allocations (a 112-byte frame
  allocated 416 MB), and the rest listed in commit `4a25365`, each with a test.

### Performance, with its conditions

On one machine, no network hop in any figure: the matcher alone 862,813
orders/s on one thread; the engine with pre-signed orders 413,000–589,000
applied/s; four BFT validators on one 18-core M5 Max committing 60,000
signed orders/s, every signature verified by every validator, at 12–14 ms
p50 from proposal to commit. No multi-machine run exists yet, so no
end-to-end latency with a network in it has been measured.
[`docs/performance.md`](docs/performance.md) §0 sets each figure against
Hyperliquid's published ones.

### Known issues

Listed in full in [`SECURITY.md`](SECURITY.md). The ones that matter first:

* The shipped validator, oracle and treasury keys are derived from seeds in
  this repository.
* The gateway holds every user's trading key; withdrawals are disabled.
* One key sets every oracle price, with no bound on a single move.
* A single peer can stall a node several ways.
* Single-sequencer mode can fork after a power cut; BFT mode does not.
* Block bodies are pruned to the last 10 by default and there is no state
  sync, so a node that falls far behind cannot rejoin without a copy of a
  peer's database.
