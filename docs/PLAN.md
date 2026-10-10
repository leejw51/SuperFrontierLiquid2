# SuperFrontierLiquid — Build Plan

The development plan for a personal exchange prototype with on-chain central limit order books for
spot, perpetual and dated-futures markets, across crypto and synthetic equity
underlyings. Every order rests on chain, every fill is a state transition, and
every block commits to a merkle state root any node can recompute.

It is two things at once, deliberately.

A **playable exchange simulator**: sign in with a mnemonic, claim test money
from a faucet, and trade against bot market makers while funding accrues and
liquidations cascade around you.

An **experimental testbed**: deterministic execution, Cap'n Proto RPC,
RocksDB for consensus state, SQLite for application state, and every
third-party dependency behind a trait with a mock on one side and room for a
real implementation on the other.

Hyperliquid is the design reference. There is no affiliation with it or with
any other exchange, and nothing here settles into a real asset.

---

## 1. Status

Fifteen workstreams have run: nine building, four reviewing, and the fixes that
followed. Every figure below is measured rather than estimated.

| Layer | State | Evidence |
|---|---|---|
| L1 core: spot, perps, dated futures | Working | 1,538 workspace tests, 125 suites |
| Market universe: 8 assets, 17 instruments | Done | `genesis-full.jsonl` |
| Infrastructure ports (traits) | Done | 15 tests |
| SQLite store, event bus, mocked feeds | Done | 108 tests |
| Design system, 8 screens | Done | `docs/design/reference.html` |
| Gateway: REST + WebSocket + EVM login | Done | 395 tests |
| Web app: Rust to WebAssembly, 8 screens | Done | 70 tests, 2 MB bundle |
| Simulation game + invariant checker | Done | 33 tests, 11 defects found |
| Oracle price publisher | Done | 33 tests, all 10 derivative markets live |
| Peer-to-peer sync | Done | 61 tests, live 3-node mesh |
| Order-throughput benchmark | Done | see section 10 |
| End-to-end suite over the real binaries | Done | **52 tests, all passing** |
| Exchange-core defect fixes | Done | F1–F12 closed |
| Security fixes | Done | 10 findings closed |
| UI fixes | Done | 6 criticals closed |

**The honest summary.** The stack runs end to end and it is verified doing so,
not asserted: a suite that boots the real node and gateway drives the whole
journey — sign in with a mnemonic, claim the faucet, place an order, watch it
rest and fill — and every one of its 52 tests passes.

What remains is written down rather than hidden. Every exchange-core defect
found so far is closed, including the funding-rate unit conflict in section 8,
which took the reading the design intended and needs a fresh genesis.
The throughput target is not met: 30,650 matched orders per second against a
target of 50,000. And several operator controls bind only the gateway process,
which the API now says plainly instead of implying otherwise.

### What the reviews cost, and were worth

Four reviews ran against a live stack rather than reading code, and found 124
defects that 1,432 unit tests had not. The reason is worth keeping: **every unit
test agrees with the component it tests.** It took crossing process boundaries
to find that a block's mutation was not transactional, that the operator role
was claimable by any stranger, that the faucet minted before it recorded, and
that the product looked excellent on fixtures and hostile on the empty chain
every real first visitor sees.

---

## 2. Architecture

```
            Browser — Rust compiled to WebAssembly
                    |  JSON + WebSocket
                    |  every number is a decimal string
                    v
        +-------------------------+        SQLite
        |       cb-gateway        |<---->  users, sessions, faucet ledger,
        |  EVM signature login    |        candles, leaderboard, audit log
        |  custodial trading key  |
        +-------------------------+
                    |  Cap'n Proto RPC
                    v
        +-------------------------+        RocksDB
        |        cb-node          |<---->  accounts, orders, books, positions,
        |       sequencer         |        margin, blocks, merkle trie
        +-------------------------+
             ^                ^
             |  gossip        |  Cap'n Proto RPC
        +---------+      +-------------------+
        | replica |      | cb-simgame / bots |
        +---------+      +-------------------+
```

**Two storage engines, on purpose.** RocksDB holds consensus state, where write
throughput and deterministic iteration order decide block time. SQLite holds
application state, where relational queries and migrations matter and consensus
does not. Neither is reachable except through a trait.

