# Core review — consensus, execution and the trust boundary

Date: 2026-09-19. Reviewer: code audit at the project owner's request, followed
by fixes in the same session.
Scope: everything `docs/security-review.md` (2026-09-18) did not cover —
`cb-crypto`, `cb-tx`, `cb-rpc` admission, `cb-block` (executor, ordering,
staged state), `cb-engine` (margin, liquidation, funding, mark, matchers),
`cb-consensus`, `cb-node` (sequencer, vote pool, replica), `cb-net/syncer`,
`cb-trie`, and the `cb-oracle` publisher where it touches a consensus rule.

Like the earlier review, each finding carries two severities: **Sim** (this
deployment, faucet money) and **Real** (the same code with real value behind
it). Each is also labelled by how it was established:

* **Verified** — reproduced by a test committed alongside the fix, in the
  `qa_*.rs` style the repository already uses.
* **Read from code** — the defect is unambiguous in the source but was not
  driven live. Where a fix landed, its unit test covers the changed behaviour.

Nothing here re-finds F1–F12 from `docs/PLAN.md` §8. F11 and F12 were checked,
confirmed open, and then closed in the second pass (see the end of this
document); F12 was closed by taking the reading the design plainly intended.

---

## Summary

| # | Finding | Sim | Real | Status |
|---|---|---|---|---|
| C1 | Nothing on a live node produces `FundingTick` or `FuturesSettle`; the chain halts at the first funding boundary (height 3,600) | Critical | Critical | Fixed |
| C2 | Verifying replicas trust the leader's pre-recovered `senders` and never check a transaction signature | Low | Critical | Fixed |
| H1 | Replicas do not check `prev_hash` or `tx_root` | Low | High | Fixed |
| H2 | A future that expires without a settlement price halts the chain permanently; replicas also skip the settle requirement the leader enforces | High | High | Fixed |
| H3 | Intra-block ordering breaks per-account nonce order, so a cancel sent after a placement in the same block is dropped | High | High | Fixed |
| M1 | `OracleUpdate.submitted_height` is unbounded; one bad post bricks a market's oracle forever | Medium | High | Fixed |
| M2 | ECDSA signatures are malleable (no low-S rule): one transaction, two hashes | Low | Medium | Fixed |
| M3 | Votes are accepted for hashes the leader never produced; the vote pool is never pruned | Low | Medium | Fixed |
| M4 | `pack_seq` keeps 16 bits of `tx_index`; `max_block_txs` is unbounded, so two orders in one block can share a book-index key | Low | High | Fixed |
| M5 | `Withdraw` locks funds forever; `WithdrawSettled` releases nothing | Low | High | Fixed (refused) |
| M6 | A dated future keeps trading past `expiry_height` until it happens to settle | Medium | Medium | Fixed |
| L1 | Gossiped transactions skip the chain-id admission check | Low | Low | Fixed |
| F11 | Resting perpetual orders reserve no collateral (PLAN §8) | Medium | High | Fixed |
| F12 | Funding rate clamped per hour, accrued per block (PLAN §8) | High | High | Fixed |
| L2 | No domain tag on the transaction signing hash | Info | Low | Fixed |
| L3 | The liquidated owner may be their own keeper and recoup 0.2% | Low | Low | Fixed |
| L4 | Insurance exhaustion halts only perpetual markets, never a dated future | Low | Medium | Fixed |
| L5 | Replicas do not validate the block timestamp | Info | Low | Fixed |
| L6 | Within a rank, blocks execute in ascending sender address | Info | Low | Fixed |

---

# Findings

## C1. The chain halts at height 3,600 — Verified

**Weakness.** `build_and_apply` refuses to produce a block at any height that
is a funding-interval boundary for a registered perpetual unless the block
carries a `FundingTick` signed by the proposer (`enforce_required_funding_ticks`,
`cb-block/src/block.rs`). Every genesis in the repository sets
`funding_interval_blocks` to 3,600. The only code that constructs a
`FundingTick` or `FuturesSettle` is in `cb-sim` and `cb-simgame`, which drive
the executor in-process. The live sequencer (`cb-node/src/sequencer.rs`) drains
the mempool and builds; it never injects a system transaction, and neither the
oracle publisher nor the gateway signs as the proposer.

