# SuperFrontierLiquid — Status and Forward Plan

Written 2026-09-19. This is the hand-over document: where the project actually
stands, what was built, what is broken, and what to do next in what order.

`docs/PLAN.md` is the design. This is the state.

**2026-09-25: BFT.** The chain can now order blocks by a Byzantine-fault-
tolerant protocol among its validators — chained HotStuff, two-chain commit,
one-block finality — with execution after commit (ADR 0008). Four validators
as processes on one Apple M5 Max (18 cores, 36 GB — not the ten-core M4
every other figure in this document was taken on) commit 60,000 signed
orders a second at 5 ms p50 and 15–29 ms p99 from proposal to commit with
no backlog, and 73,000–76,000 on three of the four when offered 150,000
(2026-09-29; the 2026-09-25 code did 60,000 at 12–14 ms), every signature
verified by every validator, all four at the same root at every height,
and three of them carry on at full speed when one is killed. About half
of every committed block is the load generator's orders refused for empty
balances, so the applied rate is about half the committed one. "Commit" is the transaction's
proposal being ordered by a quorum, counted by the `consensus status` line,
and the latency is proposal timestamp to commit on one clock with no network
hop in it; `docs/performance.md` §0 sets these next to Hyperliquid's
figures with the same labels. A killed validator restarted on a quiet chain syncs the blocks it
missed and is voting again within seconds; restarted while the load
continued, it synced and stayed consistent but had not resumed voting when
a 75 s window ended (ADR 0008, *Consequences*). The single-sequencer scheme
is untouched and still the `make start` default. `make startbft`,
`make loadbft`. What is not done: that rejoin under sustained load, and
above ~80,000 offered orders/s one validator falls behind on proposal
validation; equivocation is logged, not slashed. The gateway now reads
the block's `applied` mask, so its order records no longer take
inclusion for execution (closed 2026-09-29, ADR 0008, *Consequences*). The
rest of this document describes the sequencer scheme unless it says
otherwise.

---

## 1. Where it stands

| | |
|---|---|
| Crates | 26 |
| Rust | 133,792 lines |
| Documentation | 9,940 lines |
| Workspace tests | 2,096 across 180 suites, all passing, 55 ignored (2026-09-25 audit; was 1,696 before it) |
| End-to-end tests over the real binaries | 52, all passing |
| Commits | 30 |

**The product works end to end, and that is verified rather than asserted.** A
suite boots the actual node and gateway binaries and drives them as a browser
would: sign in with a BIP-39 mnemonic, claim the faucet, place a limit order,
watch it rest, fill and cancel. Every one of its 52 tests passes.

What is not finished is correctness in two named places and speed against the
stated target. Both are in section 3, with nothing hidden.

---

## 2. What was built

Nine building workstreams, four reviews, and six rounds of fixes.

### The chain

A Layer 1 with on-chain central limit order books for spot, perpetual and dated
futures across eight assets and seventeen instruments. Deterministic execution,
Cap'n Proto RPC, RocksDB for consensus state behind a merkle trie.

Block execution is **transactional**: the database has a staged mode, so a
failed transaction is rolled back and the block keeps the rest. State, block
bytes and head pointer commit in one batch, and the state root is checked
before anything is written.

Peer-to-peer sync replaced the leader-star topology: handshake, seed peers,
peer exchange, announce-then-fetch gossip, catch-up from any peer, and peer
scoring with temporary bans. Gossip verifies unconditionally, and forwarding
happens only after local acceptance, which makes "forward at most once"
structural rather than a rule someone has to remember.

### The exchange around it

A gateway translating between the browser and Cap'n Proto, with Ethereum-compatible
signature login: keys derive from a mnemonic at `m/44'/60'/0'/0/i`, addresses
are EIP-55, and the login challenge is signed with EIP-191. Verification is
pinned to published `eth-account` vectors, so the same phrase opens the same
account here, in MetaMask and in other compatible EVM tooling.

An oracle publisher keeps all ten derivative markets priced, driving the mocked
feed through the same port a production feed would replace.

A trading terminal in Rust compiled to WebAssembly: eight screens, a 2 MB
bundle, and all nine of the states a trading interface lives or dies by.

A bot-driven simulation that re-derives nine invariants from raw state at every
block boundary, and an order-matching benchmark that measures matches rather
than transfers.

### Infrastructure, kept replaceable

Every third-party dependency sits behind a trait in `cb-ports`: the application
database, the event bus, the ordered key-value store, market data, reference
data, custody, identity and the clock. Nothing above them names a backend, and
each ships a fault-injecting decorator so callers can exercise failure paths.

---

## 3. What is open

Ordered by what it costs, not by where it lives.

### 3.1 Decided on 2026-09-20: the funding rate is per interval

*(Kept for the record; the decision is made and the code follows it. If the
owner wants the other reading, it is one line in `cb-engine/src/funding.rs`
and a fresh genesis.)*

