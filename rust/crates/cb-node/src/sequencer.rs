//! Block-building loop for the sequencer.
//!
//! Two tx sources land in each block: (a) RPC traffic that arrived through the mempool —
//! deserialized + ECDSA-recovered exactly once here, (b) a trusted in-process channel
//! used by the benchmark generator that pushes pre-verified `VerifiedTx` directly,
//! bypassing the mempool round-trip. Both are drained, mixed, and committed in one block.
//!
//! Every block header is signed with the sequencer's secp256k1 key. Replicas recover
//! the address from the signature and reject blocks whose proposer doesn't match the
//! validator currently entitled to lead (recorded in `Meta::proposer_addr`).
//!
//! After a block is applied locally, the leader inserts its own vote into the shared
//! `VotePool`. Followers send their votes back over the leader's RPC; once the pool
//! reaches `quorum_count(N)`, the resulting `QuorumCert` is persisted to `CF_QC` and
//! becomes the next block's `parent_qc` — the chained-HotStuff finality proof.
//!
//! Optional pruning trims block bytes older than `head - keep_recent` so benchmark runs
//! don't grow RocksDB unboundedly. Account state, trie nodes, and meta are never pruned.

use crate::vote_pool::SharedVotePool;
use cb_block::{execute_and_hand_off, required_system_txs, Block, ExecCtx, ExecError, SealChain};
use cb_consensus::{vote_signing_bytes, Vote};
use cb_crypto::{address_from_pubkey, keccak256, sign, PublicKey};
use cb_dashboard::Metrics;
use cb_rpc::PendingNonces;
use cb_state::{Accounts, Blocks, Database, Mempool, Meta, Qcs};
use cb_tx::{SignedTx, Transaction, VerifiedTx};
use cb_types::{Address, H256};
use crossbeam_channel::Receiver;
use secp256k1::SecretKey;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub struct PruningCfg {
    /// Number of recent blocks to retain (older blocks are deleted from CF_BLOCKS +
    /// CF_BLOCK_HASH). `None` means pruning is disabled.
    pub keep_recent: Option<u64>,
}

impl PruningCfg {
    pub fn disabled() -> Self {
        Self { keep_recent: None }
    }
    pub fn is_enabled(&self) -> bool {
        self.keep_recent.is_some()
    }
}

