# Performance

Measured order-matching throughput for SuperFrontierLiquid, how it was measured,
and what each number does and does not include.

The rule this document exists to enforce: **state what a number measures in the
same breath as the number.** The failure this project already contained was a
true number with a false label — a six-figure "orders per second" produced by a
generator that placed no orders. Every figure below names its path, its
workload and its exclusions.

---

## 0. Re-measured 2026-09-21, after the throughput work

Everything from §1 down describes the code as it was before `docs/STATUS.md`
§3.4a — the first of the sixteen defects found by profiling a loaded node,
fixed the same day. The methodology in §2 and §10 still holds; the figures
below replace the ones in §3, §4, §7 and §8. **One machine: the Apple M4,
10 cores, 16 GB of §1. Single sequencer, no quorum, merkle-trie state root
(the scheme ADR 0007 has since replaced).** Two later measurements are on
other code and, for the second, another machine, and are kept in their own
records rather than mixed into this table: ADR 0007 (2026-09-23) re-ran
`make orderbench` under the hash-chain root, and ADR 0008 (2026-09-25) is
the four-validator BFT chain on an M5 Max (18 cores, 36 GB). The
comparison section at the end of §0 states all of them side by side with
their labels.

| Benchmark | What it measures | Then | Now |
|---|---|---|---|
| `cargo bench -p cb-state --bench bench_spot_match_throughput` | the spot matcher alone: `SpotBook::place` in memory, one thread, nothing else | — | **862,813 orders/s, 431,407 matched/s** |
| `cargo bench -p cb-state --bench bench_order_match_throughput` | the perp matcher alone, same idea | ~360,000 orders/s | 310,000 orders/s |
| `make orderbench` | matcher, executor, tries, commit; pre-signed, no network. This row is the 2026-09-21 merkle-trie engine; ADR 0007 re-measured it on 2026-09-23 under the hash chain at 571,000–589,000 applied / 285,000–294,000 matched (128 accounts, `--producers 8 --prefill`) and 413,000–417,000 / 206,000–208,000 (100,000 accounts) | 30,650 matched/s | **66,400 matched/s**, 132,834 applied/s |
| `make bench` | transactions/s, pre-verified transfers past the mempool | ~280,000 tps | 239,294 tps over 60 s |
| `make orderbench-rpc` | one client, one connection, over TCP | 19,052 applied/s | 18,860 applied/s, 9,411 matched/s |
| `make start2` | the shipped stack with the in-process generator: pre-verified buyers and sellers straight into the matcher (the pure stress test; validation is the risk engine's job) | — | **221,586 orders/s** with 8 signing threads, 192,690 with 6 |
| `make start3` | the shipped stack, six clients signing real orders, every signature recovered on a pool across all cores, admission in arrival order | 1,200 tx/s, falling (as `make load`) | **36,806 orders/s** (40,895 tx/s), nothing refused; 60,166 with node and clients alone; one connection 22,380 |
| `make start && make load` | the same over RPC, before the verification pool (four clients) | — | 58,012 tx/s, 52,127 orders/s |

Three of those need a word.

**`make start2` measures the node with nothing in front of it.** The
generator runs inside the node, signs on its own threads, and hands each
order to the sequencer as a `VerifiedTx` with the sender attached; no
signature is recovered, no RPC is crossed. That is the premise the owner set —
order validation belongs to a risk engine ahead of this node — and it is the
same premise behind Hyperliquid's engine figure. Everything after admission
is real: the block loop, matching, balances, the four tries and the state
root, RocksDB on the writer thread, the gateway streaming blocks. On this
machine the eight signing threads (~224,000 signs/s) and the block loop
(~48,000 orders per ~230 ms block, ~210,000/s) are within ten percent of
each other, so neither can be called the ceiling from this run; separating
them needs more cores than the generator and the node share here.

**`make orderbench-rpc` did not move, and cannot.** Its client opens a single
Cap'n Proto connection, and its server is `cb_rpc::run_local_serve`, the
single-threaded one. Signature recovery for that connection therefore runs on
one core at each end, which is what its ~19,000 orders/s is. The node the
Makefile actually starts now spreads connections over a pool
(`cb_rpc::run_local_serve_multi`), so this benchmark no longer describes it:
`make start && make load`, with four clients signing over RPC, reached 52,127
orders/s on the same machine before the verification pool, and `make start3`
36,806 (60,166 with node and clients alone) after it. (An earlier draft of
this sentence attributed the 52,127 to `make start2`, which is the in-process
generator with no RPC in it at all — the mislabelling this document exists to
prevent.) Reading this row as the node's RPC ceiling would understate it by
about threefold.

**The matcher is not the cost.** Alone, the spot book places 862,813 orders a
second on one thread — 1.2 µs an order — which is four times Hyperliquid's
published figure for their whole engine. The block loop around it applies
132,834 a second, 7.5 µs an order, so matching is 12% of what the loop does
per order and the other 88% is bookkeeping. A sample of the loop on the
no-signature path attributes that remainder: trie hashing 19% (the accounts
trie, keccak over roughly 160 nodes per touched account), account load and
journal 12%, bincode serialisation into the stage 9%, cold-account prefetch
7.5%, ordering 6% (one keccak per sender per block), staged writes 6%, block
encoding 1.5%. Keccak in all its places is 29% of the thread. The levers,
in order of size, as named on 2026-09-21: carry accounts across blocks in
memory (removes the prefetch and most of the load cost); hash the accounts
trie's subtrees in parallel, or change its shape — a 160-deep binary trie
costs four times the hashes of a hex Patricia trie, but that changes every
state root; serialise once at commit rather than per mutation.

Where those levers stand on 2026-09-25, read from the code rather than
re-measured: accounts *are* carried across blocks (`BlockState::finish` hands
them to the next block through the stamped slot the books use, ADR 0007
decision 4), though the node itself still does not call `preload_state` at
startup — only `cb-order-bench` does — so an account's first touch after a
restart is still a RocksDB read. The accounts trie is *gone* from the block
path: the root is the hash chain of ADR 0007 (`cb_state::state_hash`), and
`cb-trie` has no caller outside its own tests and a bench, so parallelising
it would change nothing measured. Accounts are serialised once at commit
(`BlockState::finish`); order rows are still encoded on every `Orders::put`,
including the read-decode-encode-put of a partial fill. And the "four
separate writes per order" of §3.4 and §9 no longer reach RocksDB at all —
every column-family write goes to the block's in-memory stage and lands in
one batch on the writer thread — so what is left of that item is four map
inserts and their undo-journal entries per placement, not four `write()`
syscalls.

**`make orderbench` now writes the way the node does.** Each block's RocksDB
write goes to a writer thread while the next block is built, and the measured
window closes only when the last write has landed. `--sync-writes` restores the
old behaviour for comparison: same binary, same minute, 114,309 applied/s
synchronous against 132,834 asynchronous, with the commit phase falling from
31.8% to 18.4% of the block loop. What remains in that phase is the trie hash,
which the header needs and block N+1's ordering depends on, so it stays on the
matching thread by design.

**`make start3` is bounded by the clients on one machine.** Six cb-sim
workers each sign on one thread, and on a box that is also running the node,
the gateway, the oracle and the web server they reach 40,895 tx/s between
them; with only the node and the clients on it, 66,995. The node sits near one
core of ten with the verification pool mostly idle and the mempool empty, so
neither the pool nor the block loop is what stops it. The figure that shows what the pool changed is
the single connection: 22,380 orders/s where recovery pinned to that
connection's thread gave about 14,000, and now the client's own signing is
what stops it. A second machine for the clients would show the node's side
alone.

**`make bench` measures transfers, not orders**, through the in-process
generator that skips signature recovery and the mempool. It is unchanged work,
and the throughput work above did not target it; the difference from the older
figure is run-to-run and hardware state, not a regression anyone introduced.

`make orderbench-determinism` replays one workload twice and the state roots
still match.

### Latency, paced rather than saturated

Open-loop latency is queue depth, not service time — §4.2 makes that point and
it still applies. Paced to 8,000 orders/s over RPC (5,735 achieved), submit to
fill is **p50 26.8 ms, p99 56.6 ms**, nothing rejected.

Read it for what it is. The client sits on the same machine, so no network is
in it, and this node is a single sequencer: "filled" means the sequencer put
the order in a block, not that a quorum finalised it. A figure from a chain
with validators voting would be larger by at least the round it takes a
quorum certificate to ride in the next block.

### Re-measured 2026-09-29: the BFT chain after the throughput audit

The 2026-09-25 record above read "the box saturates at 40,000–47,000" and
blamed signature recovery. Profiling a validator under load found
otherwise, and five changes followed, each measured alone (commits
`e459cdc`, `cb52098`, `f6878ab`, `04eebb6`, `dd7b0b7`). **Conditions for
every row: one M5 Max (18 cores), four validators as processes plus the
load generator on it, no network hop, every signature recovered by every
validator, `make startbft` + `make loadbft`.** "Committed" is transactions
ordered in committed proposals per validator (`consensus status`, `txs`
over 5 s); latency is proposal timestamp to commit on one clock.

| After | 60,000 offered: committed | p50 | p99 | Backlog | Executor per block |
|---|---|---|---|---|---|
| 2026-09-25 code | 51,000/s | 60–70 ms | 103–137 ms | growing; 20% of submissions refused `BadNonce` | 39 ms (42 µs a transaction) |
| A. rollback keeps the order books | 51,000/s | 29–42 ms | 57–78 ms | none | 1.7 ms (1.6 µs) |
| A + C. a refused transaction consumes its nonce | 49,000/s | 32–35 ms | 58–73 ms | none; 20% still refused, the admission ledger was reset on every refusal | 1.5 ms |
| A + B + C. propose past committed-but-unexecuted | 60,000/s | 9–15 ms | 25–46 ms | none | ~1 ms |
| + D. bounded parallel jobs, threads budgeted per validator | 60,000/s | 4 ms | 11–17 ms | none | ~1 ms |
| + E. duplicates dropped at admission (8 workers) | 60,000/s | 5 ms | 15–29 ms | none, 0 refused | 0.4–5 ms |

What each found: (A) `BlockState::rollback` cleared every cached order
book, so each refused order made the next placement rebuild both books
from RocksDB through the tombstones of every filled order — three
quarters of the executor's time. (B) The leader's nonce baseline was the
*executed* account nonce; with execution a block or two behind commit,
every sender's next transaction looked "ahead" and proposals shrank to a
few dozen while mempools held 250,000. (C) A refused transaction left a
hole in its sender's nonce sequence, stranding the client's pipelined
window. (D) Four validators each sized two rayon pools to all 18 cores;
144 threads spun for jobs of a few microseconds. (E) A forwarded copy
arriving after the proposal it was in was recovered again and queued as a
dead entry.

Offered more than the chain commits:

| Offered | Generator | Committed | p50 | p99 | What happened |
|---|---|---|---|---|---|
| 100,000 | 8 workers | 51,000–78,000/s on three validators | 44–60 ms | 110–206 ms | the fourth validator fell behind (tens of seconds on its clock) and caught up after the load |
| 150,000 | 8 workers, 1,500 in flight each | 73,000–76,000/s on three validators; the generator itself delivered 77,000/s | 70–83 ms | 142–220 ms | as above; the chain took everything the generator could offer |
| 200,000 | 12 workers, 4,000 in flight each | collapsed to 2,000–10,000/s | — | — | one validator's mempool hit its 262,144 cap, admission refused, the generator's nonces desynchronised (598,000 `BadNonce`) |

**Applied against committed.** The generator's traders run out of balance
and keep sending, so about half of every committed block is refused at
execution with `Insufficient` — the whole of the refusals, by the
per-block reason histogram the executor now logs. The applied rate is
therefore about half the committed rate: ~30,000 applied orders/s at
60,000 committed, ~37,000 at 75,000. That is the load generator's
workload, not the chain's cost — a refused order costs the executor a
rollback of nothing — but a figure quoted as "orders/s" against
Hyperliquid's should be the applied one until the generator funds its
traders. The 2026-09-25 figures were taken with the same generator and
carried the same share of refused orders; they were not labelled so.

**The engine's ceiling on this box, for scale** (`make start2`, single
sequencer, the whole stack — node, gateway, oracle — with the node's own
generator; 2026-09-29, M5 Max). The stress tier now starts a fresh chain
from `genesis-stress.jsonl`, whose 64 traders hold 1e32 of every asset so
nothing is refused for balance, and `CB_BENCH_UNSIGNED=1` makes the
generator stamp placeholder signatures instead of signing — the node
recovers nothing on this path either way, so the block loop does the same
work. With the generator signing on 8 threads: **458,000 orders/s
applied**, bounded by the signing (the node process at 874% CPU). With
placeholder signatures: **1,023,000 orders/s applied, none refused**, the
block-loop thread at 100% and the generator's threads at 7–9% — the
single-threaded ordering, matching and state bookkeeping is the bound.
(871,000 before `PendingNonces::retire_settled` stopped reading an account
row per transaction for a ledger that held nothing: 11% of the thread.)
Of the thread: matching ~15%, order and book-index row writes ~15%,
account reads and decodes ~9%. No signature anywhere, no consensus, no
network: the number says what the engine and its block loop can do on one
core, and nothing about a validator.