**Consequence.** On any chain started with `make startperps` or `make up`, the
3,600th block fails with `MissingRequiredFundingTick`. The sequencer loop logs
the error, sleeps one interval and retries the same height forever — and each
retry drains the mempool first, so every user transaction submitted from then
on is silently dropped. `docs/runbook.md` §10.5 documents the symptom as a
thing that happens. The 52-test end-to-end suite never reaches height 3,600.

**Severity.** Sim: Critical (every long-running deployment dies). Real:
Critical.

**Fix.** `cb_block::required_system_txs(db, height)` is the single,
deterministic description of what a block at `height` must carry — the same
function backs the leader's and the replica's enforcement, so the two rules
cannot drift. The sequencer calls it for `head + 1` before every build, signs
one transaction per entry with the proposer key at the proposer account's
current nonce, and prepends them to the batch. `main.rs` refuses to start a
sequencer whose genesis recorded no proposer, since that chain could never pass
the boundary. Covered by `cb-node/tests/system_txs.rs`.

## C2. Replicas never verify a transaction signature — Read from code

**Weakness.** `Block.senders` carries the leader's pre-recovered sender for
every transaction. `apply_remote_block` (`cb-net/src/syncer.rs`) uses those
addresses verbatim whenever the field is non-empty, and only recovers senders
itself on the legacy path where it is empty. Every block the current leader
produces fills the field. Both strict-sync and the peer-to-peer replica go
through this function, so no verifying node in the system checks that any
transaction was signed by the account it debits.

**Consequence.** A sequencer can include a transaction it signed itself with
`senders[i]` set to any victim, execute it against the victim's balance, and
every replica will re-execute it identically, reproduce the same state root and
vote for it. The README's guarantee — "a replica that re-executes the same
block must arrive at the same root, or it halts" — holds, but it certifies the
leader's arithmetic, not the transactions' authenticity. The `Block` doc
comment describes this as a deliberate throughput trade; that is a defensible
choice for a fast-mirror node and not for a node whose job is verification.

**Severity.** Sim: Low (the sequencer is trusted by construction). Real:
Critical.

**Fix.** `apply_remote_block` now recovers every sender in parallel and, when
the block also carries `senders`, requires them to agree; a disagreement is
`SyncError::SenderMismatch` and the block is rejected. The leader's list is
kept as the prefetch hint it was already useful as. The cost is one ECDSA
recovery per transaction on verifying replicas, which is what verification
costs; the performance document should quote replica apply rate with it.
Covered by `cb-net/tests/sync.rs::replica_rejects_a_block_whose_senders_are_forged`.

## H1. `prev_hash` and `tx_root` are never checked — Read from code

**Weakness.** `apply_remote_block` checks height monotonicity, the proposer
signature and the state root. It does not compare `header.prev_hash` to the
local head hash, nor `header.tx_root` to the hash of the transactions it is
about to execute.

**Consequence.** The header's hash chain is unverified on replicas, so a block
whose header links to a different parent is adopted if it executes cleanly;
and a body whose transactions do not match the signed root is executed as if
they did. The state-root check limits the damage on its own, but it is the
wrong check to be relying on for either property, and neither costs anything.

**Severity.** Sim: Low. Real: High.

**Fix.** Both checks added before execution; `SyncError::WrongParent` and
`SyncError::TxRootMismatch`. Covered in `cb-net/tests/sync.rs`.

## H2. A future expiring without a price halts the chain — Verified

**Weakness.** `enforce_required_futures_settle` demands a `FuturesSettle` at
exactly `expiry_height`, and `apply_futures_settle` refuses to settle at a zero
price (no oracle, empty TWAP window). When both are true the block at
`expiry_height` can never be built: the required transaction is present and it
fails, so the post-loop check fails, so the block fails, and the height never
advances. The design (§8.3) says settlement should be *delayed* in this case.
Separately, `apply_remote_block` enforced the funding-tick rule but not the
settlement rule, so a replica would accept a block the leader could not have
produced.

**Severity.** Sim: High. Real: High.

