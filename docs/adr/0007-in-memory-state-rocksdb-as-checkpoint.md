# ADR 0007 — One sequencer, state in memory, a hash chain instead of merkle tries

Status: accepted in part (see *Where it stands*). Date: 2026-09-23.
Decision 6 ("No BFT") is superseded by [0008](0008-bft-ordering-execute-after-commit.md)
as of 2026-09-25; the rest stands.
Amends [0002](0002-rocksdb-for-consensus-sqlite-for-application.md): RocksDB
stays the store for consensus state, stops being where the matching thread
reads it, and no longer holds merkle trie nodes.

## Context

### What the chain is for

The owner decided the shape on 2026-09-23. It is **a central order book whose
change log is copied to several machines.** One sequencer orders every
transaction and executes it. Replicas receive the blocks, re-execute them and
check they reach the same state. There are no validators to convince, so
there is no BFT: nothing waits on a vote or a quorum certificate for
ordering. The block stream is a replication log that can be checked, not a
consensus protocol.

That removes the reason for merkle tries. A merkle root gives two things over
a plain hash: an inclusion proof for one key, and a way to check a whole
snapshot without replaying it. Nothing in this tree used either. No RPC
returned a proof and there was no light client. The root had one consumer,
a replica comparing its own re-execution against the header.

### The goal, stated so it can be measured

"Faster than Hyperliquid" means **`make orderbench` at or above 200,000
applied orders/s on one ten-core machine, with matched/s next to it**, and
the same measured with 100,000 accounts, not only the default 128.
Hyperliquid's published 200,000 orders/s is an engine figure (HyperBFT,
many validators). `orderbench` is this repository's engine figure: matcher,
executor, state commitment and commit, with pre-signed orders and no network.

This work **does nothing for the verified end-to-end path** (`make start3`,
36,806 orders/s). That path is bound by secp256k1 recovery on ten cores, and
is a separate problem.

### What the profile said

`sample` over a 3,000-block `orderbench` run, before any change
(inclusive shares of the block thread):

| Where | Share |
|---|---|
| `apply_tx_at` in total | 41.3% |
| of which `SpotBook::place`, the matching itself | 15.2% |
| of which `Orders::get` (per-fill maker read) | 5.5% |
| of which `BlockState::load` + `journal_account` | 7.5% |
| of which `Orders::put` + `BookIndex::put` | 4.1% |
| `SignedTx::hash` | 20.5% |
| of which the benchmark's own role map, not the node | about 14% |
| `BlockState::commit` (accounts trie 12.2%, trie hashing 9.6%) | 16.3% |
| `order_for_block` (a keccak per transaction) | 6.9% |
| `Block::compute_tx_root` | 6.7% |

With 128 accounts, trie hashing looked like a tenth. With 100,000 accounts,
the **commit step was 91% of the block loop and the engine fell to 13,334
orders/s.** The binary trie costs about log₂N hashes and RocksDB path reads
per changed key. A hash of what changed costs neither.

## Decision

1. **The state commitment is a hash chain over each block's changes**
   (`cb_state::state_hash`):
   `root_N = sha256(domain ‖ root_{N−1} ‖ sha256(changes of block N in (cf, key) order))`.
   Genesis hashes every committed row once. The header field keeps its name
   and 32 bytes (`state_root`), so wire formats, the gateway and the
   dashboard are unchanged. It is a consensus change: a new genesis is
   required, and `Meta::state_hash_scheme` records the scheme.
2. **Writes, not reads.** The diff contains every committed row the block
   wrote, and every account whose final value differs from its value at the
   start of the block. Reads never enter it. A replica prefetches, and the
   benchmark loads accounts it does not change, so the set of accounts *read*
   differs between nodes; the set *changed* does not.
3. **Wider coverage than before.** The tries covered accounts, positions,
   markets and margin. The chain also covers order rows, both book indexes,
   funding, oracle state and the liquidation queue (`COMMITTED_CFS`). A
   replica whose book diverges is caught at the block that diverged it.
4. **The sequencer owns the state in memory; RocksDB is written, not read.**
   Order books and every account row are carried from one block to the
   next, and `cb_block::preload_state` loads all of them before the first
   block.
5. **The matching thread only asks for a write.** `execute_and_hand_off`
   orders and executes a block, moves its rows (not copies) into an overlay
   together with the new head height and timestamp, and returns. The writer
   thread seals it: the diff hash, `tx_root`, header, signature, block
   encoding and head hash, then the RocksDB write, then the announcement. It
   works in height order and carries the previous root and hash itself
   (`SealChain`). The synchronous `build_and_apply` is the same two
   functions, `execute_block` and `seal_block`, run on one thread, so both
   give the same block. `pipelined_seal.rs` checks this: a chain held four
   blocks deep in the overlay replays on a replica block for block, and,
   drained every block, it is the synchronous chain byte for byte. The
   leader-local ordering seed is now the latest *sealed* head hash, which
   can be a few blocks old. That is not a consensus change.
6. **No BFT.** Removing votes and quorum certificates from the block path is
   a separate change and not yet made.
7. **Reads never go to the matching thread.** Balances and books are served
   from the overlay-backed view (RPC threads), from the gateway's mirror fed
   by the block stream, or from replicas. The sequencer takes writes only.