### Crate map

Ported from the upstream Layer 1:

| Crate | Responsibility |
|---|---|
| `cb-types` | Addresses, hashes, asset and market ids, fixed-point scales, symbols |
| `cb-crypto` | keccak-256, secp256k1 sign and recover |
| `cb-schema` | Cap'n Proto schemas and canonical encoding |
| `cb-trie` | Merkle trie over a key-value store |
| `cb-state` | RocksDB column families, state root |
| `cb-tx` | Transactions, signing, validation |
| `cb-engine` | Pure matching, margin, funding, liquidation maths |
| `cb-block` | Block builder and deterministic executor |
| `cb-consensus` | Votes and quorum certificates |
| `cb-genesis` | Genesis loader |
| `cb-rpc` | Cap'n Proto server, client and streams |
| `cb-net` | Peer-to-peer sync: handshake, discovery, gossip, catch-up |
| `cb-node` | Sequencer and replica binary |
| `cb-cli`, `cb-client` | Keygen, send, query, trading REPL |
| `cb-dashboard` | Operator status dashboard |
| `cb-sim` | Throughput stress simulator |

Built for this project:

| Crate | Responsibility |
|---|---|
| `cb-ports` | Trait definitions for every replaceable dependency |
| `cb-store-sqlite` | Application database over SQLite, with migrations |
| `cb-bus` | Event bus: in-memory implementation, Kafka-shaped API |
| `cb-feed` | Mocked crypto and equity price sources, seeded |
| `cb-gateway` | Browser-facing REST and WebSocket, Cap'n Proto bridge |
| `cb-web` | Rust and WebAssembly trading terminal |
| `cb-simgame` | Bot-driven simulation with continuous invariant checks |

### The replaceability contract

`cb-ports` holds the traits. Nothing above them may name a concrete backend;
wiring happens once, in a binary's composition root.

| Trait | Default here | Production substitute |
|---|---|---|
| `AppDatabase` | SQLite | PostgreSQL, MySQL |
| `EventBus` | in-process broadcast | Kafka, NATS |
| `KvStore` | RocksDB | any ordered key-value store |
| `MarketDataFeed` | seeded random walk | exchange feeds, vendor APIs |
| `EquityReferenceData` | static fixture | reference data vendor |
| `CustodyProvider` | simulated | qualified custodian |
| `IdentityProvider` | EVM signature | OIDC, SSO, WalletConnect |
| `Clock` | injectable | system clock |

Each trait ships a fault-injecting decorator, so callers can test failure paths
rather than only the happy one.

---

## 3. Product scope

### Markets

| Class | Underlyings | Instruments |
|---|---|---|
| Crypto | BTC, ETH, SOL | spot, perpetual, dated future |
| Equity | AAPL, TSLA, NVDA, SPY, MSFT | spot, perpetual |

Eight assets, seventeen instruments. Decimals are BTC 8, ETH 18, USD 6, SOL 9
(matching lamports), equities 6. Equity leverage caps at 5x against BTC's 50x,
because a synthetic with no arbitrage path to a real share can decouple, and
single names gap on earnings.

Equity instruments are cash-settled against a mocked reference price, which is
what allows an equity perpetual to exist at all.

### Screens

| Route | Screen |
|---|---|
| `/` | Landing, and the two-path mnemonic sign-in |
| `/trade` | Trading terminal |
| `/portfolio` | Equity, margin, liquidation risk, positions, ledger |
| `/faucet` | Claim test money, cooldown, grant history |
| `/leaderboard` | Ranking, with bots marked as bots |
| `/chain` | Block explorer |
| `/account` | Profile, both addresses, sessions, activity |
| `/admin` | Operator dashboard |

### Player loop

1. Sign in with a disposable BIP-39 test mnemonic — reuse a prototype wallet,
   or generate a fresh one. The phrase never leaves the browser.
2. Claim faucet funds. Rate-limited, recorded in an append-only ledger.
3. Trade spot, open leveraged perpetual positions, hold a future to expiry.
4. Watch funding accrue, get liquidated, climb the leaderboard.

Bot traders — market maker, momentum, noise, whale, liquidator, arbitrageur —
keep the books alive so a lone human always has a counterparty.

---

## 4. Identity: Ethereum-compatible signatures

