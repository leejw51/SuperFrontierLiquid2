# ADR 0008 — Byzantine-fault-tolerant ordering, execution after commit

Status: accepted. Date: 2026-09-25.
Supersedes the "No BFT" clause of [0007](0007-in-memory-state-rocksdb-as-checkpoint.md)
(its decision 6). Everything else in 0007 stands: the hash chain, state in
memory, the pipelined seal.

## Context

On 2026-09-23 the owner decided the chain was "a central order book whose
change log is copied to several machines": one sequencer, no votes on the
block path (ADR 0007). On 2026-09-25 the owner reversed that: the chain
should order blocks the way Hyperliquid does, by a Byzantine-fault-tolerant
protocol among validators, and be faster than Hyperliquid at it.

What Hyperliquid publishes: HyperBFT, "heavily inspired by HotStuff and its
successors", one-block finality, block finality around 0.07 s, a median of
about 0.1–0.2 s end to end, and 200,000 orders/s for HyperCore on dedicated
validator hardware.

### What the codebase imposes

The executor has no rollback. The writer thread seals and persists every
executed block within a few blocks of its execution; accounts and books are
carried in memory from one block to the next; nothing can be discarded once
applied. A protocol that executes a proposal *before* voting on it needs to
throw away the proposals that lose, which means rewriting the state
carry-over. A protocol that executes only what has already committed needs
nothing of the kind, because a committed prefix never reverts. That settles
the shape: **consensus orders, execution follows.**

Two smaller things followed from it:

* A proposer does not know the height its payload will execute at, nor the
  state it will execute against, so it cannot sign the funding ticks and
  settlements a block must carry. The executor injects them.
* Every validator executes every committed payload itself, so anything
  execution depends on — the intra-block ordering seed, the proposer whose
  name authorises the system actions — must come from the proposal, not
  from the node's own head.

## Decision

1. **`cb_consensus::bft` orders; the node executes what it commits.** A
   chained HotStuff with a two-chain commit rule and a timeout-certificate
   pacemaker (the DiemBFT v4 / Jolteon shape). Rounds have a rotating
   leader; a proposal extends the highest certificate its leader holds; a
   round ends with a quorum certificate or a timeout certificate; a
   certified proposal whose parent was certified in the round before
   commits the parent and every uncommitted ancestor. `n = 3f + 1`, quorum
   `ceil(2n/3)`: four validators tolerate one Byzantine validator, seven
   tolerate two. Votes are broadcast, so every validator forms every
   certificate itself and commit is one hop shorter than with votes sent
   to the next leader.

2. **The engine is a pure state machine.** Messages and clock readings in,
   messages and committed proposals out, no sockets, no clock, no
   database. A deterministic simulator runs four to seven of them through
   crashes, a restart, an equivocating leader, a partition and lossy links,
   and checks that committed sequences agree byte for byte. The node is a
   driver around it (`cb_node::validator`).

3. **A proposal is an ordering claim, not a state claim.** It carries the
   payload's transaction root, its parent, the certificates that justify
   it, and — for an audit, not for consensus — the proposer's latest
   executed height and root. A validator whose own root at that height
   differs refuses to vote (a *veto*): the engine records the proposal so
   the tree stays connected but never votes for it. Divergent execution is
   caught within a couple of blocks by the validators themselves.

4. **The executed block is the proposal's payload, whole.** A transaction
   that fails at execution stays in the block and is skipped, identically,
   by every validator and every observer; the block's transaction list and
   root are the committed payload's. This is the opposite of the sequencer
   scheme, where a refused transaction never enters a block, and it is
   what makes a block verifiable against its proposal.

