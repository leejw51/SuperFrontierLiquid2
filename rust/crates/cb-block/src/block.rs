//! Block + builder.

use crate::executor::{apply_tx_at, consume_nonce, failure_consumes_nonce, ExecCtx, ExecError};
use crate::header::BlockHeader;
use crate::ordering::order_for_block;
use crate::state::BlockState;
use cb_crypto::keccak256;
use cb_engine::compute_settlement_price;
use cb_state::{
    chain_state_hash, diff_hash, Blocks, CfMap, Database, FuturesMarkets, Meta, OracleState,
    PerpMarkets, SealFn, Sealed, ALL_CFS, CF_META,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, DerivId, FutureMarket, H256};
use serde::{Deserialize, Serialize};

/// The most transactions one block may carry.
///
/// `executor::pack_seq` keeps sixteen bits of a transaction's index in the
/// block, and a resting order's book-index key is `(price, seq)`. Two orders
/// at index `i` and `i + 65_536` would share a key: the second overwrites the
/// first's index entry and a cancel of either deletes the other's, leaving
/// locked collateral behind an order the matcher can no longer see. The
/// builder and the verifier both refuse a larger block, and the node clamps
/// `--max-block-txs` to it.
pub const MAX_BLOCK_TXS: usize = u16::MAX as usize;

/// A block. Carries the signed txs PLUS the leader's pre-recovered senders.
/// The senders Vec is parallel to txs (same length, same order).
///
/// Verifying replicas do **not** trust this list: `apply_remote_block`
/// recovers every sender itself and rejects the block if the two disagree
/// (core review C2). The list survives as a prefetch hint and as a
/// cross-check that names the exact transaction when a leader lies; the
/// fast-mirror replica, which never decodes a block, is the only node that
/// takes the leader's word, and it says so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub txs: Vec<SignedTx>,
    /// Pre-recovered senders, parallel to `txs`. Empty for legacy blocks
    /// produced before this field existed; followers fall back to recovery
    /// in that case.
    #[serde(default)]
    pub senders: Vec<Address>,
    /// Under BFT, parallel to `txs`: whether each transaction executed. The
    /// block keeps the whole committed payload, and a refused transaction
    /// now consumes its nonce like an applied one (see
    /// [`crate::failure_consumes_nonce`]), so this mask is the only way a
    /// reader tells the two apart. Every validator computes it and a replica
    /// refuses a block whose mask differs from its own re-execution. Empty
    /// under the sequencer scheme, where every transaction in a block
    /// executed.
    #[serde(default)]
    pub applied: Vec<bool>,
}

impl Block {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Block encode")
    }
    pub fn decode(b: &[u8]) -> bincode::Result<Self> {
        bincode::deserialize(b)
    }

    pub fn compute_tx_root(txs: &[SignedTx]) -> H256 {
        let mut concat = Vec::new();
        for t in txs {
            concat.extend_from_slice(&bincode::serialize(t).expect("SignedTx encode"));
        }
        keccak256(&concat)
    }
}

pub struct BuildResult {
    pub block: Block,
    pub state_root: H256,
    /// Transactions that were drained into this block and refused. They are
    /// **not** in `block.txs` — see the apply loop in [`build_and_apply`] for
    /// why a failed transaction is dropped rather than recorded on chain. The
    /// sequencer logs these and publishes them on the `streamRejected` RPC,
    /// which is how a gateway learns that an order it submitted died.
    pub rejected: Vec<RejectedTx>,
}

/// One transaction the executor refused while building a block, with enough
/// identity for the node to say which submission died and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedTx {
    pub hash: H256,
    pub sender: Address,
    pub nonce: u64,
    pub error: ExecError,
}

/// A block was applied but did not reproduce the state root it claims.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("tx apply failed: {0}")]
    Apply(#[from] ExecError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("state root mismatch: expected {expected:?}, got {got:?}")]
    StateRootMismatch { expected: H256, got: H256 },
    /// Under BFT: the block's `applied` mask does not say what this node's
    /// own re-execution found. The state root would still match — the mask
    /// is not in it — but the gateway and every observer read the mask to
    /// tell a refused transaction from an applied one, so a block whose
    /// leader lied in it is refused here.
    #[error("applied mask at height {h} differs from re-execution")]
    AppliedMaskMismatch { h: u64 },
}