**The funding rate had two contradictory units.** The clamp documents itself as
0.5% per hour; the executor accrues `rate * elapsed_blocks` calling the rate
per-block; settlement lands every 3,600 blocks. So an hourly cap is applied
3,600 times an hour. A seeded squeeze pays $3.46M of funding, drains the
insurance fund to zero and leaves $1.24M of bad debt.

It was invisible until funding was made to flow, and **no invariant catches
it** — value is conserved, it just moves violently. Two readings are
self-consistent: the rate is per block and the documentation is wrong, or the
rate is per interval and the accrual should divide by `interval_blocks`. The
second is almost certainly intended.

This decides what a block is worth in funding time and changes every funding
number on the chain, so settle it before the next genesis rather than patching
after.

**2026-09-27: the index also left out the price.** Settlement pays
`qty * Δindex / 1e9`, and the index accrued the bare rate, so every market
paid funding as if it traded at $100: a 1 BTC long at $50,000 paid $0.05 an
interval at 0.05% instead of $25. The index now accrues `rate * mark / $100`
(`cb_engine::accrue_index_delta`), so a position pays notional times rate.
This changes execution: an existing chain's blocks no longer replay to
their recorded roots, so start a fresh genesis.

### 3.2 Capabilities the chain does not have

Each of these forces the gateway to either refuse honestly or hold state it
cannot guarantee. All are now labelled in the API rather than implied away.

| Missing | Consequence |
|---|---|
| `AdjustMargin` is unsupported in the executor | No gateway-created account can trade a derivative at all |
| No halt or risk transaction | Operator controls bind one process, are ignored by other clients, and vanish on restart |
| `FuturesSettle` carries no price and is proposer-only | An operator cannot settle a future |
| `lot_size` and per-market margin are not enforced | A default tier table gates every order; genesis parameters are decorative |
| No transaction-status call | An order dropped at block build returns `201 pending` with only a log line as evidence |

### 3.3 No open exchange-core defects

F11 closed on 2026-09-20: resting perpetual orders now reserve margin through
a per-owner order index and an initial-margin gate that charges the account's
potential position. Every defect the simulation and the two reviews found is
fixed; `docs/core-review.md` lists the five that need a fresh genesis.

### 3.4 Throughput is short (as measured 2026-09-18; superseded by §3.4a and ADR 0007)

The table is kept as the starting point. Every figure is the M4 (10 cores,
16 GB), single sequencer, merkle-trie root, `cb-order-bench` in-process.

| Metric | Target | Measured 2026-09-18 |
|---|---|---|
| Matched orders/sec, in-process | 50,000 | 30,650 |
| Orders/sec over RPC | 20,000 | 19,052 applied, 9,518 matched |
| p99 submit-to-fill | 250 ms | 68 ms in-process |
| Block time under load | stable | 30 ms against a 50 ms interval |

Latency and block time are met. The 200,000 burst figure is not close on any
path. The profile names the next lever: the order book performs four separate
writes per order that should be batched. Signature recovery is 47% of the
submit handler but only caps at about 1.9x, and the mempool is not a limit at
all. (The four writes now go to the block's in-memory stage and reach RocksDB
in one batch on the writer thread; that lever, as stated here, is closed.)

### 3.4a Throughput: sixteen defects found by profiling, all fixed — 221,586 orders/s unverified, 36,806 verified

Found and fixed 2026-09-21 by profiling `make start2` (the full stack plus
`cb-sim` buyers and sellers over RPC) with `sample` and re-measuring after each
change. Every number in this section is one machine — the Apple M4, 10
cores, 16 GB of `docs/performance.md` §1 — running the single-sequencer
scheme with no quorum, a chain built from `genesis-full.jsonl`, and the same
workload: four load workers over 64 traders, 90% spot orders on two markets,
10% transfers.

| After | Applied tx/s, steady state |
|---|---|
| (before any of this) | 1,000–1,400, decaying from 3,600 |
| book hydrated through the index | 1,600–2,000 |
| mempool in memory | 2,900–3,900 |
| order column families tuned | 9,600–11,000 |
| RPC on its own threads | 52,000–55,000 |
| order books carried between blocks | 48,000–54,000 |
| open-file limit and chain-id read fixed | 52,000–54,000 orders/s, 0 rejected in 3,127,000 |
| per-order reads removed, RocksDB write off the matching thread | 50,000–53,000, unchanged: end to end is bound by signature recovery, not the block loop |

The engine on its own, `make orderbench` (pre-signed, no network): **132,834
orders/s applied and 66,400 matched/s**, against 30,650 matched/s in §3.4's
table and a project target of 50,000. That is the merkle-trie engine of
2026-09-21; ADR 0007 replaced the root with a hash chain on 2026-09-23 and
re-measured the same benchmark at 571,000–589,000 applied / 285,000–294,000
matched (128 accounts, `--producers 8 --prefill`) and 413,000–417,000 /
206,000–208,000 (100,000 accounts). The 118,233 / 59,093 and 124,419 /
62,188 quoted elsewhere are earlier readings of the same day's work, before
defects 8 and 9. `make orderbench-rpc` went from 9,049 to
16,933 applied/s (one connection, one recovery core each side — it cannot see
the RPC pool).