/// Run the block-building loop until cancelled. Drains up to `max_block_txs` per block.
/// `bypass` is an optional in-process queue of pre-verified txs (benchmark mode).
/// `vote_pool` is the leader-side pool used for self-vote and follower-vote accumulation;
/// when `None`, voting is disabled (single-node-leader mode kept for benchmarks).
#[allow(clippy::too_many_arguments)]
pub async fn run_sequencer(
    db: Database,
    ctx: ExecCtx,
    notify: Arc<Notify>,
    block_interval_ms: u64,
    max_block_txs: usize,
    metrics: Arc<Metrics>,
    bypass: Option<Receiver<VerifiedTx>>,
    pruning: PruningCfg,
    proposer_key: Arc<SecretKey>,
    vote_pool: Option<SharedVotePool>,
    pending_nonces: Option<Arc<PendingNonces>>,
    rejected_log: Option<Arc<cb_rpc::RejectedLog>>,
) -> anyhow::Result<()> {
    if let Some(keep) = pruning.keep_recent {
        // Pruning reads block bodies straight from disk; nothing may be in
        // flight when it runs.
        db.flush_pending()?;
        startup_prune(&db, keep);
    }
    let proposer_pk = PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &proposer_key);
    let proposer_addr = address_from_pubkey(&proposer_pk);

    let interval = Duration::from_millis(block_interval_ms);
    let mut last_block_at = std::time::Instant::now();
    // The previous sealed block's root and hash. Blocks are sealed on the
    // writer thread, in height order, and each seal advances this; the loop
    // below never waits for it (ADR 0007).
    let seal_chain = std::sync::Arc::new(std::sync::Mutex::new(SealChain::at_head(&db)));
    loop {
        // Pace the loop: a block is cut when the interval has elapsed or a
        // full block's worth is waiting, whichever comes first. Without this
        // the loop cut a block the moment anything was pending — harmless
        // while the RocksDB write throttled it, but once the write moved to
        // its own thread the loop spun at 1,300 blocks a second with six
        // transactions each, and the per-block fixed costs (ordering, the
        // trie hash, the header) ate the throughput the write had freed.
        let waiting = Mempool::new(&db).len() + bypass.as_ref().map(|rx| rx.len()).unwrap_or(0);
        let since = last_block_at.elapsed();
        if waiting < max_block_txs && since < interval {
            tokio::time::sleep(interval - since).await;
        }
        let pending = Mempool::new(&db).drain(max_block_txs);
        // Parallel decode: every blob is `MempoolRecord` (sender pre-recovered
        // at submit). Bad/legacy entries are dropped silently — same as the
        // prior serial loop's `if let Ok(...)` forgiveness.
        let raw_blobs: Vec<Vec<u8>> = pending.iter().map(|(_, v)| v.clone()).collect();
        let mut verified: Vec<VerifiedTx> = cb_tx::decode_mempool_batch(&raw_blobs);
        for (h, _) in &pending {
            Mempool::new(&db).delete(h);
        }
        // Pull pre-verified txs from the bypass channel (benchmark) up to the same cap.
        if let Some(rx) = &bypass {
            let cap = max_block_txs.saturating_sub(verified.len());
            for _ in 0..cap {
                match rx.try_recv() {
                    Ok(v) => verified.push(v),
                    Err(_) => break,
                }
            }
            metrics.set_channel_depth(rx.len() as u64);
        }
        if verified.is_empty() {
            tokio::time::sleep(Duration::from_millis(block_interval_ms)).await;
            continue;
        }
        // The system transactions this height must carry — funding ticks at
        // an interval boundary, settlements for expired futures — signed by
        // this node as proposer. The executor requires them and refuses the
        // block without them; before this was here nothing on a live node
        // produced them, and every chain halted at its first funding
        // boundary, re-draining and dropping the mempool once per interval
        // for ever. `required_system_txs` is the same list the enforcement
        // reads, so what is injected is exactly what is demanded.
        let next_height = Meta::new(&db).head_height() + 1;
        verified.extend(system_txs_for(
            &db,
            next_height,
            &proposer_key,
            proposer_addr,
        ));
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let key_for_sign = proposer_key.clone();
        let signer = move |preimage: &[u8]| -> Vec<u8> {
            let digest = keccak256(preimage);
            let sig = sign(&key_for_sign, digest).expect("sequencer sign");
            sig.to_vec()
        };
        let view = Meta::new(&db).view();
        let prev_height = Meta::new(&db).head_height();
        let parent_qc = Qcs::new(&db).get(prev_height).unwrap_or_default();
        // Every sender in the batch, so the block's outcome can be reconciled
        // against the nonce ledger whichever way it goes.
        let batch_senders: Vec<Address> = verified.iter().map(|v| v.sender).collect();
        // Everything that announces the block runs once it is on disk — from
        // the writer thread when writes are asynchronous, inline otherwise.
        // The notification wakes the block streams and the p2p announcer,
        // both of which read the durable view, so a peer is never told about
        // a block a crash could still take back.
        let on_durable = {
            let notify = notify.clone();
            let vote_pool = vote_pool.clone();
            let db = db.clone();
            let proposer_key = proposer_key.clone();
            move |height: u64, block_hash: H256| {
                // Leader self-votes as soon as the block is durable. With
                // n=3 / quorum=2, one follower vote then forms QC[H].
                if let Some(pool) = &vote_pool {
                    cast_self_vote(
                        pool,
                        &db,
                        &proposer_key,
                        proposer_addr,
                        view,
                        height,
                        block_hash,
                    );
                }
                notify.notify_waiters();
            }
        };
        match execute_and_hand_off(
            &db,
            &ctx,
            verified,
            ts,
            view,
            parent_qc,
            std::sync::Arc::clone(&seal_chain),
            signer,
            on_durable,
            |_, _, _| {},
        ) {
            Ok(res) => {
                let n = res.applied.len() as u64;
                // Orders and transactions are counted separately. `n` is every
                // transaction in the block; `orders` is the subset that placed
                // an order. Classifying here rather than in the metrics module
                // is deliberate — the metrics module has no way to tell them
                // apart, and inferring one from the other is what produced the
                // "transfers reported as orders" bug.
                let orders = res
                    .applied
                    .iter()
                    .filter(|(_, is_order, _)| *is_order)
                    .count() as u64;
                last_block_at = std::time::Instant::now();
                let height = res.height;
                tracing::info!(
                    height,
                    txs = n,
                    rejected = res.rejected.len(),
                    "produced block"
                );
                for (side, market) in res.applied.iter().filter_map(|(_, _, sm)| *sm) {
                    metrics.record_applied_order(side, market);
                }
                metrics.record_block(n, orders);

                // A rejected transaction is not in the block and never will
                // be. Say so, per transaction, with enough identity for an
                // operator to match it to a submission — the client was told
                // "accepted" and nothing else would ever tell it otherwise.
                //
                // Forgetting the sender is the other half: the nonce ledger
                // must not keep expecting the nonce after one the chain never
                // consumed, or every later submission from that address is
                // measured against a number the chain cannot reach. Dropping
                // the entry makes the next submission answer `badNonce` with
                // the real value, which is what a gateway's retry path needs.
                // Published before the per-transaction log lines so a
                // subscriber learns of the refusal as early as the operator.
                if let Some(log) = &rejected_log {
                    log.push(height, &res.rejected);
                }
                for r in &res.rejected {
                    tracing::warn!(
                        height,
                        tx = %hex::encode(r.hash.0),
                        sender = %hex::encode(r.sender.0),
                        nonce = r.nonce,
                        error = %r.error,
                        "transaction rejected; it is not in the block"
                    );
                    if let Some(p) = &pending_nonces {
                        p.forget(&r.sender);
                    }
                }
                if let Some(p) = &pending_nonces {
                    p.retire_settled(&db, &batch_senders);
                }

                // The self-vote and the head notification have already run,
                // or will run, from `on_durable` above. Pruning stays here,
                // on this thread: it writes, and a write issued from the
                // writer thread cannot wait for that thread to drain.
                if let Some(keep) = pruning.keep_recent {
                    // With sealing pipelined, the newest `MAX_PENDING` bodies
                    // may not be written yet; `keep` must stay above that for
                    // the pruned height to have a body to delete.
                    prune_one(&db, height, keep);
                }
            }
            Err(ExecError::NoApplicableTxs { rejected }) => {
                // Not a failure of the block: every transaction drained was
                // individually refused, so there was nothing to produce. The
                // chain is fine and the senders need their nonce expectation
                // cleared, same as for a partial rejection.
                tracing::warn!(
                    rejected = rejected.len(),
                    "no applicable transactions; no block produced"
                );
                // No block, so no height to attribute them to; `0` says so.
                if let Some(log) = &rejected_log {
                    log.push(0, &rejected);
                }
                for r in &rejected {
                    tracing::warn!(
                        tx = %hex::encode(r.hash.0),
                        sender = %hex::encode(r.sender.0),
                        nonce = r.nonce,
                        error = %r.error,
                        "transaction rejected; no block was produced"
                    );
                }
                if let Some(p) = &pending_nonces {
                    for a in &batch_senders {
                        p.forget(a);
                    }
                }
                tokio::time::sleep(Duration::from_millis(block_interval_ms)).await;
            }
            Err(e) => {
                // A genuine block-level failure — a missing required funding
                // tick or settlement, or a storage error. No state was
                // written: the whole block ran on a staged handle and the
                // batch was never issued.
                tracing::error!("block build failed: {e:?}");
                if let Some(p) = &pending_nonces {
                    for a in &batch_senders {
                        p.forget(a);
                    }
                }
                tokio::time::sleep(Duration::from_millis(block_interval_ms)).await;
            }
        }
        // Yield to give the bench task a chance to refill the channel.
        tokio::task::yield_now().await;
    }
}