/// Re-execute a block somebody else produced and, **only if it reproduces the
/// state root it claims**, commit it.
///
/// This is the replica half of [`build_and_apply`], and it exists so both
/// halves share one commit. Two properties matter and neither held before:
///
/// * **The root check precedes the write.** Every transaction runs against a
///   staged `Database`, so until the final `write_batch` nothing this block
///   did exists outside memory. A block that fails the check is dropped on the
///   floor and the database is byte-for-byte what it was — which is what
///   `cb-net`'s syncer module header has always claimed and never delivered.
/// * **The commit is one write.** State, block bytes, hash index and head
///   pointer land in a single `WriteBatch`, so a crash leaves the node at
///   height N or at N-1, never holding a head pointer to a block whose state
///   was never applied.
///
/// Unlike the leader's path, a transaction that fails here fails the *block*.
/// That is the correct asymmetry: the leader chooses which transactions to
/// include, so a block that reaches a replica containing a transaction that
/// cannot execute is an invalid block.
#[allow(clippy::too_many_arguments)]
pub fn verify_and_commit(
    db: &Database,
    ctx: &ExecCtx,
    txs: &[VerifiedTx],
    height: u64,
    block_hash: H256,
    block_bytes: &[u8],
    expected_root: H256,
    timestamp: u64,
    prefetch: &[Address],
) -> Result<H256, VerifyError> {
    verify_and_commit_with(
        db,
        ctx,
        txs,
        height,
        block_hash,
        block_bytes,
        expected_root,
        timestamp,
        prefetch,
        false,
        None,
    )
}

/// [`verify_and_commit`] with the BFT execution rule when `skip_failures`:
/// a transaction that fails is skipped rather than failing the block, exactly
/// as the validators that executed the committed payload skipped it, and
/// `txs` includes the synthetic system transactions ahead of the block's.
#[allow(clippy::too_many_arguments)]
pub fn verify_and_commit_with(
    db: &Database,
    ctx: &ExecCtx,
    txs: &[VerifiedTx],
    height: u64,
    block_hash: H256,
    block_bytes: &[u8],
    expected_root: H256,
    timestamp: u64,
    prefetch: &[Address],
    skip_failures: bool,
    expected_applied: Option<&[bool]>,
) -> Result<H256, VerifyError> {
    if txs.len() > MAX_BLOCK_TXS {
        return Err(VerifyError::Apply(ExecError::TooManyTxs {
            got: txs.len(),
            max: MAX_BLOCK_TXS,
        }));
    }
    // The height a transaction executes at feeds `pack_seq` and the derived
    // order ids, so it is taken from the block being verified rather than from
    // whatever the caller's `ctx` happened to carry.
    let block_ctx = ExecCtx {
        treasury_signer: ctx.treasury_signer,
        block_height: height,
        proposer_addr: ctx.proposer_addr,
    };
    let mut state = BlockState::new(db);
    if !prefetch.is_empty() {
        state.prefetch(prefetch);
    }
    let mut kept = 0u32;
    // Under BFT, which of the payload's signed transactions executed: what
    // the block's `applied` mask must say, if the block carries one.
    let mut applied: Vec<bool> = Vec::new();
    for v in txs.iter() {
        if skip_failures {
            let sp = state.savepoint();
            let ok = match apply_tx_at(&mut state, &block_ctx, v, kept) {
                Ok(()) => {
                    kept += 1;
                    true
                }
                Err(e) => {
                    state.rollback(sp);
                    if failure_consumes_nonce(v, &e) {
                        consume_nonce(&mut state, v.sender);
                    }
                    false
                }
            };
            if !v.is_synthetic() {
                applied.push(ok);
            }
        } else {
            apply_tx_at(&mut state, &block_ctx, v, kept)?;
            kept += 1;
        }
        state.commit_point();
    }

    if let Some(expected) = expected_applied {
        if skip_failures && !expected.is_empty() && expected != applied.as_slice() {
            return Err(VerifyError::AppliedMaskMismatch { h: height });
        }
    }

    let mut batch = db.batch();
    let roots = state
        .commit_into(&mut batch)
        .map_err(|e| VerifyError::Storage(e.to_string()))?;
    let state_root = roots.state_root;
    if state_root != expected_root {
        // `batch` is dropped un-written. Nothing was persisted.
        return Err(VerifyError::StateRootMismatch {
            expected: expected_root,
            got: state_root,
        });
    }

    Blocks::new(db)
        .put_into_batch(&mut batch, height, block_hash, block_bytes)
        .map_err(|e| VerifyError::Storage(e.to_string()))?;
    Meta::new(db)
        .set_head_into_batch(&mut batch, height, block_hash, state_root, timestamp)
        .map_err(|e| VerifyError::Storage(e.to_string()))?;
    db.write_batch(batch)
        .map_err(|e| VerifyError::Storage(e.to_string()))?;
    Ok(state_root)
}