An account is a secp256k1 key derived from a BIP-39 mnemonic at the BIP-44 path
`m/44'/60'/0'/0/i`. The address is EIP-55 checksummed hex. Login is a signed
challenge using EIP-191 `personal_sign`. The mnemonic never reaches the server.

The consequence worth stating: this is not a bespoke login. The same mnemonic
gives the same address here, in MetaMask, in `cb-client`, and in the SuperFrontier
Wallet. Every piece is verified against published `eth-account` vectors rather
than against our own implementation, which is the difference between being
correct and being self-consistently wrong.

**Two addresses, and why.** Logging in proves control of an address but hands
the gateway no key, so it cannot sign chain transactions as you. Each account
therefore has a `login_address` you control and a `trading_address` whose key
the gateway derives as `HKDF(server_secret, login_address)` and never stores.
That key holds the test money and signs orders, so placing a trade is one click
rather than a wallet prompt. This is a simulator trade-off, stated in the API
contract and on the account page rather than hidden. A production exchange
would have the chain accept user-signed orders; that is a named follow-on.

---

## 5. Milestones

| | Milestone | State |
|---|---|---|
| M1 | Baseline: ported workspace, stable toolchain, tests green | Done |
| M2 | Ports, SQLite store, event bus, mocked feeds | Done |
| M3 | Market universe: SOL and equities, symbol mapping | Done |
| M4 | Gateway: REST, WebSocket, EVM login, faucet | Done |
| M5 | Frontend: eight screens in Rust and WebAssembly | Done |
| M6 | Simulation game and invariant checker | Done |
| M7 | Exchange-core defect fixes | In progress |
| M8 | Peer-to-peer sync | In progress |
| M9 | Throughput: meet the table in section 10 | In progress |
| M10 | Hardening: fault injection, persistence gaps, runbook | Not started |

Each milestone ends with `cargo test --workspace` green and a commit.

---

## 6. Team

| Role | Owns |
|---|---|
| PM | Plan, API contract, docs, milestone acceptance |
| Designer | Design system, screen specifications, interaction states |
| Core | `cb-types`, `cb-schema`, `cb-genesis`, market universe |
| Core fixes | `cb-engine`, `cb-block`, `cb-state` defect repair |
| Gateway | `cb-gateway`, auth, faucet, Cap'n Proto bridge, WebSocket |
| Frontend | `cb-web` |
| QA | `cb-simgame`, invariants, defect reports |
| Performance | Benchmarks, profiling, `docs/performance.md` |
| Networking | `cb-net`, peer-to-peer sync |

`docs/api-contract.md` is the frozen seam between gateway and frontend.
Changing it requires updating that document in the same commit and announcing
it to both owners.

---

## 7. Invariants, and which of them hold

The simulation asserts these at every block boundary, not by sampling.

| | Invariant | State |
|---|---|---|
| 1 | Conservation of every asset, except at explicit mint and burn | Holds only because funding never flows |
| 2 | No negative free or locked balance | Holds |
| 3 | Locked balance equals margin plus resting collateral | Holds on derivatives, fails on spot |
| 4 | Best bid strictly below best ask at every block boundary | Holds |
| 5 | Long open interest equals short open interest | Holds |
| 6 | Accounts below maintenance reach the liquidation queue | Holds |
| 7 | Funding transfers net to zero per tick | Holds only because funding never flows |
| 8 | Two nodes replaying identical blocks reach an identical root | Holds |
| 9 | A future at expiry settles every position, leaving zero interest | Holds |

Invariant 1 is the one to watch. Comparing mark-to-market equity drifts with
price and truncation, so the checker instead tracks a quantity provably
unchanged across opens, closes, flips, funding, liquidation fees and
settlement. Any movement beyond budgeted rounding dust is a leak, not noise.

---

## 8. Known defects

Found by the simulation, each with a committed reproduction under
`rust/crates/cb-block/tests/qa_*.rs`. Full detail in `docs/qa-report.md`.
All twelve are **fixed**, and each reproduction asserts the correct behaviour
instead of the broken one. F11 and F12 closed on 2026-09-20; see
`docs/core-review.md` for what was decided and why.