5. **The finality proof rides in the header, and it is the same bytes on
   every validator.** The proposal header and its quorum certificate are
   sealed into the block header's `parent_qc` slot, which is part of the
   hashed header. A proposal can be certified twice, by different quorums,
   and a validator commits it on whichever certified child it sees first —
   a child that may itself be abandoned — so the certificate a validator
   happens to hold at commit time is its own, and sealing it made honest
   validators' block hashes diverge from that height on
   (`tests/finality.rs` reproduces it with four honest validators). The
   certificate sealed is therefore the one carried in the header of the
   proposal's *successor on the committed chain*, which is the same
   everywhere because the committed chain is, and which is known one
   commit later. The alternative, a certificate chosen canonically at
   commit time, does not exist: at that moment two validators can hold two
   different valid quorums and nothing in common that picks one.

   *Execution does not wait for it.* Execution needs the payload, the
   ordering seed and the proposer, all fixed at commit. The engine hands
   the newest committed proposal over at once (`Action::Ordered`) and
   again, with its certificate, in the next `Action::Commit`. The driver
   executes it at commit (`cb_node::bft_writer::BlockWriter`) and hands its
   rows to the database as a *deferred write* (`Database::write_deferred`):
   every read sees them at once; the state root — which does not involve
   the certificate — is computed as soon as everything before the block is
   on disk and published as the executed head the proposer audit compares;
   and the block is sealed and written when the certificate arrives. At
   most one block is ever executed and unsealed. What waits is only the
   block body and the head hash, one round.

   *Crashes, catch-up, peers.* The rows, the body and the head hash reach
   RocksDB in one batch at the seal, or not at all, so a validator killed
   with an unsealed block restarts from the last sealed one, fetches the
   proposal again and executes it again from identical state. A catch-up
   by block sync discards the unsealed block before any block lands from a
   peer (the peers' sealed copy lands in its place), and the driver drops
   the old tree's commits while it runs. Peers are never served an unsealed
   block: it has no body, and the head the node announces is read from the
   durable view. A block-sync peer and a restarted validator read the same
   bytes back (`Root::from_finality` takes the sealed certificate as the
   root's), and `tests/bft_execution.rs` checks that every validator seals
   identical blocks at every height, with and without a writer thread,
   after a kill with an unsealed block, and after a catch-up that discards
   one. Nothing in the node compares block hashes across validators at run
   time; the proposer's executed-root audit covers the state root, which
   does not cover the proof. An observer verifies the proof — the leader's
   signature, a quorum of the set, the payload root equal to the block's
   transaction root, the proposal's height equal to the block's, and the
   proposal extending the previous block's (its round later, its parent
   that proposal or a later one) — instead of a signature over the header.
   Before the 2026-09-27 audit the height and chain checks were missing,
   and a peer could serve an old block's proposal again as the next block.
   What is still not proven per block is the commit itself: the proof is
   one certificate, and a proposal certified on a branch that was later
   abandoned, at the right height and extending the right block, would
   pass. Proving commit needs the certified child from the round after, or
   a chain of them, carried in the block. No header
   field changed; `Meta::consensus_scheme` says which check applies, set
   once by `init --consensus bft`.

6. **System transactions are synthetic.** The required funding ticks and
   settlements at a height are injected by the executor as unsigned
   transactions in the proposer's name, ahead of the payload, on every
   validator and in every replay. They consume no nonce and are not in
   the block; the state root covers what they did.

7. **Transactions move between validators in batches, not by gossip.** A
   validator forwards what its admission accepted to the others as one
   message every few milliseconds over the consensus channel; the receiver
   verifies each as if a client had sent it. Announce-and-fetch gossip
   costs three messages a transaction a peer and saturated the peer-to-peer
   engine at 40,000 transactions a second, with consensus messages queued
   behind it. A validator proposes a sender's transactions only as an
   unbroken nonce run continuing from the chain and from the proposals in
   flight, so a forwarded transaction that arrives after its successor
   waits in the mempool rather than being proposed out of order.

8. **Leader reputation, as a function of the chain being extended.** A
   validator whose turn by the plain round robin ended in a timeout
   certificate stands aside from leading for 128 rounds, doubling for each
   further one in a row up to 8,192, and reset when it leads a round on
   the chain. Without this a dead validator's turn costs a full round
   timer every `n` rounds for as long as it is dead.

   *Whose schedule.* The leader of round `r` is computed from the proposal
   a round-`r` proposal extends: `Reputation` is a fold along that
   proposal's chain of ancestors (every certified proposal on it, not only
   the committed ones), cached per proposal in the engine's tree. Every
   validator that could vote for a proposal holds its parent's chain — an
   orphan gets no vote — so all of them reach the same verdict on whether
   its proposer leads; and a leader judges itself on its highest
   certificate, which is exactly the parent its proposal carries.
   Validators can still disagree about who *should* propose (their
   highest certificates differ), never about whether a given proposal is
   the leader's. An earlier version kept the list in memory, updated from
   each validator's own committed chain: a restarted validator came back
   with it empty, validators whose commit points differed disagreed, and
   the chain stalled until a bench ran out (4.7–7.9 s of no commits at a
   200 ms timer after one restart; 9 of 2,600 long-sweep runs).

   *Restarts.* Every entry of the fold comes from one header alone (the
   blame for a timeout falls on the plain round robin's leader of that
   round, a substitute's failure included), and folding is idempotent and
   order-insensitive. The driver writes the snapshot at its commit point
   to RocksDB whenever the commit point moves, before the commits reach
   the executor, and a restarted validator starts its tree's root from it
   (`persist_reputation`, `Root::with_reputation`): the proposals between
   its executed head and that commit point come back from peers and fold
   in as nothing.

   *When snapshots differ anyway* — a wiped disk, a catch-up by block sync
   past rounds the validator never saw — two things bound the damage. A
   round more than two rounds past the chain's last link (a proposal
   extending the round right before it) is led by the plain round robin,
   which needs no history: a schedule disagreement costs at most two
   timeouts, then the chain moves, and the timeouts it produces land on the
   chain for everyone. The price in normal operation: after any timeout,
   the round following the proposal that recovers from it is the plain
   round robin's, so a dead validator costs one more timer there `1/n` of
   the time (and takes another strike). And a timeout stops counting 8,192 rounds after it happened,
   so what a validator missed stops mattering. Measured in the simulator
   with a dead validator and a restarted one that lost its snapshot:
   commits resume within one to five simulated seconds
   (`a_validator_restarted_without_its_snapshot_recovers`). A validator
   still stores any set member's justified proposal and uses the schedule
   only to decide whom to vote for.