**Fix.** The requirement is now: every unsettled future with
`expiry_height <= height` whose settlement price is computable from the state
at the parent block. It is a pure function of state, so leader and replicas
agree; a future with no price waits, frozen (see M6), until an oracle post
makes it settleable. `apply_remote_block` enforces it. Covered in
`cb-block/tests/futures_settle.rs`.

## H3. Same-block cancel-after-place is dropped — Verified

**Weakness.** `order_for_block` sorts by `(rank, sender, nonce)` with cancels
in rank 0 and placements in rank 2. An account that places at nonce `n` and
cancels at nonce `n+1` inside one block interval has its cancel sorted *ahead*
of its placement, where it fails `BadNonce` and is dropped; the placement then
executes. The gateway pipelines nonces within a block interval, so this is an
ordinary user action, not an edge case.

**Severity.** Sim: High (a cancel that silently does nothing is the worst
outcome an order-management UI can have). Real: High.

**Fix.** Transactions are grouped by sender, each group sorted by nonce, and
groups are ranked by their *first* transaction. Cancels still go first across
accounts; within an account, nonce order is never violated. This is a
leader-local change — replicas apply the committed order — so it is not a
consensus change. Covered by `cb-block/tests/ordering.rs`.

## M1. `submitted_height` is unbounded — Verified

**Weakness.** `check_freshness` rejects an `OracleUpdate` whose
`submitted_height` is below the stored `source_height`, and nothing bounds it
above. An authority that posts `u64::MAX` — by bug or by fat finger — makes
every later update stale, permanently, with no operator action that can undo
it short of a new genesis. The oracle publisher (`cb-oracle/src/supervisor.rs`)
advanced a private counter by at least one per tick, so it drifted ahead of
the chain whenever it ticked faster than blocks were produced.

**Severity.** Sim: Medium. Real: High (an oracle that cannot be updated is a
market that cannot be marked or liquidated).

**Fix.** The executor rejects `submitted_height > block_height` with
`OracleFromFuture` — a height the chain has not reached is not a height a
price was observed at. The publisher now anchors on the height the chain last
recomputed a mark at instead of counting its own ticks. Covered in
`cb-block/tests/oracle_update.rs`.

## M2. Signature malleability — Verified

**Weakness.** `cb_crypto::recover` accepts either of the two `s` values that
verify for a given `(r, message)`. `SignedTx::hash` covers the signature, so a
relayer or gossip peer can turn one transaction into two mempool entries with
different hashes. The nonce rule stops both from executing, but the duplicate
costs a `BadNonce` rejection at block build, which resets the sender's
admission nonce and spams the rejection log; over gossip it is a free
amplifier.

**Severity.** Sim: Low. Real: Medium.

**Fix.** `recover` rejects high-S signatures. libsecp256k1 already emits
low-S, so no signer in the repository changes. Covered in `cb-crypto`.

## M3. Votes on phantom hashes; unbounded vote pool — Read from code

**Weakness.** `SequencerVoteHandler` rejects a vote only when its height is
above the head. A vote for a hash the leader never produced at a known height
is accepted into a bucket. `VoteRejection::UnknownBlock` and `StaleHeight`
exist and are never returned. `VotePool::prune_below` exists and is never
called, so buckets accumulate for the life of the process.

**Severity.** Sim: Low. Real: Medium (a colluding pair could certify a hash
nobody executed; and an unbounded map on a long-running validator).

**Fix.** The handler looks the hash up in `CF_BLOCK_HASH` and returns
`UnknownBlock` unless it sits at the claimed height. The pool is pruned below
each new QC. A late vote for a pruned height is therefore `UnknownBlock`,
which is the honest answer. Covered in `cb-node`.

## M4. 16-bit `tx_index` in `pack_seq` — Read from code

**Weakness.** `pack_seq` packs `tx_index & 0xffff`. `--max-block-txs` defaults
to 10,000 and has no upper bound. A block with more than 65,535 transactions
gives two orders the same `seq`, and the book index is keyed by
`(price, seq)`: the second order overwrites the first's index entry, and a
cancel of one deletes the other's, leaving locked collateral behind an order
the matcher can no longer see.

