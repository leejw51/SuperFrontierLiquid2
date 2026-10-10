//! Replica loop: connect to sequencer, stream blocks, persist + vote.
//!
//! Two modes are supported, selected at startup:
//!
//! * **strict-sync** (`--strict-sync`): decode every block, recover senders if
//!   the leader didn't pre-fill them, re-execute the tx list, recompute the
//!   `state_root`, and only then accept and persist. Vote submission is
//!   awaited synchronously before pulling the next block. This is the
//!   pre-existing behavior and matches `cb-net::apply_remote_block`.
//!
//! * **fast-mirror** (default): treat the replica as a block-log backup.
//!   Pull pre-encoded Cap'n Proto block bytes via streamBlocksRaw and write
//!   them, plus a head pointer, atomically into `CF_MIRROR_BLOCKS` —
//!   nothing else. No deep decode, no apply, no `state_root` recompute, no
//!   QC tracking, no sync vote roundtrip. We keep N requests in flight on
//!   the capnp-rpc stream so the receive loop never stalls waiting on a
//!   single round-trip. Vote attests "received" not "validated" — the
//!   trade-off the operator opts into when running with the default flags.
//!
//! With a 3-validator cluster and quorum=2, exactly one replica vote (on top
//! of the leader's self-vote) is enough to form a QC for that height in
//! either mode.

use crate::failover::{decide, Decision, ViewTimer};
use cb_block::ExecCtx;
use cb_consensus::{vote_signing_bytes, Vote};
use cb_crypto::{address_from_pubkey, sign, PublicKey};
use cb_dashboard::Metrics;
use cb_net::apply_remote_block;
use cb_rpc::{connect, Client};
use cb_state::{Database, Meta, MirrorBlocks};
use cb_types::{Address, H256};
use secp256k1::SecretKey;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

/// One bounded log line per this many blocks in fast-mirror mode. The fast
/// path runs at hundreds of blocks per second, so per-block logging would
/// itself become a meaningful cost (and noise).
const FAST_MIRROR_LOG_EVERY: u64 = 100;

/// Number of `next_request()` promises kept in flight on the fast-mirror
/// stream. Capnp-rpc multiplexes them on the single TCP connection and
/// returns each reply as soon as the server completes it, so a small window
/// hides the round-trip latency behind the per-block work the replica is
/// already doing.
const FAST_MIRROR_PIPELINE: usize = 4;

/// Ask the leader whether it is there. One point read on its side.
async fn leader_answers(client: &Client) -> bool {
    client.cap.chain_info_request().send().promise.await.is_ok()
}

/// Turn "the leader is gone" into an exit: take over if this node leads the
/// next view and it is safe, otherwise report why not.
fn finish(db: &Database, me: Address, otherwise: Exit) -> anyhow::Result<Exit> {
    match decide(db, me, true) {
        Decision::Promote { view, head } => {
            tracing::warn!(
                view,
                head,
                "the leader is gone and this node leads the next view; taking over"
            );
            Ok(Exit::Promote { view })
        }
        Decision::Stay(reason) => {
            tracing::warn!(?reason, "the leader is gone; this node is not taking over");
            Ok(otherwise)
        }
    }
}

/// What a follower needs in order to take over when the leader stops.
#[derive(Clone, Copy, Debug)]
pub struct FailoverCfg {
    /// This node's validator address.
    pub me: cb_types::Address,
    /// No block applied for this long means the leader is gone.
    pub silence: std::time::Duration,
}

/// Keep a replica's stored blocks to the same window the leader keeps.
///
/// A replica used to keep every block it ever received, whichever mode it ran
/// in, while the node producing them pruned. On a chain under load that is
/// the follower filling a disk the leader deliberately does not fill. Bodies
/// are the archive; state, the tries and the head are what answer questions
/// about the present, and none of those is pruned.
pub(crate) fn prune_replica(db: &Database, head: u64, keep_recent: Option<u64>, strict: bool) {
    let Some(keep) = keep_recent else { return };
    if strict {
        crate::sequencer::prune_one(db, head, keep);
        return;
    }
    // Fast mirror stores bodies in its own column family, and stores nothing
    // else, so the same window applies there.
    let Some(cutoff) = head.checked_sub(keep) else {
        return;
    };
    if cutoff < 1 {
        return;
    }
    if let Some(lowest) = MirrorBlocks::new(db).lowest() {
        if lowest <= cutoff {
            MirrorBlocks::new(db).prune_at(lowest);
        }
    }
}

