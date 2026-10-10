//! Replica syncer: receive a block, verify the proposer signature, re-execute, verify
//! state_root, and adopt any parent QuorumCert it carries as proof of the previous
//! block's finality.

use cb_block::{Block, ExecCtx};
use cb_consensus::bft::{Finality, ValidatorSet};
use cb_consensus::{quorum_count, QuorumCert};
use cb_crypto::{keccak256, recover, Signature};
use cb_state::{Database, Meta, Qcs, Validators};
use cb_tx::VerifiedTx;
use cb_types::{Address, H256};
use rayon::prelude::*;
use thiserror::Error;

/// Why a block handed to [`apply_remote_block`] was refused.
///
/// Every variant is a halt: partial state is never persisted, so a node that
/// returns one of these is exactly where it was before the block arrived.
#[derive(Debug, Error)]
pub enum SyncError {
    /// Re-execution produced a different state root than the header claims.
    /// This is the check the whole design exists to preserve.
    #[error("state root mismatch at height {h}: expected {expected}, got {got}")]
    StateRootMismatch {
        /// Height of the offending block.
        h: u64,
        /// The root the header claimed.
        expected: H256,
        /// The root re-execution actually produced.
        got: H256,
    },
    /// Under BFT: the block's `applied` mask does not match what this node's
    /// re-execution found. The state root is not enough here — the mask is
    /// not in it — and readers trust the mask to tell refused from applied.
    #[error("applied mask at height {h} differs from re-execution")]
    AppliedMaskMismatch {
        /// Height of the offending block.
        h: u64,
    },
    /// A transaction in the block failed to apply.
    #[error("apply error at height {h}: {err:?}")]
    Apply {
        /// Height of the offending block.
        h: u64,
        /// The executor's complaint.
        err: cb_block::ExecError,
    },
    /// The block does not sit at `head + 1`.
    #[error("non-monotonic height: head {head}, incoming {incoming}")]
    NonMonotonic {
        /// The local head.
        head: u64,
        /// The height the block claimed.
        incoming: u64,
    },
    /// A transaction's signature did not recover.
    #[error("bad tx signature in block at height {h}")]
    BadSignature {
        /// Height of the offending block.
        h: u64,
    },
    /// The proposer signature was not 65 well-formed bytes.
    #[error("malformed proposer signature at height {h}: {reason}")]
    MalformedProposerSig {
        /// Height of the offending block.
        h: u64,
        /// What the crypto layer said.
        reason: String,
    },
    /// The proposer signature recovered to someone other than the validator
    /// entitled to lead this view.
    #[error("proposer mismatch at height {h}, view {view}: expected {expected}, got {got}")]
    WrongProposer {
        /// Height of the offending block.
        h: u64,
        /// The view the header claims to be proposed in.
        view: u64,
        /// The validator entitled to lead that view.
        expected: Address,
        /// Who actually signed.
        got: Address,
    },
    /// The block's `prev_hash` is not the hash of our head.
    #[error("wrong parent at height {h}: header links to {claimed}, head is {head}")]
    WrongParent {
        /// Height of the offending block.
        h: u64,
        /// The parent hash the header claims.
        claimed: H256,
        /// The hash of the block this node actually holds at `h - 1`.
        head: H256,
    },
    /// The header's timestamp is earlier than the head's. Never a wall-clock
    /// bound — replay must stay deterministic — only "does not run backwards".
    #[error("timestamp regressed at height {h}: header says {claimed}, head is {head}")]
    TimestampRegressed {
        /// Height of the offending block.
        h: u64,
        /// The timestamp the header claims.
        claimed: u64,
        /// The head block's timestamp.
        head: u64,
    },
    /// The transactions in the body do not hash to the root in the header.
    #[error("tx root mismatch at height {h}: header says {expected}, body hashes to {got}")]
    TxRootMismatch {
        /// Height of the offending block.
        h: u64,
        /// The root the header claims.
        expected: H256,
        /// The root the body actually hashes to.
        got: H256,
    },
    /// A transaction's signature recovers to an address other than the one
    /// the leader listed for it. This is the check that makes re-execution
    /// mean something: without it a leader can attribute a transaction it
    /// signed itself to any account, and every replica re-executes the
    /// theft, reproduces the root and votes for it.
    #[error("sender mismatch at height {h}, tx {index}: leader says {claimed}, signature is {recovered}")]
    SenderMismatch {
        /// Height of the offending block.
        h: u64,
        /// Index of the transaction in the block.
        index: usize,
        /// The sender the leader supplied.
        claimed: Address,
        /// The sender the signature actually recovers to.
        recovered: Address,
    },
    /// The pre-recovered sender list does not line up with the transactions.
    #[error("senders len {got} != txs len {expected} at height {h}")]
    SendersMismatch {
        /// Height of the offending block.
        h: u64,
        /// Number of transactions.
        expected: usize,
        /// Number of senders supplied.
        got: usize,
    },
    /// The database refused a write.
    #[error("storage: {0}")]
    Storage(String),
    /// Under BFT: the header's finality proof is missing, malformed, not a
    /// quorum of the validator set, or not for this block.
    #[error("finality proof rejected at height {h}: {reason}")]
    BadFinality {
        /// Height of the offending block.
        h: u64,
        /// What was wrong with it.
        reason: String,
    },
}