| | Defect | Severity |
|---|---|---|
| F1 | A reduce-only order flips the position and skips the margin gate | Critical |
| F2 | New positions inherit a zero funding index and are charged for a period before they existed, minting collateral | High |
| F3 | Margin accounts sit outside the state root, so replicas can disagree while reporting identical roots | High |
| F4 | Bad debt is written by three paths, read by one, and ignored by every risk calculation | High |
| F5 | An account blocks its own liquidation with a resting order | High |
| F6 | A resting spot buy filled in two pieces over-unlocks and panics the executor | High |
| F7 | Spot price improvement is locked forever and never returned on cancel | Medium |
| F8 | Perpetual maker and taker fees are dead fields, never charged | Medium |
| F9 | The funding premium is never computed, so funding is inert | Medium |
| F10 | A zero-quantity perpetual order aborts the executor under debug assertions | Medium |
| F11 | Perpetual resting orders reserve no collateral, so N orders each pass the gate independently | Medium |
| F12 | Funding rate is clamped per hour but accrued per block, a 3,600x overcharge | High |

A second set was found by the core review of 2026-09-19 and is tabled in
`docs/core-review.md` under its own numbering (C1–C2, H1–H3, M1–M6, L1–L6),
each with a Sim and a Real severity. The twelve with a fix carry a reproduction
under `rust/crates/cb-block/tests/{ordering,core_review,oracle_update,futures_settle}.rs`,
`rust/crates/cb-net/tests/sync.rs` and `rust/crates/cb-node/tests/system_txs.rs`.
The largest was not an exchange defect at all: **nothing on a live node produced
the `FundingTick` the executor requires at every funding boundary, so every
chain started from the shipped genesis halted at height 3,600.** The sequencer
now produces the required system transactions itself.

F11 needed a per-owner index on the perpetual order column family, which was
keyed by order id alone; `perp_orders_by_owner` is that index, and the
initial-margin gate now charges an account for every order it has resting.

A twelfth defect surfaced during the repair: liquidation of a dated future
resolved through the perpetual market table alone and returned "unknown
market", rejecting 1,536 triggers in 400 blocks. Margin is cross-market, so the
liquidation engine does not get to know about only one contract kind.

### F12, fixed: the funding rate had two contradictory units

Found only because fixing F9 made funding actually flow. It is the most
consequential open item in the project.

`FundingClamp::DEFAULT` documents its outer bound as **0.5% per hour**. The
executor then accrues `rate * elapsed_blocks`, with a comment stating the
protocol treats the rate as a **per-block** unit. Settlement lands on a
funding-interval boundary of 3,600 blocks. So a rate capped as hourly is
applied 3,600 times per hour: at the ceiling that is 1,800% per interval
instead of 0.5%.

Nobody noticed because funding was inert. With it flowing, a seeded squeeze now
pays $3.46M of funding, drains the insurance fund to zero and leaves $1.24M of
bad debt. No invariant breaks — the value is conserved, it just moves violently
— which is precisely why a conservation check alone would never have caught it.

Two readings are self-consistent and they cannot both be right:

1. The rate is per block, and the clamp documentation is wrong. A 0.5%
   per-block ceiling is not a defensible funding cap at any block time.
2. The rate is per funding interval, and the accrual is wrong. A per-block
   delta should then be `rate * elapsed / interval_blocks`, which makes the
   hourly settlement match the documented cap.

Reading 2 was taken, since every constant is named per hour and the interval
is 3,600 blocks: `accrue_index_delta` now pro-rates the interval rate over the
blocks elapsed. It changes every funding number on the chain, so a chain must
be started from a fresh genesis. If the owner decides the rate should instead
be per block, the change is one line in `cb-engine/src/funding.rs`.

### Gaps outside the exchange core

- The node exposes no trade stream, so the gateway derives trades per block.
  Spot trades and candles are unavailable.
- Gateway-side state — suspensions, market halts, risk overrides, session
  metadata, handle changes — does not survive a restart, because the
  application store implements the database port and not the key-value port.
- `lot_size` and per-market margin parameters are stored and displayed but not
  enforced by the executor; a default tier table gates every order.
- ETH/USD spot is unlisted: ETH's 18 decimals against the fixed divisor give a
  sub-unit wire price scale. Surfaced as an error rather than rounded away.