**Severity.** Sim: Low (nobody sets it that high). Real: High.

**Fix.** `cb_block::MAX_BLOCK_TXS = 65_535`. `build_and_apply` and
`verify_and_commit` reject a larger block; `main.rs` clamps the flag.

## M5. `Withdraw` strands funds — Read from code

**Weakness.** `Withdraw` locks the amount on the sender's account.
`WithdrawSettled` only advances the treasury's nonce; it never burns or
releases the lock, and it carries no amount or reference the executor could
use to find it. Nothing in `cb-client`, `cb-gateway` or `cb-web` sends either,
so the path is unreachable today — but it is on the wire, accepted by
`validate_static`, and executes.

**Severity.** Sim: Low. Real: High.

**Fix.** Both variants return `Unsupported`, exactly as `AdjustMargin` does,
until a withdrawal ledger exists. Failing closed is preferable to a signed
path that permanently reduces a balance.

## M6. A future trades past its expiry — Read from code

**Weakness.** `FutureMarket::expiry_height` is documented as "the height after
which the market is frozen except for `FuturesSettle`", but the placement
handler only checked `final_settlement_px_q8.is_some()`. Between expiry and
settlement — which, with H2, could have been forever — the book stayed open.

**Fix.** `PlacePerpOrder` on a future at `block_height >= expiry_height` is
`MarketExpired`.

## L1. Gossiped transactions skip the chain-id check — Read from code

`DbChain::accept_tx` runs `validate_static` and signature recovery and puts the
transaction straight into the mempool; the RPC path also checks the chain id.
The executor rejects the transaction at build, so nothing executes, but the
mempool accepts foreign traffic. Fixed with the same check.

## L2. No domain tag on the transaction signing hash — Fixed (second pass)

`SignedTx::signing_hash` was `keccak256(bincode(Transaction))` and the header
preimage was bare `bincode(header)`. Votes and view-changes were domain-tagged;
these two were not, so a transaction and a header hashed into one space. No
collision was reachable, but a signature scheme should not rest on that
argument holding for every future message type. Both preimages now carry a
tag (`CB-TX\0`, `CB-BLOCK\0`). Every signer in the repository goes through
`SignedTx::sign_with` / `BlockHeader::signing_bytes`, so nothing else changed
— but every previously signed transaction is now invalid. **Fresh genesis.**
`cb-tx` unit test `the_signing_hash_is_domain_tagged`.

## L3. Self-liquidation — Fixed (second pass)

`LiquidationTrigger` is permissionless and the keeper share (0.2%) was paid to
the sender, so an owner who triggered their own liquidation recouped it. The
close still runs — that is what the trigger is for — but when the sender is
the owner the keeper share goes to the insurance fund. `cb-block/tests/liquidation.rs::an_owner_who_triggers_their_own_liquidation_earns_no_keeper_share`.

## L4. Insurance exhaustion halts only perpetuals — Fixed (second pass)

`absorb_bad_debt` flipped `paused_reduce_only_only` on a `PerpMarket`; a
dated future had no such flag, so one whose losses exhausted the fund kept
accepting opening orders. `FutureMarket` now carries the flag
(`#[serde(default)]`, capnp field `@18`, on the gateway snapshot as
`reduce_only_mode`), the halt path sets it for either kind, and the
settlement handler re-reads the market row before its final write so the flag
it set a moment earlier is not overwritten. Bincode re-encodes every futures
row, which moves the markets-trie leaves: **fresh genesis.**
`cb-block/tests/futures_settle.rs::a_future_that_exhausts_the_insurance_fund_enters_the_reduce_only_halt`.

## L5. Block timestamp — Fixed (second pass)

`Meta` now records the head block's timestamp with the head pointer, in the
same batch. A replica refuses a header whose timestamp is below the head's
(`SyncError::TimestampRegressed`), and the builder clamps its own clock to the
head's so a leader whose wall clock stepped back cannot produce a block every
replica rejects. Never a wall-clock bound — replay must stay deterministic —
only "does not run backwards".
`cb-net/tests/sync.rs::replica_rejects_a_block_whose_clock_runs_backwards`.

## L6. Sender-address priority — Fixed (second pass)