**What bounds it now.** Not signature recovery, which this box does four
times over at 75,000/s with cores to spare. Above ~80,000 offered, one of
the four validators falls behind: the leader's proposal reaches it before
the forwarded copies of the transactions in it, so it recovers every
signature in the proposal on the validate path, queues behind that, and
never catches up while the load lasts. The others carry on — the fault
model at work — but the fourth's mempool fills. Two levers, neither taken
yet: have validation wait briefly for the forwarded copies, or have the
proposal carry the recovered senders (which a validator still verifies,
about twice as fast as recovery). And the generator shares the machine:
with twelve signing workers it is a third of the box.

### Against Hyperliquid's published figures

This is the one comparison table; `README.md`, `docs/STATUS.md` §3.4a and
ADR 0008 point here rather than carrying their own. Hyperliquid's
documentation gives 200,000 orders/s for HyperCore, block finality around
0.07 s, and a median of about 0.1–0.2 s end to end for a co-located client.
Those are figures for a production network of many validators on dedicated
hardware, with every order signed by a client and verified by every
validator. Each row below says which of those conditions the figure here
shares, and on which machine it was taken. Two machines appear: the **M4**
(10 cores, 16 GB) of §1, on which everything in this document and in
`docs/STATUS.md` §3.4a was measured, and the **M5 Max** (18 cores, 36 GB)
of ADR 0008. The two are not compared with each other anywhere; nothing was
re-measured across the move.