/// Apply pre-verified `txs` (sender already recovered) in canonical intra-block order,
/// commit state in one batched write, persist the block, advance head.
///
/// `view` is the consensus view this block is proposed in (leader is
/// `validators[view % N]`). `parent_qc_bytes` is the bincode-serialized
/// `QuorumCert` proving block `height - 1` was committed by 2/3 of validators;
/// pass an empty `Vec` until the voting layer is wired (block 1 always passes
/// empty since genesis was never voted on).
///
/// `sign_header` receives the canonical preimage (the header bytes with an empty
/// `sequencer_sig` slot, exactly what `BlockHeader::signing_bytes` returns) and must
/// return the proposer's signature over those bytes. Replicas recover this signature
/// and reject blocks whose recovered address does not match the expected leader.
///
/// Caller MUST ensure `!txs.is_empty()` (no-empty-block invariant).
pub fn build_and_apply<F>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    view: u64,
    parent_qc_bytes: Vec<u8>,
    sign_header: F,
) -> Result<BuildResult, ExecError>
where
    F: FnOnce(&[u8]) -> Vec<u8>,
{
    build_and_apply_then(
        db,
        ctx,
        txs,
        timestamp,
        view,
        parent_qc_bytes,
        sign_header,
        |_, _| {},
    )
}

/// [`build_and_apply`], plus something to run once the block is on disk.
///
/// `on_durable(height, block_hash)` runs after RocksDB has the block's batch:
/// before this returns when the database writes synchronously, and on the
/// writer thread when it does not (`Database::enable_async_writes`). The
/// sequencer puts there everything that tells the outside world a block
/// exists — the notification that drives block streams and the p2p announcer,
/// its own vote, pruning — so that a crash can never leave a peer holding a
/// hash this node no longer has. Reads of the block's effects need no such
/// care; they go through the overlay from the moment this returns.
///
/// This executes and seals on the calling thread. A pipelined leader uses
/// [`execute_and_hand_off`] instead, which seals on the writer thread; both
/// are [`execute_block`] followed by [`seal_block`], so both produce the same
/// block and the same root.
#[allow(clippy::too_many_arguments)]
pub fn build_and_apply_then<F, D>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    view: u64,
    parent_qc_bytes: Vec<u8>,
    sign_header: F,
    on_durable: D,
) -> Result<BuildResult, ExecError>
where
    F: FnOnce(&[u8]) -> Vec<u8>,
    D: FnOnce(u64, H256) + Send + 'static,
{
    let mut chain = SealChain::at_head(db);
    let ex = execute_block(db, ctx, txs, timestamp, |_, _, _| {})?;
    let Executed {
        height,
        timestamp,
        txs,
        applied: _,
        rejected,
        mut rows,
    } = ex;
    push_head_progress(&mut rows, height, timestamp);
    // The sequencer scheme drops a refused transaction from the block, so
    // every transaction in one executed and the mask stays empty.
    let sealed = seal_block(
        &mut chain,
        &rows,
        height,
        timestamp,
        view,
        parent_qc_bytes,
        txs,
        Vec::new(),
        sign_header,
    );
    // One write. State, block bytes, the hash index and the head pointer all
    // land together or not at all — a crash here cannot leave the head
    // pointing at a block whose state was never applied, which is how a
    // sequencer used to fork its own chain on a plain `kill`.
    let mut batch = db.batch();
    Database::maps_into_batch(&mut batch, &rows).map_err(|e| ExecError::Storage(e.to_string()))?;
    for (idx, k, v) in &sealed.extra {
        batch
            .put(ALL_CFS[*idx], k, v)
            .map_err(|e| ExecError::Storage(e.to_string()))?;
    }
    let block_hash = sealed.block_hash;
    db.write_batch_then(batch, move || on_durable(height, block_hash))
        .map_err(|e| ExecError::Storage(e.to_string()))?;

    Ok(BuildResult {
        block: sealed.block,
        state_root: sealed.state_root,
        rejected,
    })
}

/// A block the leader has executed and not yet sealed: its transactions in
/// block order, the ones it refused, and every row it wrote.
pub struct Executed {
    pub height: u64,
    /// The header timestamp, already clamped to never run backwards.
    pub timestamp: u64,
    /// The block's transactions in block order. Under the sequencer scheme
    /// these are exactly the ones that executed; under BFT they are the whole
    /// committed payload, and `applied` says which of them executed.
    pub txs: Vec<VerifiedTx>,
    /// Parallel to `txs`: whether each one executed. All true under the
    /// sequencer scheme.
    pub applied: Vec<bool>,
    pub rejected: Vec<RejectedTx>,
    /// Every row the block wrote, one map per column family in `ALL_CFS`
    /// order (`BlockState::finish`).
    pub rows: Vec<CfMap>,
}