**Where it ends up: three targets.** The owner set them on 2026-09-21.

| Target | What runs | What it measures | Result, the ten-core M4, single sequencer |
|---|---|---|---|
| `make start` | the stack plus one cb-sim worker at 20 orders/s over RPC | nothing; the books move for testing | — |
| `make start2` | the stack plus the node's in-process generator, pre-verified orders straight into the sequencer | the block loop and matcher with nothing in front of them (validation is the risk engine's job) | **221,586 orders/s**, 8 signing threads |
| `make start3` | the stack plus six cb-sim workers signing real orders over RPC, every signature recovered on a pool across all cores | the whole path a client takes, verification included | **36,806 orders/s** (40,895 tx/s, 5,008 per block, 8.2 blocks/s), nothing refused. Without the gateway, oracle and web app alongside, 60,166. One connection alone 22,380 |

`make start2` in detail — on the same machine, on a chain 1.26 million
blocks deep:

| | orders applied/s | buys/s | sells/s | rejected |
|---|---|---|---|---|
| `make start2`, 6 signing threads | 192,690 | 96k | 96k | 0 |
| `make start2 STRESS_WORKERS=8` | **221,586** | 110,869 | 110,717 | 0 |

Blocks of about 48,000 orders every 230 ms, RocksDB written on the writer
thread, state roots computed, the dashboard and gateway attached. On this
machine the generator and the block loop are within about ten percent of
each other — eight signers at the measured ~28,000 signs/s each is ~224,000/s,
and 48,000 orders per 230 ms block is ~210,000/s — so which one is the
ceiling cannot be separated on ten cores; going from six signers to eight
raised the figure, which says the generator was the limit at six and says
nothing about eight. Hyperliquid's published figure for HyperCore is 200,000
orders/s, on dedicated validator hardware with pipelined BFT finality; this is
one machine also running the generator, a single sequencer with no quorum,
and pre-verified input by the owner's own premise.

The sixteen, in the order the profile surfaced them. The first five were found
on a fresh chain; six and seven only appear once a chain has grown, and were
found by running the fixed node against the 5.8 GB chain this testing built;
eight and nine came from profiling the engine benchmark after the rest; ten
to twelve from the first run of the pure stress test; thirteen to sixteen
from the verified stress test (`make start3`) and from running against the
chain those runs left behind. Each fix that could change behaviour carries a
test.

**1. The spot book was rebuilt by scanning every order on every market.**
`BlockState::spot_book_mut` called `Orders::list_for_market`, which scans
`CF_ORDERS` with an empty prefix and filters afterwards — 70% of the process's
CPU while fewer than a hundred orders were resting, because the iterator walks
every key a fill deleted. `BookIndex::list_orders_for_market` now reads one
`market ‖ side` prefix per side. The code had asked for exactly this: *"when
the order CF outgrows it we add a per-market secondary index."*

**2. The mempool was a column family.** `Mempool::drain(n)` took `n` items but
visited every delete marker before them, so its cost grew with everything the
node had ever processed: 61% of CPU once the book scan was gone, and the reason
blocks carried 120 transactions against a ceiling of 10,000. It is now a
`BTreeMap` on the `Database` handle, in the same hash order. Pending
transactions no longer survive a restart, which is what a mempool should be.

**3. The order column families were tuned for bulk, not churn.** A 32 MiB
memtable buffers on the order of half a million deletes before it flushes, and
every scan walks them in memory. `CF_ORDERS`, `CF_BOOK_INDEX` and the three perp
equivalents now use a 2 MiB buffer, compact L0 at two files and re-compact after
a minute. This is what removed the decay: throughput had fallen by two thirds
over ten minutes and now holds flat.

**4. The RPC server shared the block builder's thread.** `cb_rpc::serve`, the
sequencer loop and the dashboard were all `spawn_local` on one
`current_thread` runtime, so the node used one core of ten and admitted nothing
while a block was building. Signature recovery — 35% of the process — ran there
too. `cb_rpc::run_local_serve_multi` now accepts on one thread and pins each
connection to one of N worker runtimes (`--rpc-threads`, default half the
cores, capped at eight). This was the single largest win: 11,000 to 55,000.

**5. The books were thrown away at the end of every block.** Hydration was
still half the executor's time even through the index. `BlockState` now takes
the previous block's books from a slot on the `Database` handle, stamped with
the height they describe, and leaves its own there at commit. A block whose
write never lands leaves a stamp no later block accepts, so every abandoned
path rebuilds without anything having to notice. `spot_book_cache.rs` runs the
same 30 blocks — partial fills, cancels, and a transaction that fails after
touching the book — with the cache and with it defeated, and compares state
roots block for block.