**Where this design is ahead, by the axis measured:**

| Axis | Hyperliquid, published | Here | Machine, scheme, what is and is not in it |
|---|---|---|---|
| Matcher alone | 200,000 orders/s is their whole-engine figure | 862,813 orders/s placed, 431,407 matched, one thread | M4. `SpotBook::place` in memory. No executor, no state, no signatures, no network. A ceiling, not a system. |
| Engine, pre-signed | 200,000 orders/s | 571,000–589,000 applied / 285,000–294,000 matched at 128 accounts; 413,000–417,000 / 206,000–208,000 at 100,000 accounts (ADR 0007, 2026-09-23) | Machine not named in ADR 0007's record. `make orderbench ORDERBENCH_ARGS="--producers 8 --prefill"`: matcher, executor, hash-chain root, RocksDB on the writer thread. **No signature recovery, no network, no quorum.** The earlier 132,834 / 66,400 in the table above is the same benchmark on the merkle-trie root, M4, 2026-09-21. |
| Execution of a committed BFT payload | — | 10,000 orders in 11–12 ms on the executor thread, about 850,000 orders/s (ADR 0008) | M5 Max. One payload's execution time, not a sustained rate; the payload was committed at 60,000 orders/s. |
| Block finality, one box | about 0.07 s | 5 ms p50, 15–29 ms p99 at 60,000 orders/s; 70–83 ms p50 at 75,000 (2026-09-29; the 2026-09-25 code did 12–14 ms p50 at 60,000) | M5 Max, four validators as processes on localhost, every signature recovered by every validator. Proposal timestamp to commit on one clock. **No network hop in it**; each real hop adds one RTT, and a two-chain commit is three hops. |
| Submit to fill, one box, sequencer | 0.2 s median | 26.8 ms p50, 56.6 ms p99 at 5,735 orders/s | M4, single sequencer. No network, no quorum: "filled" is the sequencer putting the order in a block. |

**Where it is behind:**

| Axis | Hyperliquid, published | Here | Machine, scheme, what is and is not in it |
|---|---|---|---|
| Verified end to end, single sequencer | 200,000 orders/s | 36,806 orders/s with the whole stack and six signing clients on the box; 60,166 with node and clients alone | M4, `make start3`. Every signature recovered, over RPC, no quorum. Bounded by the clients' signing on the shared cores, not by the node — but the node's own ceiling on a second machine is not measured. |
| Verified, BFT-ordered | 200,000 orders/s, many validators, dedicated hardware | 60,000 orders/s committed at 5 ms p50 with no backlog; 73,000–76,000 sustained by three of four validators when offered 150,000, the fourth falling behind; about 37,000 of those *applied*, the rest refused for the generator's traders' empty balances (2026-09-29, the subsection above) | M5 Max, `make startbft` + `make loadbft`, four validators sharing 18 cores with eight signing workers. "Committed" counts transactions ordered in committed proposals (the `consensus status` line); under BFT a refused transaction stays in the block and consumes its nonce. Four recoveries a transaction on one chip; four machines would do one each. |
| End to end with a network, BFT | 0.1–0.2 s median | **not measured** | No run has had a network hop or a second machine in it. |
| Matched orders/s at 100,000 accounts, engine | not published; 200,000 is their order rate | 206,000–208,000 in ADR 0007's table, "not yet" past 200,000 in its text | The ADR's table and prose disagree by the run-to-run spread; counted here as level with their *order* rate, not ahead of it. At 128 accounts the matched count (285,000–294,000) is clearly past it. |