/// What a block executed under BFT consensus takes from its committed
/// proposal rather than from this node: the ordering seed and the proposer.
///
/// Under the sequencer scheme both come from the local head (the sealed head
/// hash and `Meta::proposer_addr`), which is fine when one node orders and
/// everyone else replays its order. Under BFT every validator executes the
/// same committed payload independently, so anything execution depends on
/// must come from the proposal: the seed is the parent proposal's id and the
/// proposer is the round's leader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockOrigin {
    pub seed: H256,
    pub proposer: Address,
}

/// The system transactions the block at `height` must carry, as the
/// executor's own unsigned transactions (`VerifiedTx::synthetic`), in the
/// order the enforcement checks name them.
///
/// Under BFT the proposer does not know, when it proposes, the height its
/// payload will execute at or the state it will execute against; a signed
/// tick in the payload could be for the wrong height or use a nonce the
/// proposer's earlier ticks have already spent. So the ticks are not in the
/// payload at all. Every validator, and every observer replaying the block,
/// injects the same ones from the same rule against the same parent state.
/// The block's transaction list and root do not include them; the state root
/// covers what they did.
pub fn synthetic_system_txs(
    db: &Database,
    height: u64,
    proposer: Address,
    chain_id: u32,
) -> Vec<VerifiedTx> {
    required_system_txs(db, height)
        .into_iter()
        .map(|body| {
            VerifiedTx::synthetic(
                Transaction {
                    nonce: 0,
                    chain_id,
                    body,
                },
                proposer,
            )
        })
        .collect()
}

/// Order `txs`, execute them against the head, and return the block's rows.
///
/// Nothing is hashed, signed or written. `observe(state, tx, outcome)` is
/// called before each transaction with `None` and after it with its result;
/// the node passes a no-op and the order benchmark counts fills with it.
pub fn execute_block<O>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    observe: O,
) -> Result<Executed, ExecError>
where
    O: FnMut(&mut BlockState, &VerifiedTx, Option<&Result<(), ExecError>>),
{
    execute_block_with(db, ctx, txs, timestamp, None, observe)
}