/// Sign every system transaction the block at `height` must carry.
///
/// Nonces start at the proposer account's chain nonce and run consecutively,
/// which is what the executor demands of any sender. The block builder sorts
/// by `(sender, nonce)` so they apply in this order regardless of where they
/// land in the batch.
///
/// Returns nothing when the chain's recorded proposer is not this node: the
/// executor would reject the transactions as unauthorised, and a block that
/// needed them could not be built by this key in the first place —
/// `main.rs` refuses to start a sequencer in that state.
fn system_txs_for(
    db: &Database,
    height: u64,
    proposer_key: &SecretKey,
    proposer_addr: Address,
) -> Vec<VerifiedTx> {
    if Meta::new(db).proposer_addr() != proposer_addr {
        return Vec::new();
    }
    let bodies = required_system_txs(db, height);
    if bodies.is_empty() {
        return Vec::new();
    }
    let chain_id = Meta::new(db).chain_id();
    let mut nonce = Accounts::new(db)
        .get(&proposer_addr)
        .map(|a| a.nonce)
        .unwrap_or(0);
    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies {
        let tx = Transaction {
            nonce,
            chain_id,
            body,
        };
        match SignedTx::sign_with(proposer_key, tx) {
            Ok(signed) => {
                out.push(VerifiedTx::from_parts_unchecked(signed, proposer_addr));
                nonce += 1;
            }
            Err(e) => {
                // Signing a fixed-shape transaction with a key that already
                // signs every block header cannot fail; if it does, the block
                // will fail its required-tx check and say which one is missing.
                tracing::error!(?e, height, "could not sign a system transaction");
            }
        }
    }
    tracing::info!(height, count = out.len(), "injecting system transactions");
    out
}