## Where it stands (measured 2026-09-23, same machine)

*The record does not name the machine. `docs/performance.md` §1 is a
ten-core M4 (2026-09-18 to 09-21) and ADR 0008 an 18-core M5 Max
(2026-09-25); these figures sit between the two dates and are labelled as
their own until re-measured on a named one.*

`make orderbench ORDERBENCH_ARGS="--producers 8"`: 200 blocks of 2,000
orders (`--prefill` for the last row). The generator needed eight signing
threads to keep up once the engine got faster; four was the old limit.
`make orderbench` alone passes neither flag.

| Step | 128 accounts, applied / matched per s | 100,000 accounts, applied / matched per s |
|---|---|---|
| Before (benchmark as shipped, 4 producers) | 266,000 / 133,000 | — |
| Benchmark stops timing itself (§ harness) | 341,000 / 170,000 | 13,334 / 6,665 |
| Merkle tries → hash chain | 391,000 / 195,000 | 151,846 / 75,900 |
| Ordering key once per sender; no per-fill `Orders::get` | 417,000–449,000 / 208,000–224,000 | 155,575 / 77,762 |
| Accounts carried between blocks | 396,000–420,000 / 198,000–210,000 | 174,000–180,000 / 87,000–90,000 |
| All accounts and books loaded before the window (15 ms for 100,000, untimed) | 434,000–438,000 / 217,000–219,000 | 203,700–205,000 / 101,800–102,500 |
| Benchmark takes a savepoint and commit point per transaction, as the node does | 445,000–447,000 / 222,000–223,000 | 308,000–312,000 / 154,000–156,000 |
| Fx hashing for the per-block maps | 457,000 / 228,000 | 324,000–334,000 / 162,000–167,000 |
| Sealing moves to the writer thread (open loop: the 8 signers are now the limit) | 515,000 / 257,000 | — |
| Same, channel prefilled so the window times the engine, not the signers | 571,000–589,000 / 285,000–294,000 | 413,000–417,000 / 206,000–208,000 |

The last row is the engine's capacity: 400,000 orders are signed and queued
before the window opens (`--prefill`, about 0.7 s, untimed), and the window
closes when the last block is on disk. Handing a block to the writer costs
0.2% of the block thread, so the writer keeps pace and the figure is the
matching thread's rate, not the sealer's. Once the engine outran eight
signing threads, an open-loop run measured the signers.

Two rows are measurement fixes, not engine gains. The first: the benchmark
was keccak-hashing every transaction twice for its own bookkeeping and
reading the clock twice per order. The second: it never called
`commit_point`, so the account undo journal grew across a whole block and
its duplicate scan was quadratic in accounts per block. At 100,000 accounts
that was a third of the block thread, and `build_and_apply` never did it.
The 128-account rows around "accounts carried" were taken with background
load on the machine, and vary by about 5%.

**Against Hyperliquid's 200,000:** applied orders per second pass it at both
account counts, by about 2.3× at 128 accounts and 1.6× at 100,000. Matched
orders per second pass it at 128 accounts and not yet at 100,000. None of
this has signature verification in it. Hyperliquid's figure also includes a
consensus protocol this design deliberately does not have.

**Where the block thread goes now** (100,000 accounts, `sample`, before the
Fx change): matching 18%, account `load`/`put` 23% (clones of
`BTreeMap`-backed `AccountState`), commit 17% (the SHA-256 diff and batch
assembly), keccak 10% (ordering keys and `tx_root`), SipHash about 6.5%
(since replaced by Fx).

## Next, in order

1. **Load all state at node startup.** The sequencer does not call
   `preload_state` yet, and positions, margin and perp books are not in it.
2. **Build with LTO and one codegen unit.** This needs `capnp` on the PATH,
   because changing the profile re-runs `cb-schema`'s build script.
3. **Dense in-place accounts**: balances as a fixed array, no clone per
   `load`, an undo journal instead of copies.
4. **Order rows written once per block**, from the book's final state,
   instead of once per mutation.
5. ~~A sealer thread~~: done, as the writer thread (decision 5).
6. **A block journal as the durability point**, with RocksDB as a
   checkpoint that lags it; recovery is checkpoint plus replay.
7. **Remove votes and quorum certificates** from the block path.

Each step lands with an `orderbench` before/after figure at both account
counts.

## Consequences

**Gained.** Commit cost proportional to what changed, not to log N. No trie
nodes written, so `CF_TRIE_NODES` stops growing (ADR 0002's unbounded growth
ends for new chains). Order books and funding are covered by the commitment.

**Given up.**
- **No inclusion proofs.** Nothing used them, but a future light client or
  proof-of-reserves would need a merkle tree built separately, off the hot
  path.
- **A snapshot cannot be checked against a header without replay.** A new
  node either replays from genesis or trusts the snapshot's source. If that
  matters later, carry a full-state hash at snapshot heights, computed off
  the matching thread.
- **Latent divergence is caught late.** A row that differs between two nodes
  but is never written again is not detected until a block writes it. The
  merkle root caught it at the next commit of any key under the same trie.
  In practice a divergent row changes the next write that reads it, and that
  is the block that fails.
- **Old data directories are incompatible.** Their roots are merkle roots.
- **The benchmark's history before 2026-09-23 is not comparable**, because
  the harness was timing its own hashing.