Within a rank, `order_for_block` executed accounts in ascending address, a
permanent priority advantage for a vanity address. Accounts within a rank are
now ordered by `keccak(parent_hash || sender)`: a permutation nobody can
choose that changes every block. Leader-local, like the rest of the sorter.
The order benchmark's equivalence test now aligns the two databases' head
hashes, since two independently signed warmups need not hash alike.

## F11 and F12 (from `docs/PLAN.md` §8) — Fixed (second pass)

**F12.** The funding rate is now a **per-interval** rate, exactly as
`FundingClamp` documents (0.5% per hour) and the dashboard displays:
`accrue_index_delta` pro-rates it, `delta = rate × elapsed / interval_blocks`,
so one interval accrues one rate and the per-block reading — the 3,600×
overcharge — is gone. This is reading 2 of the two the design called
self-consistent, and it was chosen because every named constant is hourly and
the settlement interval is 3,600 blocks; it is one constant to reverse if the
owner decides otherwise. Every funding number on the chain changes: **fresh
genesis.** Pinned by `cb-engine` `accrue_pro_rates_the_interval_rate_over_the_blocks_elapsed`
and the updated expectations in `funding_tick.rs` and `e2e_perp_lifecycle.rs`.

**F11.** Resting perpetual orders now reserve margin. A secondary column
family `perp_orders_by_owner` (`owner || order_id`, maintained only by
`PerpOrders::put`/`delete`) gives the executor an account's resting orders
without a scan, and the initial-margin gate charges the requirement against
the account's *potential* position per market — the larger of
`(long + Σ resting bids) − short` and `(short + Σ resting asks) − long`,
reduce-only orders excluded, so an order against a held position is charged
net of what it closes — while equity still comes from what is actually held
(`passes_initial_margin_with_exposure`). N orders that each pass the gate
alone can no longer all rest. A data directory from before this change has
resting orders that are not in the index: **fresh genesis.**
`cb-block/tests/qa_perp_findings.rs::a_second_resting_order_the_account_cannot_also_afford_is_rejected`
`opposite_side_resting_orders_are_charged_as_the_larger_side_not_the_sum` and
`a_resting_order_against_a_held_position_is_charged_net_of_that_position`.

---

# What was checked and found sound

Listed so the next reviewer does not repeat it.

* **Staged execution.** `Database::staged` overlays every CF read, including
  prefix iteration (`MergedIter` merges the stage in key order with tombstones),
  so a transaction sees what an earlier one in the same block wrote, and
  `savepoint`/`rollback` covers the stage, the account cache and all three
  touched-key sets. The commit is one `WriteBatch`.
* **Determinism.** Every iteration on the execution path is over RocksDB lex
  order or a `BTreeMap`, and every `HashMap`/`HashSet` is either sorted before
  it drives a write or used only for membership.
* **Balance arithmetic.** `AccountState` is checked arithmetic returning
  errors, not panics; every executor caller turns them into a rejected,
  rolled-back transaction.
* **Spot locks.** `spot_lock_required` is the single definition; releases
  telescope to the placement lock (F6/F7 fixes hold).
* **Margin and PnL scaling.** The `1e10` denominator is asset-independent by
  construction and pinned by tests. Realised PnL rounds toward zero on both
  sides of a fill, so it conserves.
* **Funding settlement.** Positions are settled in `(market, owner)` byte
  order; the entry index is volume-weighted on adds (F2 fix holds).
* **Quorum certificates.** `QuorumCert::verify` checks per-vote signature,
  view, height, hash, set membership and uniqueness. Vote preimages are
  domain-tagged and bound to `(view, height, hash)`.
* **Trie.** Leaves carry the full 32-byte path and are tagged `0x00`; branches
  `0x01`; root is insertion-order independent (proptest).
* **Chain id.** Checked at RPC admission and again in the executor.

# What was not covered

* `cb-gateway`, `cb-web`, `cb-net/p2p` beyond the block-acceptance seam —
  see `docs/security-review.md`.
* Cap'n Proto decoding limits (`cb-rpc/src/conv.rs`) — message size and list
  bounds were not audited.
* Cap'n Proto decoding limits remain the one unaudited surface.