fn cast_self_vote(
    pool: &SharedVotePool,
    db: &Database,
    sk: &SecretKey,
    addr: cb_types::Address,
    view: u64,
    height: u64,
    block_hash: H256,
) {
    let preimage = H256(vote_signing_bytes(view, height, block_hash));
    let sig = match sign(sk, preimage) {
        Ok(s) => s.to_vec(),
        Err(e) => {
            tracing::error!(?e, "leader self-sign failed");
            return;
        }
    };
    let vote = Vote {
        view,
        height,
        block_hash,
        validator: addr,
        sig,
    };
    match crate::vote_pool::add_vote_shared(pool, db, vote) {
        Ok(crate::vote_pool::VoteOutcome::Quorum) => {
            pool.lock()
                .expect("vote pool poisoned")
                .prune_below(height.saturating_sub(1));
            tracing::info!(height, view, "self-vote alone reached quorum (n=1)");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(?e, "leader self-vote rejected by pool"),
    }
}

/// `VoteHandler` adapter that wires the RPC server's `submit_vote` into the
/// leader's `VotePool`. Returns the high-level outcome the server propagates
/// back to the calling follower.
pub struct SequencerVoteHandler {
    pub db: Database,
    pub pool: SharedVotePool,
}

impl cb_rpc::VoteHandler for SequencerVoteHandler {
    fn handle(&self, vote: Vote) -> Result<cb_rpc::VoteAccepted, (cb_rpc::VoteCode, String)> {
        // Reject votes for blocks the leader hasn't produced yet — the only
        // way that can happen is a malicious or misconfigured follower.
        let head = Meta::new(&self.db).head_height();
        if vote.height > head {
            return Err((
                cb_rpc::VoteCode::UnknownBlock,
                format!("vote for height {} but head is {head}", vote.height),
            ));
        }
        // And for hashes this node never produced at that height. A vote is
        // an attestation about one specific block; one naming a hash that is
        // not ours is either a fork or a fabrication, and a bucket for it is
        // a certificate waiting for a second signer. A height that pruning
        // has already dropped from the hash index answers the same way, which
        // is the honest answer for a vote that arrives that late.
        if Blocks::new(&self.db).height_of(&vote.block_hash) != Some(vote.height) {
            return Err((
                cb_rpc::VoteCode::UnknownBlock,
                format!(
                    "vote names block {} at height {}, which this node does not hold",
                    hex::encode(vote.block_hash.0),
                    vote.height
                ),
            ));
        }
        let height = vote.height;
        // The signature is recovered before the pool is locked: see
        // `vote_pool::add_vote_shared`.
        match crate::vote_pool::add_vote_shared(&self.pool, &self.db, vote) {
            Ok(crate::vote_pool::VoteOutcome::Accepted) => Ok(cb_rpc::VoteAccepted::Stored),
            Ok(crate::vote_pool::VoteOutcome::Quorum) => {
                // A certificate at `height` is the last thing anyone needs
                // from the votes below it. Without this the pool grew by one
                // bucket per block for the life of the process. The bucket at
                // `height` itself stays: it holds the duplicate-voter guard,
                // and dropping it would let a replayed vote re-form the
                // certificate.
                self.pool
                    .lock()
                    .expect("vote pool poisoned")
                    .prune_below(height.saturating_sub(1));
                Ok(cb_rpc::VoteAccepted::QuorumFormed)
            }
            Ok(crate::vote_pool::VoteOutcome::Duplicate) => Err((
                cb_rpc::VoteCode::Duplicate,
                "vote already counted for this validator".into(),
            )),
            Err(crate::vote_pool::VoteRejection::UnknownValidator) => Err((
                cb_rpc::VoteCode::UnknownValidator,
                "validator not in genesis set".into(),
            )),
            Err(crate::vote_pool::VoteRejection::MalformedSig) => {
                Err((cb_rpc::VoteCode::MalformedSig, "malformed signature".into()))
            }
            Err(crate::vote_pool::VoteRejection::SignerMismatch) => Err((
                cb_rpc::VoteCode::SignerMismatch,
                "signer recovered does not match claimed validator".into(),
            )),
            Err(e) => Err((cb_rpc::VoteCode::Internal, format!("{e}"))),
        }
    }
}

/// After producing a block at `head`, drop the block at `head - keep_recent` if it
/// still exists. Steady-state cost: one RocksDB get + one batched delete per block.
pub fn prune_one(db: &Database, head: u64, keep_recent: u64) {
    let cutoff = match head.checked_sub(keep_recent) {
        Some(t) if t >= 1 => t,
        _ => return,
    };
    // The oldest block still stored, not `head - keep_recent`: after a
    // startup that hit its budget there is a backlog below the cutoff, and
    // pruning only the cutoff block would leave it there for ever.
    let target = match Blocks::new(db).lowest_height() {
        Some(l) if l <= cutoff => l,
        _ => return,
    };
    let bytes = match Blocks::new(db).get(target) {
        Some(b) => b,
        None => return,
    };
    if let Ok(block) = Block::decode(&bytes) {
        Blocks::new(db).prune_at(target, block.header.hash());
    }
}

/// One-time backfill: when a node restarts with pruning enabled after running without
/// it, sweep any blocks below the keep window that the per-block prune would otherwise
/// never reach. Safe no-op on a fresh DB.
/// Blocks one startup will prune before letting the sequencer begin. The
/// rest is caught up by `prune_one`, one block per produced block, and by the
/// next startup.
const STARTUP_PRUNE_BUDGET: u64 = 50_000;

/// Drop block bodies below `head - keep_recent`, starting from the lowest
/// height still stored.
///
/// It used to start at height 1 every time. On a chain that pruning had
/// already emptied below the cutoff that was a wasted pass; on a chain
/// pruning was newly switched on for, it was one point read, decode and
/// delete per block of the whole history — 1.26 million of them on the chain
/// a day of stress runs had built — on the thread the sequencer is waiting to
/// start on. The node looked alive, answered the dashboard, and produced no
/// blocks. `Blocks::lowest_height` finds the real starting point with one
/// seek, and the budget below keeps the first startup on a long chain short
/// enough to be a pause rather than an outage.
pub fn startup_prune(db: &Database, keep_recent: u64) {
    use cb_state::Meta;
    let head = Meta::new(db).head_height();
    let cutoff = match head.checked_sub(keep_recent) {
        Some(c) if c >= 1 => c,
        _ => return,
    };
    let from = Blocks::new(db).lowest_height().unwrap_or(1);
    if from > cutoff {
        return;
    }
    let mut pruned = 0u64;
    for h in from..=cutoff {
        if pruned >= STARTUP_PRUNE_BUDGET {
            tracing::info!(
                pruned,
                remaining = cutoff - h + 1,
                "startup prune budget reached; the rest is pruned as blocks are produced"
            );
            return;
        }
        if let Some(bytes) = Blocks::new(db).get(h) {
            if let Ok(block) = Block::decode(&bytes) {
                Blocks::new(db).prune_at(h, block.header.hash());
                pruned += 1;
            }
        }
    }
    if pruned > 0 {
        tracing::info!(pruned, from, head, keep_recent, "startup prune complete");
    }
}

// Re-export so callers don't need to dig into the submodule.
pub use crate::vote_pool::new_shared as new_vote_pool;