/// [`execute_block`] with the block's origin supplied by a committed
/// proposal (BFT) rather than read from the local head.
///
/// With an origin: `txs` is the committed payload, already in canonical
/// order, and is executed as given; the required system transactions are
/// injected as synthetic ones ahead of it; a transaction that fails is
/// skipped and stays in the block, so the block's transaction list and root
/// are the payload's. Without one this is the sequencer's block build.
pub fn execute_block_with<O>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    origin: Option<BlockOrigin>,
    mut observe: O,
) -> Result<Executed, ExecError>
where
    O: FnMut(&mut BlockState, &VerifiedTx, Option<&Result<(), ExecError>>),
{
    assert!(
        !txs.is_empty(),
        "no-empty-block invariant must be enforced by caller"
    );
    if txs.len() > MAX_BLOCK_TXS {
        return Err(ExecError::TooManyTxs {
            got: txs.len(),
            max: MAX_BLOCK_TXS,
        });
    }
    // The block we're building lands at head_height + 1. Capture this and the
    // proposer once so the per-tx ExecCtx is consistent across the apply loop.
    // M2: `proposer_addr` gates `FundingTick` system txs and is consulted by
    // the boundary check below. M3: `block_height` feeds deterministic perp
    // `seq` derivation in `apply_tx_at`.
    let height = Meta::new(db).head_height() + 1;
    let (txs, proposer_addr) = match origin {
        None => {
            // The parent hash seeds the per-block permutation of accounts
            // within a rank; the head has not moved yet, so this is the
            // parent of the block being built.
            (
                order_for_block(txs, Meta::new(db).head_hash()),
                Meta::new(db).proposer_addr(),
            )
        }
        Some(o) => {
            let chain_id = Meta::new(db).chain_id();
            let mut all = synthetic_system_txs(db, height, o.proposer, chain_id);
            all.extend(txs);
            (all, o.proposer)
        }
    };
    let bft = origin.is_some();
    // A header's clock never runs backwards on this chain, and replicas
    // refuse one that does (`apply_remote_block`). A leader whose wall clock
    // stepped back would otherwise produce a block every replica rejects.
    let timestamp = timestamp.max(Meta::new(db).head_timestamp());
    let block_ctx = ExecCtx {
        treasury_signer: ctx.treasury_signer,
        block_height: height,
        proposer_addr,
    };

    // Block-level FundingTick boundary check (M2): at every height that crosses
    // a funding-interval boundary for any registered perp, the block MUST
    // contain a FundingTick { market } system tx whose sender is `proposer_addr`.
    // This commits the funding action to the tx_root so replicas re-execute it
    // verbatim — no out-of-band "did the leader run the funding hook?" check.
    enforce_required_funding_ticks(db, height, proposer_addr, &txs)?;
    // Block-level FuturesSettle boundary check (M5): at every dated-future
    // expiry height the block MUST contain exactly one `FuturesSettle` tx
    // for that market, signed by the proposer. Same rationale as funding —
    // settlement actions live in the tx_root so replicas verify them by
    // re-executing.
    enforce_required_futures_settle(db, height, proposer_addr, &txs)?;

    let mut state = BlockState::new(db);

    // ---- The apply loop -----------------------------------------------
    //
    // A transaction that fails is **rejected individually and dropped**: it
    // does not enter `block.txs`, it leaves no state behind, and the block
    // carries on with the rest. Before this, a single `?` here turned one bad
    // transaction into a dead block and took every honest transaction drained
    // alongside it with it — an ordinary fill-or-kill against an empty book
    // was enough.
    //
    // Drop, rather than include-and-mark. Two reasons. It is the smaller
    // consensus change: a block still means "every transaction in it
    // executed", so a replica's re-execution stays a straight
    // apply-everything-or-reject, and the state root of a block is unchanged
    // by which transactions failed while it was being built. And it keeps one
    // rule in one sentence, which the alternative — a receipt list that both
    // sides must agree on byte for byte, inside the tx_root — does not.
    //
    // The cost, stated plainly: a dropped transaction does not consume its
    // nonce, so the signed bytes stay replayable until the account's nonce
    // moves past them. That is the same window a transaction that never
    // reached the mempool has, and it is why the node reports rejections to
    // its operator instead of silently swallowing them.
    //
    // `tx_index` is the index the transaction will have **in the produced
    // block**, not in the drained batch. Resting spot and perp orders derive
    // their `seq` (and perp order ids) from `(block_height, tx_index)`, so a
    // replica re-executing the block must see the same number — which it can
    // only do if the counter skips the transactions that were dropped.
    let mut kept: Vec<VerifiedTx> = Vec::with_capacity(txs.len());
    let mut rejected: Vec<RejectedTx> = Vec::new();
    // Under BFT the block keeps every payload transaction, applied or not,
    // so its list and root are the committed payload's; this is the mask.
    let mut all: Vec<VerifiedTx> = Vec::new();
    let mut applied: Vec<bool> = Vec::new();
    for v in txs.into_iter() {
        let tx_index = kept.len() as u32;
        let sp = state.savepoint();
        observe(&mut state, &v, None);
        let outcome = apply_tx_at(&mut state, &block_ctx, &v, tx_index);
        observe(&mut state, &v, Some(&outcome));
        let ok = outcome.is_ok();
        match outcome {
            Ok(()) => {
                state.commit_point();
                kept.push(v.clone());
            }
            Err(error) => {
                state.rollback(sp);
                if bft && failure_consumes_nonce(&v, &error) {
                    consume_nonce(&mut state, v.sender);
                }
                state.commit_point();
                rejected.push(RejectedTx {
                    hash: v.signed.hash(),
                    sender: v.sender,
                    nonce: v.signed.tx.nonce,
                    error,
                });
            }
        }
        if bft && !v.is_synthetic() {
            all.push(v);
            applied.push(ok);
        }
    }
    if kept.is_empty() && !(bft && !all.is_empty()) {
        // Nothing survived. Producing an empty block would violate the
        // no-empty-block invariant, and there is nothing to commit anyway.
        // Under BFT a payload of signed transactions that were all refused
        // is still a block — every one of them consumed its nonce, and that
        // has to land — so only a payload with no signed transaction at all
        // (a forged system transaction, say) comes to nothing.
        return Err(ExecError::NoApplicableTxs { rejected });
    }
    // Re-run the boundary checks against the transactions that actually
    // survived. The pre-loop run catches a leader that never included the
    // required tick; this one catches the case where it did include it and the
    // tick itself failed to execute. A block that ships without a required
    // tick is a block every replica rejects, so it must not be produced.
    enforce_required_funding_ticks(db, height, proposer_addr, &kept)?;
    enforce_required_futures_settle(db, height, proposer_addr, &kept)?;
    let (txs, applied) = if bft {
        (all, applied)
    } else {
        let n = kept.len();
        (kept, vec![true; n])
    };

    let rows = state
        .finish()
        .map_err(|e| ExecError::Storage(e.to_string()))?;
    Ok(Executed {
        height,
        timestamp,
        txs,
        applied,
        rejected,
        rows,
    })
}

