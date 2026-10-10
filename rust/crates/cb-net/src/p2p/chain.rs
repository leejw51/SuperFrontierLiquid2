//! The seam between gossip and the chain.
//!
//! [`ChainAccess`] is everything the gossip driver needs from the node: read a
//! block, verify a block, accept a transaction. It is a trait so the tests can
//! substitute an in-memory chain for the parts of the design that have nothing
//! to do with execution (discovery, dedup, scoring, partition healing) while
//! the parts that *are* about execution run against a real RocksDB-backed
//! [`Database`].
//!
//! # The rule this module exists to enforce
//!
//! [`DbChain::verify_and_apply`] is a thin wrapper over
//! [`crate::apply_remote_block`] — the same function the original leader-star
//! replica used. It re-executes every transaction and compares the recomputed
//! state root against the one in the header. There is no second path, no
//! "trusted peer" flag, and no fast-mirror equivalent on the gossip side.
//!
//! That is a deliberate difference from the legacy `--sequencer` replica,
//! whose default fast-mirror mode stores the leader's bytes without
//! re-executing them. Fast-mirror is defensible when the bytes come from the
//! one node you already had to trust to produce them. It is not defensible
//! when the bytes come from an arbitrary peer, and gossip is exactly the case
//! where they do — so the gossip path verifies, always, and pays the CPU.

use crate::p2p::engine::RejectReason;
use crate::{apply_remote_block, SyncError};
use cb_block::{Block, ExecCtx};
use cb_state::{Blocks, Database, Mempool, Meta};
use cb_types::H256;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// What the gossip driver needs from the node's chain.
pub trait ChainAccess {
    /// Current chain head.
    fn head_height(&self) -> u64;
    /// Canonical encoded bytes of the block at `height`, if we still hold it.
    /// `None` covers both "not produced yet" and "pruned"; a peer asking for a
    /// pruned height gets a short answer, not an error.
    fn block_bytes(&self, height: u64) -> Option<Vec<u8>>;
    /// Height of a block we hold, by hash.
    fn height_of(&self, hash: &H256) -> Option<u64>;
    /// Re-execute `bytes` and adopt the block if, and only if, the recomputed
    /// state root matches the one it claims. Returns the block's hash and
    /// height on success.
    fn verify_and_apply(&mut self, bytes: &[u8]) -> Result<(H256, u64), RejectReason>;
    /// Encoded bytes of a transaction we are holding, by hash.
    fn tx_bytes(&self, hash: &H256) -> Option<Vec<u8>>;
    /// Validate a gossiped transaction and put it in the mempool. Returns its
    /// hash on success.
    fn accept_tx(&mut self, bytes: &[u8]) -> Result<H256, RejectReason>;
    /// Remember a transaction this node accepted through some other door — an
    /// RPC client, say — so that a peer which hears the announcement can ask
    /// for the body and get it.
    ///
    /// Without this, a locally submitted order is announced to the network and
    /// then cannot be served to anyone who asks: every peer times out, and the
    /// node that submitted the order is scored down for it. That failure is
    /// invisible in a test that injects transactions through `accept_tx`,
    /// because `accept_tx` remembers them as a side effect.
    fn remember_tx(&mut self, hash: H256, bytes: Vec<u8>);
}

/// Map a sync failure onto the scoring vocabulary.
///
/// The interesting case is [`SyncError::NonMonotonic`], which maps to a benign
/// reason: it means the block did not land at `head + 1` by the time we got to
/// it, which happens when our own chain moved while the body was in flight.
/// Scoring a peer down for losing that race would punish the fastest peers.
fn classify(err: &SyncError) -> RejectReason {
    match err {
        SyncError::StateRootMismatch { .. } => RejectReason::StateRootMismatch,
        SyncError::NonMonotonic { .. } => RejectReason::OutOfOrder,
        SyncError::BadSignature { .. } => RejectReason::InvalidTransaction,
        SyncError::MalformedProposerSig { .. }
        | SyncError::WrongProposer { .. }
        | SyncError::BadFinality { .. } => RejectReason::BadProposerSignature,
        // A forged sender is a forged transaction: the block carries a
        // signature that does not say what the leader claims it says.
        SyncError::SenderMismatch { .. } => RejectReason::InvalidTransaction,
        // A body that does not hash to its header, or a header that does not
        // link to our head, is not the block it claims to be. The parent case
        // can also be a benign race — our head moved while the body was in
        // flight — but the height check above it already catches that case
        // as `NonMonotonic`, so what reaches here is a genuinely wrong parent
        // at the right height: a fork, or a lie.
        // A block whose applied mask disagrees with re-execution is likewise
        // not the block it claims to be.
        SyncError::WrongParent { .. }
        | SyncError::TxRootMismatch { .. }
        | SyncError::TimestampRegressed { .. }
        | SyncError::AppliedMaskMismatch { .. } => RejectReason::StateRootMismatch,
        // A senders/txs length mismatch is a malformed block, and a storage
        // failure leaves us unable to say the block was good. Both are treated
        // as "this body did not become a block", which is the honest verdict;
        // a storage failure scoring the sender down is the one unfair case,
        // and it is preferred to silently accepting.
        SyncError::SendersMismatch { .. } | SyncError::Apply { .. } | SyncError::Storage(_) => {
            RejectReason::ExecutionFailed
        }
    }
}