/// Why `run_replica` returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The block stream ended and this node is not taking over.
    StreamEnded,
    /// The leader went silent and this node leads the next view. The caller
    /// starts the sequencer on the same database: the view is already
    /// recorded, so every block it signs checks out against
    /// `validators[view % N]` on every follower.
    Promote { view: u64 },
}

#[allow(clippy::too_many_arguments)]
pub async fn run_replica(
    db: Database,
    ctx: ExecCtx,
    sequencer: SocketAddr,
    voter_key: Option<Arc<SecretKey>>,
    metrics: Option<Arc<Metrics>>,
    strict_sync: bool,
    _async_verify: bool,
    keep_blocks: Option<u64>,
    failover: Option<FailoverCfg>,
    peers: Vec<SocketAddr>,
) -> anyhow::Result<Exit> {
    // Who to follow, in order: the leader this node was started against, then
    // whatever other validators it was told about. A follower that cannot take
    // over must still end up behind whoever did, and the address of the new
    // leader is the one thing the validator set does not record — it holds
    // identities, not endpoints. `--peer` is that mapping.
    let mut candidates: Vec<SocketAddr> = std::iter::once(sequencer).chain(peers).collect();
    candidates.dedup();
    let mut attempt = 0usize;
    loop {
        let target = candidates[attempt % candidates.len()];
        attempt += 1;
        match follow(
            db.clone(),
            ctx,
            target,
            voter_key.clone(),
            metrics.clone(),
            strict_sync,
            keep_blocks,
            failover,
        )
        .await
        {
            Ok(Exit::Promote { view }) => return Ok(Exit::Promote { view }),
            Ok(Exit::StreamEnded) | Err(_) => {
                // Nothing to follow here. Try the next candidate, after a
                // pause so a cluster that is entirely down is not spun on.
                if candidates.len() == 1 {
                    return Ok(Exit::StreamEnded);
                }
                tracing::info!(%target, "no blocks from this address; trying the next validator");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn follow(
    db: Database,
    ctx: ExecCtx,
    sequencer: SocketAddr,
    voter_key: Option<Arc<SecretKey>>,
    metrics: Option<Arc<Metrics>>,
    strict_sync: bool,
    keep_blocks: Option<u64>,
    failover: Option<FailoverCfg>,
) -> anyhow::Result<Exit> {
    let client = connect(sequencer).await?;
    // In fast-mirror mode the head pointer lives in CF_META written by
    // MirrorBlocks::put_with_meta, so Meta::head_height() is still the right
    // resume cursor.
    let head = Meta::new(&db).head_height();
    let from = head + 1;

    let voter_addr = voter_key.as_ref().map(|sk| {
        let pk = PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), sk);
        address_from_pubkey(&pk)
    });
    if let Some(addr) = voter_addr {
        tracing::info!(%addr, mode = if strict_sync { "strict-sync" } else { "fast-mirror" }, "replica voter identity loaded");
    }

    // Catch up on whatever a previous run left outside the window.
    if let Some(keep) = keep_blocks {
        if strict_sync {
            crate::sequencer::startup_prune(&db, keep);
        }
    }

    if strict_sync {
        let mut req = client.cap.stream_blocks_request();
        req.get().set_from_height(from);
        let res = req.send().promise.await?;
        let sub = res.get()?.get_sub()?;
        run_strict_sync(
            db,
            ctx,
            client,
            sub,
            voter_key,
            voter_addr,
            metrics,
            keep_blocks,
            failover,
        )
        .await
    } else {
        let mut req = client.cap.stream_blocks_raw_request();
        req.get().set_from_height(from);
        let res = req.send().promise.await?;
        let sub = res.get()?.get_sub()?;
        // A mirror holds block bytes and no state, so it has nothing to
        // build block N+1 on and can never take over. Failover needs
        // `--strict-sync`.
        if failover.is_some() {
            tracing::warn!(
                "--failover-after has no effect in fast-mirror mode: a mirror keeps no state to lead from. Use --strict-sync."
            );
        }
        run_fast_mirror(db, client, sub, voter_key, voter_addr, metrics, keep_blocks).await?;
        Ok(Exit::StreamEnded)
    }
}

/// Pre-existing replica behavior: decode, re-execute, verify state_root,
/// persist, and synchronously await each vote ack. Untouched semantically;
/// the body was lifted from the original `run_replica` so the strict path
/// remains a verbatim alternative covered by the existing tests.
#[allow(clippy::too_many_arguments)]
async fn run_strict_sync(
    db: Database,
    ctx: ExecCtx,
    client: Client,
    sub: cb_schema::rpc_capnp::block_sub::Client,
    voter_key: Option<Arc<SecretKey>>,
    voter_addr: Option<Address>,
    metrics: Option<Arc<Metrics>>,
    keep_blocks: Option<u64>,
    failover: Option<FailoverCfg>,
) -> anyhow::Result<Exit> {
    let mut timer = failover
        .as_ref()
        .map(|f| ViewTimer::new(f.silence, Meta::new(&db).head_height()));
    loop {
        // Wait for the next block, but not for ever: a leader that has
        // stopped never sends one, and the whole point is to notice. On a
        // timeout, ask whether this node is the validator that leads next and
        // whether it is safe for it to build.
        // Wait for the next block, but not for ever.
        //
        // Silence is not death. A chain with nothing to do produces no
        // blocks — the builder refuses to make an empty one — so a quiet
        // leader looks exactly like a stopped one from here. The first
        // version of this took over from an idle leader within five seconds
        // of starting, which is worse than not having failover at all.
        //
        // So the timeout only asks the question; the leader answers it. A
        // `chainInfo` round trip is one point read on the leader and costs
        // nothing at this cadence. If it answers, the leader is alive and
        // idle: reset and keep waiting. If the round trip fails — or the
        // block stream itself broke, which is what a crashed process looks
        // like first — then the leader is gone and the decision is made.
        //
        // What this does not catch: a leader whose process is alive and
        // answering RPC but which has stopped producing blocks. That needs
        // the followers to agree among themselves, and this does not do it.
        let next = match (&mut timer, &failover) {
            (Some(t), Some(f)) => {
                match tokio::time::timeout(t.silence(), sub.next_request().send().promise).await {
                    Ok(Ok(r)) => r,
                    // The stream broke: the leader's process is gone.
                    Ok(Err(e)) => {
                        tracing::warn!("block stream ended: {e}");
                        return finish(&db, f.me, Exit::StreamEnded);
                    }
                    Err(_) => {
                        if leader_answers(&client).await {
                            tracing::debug!(
                                silence_ms = t.silence().as_millis() as u64,
                                "no block, but the leader answers; it is idle, not gone"
                            );
                            t.saw_leader();
                            continue;
                        }
                        tracing::warn!("the leader stopped answering");
                        return finish(&db, f.me, Exit::StreamEnded);
                    }
                }
            }
            _ => sub.next_request().send().promise.await?,
        };
        let block_reader = next.get()?.get_block()?;
        let block = cb_rpc::conv::block_from_capnp(block_reader)?;
        let height = block.header.height;
        let view = block.header.view;
        let block_hash = block.header.hash();
        let tx_count = block.txs.len() as u64;

        // Strict-sync decodes the block, so the order subset is known exactly.
        let order_count = block
            .txs
            .iter()
            .filter(|t| t.tx.body.is_order_placement())
            .count() as u64;

        apply_remote_block(&db, &ctx, &block)?;
        prune_replica(&db, height, keep_blocks, true);
        if let Some(t) = &mut timer {
            t.saw_block(height);
        }
        tracing::info!(height, "replica applied block");
        if let Some(m) = &metrics {
            for (side, market) in block
                .txs
                .iter()
                .filter_map(|t| t.tx.body.order_side_and_market())
            {
                m.record_applied_order(side, market);
            }
            m.record_block(tx_count, order_count);
        }

        if let (Some(sk), Some(addr)) = (voter_key.as_ref(), voter_addr) {
            let preimage = H256(vote_signing_bytes(view, height, block_hash));
            let sig = sign(sk, preimage)?;
            let vote = Vote {
                view,
                height,
                block_hash,
                validator: addr,
                sig: sig.to_vec(),
            };
            match client.submit_vote(&vote).await {
                Ok(cb_rpc::VoteAccepted::Stored) => {
                    tracing::debug!(height, "vote accepted by leader");
                }
                Ok(cb_rpc::VoteAccepted::QuorumFormed) => {
                    tracing::info!(height, "vote pushed leader to quorum");
                }
                Err(e) => {
                    tracing::warn!(height, "vote rejected: {e}");
                }
            }
        }
    }
}

/// Block-log mirror loop. Pull pre-encoded Cap'n Proto block bytes via
/// streamBlocksRaw and persist them, alongside the head pointer, in a single
/// atomic write to `CF_MIRROR_BLOCKS` + `CF_META`. We keep
/// `FAST_MIRROR_PIPELINE` requests in flight so the per-block work runs in
/// parallel with the next round-trip. Strict-sync's tracked artifacts —
/// `state_root`, parent_qc, the bincode-keyed `CF_BLOCKS`, the hash index —
/// are intentionally not maintained here. Replica is a backup for the four
/// fields the operator named: height, hash, blocktime, raw block bytes.
#[allow(clippy::too_many_arguments)]
async fn run_fast_mirror(
    db: Database,
    client: Client,
    sub: cb_schema::rpc_capnp::block_bytes_sub::Client,
    voter_key: Option<Arc<SecretKey>>,
    voter_addr: Option<Address>,
    metrics: Option<Arc<Metrics>>,
    keep_blocks: Option<u64>,
) -> anyhow::Result<()> {
    // Wrap the client in Rc so the fire-and-forget vote tasks can each hold
    // a clone for the duration of their submit_vote roundtrip without us
    // having to keep ownership in the loop.
    let client = Rc::new(client);

    // Prime the pipeline. capnp-rpc multiplexes these on the existing TCP
    // stream; we collect promises into a FIFO and await them in submission
    // order so the mirror writes stay strictly height-ordered.
    let mut in_flight: VecDeque<_> = VecDeque::with_capacity(FAST_MIRROR_PIPELINE);
    for _ in 0..FAST_MIRROR_PIPELINE {
        in_flight.push_back(sub.next_request().send());
    }

    loop {
        let pending = in_flight.pop_front().expect("pipeline drained");
        // Top up immediately so the server can start producing the next
        // block while we process this one.
        in_flight.push_back(sub.next_request().send());

        let next = pending.promise.await?;
        let env = next.get()?.get_env()?;

        let height = env.get_height();
        let view = env.get_view();
        let blocktime = env.get_blocktime();
        let tx_count = env.get_tx_count() as u64;

        let hash_bytes = env.get_hash()?;
        if hash_bytes.len() != 32 {
            anyhow::bail!("envelope hash wrong length: {}", hash_bytes.len());
        }
        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(hash_bytes);
        let block_hash = H256(hash_arr);

        // The four fields the operator asked for: height, hash, blocktime,
        // capnp bytes. Single WriteBatch covering CF_MIRROR_BLOCKS +
        // CF_META so a crash leaves the node at either height or
        // height-1 — never half-applied.
        MirrorBlocks::new(&db).put_with_meta(
            height,
            block_hash,
            blocktime,
            env.get_bytes()?,
            block_hash,
        )?;
        prune_replica(&db, height, keep_blocks, false);

        if let Some(m) = &metrics {
            // Fast-mirror stores the leader's bytes verbatim and never decodes
            // the block, so it cannot tell an order from a transfer without
            // paying for the decode this path exists to avoid. It reports zero
            // orders rather than guessing; a mirroring replica's order counter
            // is therefore always zero, which is the truthful answer here.
            m.record_block(tx_count, 0);
        }

        if height % FAST_MIRROR_LOG_EVERY == 0 {
            tracing::info!(height, tx_count, "replica mirrored block");
        }

        if let (Some(sk), Some(addr)) = (voter_key.as_ref(), voter_addr) {
            // Sign inline (one ECDSA — cheap). Submit asynchronously so
            // the receive loop can pull the next block while the leader's
            // ack is in flight.
            let preimage = H256(vote_signing_bytes(view, height, block_hash));
            let sig = sign(sk, preimage)?;
            let vote = Vote {
                view,
                height,
                block_hash,
                validator: addr,
                sig: sig.to_vec(),
            };
            let client = client.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = client.submit_vote(&vote).await {
                    tracing::warn!(height, "vote submission failed: {e}");
                }
            });
        }
    }
}