**6. The open-file limit was set for the test suite, not for a chain.**
`Database::open` capped RocksDB at 64 open SST files, to keep the parallel test
suite inside the process's descriptor limit. The chain this load had built held
204 of them. RocksDB reopened readers continuously and, because
`TableCache::FindTable` serialises on a mutex, every point read queued behind
the reopen: 64% of the block loop in `__psynch_mutexwait`, **32 transactions a
second** where a fresh chain did 53,000. The limit is now 1,024, and
`CB_MAX_OPEN_FILES` lowers it for a test run that would rather spend throughput
than descriptors. The same profile is why the order column families keep
RocksDB's default file size rather than the 4 MiB this work first gave them —
smaller files drop markers sooner but multiply exactly the count this limit is
spent on.

**7. The chain id was read from RocksDB once per transaction.** The executor's
chain-id gate called `Meta::chain_id()` for every transaction. Free on a small
database; on the one above it was the point read stuck behind that mutex.
`BlockState` reads it once when the block starts. Genesis sets the value and
nothing changes it, so every replica still compares against its own chain's id.

Together, 6 and 7 took the existing 5.8 GB chain from 32 applied tx/s to
10,411, with rejections falling from 28% to 0.002%. It stays below a fresh
chain until compaction has worked through what the earlier runs left behind.

**8. Two RocksDB point reads per order that needed no read.** The executor
fetched the spot market's configuration from `CF_MARKETS` for every placement,
and re-read the maker's order row to learn `(side, price, seq)` before it could
delete the index entry for a fill — a fifth of the executor's time in the
engine benchmark, for a row the matcher had just held. `BlockState` now caches
market configuration per block (genesis writes it, nothing in the executor
changes it), and `spot_book::BookMutation::DeleteOrder` carries the index key.
Engine benchmark: 124,419 → 131,039 orders/s applied.

**9. The block's RocksDB write ran on the matching thread.** The last thing
the block loop did was one synchronous `write_batch` — 17% of the thread, plus
the block-body persist — during which the matcher sat idle. The direction
asked for was that the matcher own one core and touch memory only, with
every heavy call elsewhere; the write is the heaviest call that can leave.
`Database::enable_async_writes` (on by default in the sequencer,
`--sync-writes` to opt out) hands each committed batch to a writer thread and
serves its rows from an overlay that every read on every handle consults
between its own stage and RocksDB, until the write lands. Two rules make it
safe, both tested in `db.rs` and `cb-block/tests/async_writes.rs`: readers look
at the overlays before RocksDB and the writer writes before it retires an
overlay, so a key is always in one of the two places a reader looks; and
nothing is announced outside the process until the write returns — the head
notification, the leader's own vote and pruning now run from the writer, and
the block stream, `getBlock`, the p2p announcer and the dashboard read a
`durable_view` that skips the overlays. A crash loses at most `MAX_PENDING`
(8) blocks nobody outside was told about. The window it does not close: a
balance read over RPC during that interval can revert. Same binary, same
minute, engine benchmark: 114,309 → 132,834 orders/s applied, 57,140 → 66,400
matched; the commit phase fell from 31.8% to 18.4% of the block loop.

End to end it changed nothing measurable — 50,000–53,000 tx/s before and
after — and the block count says why: the loop, no longer waiting on the
write, produces four times as many blocks a quarter the size, and the applied
rate stays where signature recovery on four connections puts it. The engine
gained; the path in front of it is the limit now. Rejections rose from none to
0.05% over 2.3 million submissions, the pipelined-nonce cascade in `cb-sim`
finding more seams in a faster-turning loop.

**10. The loop cut a block the moment anything was pending.** Harmless while
the synchronous write throttled it; once the write left the thread the loop
ran at 1,300 blocks a second with six transactions each, and the per-block
fixed costs ate what the write had freed. `run_sequencer` now cuts a block
when the interval has elapsed or a full block is waiting, whichever first.

**11. A batch cancel scanned the orders column family and threw the book
away.** `apply_batch_cancel_spot` found the sender's orders with
`Orders::list_for_market` — the same whole-CF scan as defect 1 — and both
cancel handlers then invalidated the cached book, so the next placement
rebuilt it through the index, the stage and every pending overlay. With
makers cancelling every 256 orders and 50,000-order blocks that was 46% and
42% of the matching thread: 32,000 orders/s. The handlers now read the
sender's orders from the book (`SpotBook::resting_orders_of`, same id order
the scan gave) and take each cancelled order out of it (`SpotBook::cancel`)
instead of dropping it. `spot_book_cache.rs` cancels every third block and
compares roots against a rebuild-every-block chain. 32,000 → 192,690.