/// [`ChainAccess`] over the node's real database.
pub struct DbChain {
    db: Database,
    ctx: ExecCtx,
    notify: Option<Arc<Notify>>,
    recent_txs: VecDeque<(H256, Vec<u8>)>,
    recent_tx_cap: usize,
    /// Whether gossiped blocks are applied here. A validator executes the
    /// committed sequence itself and must not also apply blocks a peer
    /// serves, so it serves and never applies — except while catching up,
    /// when it flips this on until it is back within reach of consensus.
    apply_remote: Arc<AtomicBool>,
}

impl DbChain {
    /// Wrap a database. `notify` is the sequencer's block-builder wakeup; pass
    /// `None` on a node that does not produce blocks.
    pub fn new(db: Database, ctx: ExecCtx, notify: Option<Arc<Notify>>) -> Self {
        Self {
            db,
            ctx,
            notify,
            recent_txs: VecDeque::new(),
            recent_tx_cap: 4_096,
            apply_remote: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The switch behind [`DbChain::serve_only`]: `true` applies gossiped
    /// blocks, `false` only serves them.
    pub fn apply_switch(&self) -> Arc<AtomicBool> {
        self.apply_remote.clone()
    }

    /// A chain that serves blocks and accepts transactions but never applies
    /// a block a peer sent: for a validator, whose blocks come from its own
    /// execution of the committed sequence.
    pub fn serve_only(db: Database, ctx: ExecCtx, notify: Option<Arc<Notify>>) -> Self {
        Self {
            apply_remote: Arc::new(AtomicBool::new(false)),
            ..Self::new(db, ctx, notify)
        }
    }

    /// The wrapped database.
    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Keep a bounded window of recent transaction bodies so a peer that
    /// hears an announcement can fetch the body from us.
    fn remember(&mut self, hash: H256, bytes: Vec<u8>) {
        if self.recent_txs.iter().any(|(h, _)| *h == hash) {
            return;
        }
        self.recent_txs.push_back((hash, bytes));
        while self.recent_txs.len() > self.recent_tx_cap {
            self.recent_txs.pop_front();
        }
    }
}

impl ChainAccess for DbChain {
    fn head_height(&self) -> u64 {
        Meta::new(&self.db).head_height()
    }

    fn block_bytes(&self, height: u64) -> Option<Vec<u8>> {
        Blocks::new(&self.db).get(height)
    }

    fn height_of(&self, hash: &H256) -> Option<u64> {
        Blocks::new(&self.db).height_of(hash)
    }

    fn verify_and_apply(&mut self, bytes: &[u8]) -> Result<(H256, u64), RejectReason> {
        if !self.apply_remote.load(Ordering::Relaxed) {
            // Not a verdict on the block: this node does not take blocks
            // from peers. Benign, so the peer is not scored down for it.
            return Err(RejectReason::OutOfOrder);
        }
        let block = Block::decode(bytes).map_err(|_| RejectReason::Undecodable)?;
        // The whole guarantee, in one call: re-execute, recompute the state
        // root, refuse the block unless they match.
        apply_remote_block(&self.db, &self.ctx, &block).map_err(|e| classify(&e))?;
        Ok((block.header.hash(), block.header.height))
    }

    fn tx_bytes(&self, hash: &H256) -> Option<Vec<u8>> {
        self.recent_txs
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, b)| b.clone())
    }

    fn accept_tx(&mut self, bytes: &[u8]) -> Result<H256, RejectReason> {
        let signed: cb_tx::SignedTx =
            bincode::deserialize(bytes).map_err(|_| RejectReason::Undecodable)?;
        cb_tx::validate_static(&signed.tx).map_err(|_| RejectReason::InvalidTransaction)?;
        // Same admission the RPC door applies: a transaction signed for another
        // chain is not this chain's transaction. The executor would refuse it
        // at block build anyway, but the mempool should not hold foreign
        // traffic that a peer can keep announcing.
        if signed.tx.chain_id != Meta::new(&self.db).chain_id() {
            return Err(RejectReason::InvalidTransaction);
        }
        let sender = signed
            .sender()
            .map_err(|_| RejectReason::InvalidTransaction)?;
        let hash = signed.hash();
        let record = cb_tx::MempoolRecord { signed, sender };
        let encoded = bincode::serialize(&record).map_err(|_| RejectReason::InvalidTransaction)?;
        Mempool::new(&self.db).put(hash, record.sender, record.signed.tx.nonce, &encoded);
        if let Some(n) = &self.notify {
            n.notify_waiters();
        }
        self.remember(hash, bytes.to_vec());
        Ok(hash)
    }

    fn remember_tx(&mut self, hash: H256, bytes: Vec<u8>) {
        self.remember(hash, bytes);
    }
}