9. **A validator rejoins from its executed head.** Its tree starts at the
   proposal behind the last block it executed, and it asks peers for the
   proposals since, sixty-four ancestors at a time from one peer at a time.
   Further behind than peers keep proposals for (4,096 rounds), it switches
   gossip's block sync on, lands the executed blocks it missed with their
   finality proofs, and starts a fresh tree at the new head. Peers keep
   block bodies for `--keep-blocks` blocks (20,000 in the cluster script),
   which is how far back a rejoin can reach.

10. **The sequencer scheme stays.** `make start` is unchanged; a chain
    initialised without `--consensus bft` runs exactly as before, and every
    test of that path still passes. The BFT chain is `make startbft`.

## Consequences

**Latency is measured from proposal to commit on one machine.** With four
validators on one M5 Max, every clock is the same clock, and the number is
the protocol's cost with no network in it. On a real network each hop adds
the round-trip time; three hops of a 50 ms RTT is 150 ms, and no protocol
changes that.

**Four validators on one machine verify every signature four times.** Each
validator recovers each transaction's signature once, at admission, which
is the whole point of BFT — a proposer's word on who signed what is not
taken. On one box that is four recoveries a transaction. The 2026-09-25
draft of this paragraph said that was what bounded the throughput at
60,000 orders/s, with the box saturating at 40,000–47,000 when offered
100,000. It was not: a profile on 2026-09-29 found the executor rebuilding
both order books from RocksDB after every refused order (three quarters
of its time), a refused transaction leaving a hole in its sender's nonce
sequence that stranded the client's pipelined window, and the leader
taking the *executed* nonce as its baseline so that a block of execution
lag emptied every proposal. With those fixed (`docs/performance.md` §0,
*Re-measured 2026-09-29*), the same box commits 60,000 orders/s at 5 ms
p50 with no backlog and 73,000–76,000 on three of four validators when
offered 150,000, the fourth falling behind on proposal validation: the
leader's proposal reaches it before the forwarded copies of its
transactions, so it recovers every signature in the proposal itself. That
is the bound now, and the generator's twelve signing workers on the same
cores after it.