**12. The dashboard shared the matching thread.** Under a saturating load the
block builder never yielded long enough to answer an HTTP request, so the one
tool for reading the node under load went dark exactly then. It has its own
thread. (The side and per-market tiles were also counted twice under the
in-process generator, once at submission and once at application; only the
applied path moves them now.)

**13. Signature recovery was pinned to the connection's thread.** One
connection was one core of admission, ~14,000 transactions a second, and the
gateway holds exactly one connection. `cb_rpc::verify` puts every
connection's submissions into one queue in arrival order; a coordinator
recovers each batch's senders on a rayon pool (`--verify-threads`, 0 = all
cores) and then admits the batch **in order** — chain id, nonce ledger,
mempool — so a client's pipelined nonces are never seen out of sequence.
`verify_pipeline.rs` pushes 2,000 of one sender's nonces through it at once:
all admitted, none refused. One connection went from ~14,000 to 22,380
orders/s, at which point the client's own signing is the limit.

**14. The mempool drained in hash order.** Harmless while admission was slower
than block production, because every drain took the whole queue and the
ordering step sorted each sender's run. With admission faster than the loop
the queue held more than a block, a drain of the first N by hash took a
random subset of each sender's nonces, the executor refused everything after
the first gap ("bad nonce: expected 1010, got 1013") and dropped it, the
ledger fell back to the chain's nonce while clients were hundreds ahead, and
six connections collapsed to 264 orders/s with a third refused. The mempool
is now per-sender queues in nonce order, drained round-robin from each
sender's front, so a drain never hands the builder a gap. Test: three senders
interleaved, a partial drain, every included run contiguous from its lowest
nonce. 264 → 60,166.

**15. Pruning on the writer thread deadlocked the node.** The durability
callback ran `prune_one`, which writes; a write issued from the writer thread
waits for that thread to drain its own queue. The sequencer then blocked on a
full queue while the writer sat inside pruning, and the node produced nothing
while answering the dashboard — which is how it was found. Pruning is back on
the sequencer thread, and `Database::write_batch_then` now writes straight
through when it is called from the writer thread, so no future callback can
reintroduce it. `a_write_from_a_durability_callback_does_not_deadlock` is the
test.

**16. Startup pruning walked the chain from height 1.** Switching pruning on
for a chain that had never had it meant one read, decode and delete per block
of the whole history — 1.26 million — on the thread the sequencer starts on.
`Blocks::lowest_height` finds the real starting point with one seek, a budget
caps the first pass at 50,000, and `prune_one` works through the rest a block
at a time.

Also on the way: the 1,024 open-file limit was still a cliff — a day of
stress runs grew the working chain to 79 GB and 3,551 SST files, and the
same thrash came back at 500 orders/s — so the default is now 16,384, with
`CB_MAX_OPEN_FILES` for test runs. Not unlimited: `-1` makes RocksDB prefetch
every SST's index and filter blocks at open, which on that chain was minutes
of startup with the node up and producing nothing. And both stress tiers now
prune block bodies (`--pruning --pruning-keep-recent 2000`), which is why
that chain grew: 48,000 orders a block, every block kept.

And one in the Makefile, found because `make start` on a running stack began
to fail loudly instead of quietly: the node `start` recipe kept its
"already running" check and its launch on separate recipe lines, so the
`exit 0` in the check ended only that line's shell and the launch still ran,
died on the RocksDB lock, and — before the pid was written only after
survival — overwrote the live node's pid file and truncated its log. That is
the whole story behind the `make status` that lied at the start of this work.
The check and the launch are one shell now; `make start` twice is harmless
again, and says so.

What still runs on the matching thread and is not matching, with why:

* **Trie hashing, ~12%.** The state root goes in the header, block N+1's
  ordering is seeded by block N's hash, and its `prev_hash` is that hash. So
  the hash chain is serial by this chain's design; no thread count changes
  that. It could still be made cheaper — hash the accounts trie's subtrees in
  parallel inside `cb-trie`, batch its node writes — but not moved. *(Moot
  since ADR 0007: the root is a SHA-256 hash chain over the block's diff and
  the tries are no longer on the block path; `cb-trie` has no caller outside
  its tests and a bench.)*
* **Block encoding, ~8%.** The body's bincode for storage. It could be
  produced on the writer, at the cost of a pending block's bytes not being in
  the overlay; nothing reads them from there today, but the exception would
  need its own argument.
* **Ordering, ~5%.** One keccak per sender per block; parallelisable, small.
* **Cold account loads.** Prefetched in parallel; the one disk read left in
  execution. Carrying accounts across blocks through the same stamped slot the
  books use would remove it — the natural next step for a memory-resident
  matcher, kept out of this change deliberately. *(Done in ADR 0007: accounts
  are carried. The startup load, `preload_state`, is still called only by the
  benchmark, not by the sequencer or the validator, so the first touch of an
  account after a restart is still a read.)*

Two things this did **not** fix, both still open:

* `cb-sim` strands a trader's pipelined transactions on `BadNonce` when one of
  them is refused. At 4,000 in flight over 10 traders that was a 43% rejection
  rate and a twentyfold throughput collapse; the default in-flight window is
  now 1,500, which holds it at 0.004%, but the cascade itself is untouched.
* The remaining engine cost is 62% execute, 25% trie commit, 7% persist. The
  next levers are the four unbatched book writes per order that §3.4 already
  names, and parallel execution. *(Both since overtaken: the trie commit is
  gone with ADR 0007's hash chain, and the book writes go to the in-memory
  stage. The levers open on 2026-09-25 are listed in `docs/performance.md`
  §0.)*

**Against Hyperliquid.** Their documented figure is 200,000 orders/s for
HyperCore. On 2026-09-21 this engine was at 118,233 applied and 59,093
matched on the ten-core M4 (132,834 / 66,400 by the end of the day; ADR 0007
then took it past 200,000 applied on 2026-09-23 with the hash-chain root and
a prefilled queue). The gap that remained on the day was architectural rather
than a defect: their order book is in memory and never touches a key-value
store per block, their consensus is pipelined, and their number is an engine
figure measured on their own infrastructure. The end-to-end RPC figure here
can never reach it on one machine for a simpler reason — signing costs about
as much as recovering, so 200,000 orders/s would need roughly ten cores
signing and ten more recovering. The comparison with every axis labelled —
engine, verified single sequencer, verified BFT, finality, and the two
machines involved — is `docs/performance.md` §0, and it is the only place
the comparison is made.

### 3.4b Leader failover, and every node keeping ten blocks

Added 2026-09-21, both asked for directly: a node that keeps only the last N
blocks, and a cluster where killing the leader does not stop the chain.

**Retention.** `--keep-blocks`, default 10, `0` to keep everything. On for
leader and follower alike; a replica used to keep every block it had ever
received while the node producing them pruned. Account state, the four tries,
the markets and the head are never pruned: bodies are the archive, and the
archive is a choice. `retention.rs` pins the window, the configurability and
that the chain is unaffected by what was pruned.

**Failover.** Four defects had to be fixed before the word meant anything:

1. **A block's proposer was checked against the address genesis names.** Any
   block from a new leader was refused by every replica, so a takeover would
   have been a split, not a handoff. The check now asks the validator set who
   leads `header.view` — which `build_and_apply` had documented all along and
   the header had always carried. `leader_rotation.rs`.
2. **The node refused to *produce* unless it was that same genesis address.**
   The mirror image of the first, on the other side: a node that correctly took
   over view 1 then declined to start. It now leads if it is
   `validators[view % N]`.
3. **A node's view never moved.** Only headers carry it, so a follower sat at
   genesis's zero for ever and would have computed the next leader from a view
   nobody was in. `apply_remote_block` now advances it with the chain.
4. **Silence was read as death.** The first version took over from a leader
   that was merely idle, within five seconds of starting, because a chain with
   nothing to do produces no blocks at all. The timer now only prompts the
   question; the leader answers it, by its block stream breaking or its
   `chainInfo` going unanswered.

The safety rule, once corrected: a candidate may lead when its head is at most
one block above the highest certificate it holds. A certificate for block N
travels in block N+1's header, so one uncertified block at the tip is the
ordinary state of every follower — the first rule demanded `head == last_qc`
and would have blocked every takeover on a live chain. Two is more than the
rest of the network can be assumed to hold, and the candidate waits.

Measured on the three-node cluster: leader at height 190, killed; the next
validator took over in its own process and produced blocks at view 1; the
third node found it through `--peer` and followed; both survivors ended at
height 418 on the same state root, with nothing refused.

What it does not do: a leader whose process is alive and answering but has
stopped producing is not detected, which needs the followers to agree among
themselves. A fast-mirror replica can never take over. Clients and the gateway
still have to be pointed at the new leader themselves. Crash faults only —
three validators tolerate one silent node and no Byzantine one.

### 3.5 Test coverage gaps

**The largest: no derivative position is ever opened end to end.** A fresh chain
had no oracle price, so `POST /positions/close`, `/margin`, `/leverage` and
`/admin/markets/{symbol}/settle` have no coverage, and liquidation, funding and
settlement are untested at the gateway seam. The oracle publisher now closes the
way in; the harness needs to use it.

Also uncovered: market orders, immediate-or-cancel, fill-or-kill, post-only and
reduce-only, so the two order-rejection codes that depend on them are untested.
Expired sessions and expired challenges need an injectable clock. The browser
itself is not covered by this suite.

### 3.6 Accepted gaps

A peer-to-peer replica does not vote, so that topology produces blocks that are
agreed but never final; use the cluster configuration when finality matters.
There is no transport encryption on the peer layer, survivable only because
gossip confers no authority — a modified block fails re-execution.

