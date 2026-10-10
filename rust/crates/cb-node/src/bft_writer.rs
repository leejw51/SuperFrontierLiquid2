//! Executing committed BFT proposals at commit, and sealing their blocks
//! when their certificates arrive.
//!
//! A committed proposal's block header carries the certificate from its
//! successor on the committed chain (ADR 0008, decision 5), which is known
//! one commit later. Execution needs none of it: the payload, the ordering
//! seed and the proposer are fixed at commit. So a proposal is executed the
//! moment it commits ([`BlockWriter::execute`]) and its rows go to the
//! database as a deferred write ([`cb_state::DeferredWrite`]): every read
//! sees them at once, the state root is computed as soon as everything
//! before it is on disk and published in [`Roots`] (the executed head the
//! proposer audit compares), and the block is sealed and written when the
//! certificate comes ([`BlockWriter::seal`]).
//!
//! # What a crash or a catch-up does to an unsealed block
//!
//! Nothing durable: its rows, its body and its head hash reach RocksDB in
//! one batch, at the seal, or not at all. A validator killed with a block
//! executed but unsealed restarts from the last sealed block, fetches the
//! proposal again from its peers and executes it again from the same state.
//! A catch-up by block sync discards it first ([`BlockWriter::reset`]) and
//! lands the block its peers sealed. Peers are never served an unsealed
//! block: the block body exists only once sealed, and the head this node
//! announces is read from the durable view.