/// The previous sealed block's state root and hash: the two things sealing
/// the next block needs from the chain. A pipelined leader keeps one of these
/// on its writer side and seals strictly in height order, so it never has to
/// read them back from a database that may not have them yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealChain {
    pub prev_root: H256,
    pub prev_hash: H256,
}

impl SealChain {
    /// The chain as the database's sealed head describes it.
    pub fn at_head(db: &Database) -> Self {
        let meta = Meta::new(db);
        Self {
            prev_root: meta.state_root(),
            prev_hash: meta.head_hash(),
        }
    }
}

/// A sealed block and the rows its seal adds to the write.
pub struct SealedBlock {
    pub block: Block,
    pub state_root: H256,
    pub block_hash: H256,
    /// `(column family index, key, value)`: the body, its hash index, and the
    /// head hash and state root.
    pub extra: Vec<(usize, Vec<u8>, Vec<u8>)>,
}

/// Seal an executed block: hash its rows into the state root, build and sign
/// the header, encode the body, and advance `chain`.
#[allow(clippy::too_many_arguments)]
pub fn seal_block<F>(
    chain: &mut SealChain,
    rows: &[CfMap],
    height: u64,
    timestamp: u64,
    view: u64,
    parent_qc_bytes: Vec<u8>,
    txs: Vec<VerifiedTx>,
    applied: Vec<bool>,
    sign_header: F,
) -> SealedBlock
where
    F: FnOnce(&[u8]) -> Vec<u8>,
{
    // The previous root chained with the hash of everything this block
    // changed (`cb_state::state_hash`). A leader that diverges on any
    // committed row produces a root no honest replica reproduces.
    let (diff, _) = diff_hash(rows);
    let state_root = chain_state_hash(chain.prev_root, diff);
    let senders: Vec<Address> = txs.iter().map(|v| v.sender).collect();
    let signed_txs: Vec<SignedTx> = txs.into_iter().map(|v| v.signed).collect();
    let mut header = BlockHeader {
        height,
        prev_hash: chain.prev_hash,
        tx_root: Block::compute_tx_root(&signed_txs),
        state_root,
        timestamp,
        view,
        parent_qc: parent_qc_bytes,
        sequencer_sig: Vec::new(),
    };
    let preimage = header.signing_bytes();
    header.sequencer_sig = sign_header(&preimage);
    let block = Block {
        header: header.clone(),
        txs: signed_txs,
        senders,
        applied,
    };
    let bytes = block.encode();
    let block_hash = header.hash();
    chain.prev_root = state_root;
    chain.prev_hash = block_hash;

    let cf = |name: &str| {
        ALL_CFS
            .iter()
            .position(|c| *c == name)
            .expect("known column family")
    };
    let mut extra = Vec::with_capacity(4);
    for (name, k, v) in Blocks::rows(height, block_hash, bytes) {
        extra.push((cf(name), k, v));
    }
    for (k, v) in Meta::sealed_head_rows(block_hash, state_root) {
        extra.push((cf(CF_META), k, v));
    }
    SealedBlock {
        block,
        state_root,
        block_hash,
        extra,
    }
}

/// Make the new head height and timestamp part of the block's rows, so the
/// next block sees them the moment these rows are visible.
fn push_head_progress(rows: &mut [CfMap], height: u64, timestamp: u64) {
    let meta_idx = ALL_CFS
        .iter()
        .position(|c| *c == CF_META)
        .expect("meta is a column family");
    for (k, v) in Meta::head_progress_rows(height, timestamp) {
        rows[meta_idx].insert(k, Some(v));
    }
}

/// One applied transaction as the node's metrics see it: `(sender, is an
/// order placement, side and market when it is one)`.
pub type AppliedTx = (
    Address,
    bool,
    Option<(cb_types::Side, Option<cb_types::MarketId>)>,
);

/// What the leader knows about a block the moment it hands it off, before it
/// is sealed.
pub struct HandedOff {
    pub height: u64,
    /// Every transaction in the block, in block order: `(sender, is an order
    /// placement, side and market when it is one)`.
    pub applied: Vec<AppliedTx>,
    pub rejected: Vec<RejectedTx>,
    /// Microseconds spent ordering and executing the block.
    pub execute_us: u64,
    /// Microseconds spent handing the rows over, including any wait for room
    /// when `MAX_PENDING` blocks are already unsealed. A figure that grows
    /// here means the writer thread, not execution, is the limit.
    pub handoff_us: u64,
}