- A wrong-chain transaction is dropped at block build rather than rejected at
  submit. Correct, but a poor experience.

---

## 9. What comes next

In priority order, because the list above is longer than any one person should
hold in their head.

1. **Fix F1.** A margin bypass is the defect that makes every other number
   meaningless.
2. **Fix F6**, then re-enable spot in the simulation, so spot invariants are
   exercised at all.
3. **Fix F2, F3, F4, F5.** After these, invariants 1, 3 and 7 should hold for
   real rather than vacuously.
4. **Land peer-to-peer sync**, and make gossip verify by default.
5. **Batch the order-book column-family writes**, which the profile names as
   the next throughput lever.
6. **Close the persistence gap** by giving the gateway a durable store for its
   own state.
7. **Enforce `lot_size` and per-market margin** in the executor, so the genesis
   parameters stop being decorative.

---

## 10. Throughput target

The goal is to beat the published figures for the system this imitates:
roughly 20,000 to 50,000 orders per second sustained, with bursts near 200,000.

A goal is only real if it is measured the same way every time, so the target is
stated as four numbers rather than one:

| Metric | Target | Measured by |
|---|---|---|
| Sustained matched orders/sec | > 50,000 | `cb-order-bench in-process` |
| Sustained orders/sec over RPC | > 20,000 | `cb-order-bench rpc` |
| p99 submit-to-fill | < 250 ms | `cb-order-bench --target-orders-per-sec ...` |
| Block time under load | stable at the configured interval | dashboard metrics |

The two rates differ because they measure different things. The in-process path
measures the matching engine and state commitment. The RPC path additionally
pays for Cap'n Proto decode, a mempool round-trip and signature recovery on
every transaction. Signature recovery was long assumed to be *the* bottleneck;
it is now measured at 47% of the submit handler, which makes it the largest
single item but not the whole story — see `docs/performance.md` §4.

**No optimisation lands without a before-and-after number from the same
command on the same machine.** A throughput claim without a reproducible
measurement is a guess, and a guess that says the work is finished is worse
than no claim at all.

### Baseline measured 2026-09-18, and what it actually measures

Release build, Apple Silicon, 32 synthetic traders, 50 ms block interval,
in-process generator:

| Reading | Value |
|---|---|
| Sustained transactions/sec over 60 s | 296,029 |
| Average transactions/sec over the run | 216,488 |
| Blocks produced | 3,599 |
| Transactions applied | 10,262,225 |
| Rejected | 0 |

**Do not quote that number as an order rate.** It is not one, and the running
system will tell you it is, which is the problem.

The in-process generator signs `TxBody::Transfer` of one unit
(`cb-node/src/benchmark.rs`). It never places an order. But the dashboard
increments `buys`, `sells`, `btc_orders` and `eth_orders` for every one of
those transfers, and reports `recent_orders_per_sec`. So the operator console
displays 296,000 "orders per second" while zero orders have been placed.

Two further reasons the figure is not comparable to a published exchange
number. The generator pushes **pre-verified** transactions straight into the
sequencer's channel, so signature recovery — the known bottleneck — is off the
measured path entirely. And it **bypasses the mempool**, which the module
header states outright as a deliberate choice to raise achievable pipeline
depth.

The RPC-path simulator (`cb-sim run`) also submits transfers rather than
orders. The perpetual scenario simulators do place orders, but they exist to
test liquidation and funding behaviour, not to measure rate.

**At the time that baseline was taken, this project had no order-throughput
benchmark at all.** It now does — see the next section — and the mislabelled
counters are fixed: transactions and orders have separate counters, the
in-process transfer generator no longer touches the order counters, and the
sequencer classifies each block's bodies so the apply side can report both.

### Order throughput, measured 2026-09-18

`cb-order-bench` (`cb-node/src/order_bench.rs`) places and matches real spot
CLOB orders and reports submitted, applied and **matched** rates separately.
Full methodology, per-phase profiles and the before/after for every change are
in **`docs/performance.md`**. Headlines: Apple M4, release build, 500-deep book,
50/50 maker/taker at a 100% cross rate, zero rejected.