**Not comparable, and not to be quoted against 200,000:** `make start2` at
221,586 orders/s (M4, 2026-09-21). It is the shipped stack with the node's
own generator handing pre-verified orders straight to a single sequencer:
no signature recovery, no RPC, no quorum, and the generator's eight signing
threads within ten percent of the block loop on the same ten cores, so
neither is known to be the ceiling. It measures the sequencer scheme's block
loop under the risk-engine premise and nothing else.

**The honest reading, per axis.** In-process matching and execution are
faster than the published engine figure on every count that excludes
signatures and a network. Finality on one box is an order of magnitude
under 0.07 s, with no network in it. On the two axes the published 200,000
actually describes — verified orders a second through consensus, and a
median with a network in the path — this chain is at 60,000 committed
(about 30,000 applied, the generator's workload being half refused) with
no backlog on one shared machine, 75,000 with one of four validators
behind, and has no number for the second. "Better than
Hyperliquid" is true of the matcher and false of the system until a
multi-machine run exists; the comparison is not narrowed here to the axes
it wins.

What bounds the sequencer-scheme verified figure is secp256k1 recovery on
the shared cores. What bounded the BFT figure was not: the 2026-09-29
subsection above found the executor rebuilding order books, nonce holes
and a stale nonce baseline, and after those the bound is one validator
falling behind on proposal validation above ~80,000 offered. The engine
levers still open — order rows encoded per mutation, `preload_state` not
called by the node, the leader hashing its own payload twice — each cost a
few percent to a few tens of percent of a thread that is not the
bottleneck, and none of them moves either verified number.

## 1. Hardware and build

| | |
|---|---|
| Machine | Apple M4, 10 cores, 16 GB |
| OS | macOS (Darwin 25.6.0) |
| Toolchain | cargo 1.97.1 (c980f4866 2026-06-30), stable |
| Build | `--release` |
| Storage | RocksDB on a fresh `tempfile::tempdir()` per run, on local SSD |
| Date | 2026-09-18 |

Every figure in this document came from the same machine and the same build
profile, with two named exceptions in §0's comparison table: ADR 0007's
`orderbench` re-measurement (2026-09-23, machine not recorded there) and ADR
0008's BFT chain (2026-09-25, Apple M5 Max, 18 cores, 36 GB). Runs are
single-process: the benchmark hosts the producers, the
sequencer and (on the RPC path) the RPC server and clients, so client and server
CPU compete on the same 10 cores. That is stated rather than corrected for,
because correcting for it would need a second machine.

---

## 2. The benchmark

`rust/crates/cb-node/src/order_bench.rs`, driven by the `cb-order-bench` binary.

The `rust/Makefile` wraps the common runs:

```
make -C rust orderbench               # in-process
make -C rust orderbench-rpc           # over RPC
make -C rust orderbench-profile       # ECDSA + mempool micro-measurements
make -C rust orderbench-determinism   # two replays, one state root
```

`ORDERBENCH_BLOCKS`, `ORDERBENCH_DEPTH`, `ORDERBENCH_TAKERS`,
`ORDERBENCH_MAKERS` and `ORDERBENCH_ARGS` override the defaults. The binary
directly:

```
cargo build --release -p cb-node --bin cb-order-bench

# engine ceiling: matcher + executor + trie commit
# (`make orderbench` passes --channel-capacity 4000, not 2000, and neither
# --producers 8 nor --prefill; ADR 0007's figures need both — see its record)
./target/release/cb-order-bench \
  --blocks 40 --book-depth 500 --takers-per-block 250 --makers-per-block 250 \
  --max-block-txs 2000 --channel-capacity 2000 in-process

# what a client pays: capnp + TCP + ECDSA recovery + mempool + real sequencer
./target/release/cb-order-bench \
  --blocks 3000 --book-depth 500 --takers-per-block 250 --makers-per-block 250 \
  --max-block-txs 20000 --producers 4 rpc --port 7812

# ECDSA and mempool micro-measurements
./target/release/cb-order-bench profile --samples 100000

# determinism harness: same workload twice through build_and_apply
./target/release/cb-order-bench --blocks 20 --book-depth 200 \
  --takers-per-block 50 --makers-per-block 50 \
  --num-makers 16 --num-takers 16 --producers 1 determinism
```

### 2.1 What each path includes

| | in-process | RPC |
|---|---|---|
| ECDSA signing (client side) | producer threads | client threads |
| **ECDSA sender recovery** | **no** | **yes** — `SignedTx::sender()` in `submitTx` |
| Cap'n Proto encode / decode | no | yes |
| TCP loopback | no | yes |
| Mempool (`CF_MEMPOOL` put + hash-ordered drain) | no | yes |
| `order_for_block` | yes | yes |
| Matcher + executor + book CF writes | yes | yes |
| Trie hashing + batched RocksDB commit | yes | yes |
| Block encode, header sign, persist, head advance | yes | yes |
| Consensus voting / quorum certificates | no | no |
| Peer-to-peer propagation | no | no |

The in-process path is the engine's ceiling. The RPC path is what a real client
experiences, minus consensus. Neither is "the" number.

### 2.2 The order mix, and why

An order that rests is cheap; an order that crosses is not. A benchmark where
nothing matches measures admission, not matching. So the workload is split and
both halves are reported separately:

* **Maker orders** — `Gtc`, `post_only: false`, priced strictly inside a
  20-level ladder so they always rest and never cross. They replenish the depth
  takers consume. This is the cheap path.
* **Taker orders** — `Ioc`, priced through the whole ladder so they cross and
  fill. `--cross-rate-bps` controls the crossing share; the default is 10000,
  i.e. **every taker crosses**.

The reported runs use a 50/50 maker/taker mix at a 100% cross rate. That is the
hardest mix available, not the most flattering: every second order does matching
work. `--cross-rate-bps 0` produces zero fills and is used as a control.

Book depth (`--book-depth`) and the cross rate are both configurable because a
benchmark pinned to one book shape tells you about that shape only.

### 2.3 How "matched" is counted

The executor returns `Result<(), ExecError>`; a filled `Ioc` and an empty one
are indistinguishable from it, and the chain has no trades column family. So on
the in-process path a taker counts as **matched** iff its base-asset balance
changed across its own `apply_tx_at` call, which for a spot fill it always does.
The check is two `BlockState::load` calls — `HashMap` lookups against the
block's warm account cache — and it sits inside the timed region rather than
being hidden outside it.

`matched_orders_per_sec` therefore counts **taker orders that produced at least
one fill**. Each consumed one or more resting maker orders, so the number of
trades is greater than or equal to it. Passive maker fills are not counted
again.

On the RPC path the benchmark cannot observe a single transaction's fill from
outside the executor, so matched takers are derived from the resting-order
delta: `(depth_before + makers_applied) - depth_after`. That is exact only
because the workload makes taker quantity equal maker quantity, so a matched
taker consumes exactly one resting order. It is a weaker measurement than the
in-process one and is labelled as such.

### 2.4 Why the workload cannot produce a rejected transaction

`build_and_apply` propagates the first `ExecError` and abandons the entire
block, so a benchmark that produces rejects silently loses every transaction in
that block and then reports a rate for work that never landed. The workload is
built so no reject is reachable: makers and takers are disjoint address sets (no
self-trade), makers are never `post_only` and never priced to cross (no
post-only cross), nothing uses `Fok` (no FOK kill), every account is funded far
past what a run can spend, prices and quantities are tick- and lot-aligned, and
each producer owns a disjoint address slice so it is the sole author of those
nonces.

Every report carries `orders_rejected`, and the binary exits non-zero if it is
not zero. All runs below reported **0 rejected**.

### 2.5 Determinism

Orders are seeded from `--rng-seed` and addresses from `--seed-prefix`. The
`determinism` subcommand runs the same workload twice through the real
`build_and_apply` and compares state roots:

```
run_a_state_root = H256(0xf2102cdfe8cea82ee0eaca5e2e76ca94a928b93c91860fdd0eb5865f4441713e)
run_b_state_root = H256(0xf2102cdfe8cea82ee0eaca5e2e76ca94a928b93c91860fdd0eb5865f4441713e)
deterministic    = yes
```

The throughput paths use producer threads, so block *boundaries* depend on
timing and the final root is not comparable across those runs. That is why the
determinism check is a separate, single-threaded mode.

---

## 3. Results — in-process path

Command: `--blocks N --takers-per-block 250 --makers-per-block 250
--max-block-txs 2000 --channel-capacity 2000 in-process`. **0 rejected in every
run below.**

### 3.1 Headline, current code

200 blocks, 500-deep book, 395,323 orders applied, 6.4 s window:

| Reading | Value |
|---|---|
| Orders submitted / sec | 61,632 |
| Orders applied in a block / sec | 61,321 |
| **Matched orders / sec** | **30,650** |
| Submit-to-fill p50 | 46.8 ms |
| Submit-to-fill p99 | 67.9 ms |
| Submit-to-fill p99.9 | 79.7 ms |
| Block time p50 / p99 | 33.4 ms / 41.2 ms (target 50 ms) |

Latency is meaningful even at saturation here because `--channel-capacity`
bounds the queue in front of the block loop at about two blocks' worth of
orders. Rate-limited to 30,000 orders/sec (27,117 achieved) it improves further:
p50 23.1 ms, p99 57.1 ms.

### 3.2 Scaling with book depth

Matched orders/sec, 40-block runs, at three depths, across the two optimisations:

| Book depth per side | Baseline | + per-block hydration | + bounded maker walk | Current code |
|---|---|---|---|---|
| 100 | 446 | 13,755 | 24,636 | **34,883** |
| 500 | 178 | 7,989 | 20,558 | **32,656** |
| 2,000 | 114 | 2,079 | 19,394 | **28,836** |

All applied orders/sec (makers + takers) on the current code: 70,167 / 65,462 /
57,718 for the same three depths.

Block time against a 50 ms configured interval:

| Book depth | Baseline p50 | Current p50 | Current p99 |
|---|---|---|---|
| 100 | 2.11 s | 27.5 ms | 37.5 ms |
| 500 | 5.94 s | 29.7 ms | 39.9 ms |
| 2,000 | 7.90 s | 30.5 ms | 68.3 ms |

Before the changes, block time degraded by two orders of magnitude under load and
grew with book depth. It now sits inside the configured interval and is nearly
flat in depth. Throughput fell roughly linearly with depth before; it now falls
about 17% between depth 100 and depth 2,000.

The "Current code" column includes a concurrent change by another engineer to
the spot fill-settlement path that landed between the third and fourth columns.
See §5.4.

### 3.3 Per-phase breakdown

From the block loop's own timers, 200 blocks, 500-deep book:

| Phase | Share |
|---|---|
| Drain producer channel | 0.23% |
| `order_for_block` | 0.71% |
| Execute (matching + state mutation + book CF writes) | 87.89% |
| Commit (four tries + one batched RocksDB write) | 7.90% |
| Persist (block encode, header sign, head advance) | 3.27% |

Trie hashing and the batched commit are **not** the bottleneck, which is worth
recording because they are a common assumption. Signature recovery does not
appear because it is not on this path at all.

### 3.4 Sampling profile, current code

macOS `sample`, 12 s, 9,846 samples on the block-loop thread, 500-deep book,
2,000-order blocks, several thousand blocks into the run. Percentages are of
that thread's total.

| Frame | Share |
|---|---|
| `apply_tx_at` | 82.3% |
| — `BlockState::spot_book_mut` → `Orders::list_for_market` (per-block hydration) | **42.5%** |
| — `BookIndex::delete` | 6.8% |
| — `Orders::put` | 6.7% |
| — `Orders::delete` | 6.4% |
| — `BookIndex::put` | 6.3% |
| — `SpotBook::place` (the actual matching) | 5.7% |
| — `Orders::get` | 2.4% |
| `BlockState::commit` | 5.8% |

Two things stand out. The four book column-family mutations total **26.2%**, and
almost all of it is write-ahead-log append and a `write()` syscall: each one is
a separate `rocksdb::DBImpl::Write`, not part of the block's `WriteBatch` the way
account and trie writes are.

And per-block hydration is back on top at 42.5%, despite running once per block
over 2,000 orders. The profile shows the scan going through `MergingIterator`
and `BlockBasedTableIterator` — merging across SST levels rather than reading a
small memtable. The likely cause is tombstone accumulation: every consumed maker
is a `CF_ORDERS` delete, the run produces millions of them, and the prefix scan
for the ~1,100 live rows has to walk past them. A short run puts the same frame
at 2%. This is a supported inference from the frames, not an isolated
measurement.

The producer threads spend 23% of their time signing and 76% parked on a full
channel, so the generator is not the limiter.

---

## 4. Results — RPC path

Command: `--blocks N --book-depth 500 --takers-per-block 250 --makers-per-block
250 --max-block-txs 20000 rpc`. **0 rejected in every run below.**

### 4.1 Ceiling

Open-loop, 4 client threads, 2,000 blocks, 144 s window, 2,743,842 orders
applied:

| Reading | Value |
|---|---|
| Orders submitted / sec | 19,119 |
| Orders applied in a block / sec | **19,052** |
| **Matched orders / sec** | **9,518** |
| Block time p50 / p99 / max | 65.6 ms / 235 ms / 443 ms |

Open-loop latency is queue depth, not service time: p50 520 ms, p99 1.34 s.

### 4.2 Latency against offered load

`--target-orders-per-sec` caps the clients so latency describes service time.
The achieved rate undershoots the target because the pacer batches to the
platform's ~1 ms timer granularity; the achieved column is what to read.

| Target | Applied / sec | Matched / sec | Fill p50 | Fill p99 | Fill p99.9 |
|---|---|---|---|---|---|
| 5,000 | 4,066 | 2,032 | 59.5 ms | 242 ms | 389 ms |
| 10,000 | 6,620 | 3,305 | **24.7 ms** | **61.8 ms** | 90.2 ms |
| 15,000 | 9,428 | 4,712 | 56.4 ms | 286 ms | 526 ms |
| open loop | 19,052 | 9,518 | 520 ms | 1.34 s | 2.05 s |

So the RPC path holds p99 submit-to-fill under 250 ms up to somewhere between
6,600 and 9,400 applied orders/sec, and loses it above that. (The 5,000 row is
the noisiest: it ran over a 619 s window against 21–50 s for the others.)

"Submit" is the instant immediately before the client's Cap'n Proto write.
"Fill" is when a local watcher, polling `Meta::head_height()` every 100 µs,
observes the block containing that transaction. The figure excludes propagation
of the block back out to a subscriber, and the poll interval bounds its
resolution.

### 4.3 Where the RPC path's time goes

Sampling profile during a saturating run, 10,204 samples per thread.

**RPC server thread** — one `LocalSet`, one thread, every submit serialised
through it:

| Frame | Samples | Share of thread |
|---|---|---|
| `submitTx` dispatch total | 7,569 | 74% |
| of which `ecdsa_recover` | ~3,580 | 35% of the thread, **47% of `submitTx`** |
| remainder of `submitTx` (capnp decode, `keccak256` over the bincode encoding, bincode re-encode, `CF_MEMPOOL` put) | ~3,990 | 39% |

**Sequencer thread** — `apply_tx_at` accounted for 5,086 samples: 1,586 in
per-block hydration and 2,250 across the four unbatched book column-family
writes, the same two items the in-process profile finds.

### 4.4 Micro-measurements

`cb-order-bench profile --samples 100000`:

| Measurement | Rate |
|---|---|
| ECDSA sender recovery, serial | 40,501 / sec |
| ECDSA sender recovery, rayon (10 cores) | 166,194 / sec (4.10x) |
| Mempool put (one RocksDB put per transaction) | 144,932 / sec |
| Mempool drain + parallel bincode decode | 2,181,761 / sec |

These settle an open question. Signature recovery **is** the single largest item
on the RPC submit path — but at 47% of `submitTx`, parallelising it alone raises
the ceiling by at most about 1.9x by Amdahl's law, not by the order of magnitude
the guess implied. Since the core review (`docs/core-review.md` C2) a verifying
replica also pays it: `apply_remote_block` recovers every sender with rayon
rather than trusting the leader's list, so the 166,194/sec parallel figure is
the replica's ceiling on transaction ingest as well as the leader's. The
replica apply rate quoted elsewhere in this document predates that change and
should be re-measured. The mempool's *rate* is not a limit at all: drain plus decode
runs at 2.2 M/sec, more than 100x the achieved order rate. The claim in
`benchmark.rs`'s header that the hash-ordered drain caps pipeline depth is about
nonce *ordering*, not throughput, and this measurement neither confirms nor
refutes it.

---

## 5. Changes made, and the before/after for each

Every change below was measured with the same command on the same machine.

### 5.1 Hydrate the spot book once per block instead of once per order

`cb-block/src/state.rs` (new `BlockState::spot_book_mut` /
`invalidate_spot_book`), `cb-block/src/executor.rs` (use it). **Shared crate.**

The spot `PlaceOrder` handler rebuilt the entire book from `CF_ORDERS` before
every order, making order placement O(resting depth). The first sampling profile
put **72.2%** of total runtime in the `CF_ORDERS` prefix scan and bincode decode
and a further **16.4%** in re-inserting the rows into a fresh `SpotBook` — 88.6%
of the run rebuilding a book that had not changed, against 6.4% actually
matching.

`BlockState` now holds one `SpotBook` per market for the life of the block. The
matcher already applies to itself exactly the mutations the executor persists, and
`SpotBook` orders everything through `BTreeMap`s keyed by `(price, seq)`, so the
cached book holds the same rows in the same order as a rehydrated one regardless
of insertion order. The two cancel handlers edit `CF_ORDERS` behind the matcher's
back and therefore invalidate the cache.

Matched orders/sec, 500-deep book: **178 → 7,989 (44.9x)**.

### 5.2 Stop materialising the whole opposite side on every order

`cb-engine/src/spot_book.rs` (`walk_makers`). **Shared crate.**

With hydration fixed, the next profile put ~80% of runtime in `SpotBook::place`,
of which 30.6% was building a `Vec` of every resting order on the opposite side —
for a workload whose orders each consume one maker. The walk now stops at the
first price level that does not cross the taker's limit, and once the resting
quantity gathered already covers the taker's quantity.

The result is a **prefix** of the old output, and `try_place` stops consuming at
exactly those same two conditions, so the same makers fill in the same order for
the same quantities and `MatchResult` is unchanged. Whole price levels are always
taken, because time priority inside a level runs on ascending `seq` while the bid
walk sees a level's seqs descending — truncating mid-level would drop the orders
with the *best* time priority.
`bounded_walk_is_a_sufficient_prefix_of_the_full_walk` in `spot_book.rs` checks
both properties against a reference implementation of the old unbounded walk,
swept over both sides, seven taker prices and five taker quantities.

Matched orders/sec, 500-deep book: **7,989 → 20,558 (2.6x)**.
Matched orders/sec, 2,000-deep book: **2,079 → 19,394 (9.3x)**.

**Combined, measured on one stable code state: 178 → 20,558 matched orders/sec at
depth 500 (115x), and 114 → 19,394 at depth 2,000 (170x).**

### 5.3 Separate order counters from transaction counters

`cb-dashboard/src/metrics.rs`, `cb-dashboard/src/templates.rs`,
`cb-dashboard/templates/index.html`, `cb-node/src/benchmark.rs`,
`cb-node/src/sequencer.rs`, `cb-node/src/replica.rs`, `cb-tx/src/tx.rs`.

The in-process transfer generator called `Metrics::record_submit_side(side,
market)` for every `TxBody::Transfer`, which moved `buys`, `sells`, `btc_orders`
and `eth_orders`. The console reported `recent_orders_per_sec` from those
counters, so it displayed a six-figure order rate for a workload containing zero
orders.

`record_submit_side` is replaced by two methods that cannot be confused:
`record_tx_submitted()` for any transaction, and `record_order_submitted(side,
market)` for an order placement. `record_block` now takes the block's
transaction count **and** its order count, with the caller classifying bodies via
the new `TxBody::is_order_placement()` — the metrics module deliberately cannot
infer one from the other, because inferring is how they got conflated. The
dashboard's hero tile is relabelled "transactions / s" and two honest order tiles
were added. The fast-mirror replica path never decodes a block, so it reports
zero orders rather than guessing, and says so in a comment.

`sustained_tps`, `tps_5s` and `tps_60s` keep their names: the Makefile summary
and `docs/runbook.md` grep for them, and they were always transaction rates.

This change has no throughput effect. It is here so no future reader repeats the
error.

### 5.4 Two measurement corrections

**A concurrent change landed mid-measurement.** Between the third and fourth
columns of §3.2, another engineer rewrote the spot fill-settlement loop (step 10
of `apply_place_spot_order`, `total_taker_lock_unlocked` → `taker_lock_held`) and
added a fourth, margin-accounts trie to the state root. The first appears to be
responsible for most of the further improvement from 20,558 to 32,656 matched
orders/sec at depth 500; the second costs a little in the commit phase, measured
at 0.04% of runtime before and 7.9% after — 0.49 s across a 6.4 s run. Neither is
attributed to the work in §5.1 or §5.2, whose before/after pairs were taken on a
single stable code state.

**An earlier draft of this document blamed the RPC path's multi-second latency
tail on `CF_MEMPOOL` tombstones. That was wrong, and the bug was in the
benchmark.** The RPC client submitted through capnp without ever yielding to the
`RpcSystem` task that writes to the socket, except when its in-flight cap filled.
At a rate-limited 500 orders/sec per client, 2,048 orders stamped but unsent is
4.1 seconds — which is precisely the 4.07 s p99 and 4.18 s maximum block gap the
earlier run reported. The client now yields on every submit and paces with
`tokio::time::sleep_until` instead of blocking the whole `LocalSet` with
`std::thread::sleep`. After the fix the RPC ceiling rose from 13,470 to 19,052
applied orders/sec, the worst observed block gap fell from 1.37 s to 443 ms, and
the sub-saturation p99 fell from 925 ms to 61.8 ms. Every RPC figure in §4 is
post-fix.

The tombstone story was not baseless, only misplaced: §3.4 finds it in
`CF_ORDERS`, where the profile actually supports it.

---

## 6. Correctness

`cargo test --workspace --no-fail-fast`: **1,432 tests passed, 0 failed**, across
118 test targets.

The targets that matter for these changes were also run individually and pass:
`cb-block` `determinism` (9), `state_root_completeness` (6), `spot_executor`
(13), `chain_id`, `equity_spot`, `qa_spot_findings`; `cb-engine` lib (145,
including the new prefix-equivalence test); `cb-net` `sync` (6, leader/replica
state-root agreement). `cb-net`'s `perp_resync` is `#[ignore]`d for a
pre-existing reason unrelated to this work.

Two benchmark-side checks were added because the module makes claims that would
otherwise be assertions about code rather than measurements:

* `instrumented_block_loop_matches_build_and_apply` feeds one order stream
  through the benchmark's instrumented block loop on one database and through the
  real `build_and_apply` on another, and asserts the state roots agree block for
  block. Without it, "the benchmark mirrors the sequencer" would be unverified.
* `bounded_walk_is_a_sufficient_prefix_of_the_full_walk` is described in §5.2.

Determinism is unaffected. Nothing here parallelises execution; the one parallel
measurement (`measure_recovery`) runs ECDSA recovery on rayon in a micro-benchmark
that touches no state.

---

## 7. Against the targets in `docs/PLAN.md` §8

| Metric | Target | Measured | Verdict |
|---|---|---|---|
| Sustained matched orders/sec, in-process | > 50,000 | 28,836–34,883 | **Not met.** 58–70% of target. |
| Sustained orders/sec over RPC | > 20,000 | 19,052 applied (9,518 matched) | **Not met**, but within 5% on the placement reading. |
| p99 submit-to-fill | < 250 ms | 68 ms in-process at 61k orders/sec; < 250 ms over RPC up to ~6,600–9,400 orders/sec | **Met in-process. Met over RPC below about 9,000 orders/sec.** |
| Block time under load | stable at the configured interval | 30 ms p50 against a 50 ms target, nearly flat in book depth | **Met.** |

The in-process target of 50,000 is stated as *matched* orders/sec. At the 50/50
mix used here, 30,650 matched/sec corresponds to 61,321 order placements/sec, so
on a placement reading the in-process target is already exceeded. It is not read
that way here, because matching is the expensive half and the harder reading is
the honest one.

---

## 8. Comparison with published figures — which comparisons are fair

Hyperliquid publishes roughly 20,000–50,000 orders per second sustained with
bursts near 200,000.

**Not a fair comparison.** Our in-process figure excludes signature recovery, the
mempool, the network and consensus entirely. Comparing it to any published
production number compares an engine to a system. It is useful as a ceiling and
as a before/after control, and for nothing else.

**The closer comparison, with caveats.** Our RPC figure — 19,052 order
placements/sec, 9,518 of them matching — includes signature verification, the
mempool, Cap'n Proto and TCP, and the real sequencer loop. It still excludes
consensus voting and peer-to-peer propagation, both of which a production
exchange pays for. It runs client and server on the same 10 cores, which costs
us, and over loopback, which flatters us. On that basis we sit just below the
lower end of the published range on placements and roughly a factor of two below
it on matches.

**Not comparable at all.** The pure-matcher criterion bench in
`cb-state/benches/bench_order_match_throughput.rs` reports roughly 360,000
orders/sec against an in-memory book. Its own header says it does not touch
RocksDB. It should never be quoted next to an exchange's published end-to-end
number, and the ~200,000/sec burst figure it compares itself to *is* an
end-to-end number.

**The honest summary: no, we do not beat the target yet.** We are at 58–70% of
the sustained in-process matched-order target and within 5% of the sustained RPC
placement target, having started the day with no order benchmark at all and a
real measured rate of 178 matched orders/sec. The burst target of 200,000 is not
close on any path.

---

## 9. What to do next, in the order the profile justifies

1. **Batch the book column-family writes into the block's `WriteBatch`.**
   `Orders::put`, `Orders::delete`, `BookIndex::put` and `BookIndex::delete` are
   26.2% of the in-process block-loop thread and 2,250 of 5,086 `apply_tx_at`
   samples on the RPC path. Each is a separate RocksDB write with its own
   write-ahead-log record and `write()` syscall, while account and trie writes
   already share one batch per block. The obstacle is that the executor reads its
   own writes back within a block, so a batch needs a read-through overlay.
   *Since done*: `BlockState` runs the block on a staged `Database` — the
   read-through overlay this item asked for — and every column-family write
   lands in one batch on the writer thread. What remains per placement is the
   in-memory map and journal work, and the bincode encode per `Orders::put`.
2. **Stop the `CF_ORDERS` scan degrading over the run.** Per-block hydration is
   42.5% of the block-loop thread late in a long run against 2% early in one, and
   the profile shows the scan merging across SST levels. If tombstones are the
   cause, the options are a deletion-aware compaction filter, `DeleteRange` for
   consumed levels, or keeping the book in memory across blocks. Isolate it
   before choosing: the evidence is a profile shape, not a measurement.
3. **Take signature recovery off the RPC server's single thread.** It is 47% of
   `submitTx`, and `submitTx` is 74% of a thread every submit is serialised
   through. Recovery is safe to parallelise because it touches no state — unlike
   execution, which must stay serial for determinism. `rayon` is already a
   workspace dependency and `cb_tx::recover_senders_par` already exists. Amdahl
   caps the win at about 1.9x; batching several pending submits before recovering
   would raise that ceiling.
4. **`apply_tx_at` reads `Meta::chain_id()` from RocksDB on every transaction.**
   It did not surface above the noise floor in either profile, but it is a
   per-transaction point lookup for a value that is fixed for the life of the
   chain. Cheap to hoist onto `ExecCtx` when someone is next in that file.

---

## 10. Reproducing every number in this document

```
cd rust
cargo build --release -p cb-node --bin cb-order-bench

# §3.1 headline + §3.3 phase breakdown
./target/release/cb-order-bench --blocks 200 --book-depth 500 \
  --takers-per-block 250 --makers-per-block 250 \
  --max-block-txs 2000 --channel-capacity 2000 in-process

# §3.2 depth sweep
for d in 100 500 2000; do
  ./target/release/cb-order-bench --blocks 40 --book-depth $d \
    --takers-per-block 250 --makers-per-block 250 \
    --max-block-txs 2000 --channel-capacity 2000 in-process
done

# §3.1 in-process latency below saturation
./target/release/cb-order-bench --blocks 1500 --book-depth 500 \
  --takers-per-block 250 --makers-per-block 250 \
  --max-block-txs 2000 --channel-capacity 4000 \
  --target-orders-per-sec 30000 in-process

# §4.1 RPC ceiling
./target/release/cb-order-bench --blocks 2000 --book-depth 500 \
  --takers-per-block 250 --makers-per-block 250 \
  --max-block-txs 20000 --producers 4 rpc --port 7821

# §4.2 RPC latency against offered load (size --blocks for a 20-60 s window)
for r in 10000 15000; do
  ./target/release/cb-order-bench --blocks 2500 --book-depth 500 \
    --takers-per-block 250 --makers-per-block 250 --max-block-txs 20000 \
    --target-orders-per-sec $r rpc --port $((7840 + r/1000))
done

# §4.4 micro-measurements
./target/release/cb-order-bench profile --samples 100000

# §2.5 determinism
./target/release/cb-order-bench --blocks 20 --book-depth 200 \
  --takers-per-block 50 --makers-per-block 50 \
  --num-makers 16 --num-takers 16 --producers 1 determinism

# §3.4 / §4.3 profiles (macOS): start a run in the background, then
#   sample <pid> 12 -f /tmp/sample.txt

# §6 correctness
cargo test --workspace --no-fail-fast
```

Every run must print `orders_rejected = 0`. The binary exits non-zero if it does
not, because a benchmark that tolerates silent rejects is measuring nothing.