/// Execute a block on the calling thread and hand it to the writer thread to
/// be sealed and written: the pipelined leader's commit (ADR 0007).
///
/// The caller — the thread that matches orders — pays only for ordering,
/// execution and moving the block's rows into an overlay, where the next block
/// and every reader see them at once. The diff hash, `tx_root`, header,
/// signature, block encoding and the RocksDB write all happen on the writer
/// thread, in height order, carrying `chain`. `on_durable(height, hash)` runs
/// there once the block is on disk.
///
/// The ordering seed is the latest *sealed* head hash, which may be a few
/// blocks behind the block being built. That ordering is leader-local —
/// replicas apply the order the block commits — so this is not a consensus
/// change; the seed only has to be something no one can choose.
#[allow(clippy::too_many_arguments)]
pub fn execute_and_hand_off<F, D, O>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    view: u64,
    parent_qc_bytes: Vec<u8>,
    chain: std::sync::Arc<std::sync::Mutex<SealChain>>,
    sign_header: F,
    on_durable: D,
    observe: O,
) -> Result<HandedOff, ExecError>
where
    F: FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
    D: FnOnce(u64, H256) + Send + 'static,
    O: FnMut(&mut BlockState, &VerifiedTx, Option<&Result<(), ExecError>>),
{
    execute_and_hand_off_with(
        db,
        ctx,
        txs,
        timestamp,
        view,
        parent_qc_bytes,
        None,
        chain,
        sign_header,
        on_durable,
        observe,
    )
}

/// [`execute_and_hand_off`] for a committed BFT proposal: `origin` carries
/// the proposal's seed and proposer (see [`execute_block_with`]).
#[allow(clippy::too_many_arguments)]
pub fn execute_and_hand_off_with<F, D, O>(
    db: &Database,
    ctx: &ExecCtx,
    txs: Vec<VerifiedTx>,
    timestamp: u64,
    view: u64,
    parent_qc_bytes: Vec<u8>,
    origin: Option<BlockOrigin>,
    chain: std::sync::Arc<std::sync::Mutex<SealChain>>,
    sign_header: F,
    on_durable: D,
    observe: O,
) -> Result<HandedOff, ExecError>
where
    F: FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
    D: FnOnce(u64, H256) + Send + 'static,
    O: FnMut(&mut BlockState, &VerifiedTx, Option<&Result<(), ExecError>>),
{
    let started = std::time::Instant::now();
    let Executed {
        height,
        timestamp,
        txs,
        applied: mask,
        rejected,
        mut rows,
    } = execute_block_with(db, ctx, txs, timestamp, origin, observe)?;
    let execute_us = started.elapsed().as_micros() as u64;
    let handoff_started = std::time::Instant::now();
    push_head_progress(&mut rows, height, timestamp);
    let applied = txs
        .iter()
        .zip(mask.iter())
        .filter(|(_, ok)| **ok)
        .map(|(v, _)| {
            (
                v.sender,
                v.signed.tx.body.is_order_placement(),
                v.signed.tx.body.order_side_and_market(),
            )
        })
        .collect();
    let seal: SealFn = Box::new(move |rows: &[CfMap]| {
        let mut c = chain
            .lock()
            .map_err(|_| "seal chain poisoned".to_string())?;
        let sealed = seal_block(
            &mut c,
            rows,
            height,
            timestamp,
            view,
            parent_qc_bytes,
            txs,
            Vec::new(),
            sign_header,
        );
        let hash = sealed.block_hash;
        Ok(Sealed {
            extra: sealed.extra,
            after: Box::new(move || on_durable(height, hash)),
        })
    });
    db.write_sealed(rows, seal)
        .map_err(|e| ExecError::Storage(e.to_string()))?;
    Ok(HandedOff {
        height,
        applied,
        rejected,
        execute_us,
        handoff_us: handoff_started.elapsed().as_micros() as u64,
    })
}

/// The perpetual markets whose funding-interval boundary lands on `height`,
/// in canonical `DerivId` order. Genesis (height 0) never ticks; height 0
/// represents "no funding has ever ticked".
fn required_funding_ticks(db: &Database, height: u64) -> Vec<DerivId> {
    if height == 0 {
        return Vec::new();
    }
    let mut required: Vec<DerivId> = PerpMarkets::new(db)
        .list()
        .iter()
        .filter(|m| {
            let interval = m.funding_interval_blocks.max(1) as u64;
            height % interval == 0
        })
        .map(|m| m.id)
        .collect();
    // Canonical DerivId byte order. Diagnostics that include a market id
    // should always name the smallest-id one missing, so replicas converge
    // on the same error.
    required.sort();
    required
}