| Metric | Target | Measured | |
|---|---|---|---|
| Sustained matched orders/sec, in-process | > 50,000 | 30,650 (61,321 placements/sec) | not met |
| Sustained orders/sec over RPC | > 20,000 | 19,052 applied (9,518 matched) | not met, within 5% on placements |
| p99 submit-to-fill | < 250 ms | 68 ms in-process; under 250 ms over RPC below ~9,000 orders/sec | met in-process, met over RPC below that load |
| Block time under load | stable at the interval | 30 ms p50 against 50 ms, nearly flat in book depth | met |

Getting there took two fixes, both found by profiling rather than by guessing.
The spot executor rebuilt the entire order book from RocksDB before **every**
order — 88.6% of runtime against 6.4% spent matching — so the book is now
hydrated once per block. The matcher then materialised the entire opposite side
of the book on every order, roughly 80% of what remained, so the maker walk now
stops at the first non-crossing price level and once it has gathered enough
resting quantity to fill the taker. Measured on one stable code state:
**178 → 20,558 matched orders/sec at depth 500, a 115x change**, and 114 →
19,394 at depth 2,000. A concurrent change to the spot settlement path by
another engineer took it further, to 30,650. Block time under load went from 5.9
seconds to 30 milliseconds against a 50 ms target.

`cargo test --workspace` is green at 1,432 tests, and the determinism,
state-root and replica-sync tests all pass. The bounded maker walk returns a
prefix of the old walk, which a dedicated test checks against a reference
implementation, so the same makers fill in the same order for the same
quantities and the state root is unchanged.

The guess that ECDSA recovery is the RPC bottleneck was half right and worth
measuring rather than acting on. Recovery is 47% of the `submitTx` handler, and
that handler is 74% of a single-threaded RPC server, so parallelising recovery
alone is capped at about 1.9x. The mempool is not a rate limit at all: drain plus
decode runs at 2.2 M transactions/sec. The two levers the profile actually
justifies are batching the order-book column-family writes — four separate
RocksDB writes per order today, 26% of the block loop — and stopping the
`CF_ORDERS` scan from degrading as deleted orders accumulate.

We do not beat the target yet, and the 200,000/sec burst figure is not close on
any path.

---

## 11. Peer-to-peer sync

Today a replica connects to one configured sequencer, pulls blocks and votes
back. That is a star, not a network: it has a single point of failure, it does
not discover peers, and a replica that falls behind can only catch up from the
leader.

What a peer-to-peer layer has to add:

1. **Handshake** carrying chain id, genesis hash and software version, so a node
   on the wrong chain is rejected at connect rather than discovered later
   through a state-root mismatch.
2. **Peer discovery** from seed nodes, plus peer exchange, with a bounded peer
   table.
3. **Block gossip** so a block reaches the network in log time rather than the
   leader serving every replica itself.
4. **Transaction gossip** so an order submitted to any node reaches the
   sequencer.
5. **Catch-up from any peer**, by height range, with the existing re-execution
   and state-root check as the acceptance test. A peer that serves a bad block
   is caught by the same verification that already exists.
6. **Peer scoring and banning**, so a peer that serves bad data is dropped
   rather than retried forever.

Gossip must not weaken the guarantee that makes the chain trustworthy: a block
is adopted only after re-execution reproduces its state root. Faster propagation
must never become a reason to trust a peer.

---

## 12. Non-goals

Real custody, real money, real equity settlement and legal compliance are all
explicitly out of scope. Validator decentralisation is no longer a non-goal:
since 2026-09-25 a chain can run under BFT ordering among the genesis
validators (`make startbft`, ADR 0008), with the single sequencer kept as the
other scheme. What is still out of scope is a *dynamic* validator set. The spot market
registry stays enumerated rather than dynamic; a dynamic registry touches
consensus and is a follow-on.

Client-side transaction *signing* is a follow-on. Login is already the user's
own key; what stays custodial is the trading key that signs orders, so placing
a trade does not need a wallet prompt. Since the money is test money the backend
mints, that is a reasonable trade for a simulator and a bad one for anything
else.

---

## 13. How to run it

```
make setup     # check tools, write rust/.env, apply genesis
make up        # start chain, gateway and web app
make open      # print every URL the stack serves
make faucet    # open the faucet and claim test money
```

`make` on its own prints the menu. Node-level targets — the three-validator
cluster, the benchmarks, the perpetual scenarios — live in `make -C rust help`.