**A refused transaction consumes its nonce, and the block says which
applied** (`cb52098`, after the 2026-09-27 audit's F8 and B1). The
2026-09-25 rule — a refused transaction stays in the block but leaves the
nonce where it was — stranded everything the sender had signed behind it.
Now one that passed the nonce check consumes it whether or not its body
succeeded; the block carries an `applied` mask, every validator computes
it, a replica refuses a block whose mask differs, and the gateway reads it
instead of comparing nonces. A payload whose signed transactions were all
refused is still a block. Consensus change; fresh genesis.

**A validator rejoining under sustained load takes a while to vote
again.** Two runs, stated separately. First: a validator killed for 30–45 s
under 20,000 orders/s and restarted while the load continued synced the
blocks it missed at about 100 blocks/s (it re-verifies every signature
with a quarter of one machine), stayed consistent with the others at
every height, and for the rest of a 75 s window alternated between
catching up and rejoining, because the tip moved faster than the shared
machine let it re-verify; it had not resumed voting when the window
ended. Second: a validator restarted on a quiet chain 716 blocks behind
synced them and was voting again within seconds, at the same height and
hash as the others. On a machine of its own the first case's numbers say
it would close the gap; that is not measured. The deterministic simulator
covers the protocol side (`a_benched_validator_rejoins_after_a_restart`).

**A round with nothing to propose still costs a round.** An idle chain
turns over a round every `idle_interval` (100 ms) so the pipeline stays
primed; a non-empty proposal is followed at once, twice — the next
certificate commits it (and it executes), the one after brings the
certificate that seals its block — so neither a lone order nor the block
that proves it waits for the idle timer.

**A block's body and head hash trail its execution by one round**
(decision 5): state, balances and fills are visible at commit, the block
that proves them is sealed when the next commit brings its certificate.
The block stream, `getBlock`, the p2p head and the dashboard read the
durable view, so they see a block one round later than the state. While a
block waits for its certificate the database writer waits with it, so
nothing queued behind it reaches disk sooner; at most one block is
waiting.

**A block no longer says which of its transactions executed, and the
gateway assumes all of them did.** `cb-gateway`'s watcher derives order
records and ledger entries from block transactions through one predicate,
`tx_applied`, which returns true because under the sequencer scheme
inclusion meant application. Under BFT a refused transaction stays in the
block, so the gateway can record an order as placed that the chain
refused, until its next account and book refresh over RPC — which is
where balances and books actually come from, so nothing is miscounted
there. The executor already computes the per-transaction outcome
(`Executed::applied`); carrying it in the block as a receipt is the fix,
and a wire-format change, so it is not in this record.

**Equivocation is tolerated, not punished.** A leader that sends two
proposals for one round gets no certificate and the round times out; the
evidence is logged and nothing slashes anything.

**The dashboard's top-line height and root are read separately** and can
show one block's height with the previous block's root under load. The
recent-blocks list is consistent per height; the status pair is not
atomic.

## Where it stands (measured 2026-09-25)

Measured before decisions 5 and 8 took their present form. Commit and
execution timing is unchanged by them (execution still happens at commit);
the block body and head hash now trail by one round, which these figures
do not measure. Not re-measured.

One M5 Max (18 cores), four validators as separate processes on localhost,
`make startbft` then `make loadbft`: six `cb-sim` workers signing real
spot orders and submitting them over RPC to validator 0, which forwards
them to the other three. Every signature recovered on every validator.
"Committed" is transactions ordered in committed proposals, as validator 0's
`consensus status` line counts them (`txs` over five seconds), not
transactions applied — under this scheme a transaction the executor refuses
stays in the block. "Refused" is admission refusals reported to the clients.
The per-block executor log line (`executed committed proposal`, with `txs`
and `rejected`) is where applied and refused-at-execution counts are, and it
is not aggregated into the status line; the runs' record covers admission.

| Offered | Committed | Commit latency p50 | p99 | Refused | Mempool backlog |
|---|---|---|---|---|---|
| 20,000 orders/s | 20,000 orders/s | 2–3 ms | 5–12 ms | 0 | none |
| 40,000 orders/s | 40,000 orders/s | 5–7 ms | 16–29 ms | 0 | none |
| 60,000 orders/s | 60,000 orders/s | 12–14 ms | 34–63 ms | 0 | none |
| 100,000 orders/s | 40,000–47,000 orders/s | 250–330 ms | 590–750 ms | 0 | growing; the box is at 100% CPU |

Those are the 2026-09-25 figures. Re-measured on 2026-09-29 after the
throughput audit (`docs/performance.md` §0, *Re-measured 2026-09-29*,
with the fix-by-fix table): 60,000 offered commits 60,000 at 5 ms p50 and
15–29 ms p99 with no backlog and nothing refused; 150,000 offered commits
73,000–76,000 at 70–83 ms p50 on three validators while the fourth falls
behind; about half of every committed block is refused at execution for
the generator's traders' empty balances, so the applied rate is about
half the committed one.

With one of the four validators killed, the other three carry 20,000
orders/s at 2 ms p50 and 4–16 ms p99; the dead validator's turns cost a
handful of timeouts before leader reputation skips it.

Execution of a committed 10,000-order proposal takes 11–12 ms on the
executor thread (about 850,000 orders/s), so execution is not what bounds
either row.

Against the published Hyperliquid figures, for what the comparison is
worth on one machine (`docs/performance.md` §0 is the full comparison,
with the sequencer-scheme figures and their machine alongside):

| | Hyperliquid, published | Here, one M5 Max, four validators, no network hop |
|---|---|---|
| Block finality | about 0.07 s | 2–3 ms p50 at 20,000 orders/s; 5–7 ms p50 and under 29 ms p99 at 40,000; 12–14 ms p50 and under 63 ms p99 at 60,000 — proposal timestamp to commit on one clock |
| End to end, median | 0.1–0.2 s | not measured with a network in the path |
| Orders/s, verified through consensus | 200,000, many validators on dedicated hardware | 60,000 committed (ordered) by four validators sharing one machine at 5 ms p50, with no backlog; 73,000–76,000 by three of four when offered 150,000 (2026-09-29); about half of those applied, the rest the generator's refused orders |
| Execution of one committed payload | — | 10,000 orders in 11–12 ms on the executor thread, about 850,000 orders/s; a per-payload rate, not a sustained one, and not the bound |

The latency comparison favours this design by an order of magnitude and
has no network in it. The throughput comparison does not favour it:
60,000–76,000 committed against 200,000, on one machine that also runs
the load generator, with one validator behind at the upper figure and
only half of the committed orders applied. A multi-machine figure does not
exist yet, and until it does the claim "faster than Hyperliquid" holds for
finality on one box and for the engine, not for verified throughput.

Correctness under the runs: no timeouts, no abandoned proposals, no client
submission refused, and all four validators at the same height with the
same hash and root at every height checked, including after a validator
was killed and restarted.

Commands: `make startbft`, `make loadbft BFT_LOAD_TPS=40000`,
`grep 'consensus status' rust/data-bft/node0.log`,
`rust/scripts/bft-cluster.sh kill 1` and `restart 1` for the failover.