/// Whether a settlement price can be computed for `market` from the state
/// this handle sees. `compute_settlement_price` returns `0` when both the
/// TWAP window and the oracle are empty, and `apply_futures_settle` refuses
/// to settle at zero — so a settle is only *required* once it can succeed.
fn settlement_price_available(db: &Database, market: &FutureMarket) -> bool {
    let store = OracleState::new(db);
    let history = store.list_mark_history(&market.id);
    let oracle = store.get_oracle(&market.id).map(|o| o.px_q8);
    compute_settlement_price(market, oracle, &history) > 0
}

/// The dated futures that must settle in the block at `height`, in canonical
/// `DerivId` order: every unsettled market at or past its expiry whose
/// settlement price is computable from the parent state.
///
/// "At or past", not "exactly at", and "computable", because the earlier rule
/// — exactly `expiry_height`, unconditionally — could not be satisfied by any
/// block when a future expired before its first oracle post: the settle was
/// present and failed, the post-loop check failed, the block failed, and the
/// height never advanced. The design (§8.3) asks for settlement to be
/// *delayed* in that case, which is what this does. The market is frozen from
/// `expiry_height` either way (`MarketView::expired`), so nothing trades in
/// the gap.
fn required_futures_settles(db: &Database, height: u64) -> Vec<DerivId> {
    if height == 0 {
        return Vec::new();
    }
    let mut required: Vec<DerivId> = FuturesMarkets::new(db)
        .list()
        .iter()
        .filter(|m| {
            m.final_settlement_px_q8.is_none()
                && height >= m.expiry_height
                && settlement_price_available(db, m)
        })
        .map(|m| m.id)
        .collect();
    required.sort();
    required
}

/// Every system transaction the block at `height` must carry, given the
/// state at its parent: one `FundingTick` per perpetual at a funding
/// boundary, one `FuturesSettle` per settleable expired future. In the order
/// the enforcement checks name them.
///
/// This is the one description of the rule. The leader's `build_and_apply`
/// and the replica's `apply_remote_block` both enforce it through
/// [`enforce_required_funding_ticks`] and [`enforce_required_futures_settle`],
/// and the sequencer *produces* the transactions from the same list — so the
/// node that must include them and the nodes that reject blocks without them
/// cannot disagree about what "them" is. Before this function existed nothing
/// on a live node built either transaction at all, and the chain halted at
/// its first funding boundary.
pub fn required_system_txs(db: &Database, height: u64) -> Vec<TxBody> {
    let mut out: Vec<TxBody> = required_funding_ticks(db, height)
        .into_iter()
        .map(|market| TxBody::FundingTick { market })
        .collect();
    out.extend(
        required_futures_settles(db, height)
            .into_iter()
            .map(|market| TxBody::FuturesSettle { market }),
    );
    out
}

/// Validate that the block contains a `FundingTick` system tx for every perp
/// market whose `funding_interval_blocks` boundary lands on `height`. The tick
/// must be signed by `proposer_addr` (followers will reject otherwise — same
/// rule as `apply_funding_tick`).
///
/// The required-tick set is a deterministic function of height + market state
/// at this block's parent — every replica computes the same set and rejects
/// the same blocks.
pub fn enforce_required_funding_ticks(
    db: &Database,
    height: u64,
    proposer_addr: Address,
    txs: &[VerifiedTx],
) -> Result<(), ExecError> {
    let required = required_funding_ticks(db, height);
    if required.is_empty() {
        return Ok(());
    }
    // Build a present-set keyed by market for O(1) membership check.
    use std::collections::HashSet;
    let present: HashSet<DerivId> = txs
        .iter()
        .filter_map(|v| match &v.signed.tx.body {
            TxBody::FundingTick { market } if v.sender == proposer_addr => Some(*market),
            _ => None,
        })
        .collect();

    for m in required {
        if !present.contains(&m) {
            return Err(ExecError::MissingRequiredFundingTick { height, market: m });
        }
    }
    Ok(())
}

/// Validate that the block contains a `FuturesSettle` system tx for every
/// dated future that [`required_system_txs`] says must settle at `height`.
/// Mirror of `enforce_required_funding_ticks`; already-settled markets are
/// never required, since settlement is final.
pub fn enforce_required_futures_settle(
    db: &Database,
    height: u64,
    proposer_addr: Address,
    txs: &[VerifiedTx],
) -> Result<(), ExecError> {
    let required = required_futures_settles(db, height);
    if required.is_empty() {
        return Ok(());
    }
    use std::collections::HashSet;
    let present: HashSet<DerivId> = txs
        .iter()
        .filter_map(|v| match &v.signed.tx.body {
            TxBody::FuturesSettle { market } if v.sender == proposer_addr => Some(*market),
            _ => None,
        })
        .collect();

    for m in required {
        if !present.contains(&m) {
            return Err(ExecError::MissingRequiredFuturesSettle { height, market: m });
        }
    }
    Ok(())
}