Gateway state that does not survive a restart: the order tracker, the account
mirror, suspensions, session metadata and transfer memos.

### 3.7 The core review

Later on 2026-09-19 a code audit of everything the security review had not
covered — crypto, transaction encoding, the executor, ordering, consensus, the
replica syncer and the trie — found seventeen items. `docs/core-review.md` has
each with a Sim and a Real severity and the test that pins it. Twelve were
fixed the same day; the ones that matter most:

* **The chain halted at height 3,600.** Nothing on a live node produced the
  `FundingTick` the executor demands at every funding boundary, so every
  deployment died at its first one, dropping the mempool once per interval for
  ever. The sequencer now signs the required system transactions itself, from
  the same list the enforcement reads.
* **Verifying replicas never checked a transaction signature.** They used the
  leader's pre-recovered senders, so re-execution certified the leader's
  arithmetic and nothing else. Every sender is now recovered and compared.
* Replicas now check `prev_hash` and `tx_root`; a future expiring without a
  price defers settlement instead of halting the chain; a same-block
  cancel-after-place from one account no longer drops the cancel; an oracle
  post cannot claim a height the chain has not reached; high-S signatures are
  refused; votes for phantom hashes are refused and the vote pool is pruned;
  blocks are capped at 65,535 transactions; `Withdraw` fails closed instead of
  stranding funds; a future is frozen from its expiry height.

The five that were documented rather than fixed on the day — the signing-hash
domain tag, self-liquidation earning the keeper share, futures never entering
the reduce-only halt, unvalidated block timestamps, sender-address priority —
were fixed on 2026-09-20, together with F11 and F12. Four of those changes
(domain tags, the futures flag, the funding unit, the order index) require a
**fresh genesis**; a data directory from before them will not verify.

---

### 3.8 2026-09-22: the simulator's controls, and three bugs found by using it

The brief for the day was to make the exchange a playable simulator with an
operator who can change the rules, in a 16-bit skin, and to drive the whole
thing from a browser under test. Doing that found three defects that every
earlier review had missed, because none of them is visible from a fresh chain.

**The gateway sat at height zero on a live chain.** `make start` against a
chain that had been running for a day showed `Height 0 · Genesis` in every
browser while the node was at 1.45 million. The gateway asks to stream blocks
from height one; the node prunes bodies below `head − keep_blocks` and its
`BlockSub.next` waited on a notify for a body that would never be re-emitted.
The gateway was connected, healthy and permanently blockless. Fixed on both
sides: `chainInfo` now carries `headHeight` and `oldestBlock`, `streamBlocks`
starts at the oldest body it holds when asked for one it has pruned, a body
pruned mid-stream is an error rather than a wait, and the gateway floors its
start height and defines `synced` as *has reached the head*, not *has a
socket*. `make start` also keeps 3,000 bodies instead of ten, because a
gateway that falls one second behind a hundred-millisecond chain otherwise
loses its stream. Tests: `cb-rpc/tests/submit_and_get.rs`,
`cb-gateway/src/node/bridge.rs`.

**The block stream broke every few minutes at a lag of three blocks.** Both
of the node's stream servers read the block body first and the head second.
The body and the head commit in one batch, so a body read a moment before the
batch and a head read a moment after made a block that existed look pruned,
and the stream failed with "missing from Blocks CF (pruned?)" while three
blocks behind. The gateway reconnected and lost nothing, but the replica's
raw stream has the same shape and a replica that re-anchors is a replica with
a gap. Both servers now read the head first; a body at or below it is stored
or pruned, never "not yet". `cb-rpc/src/stream.rs`.

**The web server reset one connection in five.** The Python static server had
the default listen backlog of five; a page that asks for twenty-five files at
once had connections reset by the kernel. It showed up as sprites that
randomly did not load and, in the browser suite, as a WebAssembly bundle that
never arrived and a page that stayed blank. `rust/scripts/serve_web.py` now
listens with a backlog of 256 and speaks HTTP/1.1 keep-alive.

**A new user's faucet grant never reached their browser.** The watcher
differences each block against a per-account baseline and seeds silently when
it has none. The order and transfer handlers prime that baseline before
submitting; the faucet did not, and the faucet is the first transaction most
accounts ever make. The grant landed on chain and the browser's equity read
zero until a reload. `POST /faucet/claim` now primes the mirror, the client
re-reads the account when a balance update arrives, and the faucet page
reloads its history on that signal rather than one block too early. Test:
`cb-gateway/tests/api_user.rs`.

**What was built.** A gateway-owned simulation switch (`--sim-bin`,
`--sim-mode off|on|max`, `--oracle-control`) that launches and kills `cb-sim`
workers and forwards scenario changes to the oracle; `make start` hands the
default buyer and seller to it instead of launching one by hand.
`GET /chain/status` names the RPC facts and `GET /chain/txs` lists the last
transactions the chain applied. The user page carries a menu of every screen
and the RPC panel; the dashboard carries the transactions table and the same
panel; the admin page carries the three-position switch. The skin is
`rust/crates/cb-web/styles/retro.css` with Grok-generated sprites under
`assets/`, documented in `docs/design-system.md` §13. `web-e2e/` is a
Playwright suite over the real stack: sign in with a phrase, faucet, buy,
sell, dashboard, and the switch. `make web-e2e` runs it.