/// Check a BFT block's finality proof against the validator set: the
/// proposal it carries was committed by a quorum, and this block is that
/// proposal's payload executed.
fn verify_finality(db: &Database, header: &cb_block::BlockHeader) -> Result<Finality, SyncError> {
    let h = header.height;
    let bad = |reason: String| SyncError::BadFinality { h, reason };
    if header.parent_qc.is_empty() {
        return Err(bad("no finality proof in the header".into()));
    }
    let fin = Finality::decode(&header.parent_qc).map_err(|e| bad(e.to_string()))?;
    let addrs: Vec<Address> = Validators::new(db)
        .list()
        .into_iter()
        .map(|v| v.addr)
        .collect();
    let set = ValidatorSet::new(addrs).map_err(|e| bad(e.to_string()))?;
    let genesis = crate::p2p::identity::GenesisId::from_db(db).0;
    fin.verify(&set, genesis).map_err(|e| bad(e.to_string()))?;
    if fin.header.round != header.view {
        return Err(bad(format!(
            "proposal round {} but header view {}",
            fin.header.round, header.view
        )));
    }
    if fin.header.payload_hash != header.tx_root {
        return Err(bad(
            "proposal payload root differs from the block's tx root".into(),
        ));
    }
    if header.timestamp != fin.header.timestamp {
        return Err(bad("block timestamp differs from its proposal's".into()));
    }
    if header.sequencer_sig != fin.header.sig {
        return Err(bad("header signature is not the proposal's".into()));
    }
    // The proof must be for this height and extend the block before it. A
    // certificate says only that a quorum voted for a proposal, not where
    // on the chain it sits, and a replayed payload always executes cleanly
    // because a failing transaction is skipped rather than refusing the
    // block: without these checks a peer could serve block 1's proposal
    // again as the next block, and the node would land it and be stuck on
    // a fork of its own for good.
    if fin.header.height != h {
        return Err(bad(format!(
            "proof is for height {} but the block is {h}",
            fin.header.height
        )));
    }
    if let Some(prev) = previous_proposal(db, h) {
        if fin.header.round <= prev.round {
            return Err(bad(format!(
                "proposal round {} does not follow the previous block's {}",
                fin.header.round, prev.round
            )));
        }
        // The parent sits at or after the previous block's proposal on the
        // committed chain. At its round it must be that proposal; after it,
        // it is one of the empty proposals in between, which carry no block
        // and so cannot be checked here.
        let parent_round = fin.header.qc.round;
        if parent_round < prev.round
            || (parent_round == prev.round && fin.header.parent_id != prev.id())
        {
            return Err(bad(
                "proof does not extend the previous block's proposal".into()
            ));
        }
    }
    Ok(fin)
}