use cb_block::{
    execute_block_with, seal_block, BlockOrigin, ExecCtx, ExecError, RejectedTx, SealChain,
};
use cb_consensus::bft::{Committed, ProposalHeader};
use cb_state::{
    chain_state_hash, diff_hash, Database, DeferredWrite, Meta, ReachedFn, SealFn, Sealed, ALL_CFS,
    CF_META,
};
use cb_tx::VerifiedTx;
use cb_types::{Address, MarketId, Side, H256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// State roots of recently executed heights: published when a block's rows
/// are final, before it is sealed.
pub type Roots = Arc<Mutex<BTreeMap<u64, H256>>>;

/// Heights of state roots kept in [`Roots`].
const ROOTS_KEPT: usize = 4_096;

/// One executed transaction as the node's metrics see it: `(sender, is an
/// order placement, side and market when it is one)`.
pub type AppliedTx = (Address, bool, Option<(Side, Option<MarketId>)>);

/// What executing a committed proposal did.
pub struct Outcome {
    pub height: u64,
    /// Every transaction that executed.
    pub applied: Vec<AppliedTx>,
    pub rejected: Vec<RejectedTx>,
    pub execute_us: u64,
}

/// The one block executed and not yet sealed.
struct Unsealed {
    id: H256,
    height: u64,
    timestamp: u64,
    round: u64,
    sig: Vec<u8>,
    txs: Vec<VerifiedTx>,
    /// Parallel to `txs`: which executed. Sealed into the block.
    applied: Vec<bool>,
    write: DeferredWrite,
}

/// Executes committed proposals in order and seals their blocks. Holds at
/// most one block executed and not yet sealed: the engine hands over the
/// newest committed proposal at commit and its certificate with the next
/// commit, before anything newer.
pub struct BlockWriter {
    db: Database,
    ctx: ExecCtx,
    chain: Arc<Mutex<SealChain>>,
    roots: Roots,
    on_sealed: Arc<dyn Fn(u64, H256) + Send + Sync>,
    pending: Option<Unsealed>,
}

impl BlockWriter {
    /// `on_sealed(height, hash)` runs once a block is on disk.
    pub fn new(
        db: Database,
        ctx: ExecCtx,
        roots: Roots,
        on_sealed: Arc<dyn Fn(u64, H256) + Send + Sync>,
    ) -> Self {
        let chain = Arc::new(Mutex::new(SealChain::at_head(&db)));
        Self {
            db,
            ctx,
            chain,
            roots,
            on_sealed,
            pending: None,
        }
    }

    /// The proposal executed and waiting for its certificate, if any.
    pub fn unsealed(&self) -> Option<H256> {
        self.pending.as_ref().map(|u| u.id)
    }

    /// Execute a committed proposal whose certificate is still to come. Its
    /// effects are visible to every read on return.
    pub fn execute(
        &mut self,
        header: &ProposalHeader,
        txs: Vec<VerifiedTx>,
    ) -> Result<Outcome, ExecError> {
        if let Some(u) = &self.pending {
            // The engine hands a proposal's certificate over before anything
            // committed after it; this is a driver bug, and executing on top
            // of an unsealed block that may yet be discarded is worse.
            return Err(ExecError::Storage(format!(
                "round {} is still unsealed; refusing to execute round {} on top of it",
                u.round, header.round
            )));
        }
        let started = std::time::Instant::now();
        let ex = execute_block_with(
            &self.db,
            &self.ctx,
            txs,
            header.timestamp,
            Some(BlockOrigin {
                seed: header.parent_id,
                proposer: header.proposer,
            }),
            |_, _, _| {},
        )?;
        let execute_us = started.elapsed().as_micros() as u64;
        let mut rows = ex.rows;
        let meta = ALL_CFS
            .iter()
            .position(|c| *c == CF_META)
            .expect("meta is a column family");
        for (k, v) in Meta::head_progress_rows(ex.height, ex.timestamp) {
            rows[meta].insert(k, Some(v));
        }
        let applied: Vec<AppliedTx> = ex
            .txs
            .iter()
            .zip(ex.applied.iter())
            .filter(|(_, ok)| **ok)
            .map(|(v, _)| {
                (
                    v.sender,
                    v.signed.tx.body.is_order_placement(),
                    v.signed.tx.body.order_side_and_market(),
                )
            })
            .collect();
        // The state root is the previous one chained with this block's
        // rows, the same computation the seal repeats; it does not involve
        // the certificate. Published once everything before this block is
        // on disk, which is when the previous root is final.
        let reached: ReachedFn = {
            let chain = self.chain.clone();
            let roots = self.roots.clone();
            let height = ex.height;
            Box::new(move |rows| {
                let prev = chain.lock().expect("seal chain").prev_root;
                let root = chain_state_hash(prev, diff_hash(rows).0);
                publish_root(&roots, height, root);
            })
        };
        let write = self
            .db
            .write_deferred(rows, reached)
            .map_err(|e| ExecError::Storage(e.to_string()))?;
        self.pending = Some(Unsealed {
            id: header.id(),
            height: ex.height,
            timestamp: ex.timestamp,
            round: header.round,
            sig: header.sig.clone(),
            txs: ex.txs,
            applied: ex.applied,
            write,
        });
        Ok(Outcome {
            height: ex.height,
            applied,
            rejected: ex.rejected,
            execute_us,
        })
    }

    /// Seal the unsealed block with `c`'s certificate, if `c` is its
    /// proposal. Returns whether it was.
    pub fn seal(&mut self, c: &Committed) -> Result<bool, cb_state::DbError> {
        if self.pending.as_ref().map(|u| u.id) != Some(c.header.id()) {
            return Ok(false);
        }
        let u = self.pending.take().expect("checked");
        let finality = c.finality().encode();
        let chain = self.chain.clone();
        let roots = self.roots.clone();
        let on_sealed = self.on_sealed.clone();
        let (height, timestamp, view, sig, txs, applied) =
            (u.height, u.timestamp, u.round, u.sig, u.txs, u.applied);
        let seal: SealFn = Box::new(move |rows| {
            let mut chain = chain
                .lock()
                .map_err(|_| "seal chain poisoned".to_string())?;
            let sealed = seal_block(
                &mut chain,
                rows,
                height,
                timestamp,
                view,
                finality,
                txs,
                applied,
                move |_| sig,
            );
            let (hash, root) = (sealed.block_hash, sealed.state_root);
            Ok(Sealed {
                extra: sealed.extra,
                after: Box::new(move || {
                    publish_root(&roots, height, root);
                    on_sealed(height, hash);
                }),
            })
        });
        u.write.seal(seal)?;
        Ok(true)
    }

    /// Execute a committed proposal whose certificate is already known, and
    /// seal it.
    pub fn execute_sealed(
        &mut self,
        c: &Committed,
        txs: Vec<VerifiedTx>,
    ) -> Result<Outcome, ExecError> {
        let out = self.execute(&c.header, txs)?;
        self.seal(c)
            .map_err(|e| ExecError::Storage(e.to_string()))?;
        Ok(out)
    }

    /// Drop the unsealed block, if any, wait until everything else is on
    /// disk, and re-read the head: for a catch-up, which lands blocks by
    /// another path.
    pub fn reset(&mut self) {
        let dropped = self.pending.take().map(|u| {
            u.write.discard();
            u.height
        });
        let _ = self.db.flush_pending();
        if let Some(h) = dropped {
            self.roots.lock().expect("roots").remove(&h);
        }
        *self.chain.lock().expect("seal chain") = SealChain::at_head(&self.db);
    }
}

fn publish_root(roots: &Roots, height: u64, root: H256) {
    let mut r = roots.lock().expect("roots");
    r.insert(height, root);
    while r.len() > ROOTS_KEPT {
        r.pop_first();
    }
}