**A funded account could not place its first spot order.** The order ticket
checked every order against `available_margin`. A spot-only account has no
margin, so a user holding a hundred thousand dollars was told the thirty-dollar
buy "needs 30.00 USD of margin, 0.00 free". `ticket::validate` now checks a
spot buy against the free quote balance and a spot sell against the free base
balance, and says which asset is short. Found by the browser suite's buy step;
pinned in `cb-web/src/ticket.rs`.

**A population switched on right after another was switched off died at
once.** A killed `cb-sim` worker leaves up to `max_in_flight` transactions in
the mempool under nonces the chain has not reached; the next worker read its
nonce from state, submitted its opening cancel, was refused, and exited. Seen
switching MAX → ON from the admin page. The switch now lets the mempool drain
for a moment between populations, and `cb-sim` re-reads the nonce and retries
its clearing step for a few seconds before giving up.

**Two languages.** The client speaks English and Korean: `cb-web/src/i18n.rs`
holds the Korean table, the switch sits with the other preferences, and
Galmuri 11 provides pixel Hangul. Documented in `docs/design-system.md` §13.

**Lint.** The toolchain on the machine moved to 1.98 and `make lint`
surfaced about eighty new clippy findings across the workspace, none of them
behavioural: unused imports left by earlier refactors, `sort_by` that should
be `sort_by_key`, doc-list indentation, hex literals grouped unevenly, wide
argument lists, manual checked division. About forty are fixed — every crate
this work touched (`cb-rpc`, `cb-gateway`'s new modules, `cb-sim`, `cb-web`)
and the base crates (`cb-types`, `cb-trie`, `cb-schema`, `cb-state`,
`cb-block`, `cb-genesis`, `cb-dashboard`, `cb-client`) — and the capnp
generator's parentheses are allowed at the generated modules. Some forty
remain in `cb-node/src/order_bench.rs`, `cb-net`, and the older `cb-gateway`
modules (`memory`, `tracker`, `leaderboard`, `marketdata`, `directory`);
`cargo clippy --workspace --all-targets -- -D warnings` lists them. None
blocks a build or a test.

## 4. The plan, in order

Each step says why it comes where it does.

**1. ~~Settle the funding-rate units.~~** Done: per interval. Reverse it in one
line if the decision goes the other way.

**2. Close the oracle gap in the test harness.** This is the highest-value
engineering step, because it unlocks roughly a third of the untested surface at
once: positions, liquidation, funding and settlement all become reachable. The
publisher exists; the harness needs to post an `OracleUpdate` before it trades.

**3. Give the chain the transactions the operator surface needs.** Halt, risk
and settlement should be chain transactions rather than gateway memory. Until
they are, an operator control is a suggestion to one process.

**4. Implement `AdjustMargin`.** Without it no ordinary account can hold a
derivative position, which makes most of the product unreachable for a real
user.

**5. ~~Batch the order-book writes.~~** Done: the block runs on a staged
`Database` and every write lands in one batch on the writer thread. What the
profile justifies now is in `docs/performance.md` §0 and ADR 0007's *Next*
list: call `preload_state` from the node, write order rows once per block from
the book's final state, and — under BFT — stop the leader hashing its own
payload twice. Re-measure with `make orderbench` before and after; a change
without a number does not land.

**6. ~~Reserve collateral for perpetual resting orders.~~** Done.

**7. Enforce `lot_size` and per-market margin**, so genesis parameters stop
being decorative.

**8. Give the gateway a durable store for its own state**, so suspensions and
halts survive a restart. The database port exists; the key-value port is what
is missing behind it.

**9. Carry votes over gossip**, so a peer-to-peer topology reaches finality.
This is consensus work rather than sync work and was deliberately left out.

---

## 5. What today's reviews cost, and were worth

Four reviews ran against a live stack rather than reading code, and found 124
defects that 1,432 unit tests had not.

The reason is worth carrying forward: **every unit test agrees with the
component it tests.** It took crossing process boundaries to find a block whose
mutation was not transactional, an operator role any stranger could claim by
typing a handle, a faucet that minted before it recorded, and a product that
looked excellent on fixtures and hostile on the empty chain every real first
visitor sees.

The reviews are kept in full: `flow-review.md`, `security-review.md`,
`ui-review.md`, `qa-report.md`, `performance.md`, `testing.md`.

Two findings were **refuted with evidence** rather than accommodated — a decimal
scale flagged as critical that was correct, and a market count that was wrong —
which is what makes the rest of them worth acting on.