/// The proposal behind the block before `h`, when that block exists and
/// carries a finality proof (genesis does not).
fn previous_proposal(db: &Database, h: u64) -> Option<cb_consensus::bft::ProposalHeader> {
    let prev = h.checked_sub(1).filter(|p| *p > 0)?;
    let bytes = cb_state::Blocks::new(db).get(prev)?;
    let block = Block::decode(&bytes).ok()?;
    if block.header.parent_qc.is_empty() {
        return None;
    }
    Finality::decode(&block.header.parent_qc)
        .ok()
        .map(|f| f.header)
}

/// Apply a block produced upstream by the leader to the local replica state.
///
/// Verifies in order: monotonic height, proposer signature recovers to the address
/// recorded in `Meta::proposer_addr` (the current leader for this view), each tx
/// signature OR adoption of the leader's pre-recovered senders (when the block
/// carries them — much faster), that re-executing produces the claimed
/// `state_root`. Halts (returns `Err`) on any check failure — partial state is
/// never persisted.
///
/// Skip-recovery mode: when `block.senders` is non-empty, replicas trust the
/// leader's recovered senders and bypass ECDSA on the hot path. State_root is
/// still recomputed and compared, so any sender-vs-tx inconsistency that would
/// shift balances will be caught by the divergent state_root. This is the same
/// shape Hyperliquid uses for its non-validator nodes.
pub fn apply_remote_block(db: &Database, ctx: &ExecCtx, block: &Block) -> Result<(), SyncError> {
    let head = Meta::new(db).head_height();
    if block.header.height != head + 1 {
        return Err(SyncError::NonMonotonic {
            head,
            incoming: block.header.height,
        });
    }

    // Who is entitled to lead this block's view.
    //
    // # Why the view and not a fixed address
    //
    // This used to be `Meta::proposer_addr()`, the single address genesis
    // names. That is correct only while one node leads for ever. The moment
    // another validator takes over — because the leader crashed, which on a
    // decentralised chain is the ordinary case and not the exception — every
    // replica would refuse its blocks, and a chain whose replicas refuse the
    // new leader has not failed over, it has split. `build_and_apply`'s own
    // documentation already said the leader is `validators[view % N]`, and
    // the header has carried its `view` all along; only this check disagreed.
    //
    // A chain with no validator set registered — a single-node dev chain, and
    // every fixture that never called `Validators::put_set` — keeps the old
    // rule, because with no set there is no rotation to speak of.
    let view = block.header.view;
    let bft = Meta::new(db).is_bft();
    // Under BFT the header is not signed as a header at all. What makes it
    // valid is the finality proof in its `parent_qc` slot: the committed
    // proposal (signed by the round's leader) and the quorum certificate for
    // it. The block's transactions are the proposal's payload, so the root
    // the proposal names must be the header's, and execution takes its seed
    // and proposer from the proposal rather than from this node.
    let (expected, origin) = if bft {
        let fin = verify_finality(db, &block.header)?;
        if fin.header.payload_len as usize != block.txs.len() {
            return Err(SyncError::BadFinality {
                h: block.header.height,
                reason: format!(
                    "proposal has {} transactions, block has {}",
                    fin.header.payload_len,
                    block.txs.len()
                ),
            });
        }
        (
            fin.header.proposer,
            Some(cb_block::BlockOrigin {
                seed: fin.header.parent_id,
                proposer: fin.header.proposer,
            }),
        )
    } else {
        let expected = Validators::new(db)
            .leader_for_view(view)
            .map(|v| v.addr)
            .unwrap_or_else(|| Meta::new(db).proposer_addr());
        let sig = Signature::from_bytes(&block.header.sequencer_sig).map_err(|e| {
            SyncError::MalformedProposerSig {
                h: block.header.height,
                reason: e.to_string(),
            }
        })?;
        let preimage = block.header.signing_bytes();
        let digest = keccak256(&preimage);
        let recovered = recover(&sig, digest).map_err(|e| SyncError::MalformedProposerSig {
            h: block.header.height,
            reason: e.to_string(),
        })?;
        if recovered != expected {
            return Err(SyncError::WrongProposer {
                h: block.header.height,
                view,
                expected,
                got: recovered,
            });
        }
        (expected, None)
    };

    // The header must extend the block this node holds, and the body must be
    // the one the header commits to. Neither used to be checked: a replica
    // compared height, proposer signature and state root, and trusted that
    // the transactions it was about to execute were the ones the signed root
    // named and that the chain of hashes it was storing linked up. The state
    // root catches most consequences of either, but it is the wrong check to
    // be relying on for them, and these cost nothing.
    let head_hash = Meta::new(db).head_hash();
    if block.header.prev_hash != head_hash {
        return Err(SyncError::WrongParent {
            h: block.header.height,
            claimed: block.header.prev_hash,
            head: head_hash,
        });
    }
    let head_timestamp = Meta::new(db).head_timestamp();
    if block.header.timestamp < head_timestamp {
        return Err(SyncError::TimestampRegressed {
            h: block.header.height,
            claimed: block.header.timestamp,
            head: head_timestamp,
        });
    }
    let tx_root = Block::compute_tx_root(&block.txs);
    if block.header.tx_root != tx_root {
        return Err(SyncError::TxRootMismatch {
            h: block.header.height,
            expected: block.header.tx_root,
            got: tx_root,
        });
    }

    let use_senders = !block.senders.is_empty();
    if use_senders && block.senders.len() != block.txs.len() {
        return Err(SyncError::SendersMismatch {
            h: block.header.height,
            expected: block.txs.len(),
            got: block.senders.len(),
        });
    }

    // Recover every sender, in parallel, unconditionally.
    //
    // The leader ships its own recovered `senders` alongside the body, and an
    // earlier version of this function used them verbatim whenever they were
    // present — which is every block the current leader produces. That made
    // re-execution certify the leader's arithmetic and nothing else: a leader
    // could sign a transaction with its own key, list any victim as its
    // sender, debit the victim, and every replica would re-execute the same
    // debit, reproduce the same root and vote for it. Recovery is what makes
    // "verified" true. The cost is one ECDSA per transaction, spread across
    // cores; the leader's list is still useful, as the prefetch hint below and
    // as a cross-check that names the exact transaction when it lies.
    let h = block.header.height;
    let recovered: Result<Vec<Address>, SyncError> = block
        .txs
        .par_iter()
        .map(|s| s.sender().map_err(|_| SyncError::BadSignature { h }))
        .collect();
    let recovered_senders: Vec<Address> = recovered?;
    if use_senders {
        for (index, (claimed, actual)) in block
            .senders
            .iter()
            .zip(recovered_senders.iter())
            .enumerate()
        {
            if claimed != actual {
                return Err(SyncError::SenderMismatch {
                    h,
                    index,
                    claimed: *claimed,
                    recovered: *actual,
                });
            }
        }
    }

    // Per-block ExecCtx: replicas use the same `block_height` and
    // `proposer_addr` the leader saw. `proposer_addr` is the address recorded
    // in `Meta::proposer_addr` (the current leader for this view), which we
    // already verified the block is signed by.
    let block_ctx = ExecCtx {
        treasury_signer: ctx.treasury_signer,
        block_height: block.header.height,
        proposer_addr: expected,
    };

    // Prefetch every sender + Transfer-recipient address in parallel via rayon
    // so the serial apply loop below hits the in-memory cache for every read
    // instead of doing a synchronous RocksDB get on first touch. On large blocks
    // this turns ~2N serial I/Os into a parallel batch.
    let touched: Vec<Address> = {
        use cb_tx::TxBody;
        let mut touched: Vec<Address> = Vec::with_capacity(block.txs.len() * 2);
        for (i, s) in block.txs.iter().enumerate() {
            let sender = recovered_senders[i];
            touched.push(sender);
            if let TxBody::Transfer { to, .. } = &s.tx.body {
                touched.push(*to);
            }
        }
        touched
    };

    // Materialize VerifiedTx up-front. We need them once for the funding-tick
    // boundary check below and again in the apply loop; building them once
    // is cheap (no ECDSA — senders are already recovered).
    //
    // Under BFT the required system transactions are not in the block: every
    // validator injected them at execution, and so does this replay.
    let mut verifieds: Vec<VerifiedTx> = match origin {
        Some(o) => cb_block::synthetic_system_txs(
            db,
            block.header.height,
            o.proposer,
            Meta::new(db).chain_id(),
        ),
        None => Vec::new(),
    };
    verifieds.extend(
        block
            .txs
            .iter()
            .enumerate()
            .map(|(i, s)| VerifiedTx::from_parts_unchecked(s.clone(), recovered_senders[i])),
    );

    // Block-level system-transaction enforcement: replicas reject blocks that
    // omit a required funding tick or a required settlement (mirror of
    // `build_and_apply`). The settlement half used to be missing here, so a
    // replica would adopt a block the leader could not have produced.
    cb_block::enforce_required_funding_ticks(db, block.header.height, expected, &verifieds)
        .map_err(|err| SyncError::Apply {
            h: block.header.height,
            err,
        })?;
    cb_block::enforce_required_futures_settle(db, block.header.height, expected, &verifieds)
        .map_err(|err| SyncError::Apply {
            h: block.header.height,
            err,
        })?;

    // Re-execute, compare the state root, and commit — in that order, in one
    // write. `verify_and_commit` runs the whole block on a staged handle, so
    // the comparison below happens while every mutation is still in memory: a
    // block that does not reproduce its root leaves this database byte-for-byte
    // as it was, which is what the module header above has always promised.
    // State, block bytes and the head pointer then land in a single
    // `WriteBatch`, so a crash mid-commit cannot desynchronise them.
    //
    // Note the `tx_index`: replicas now apply with the transaction's real
    // position in the block, exactly as the leader did. Applying every
    // transaction at index 0 (the old `apply_tx` call) gave two resting perp
    // orders in one block the same derived `seq` on a replica and different
    // ones on the leader.
    let bytes = block.encode();
    // The root comes back for callers that want it; the header's own root has
    // already been compared inside, so nothing here needs the value.
    let _state_root = cb_block::verify_and_commit_with(
        db,
        &block_ctx,
        &verifieds,
        block.header.height,
        block.header.hash(),
        &bytes,
        block.header.state_root,
        block.header.timestamp,
        &touched,
        bft,
        Some(&block.applied),
    )
    .map_err(|e| match e {
        cb_block::VerifyError::Apply(err) => SyncError::Apply {
            h: block.header.height,
            err,
        },
        cb_block::VerifyError::Storage(msg) => SyncError::Storage(msg),
        cb_block::VerifyError::StateRootMismatch { expected, got } => {
            SyncError::StateRootMismatch {
                h: block.header.height,
                expected,
                got,
            }
        }
        cb_block::VerifyError::AppliedMaskMismatch { h } => SyncError::AppliedMaskMismatch { h },
    })?;

    // If the leader embedded a parent QC, verify + persist it so this replica
    // can answer finality questions about prior heights without going back to
    // the leader. Treat verification failures as soft errors (log + continue)
    // — the block itself was already validated by the proposer signature path,
    // and rejecting it here would diverge this replica from a chain that the
    // leader and at least one other follower already accepted.
    // Keep this node's notion of the view in step with the chain. Only block
    // headers carry it, and a node that never leads would otherwise sit at
    // genesis's zero for ever — and then, on a view change, compute the next
    // leader from the wrong view and hand the chain to the wrong validator.
    if block.header.view > Meta::new(db).view() {
        Meta::new(db).set_view(block.header.view);
    }

    if !bft && !block.header.parent_qc.is_empty() {
        match QuorumCert::decode(&block.header.parent_qc) {
            Ok(qc) => {
                let validator_set: Vec<Address> = Validators::new(db)
                    .list()
                    .into_iter()
                    .map(|v| v.addr)
                    .collect();
                let q = quorum_count(validator_set.len());
                if let Err(e) = qc.verify(&validator_set, q) {
                    tracing::warn!(
                        h = block.header.height,
                        ?e,
                        "parent_qc embedded in header failed verification"
                    );
                } else {
                    Qcs::new(db).put(qc.height, &block.header.parent_qc);
                    if qc.height > Meta::new(db).last_qc_height() {
                        Meta::new(db).set_last_qc_height(qc.height);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    h = block.header.height,
                    ?e,
                    "could not decode parent_qc bytes"
                );
            }
        }
    }
    Ok(())
}
