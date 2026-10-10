//! Streaming capabilities (BlockSub, BlockBytesSub).

use crate::conv;
use capnp::capability::Promise;
use cb_block::{Block, RejectedTx};
use cb_schema::rpc_capnp;
use cb_state::{Blocks, Database, Meta};
use cb_types::{Address, H256};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Server impl for `BlockSub.next()`.
pub struct BlockSubServer {
    pub db: Database,
    pub cursor: Arc<Mutex<u64>>,
    pub notify: Arc<Notify>,
}

impl rpc_capnp::block_sub::Server for BlockSubServer {
    fn next(
        &mut self,
        _params: rpc_capnp::block_sub::NextParams,
        mut results: rpc_capnp::block_sub::NextResults,
    ) -> Promise<(), capnp::Error> {
        let db = self.db.clone();
        let cursor = self.cursor.clone();
        let notify = self.notify.clone();
        // A subscriber waiting here is not idle, however long the chain is
        // quiet: see `ServeConfig::idle_timeout`.
        let waiting = crate::client::Waiting::begin();
        Promise::from_future(async move {
            let _waiting = waiting;
            loop {
                let h = { *cursor.lock().unwrap() };
                // Read the head BEFORE the body. The block bytes and the head
                // pointer commit in one batch, so a body at or below the head
                // read here is either stored or pruned, never "not yet". The
                // other order raced: the body was read a moment before the
                // batch landed, the head a moment after, and a block that
                // existed was reported as pruned — which broke the gateway's
                // stream every few minutes at a lag of three blocks.
                let head = Meta::new(&db).head_height();
                if h <= head {
                    let Some(bytes) = Blocks::new(&db).get(h) else {
                        // Pruned out from under us: fail loudly, the same
                        // way the raw stream does. Waiting here left the
                        // gateway connected, healthy and permanently
                        // blockless whenever it asked for a height the node
                        // had already pruned.
                        return Err(capnp::Error::failed(format!(
                            "block at height {h} missing from Blocks CF (pruned?)"
                        )));
                    };
                    let block: Block = bincode::deserialize(&bytes)
                        .map_err(|e| capnp::Error::failed(format!("decode block: {e}")))?;
                    conv::block_to_capnp(results.get().init_block(), &block);
                    *cursor.lock().unwrap() = h + 1;
                    return Ok(());
                }
                // Not yet produced: wait for the next commit.
                let waiter = notify.notified();
                tokio::pin!(waiter);
                waiter.await;
            }
        })
    }
}

/// Server impl for `BlockBytesSub.next()` — the fast-mirror path.
///
/// The leader stores blocks as bincode in `CF_BLOCKS`; this server re-encodes
/// each one as Cap'n Proto on the way out and ships the framed capnp bytes in
/// the envelope's `bytes` field. The replica writes those bytes verbatim into
/// `CF_MIRROR_BLOCKS` without a deep decode. The cost of the capnp re-encode
/// lives on the leader by design — the replica side stays free of any
/// bincode/capnp transcoding work.
///
/// In addition to the bytes the envelope carries: `height`, `hash`,
/// `state_root`, `parent_qc`, `view`, `tx_count`, and the header `timestamp`
/// (as `blocktime`) so the replica can store the four fields the operator
/// asked for without paying for a parse.
pub struct BlockBytesSubServer {
    pub db: Database,
    pub cursor: Arc<Mutex<u64>>,
    pub notify: Arc<Notify>,
}

impl rpc_capnp::block_bytes_sub::Server for BlockBytesSubServer {
    fn next(
        &mut self,
        _params: rpc_capnp::block_bytes_sub::NextParams,
        mut results: rpc_capnp::block_bytes_sub::NextResults,
    ) -> Promise<(), capnp::Error> {
        let db = self.db.clone();
        let cursor = self.cursor.clone();
        let notify = self.notify.clone();
        // A subscriber waiting here is not idle, however long the chain is
        // quiet: see `ServeConfig::idle_timeout`.
        let waiting = crate::client::Waiting::begin();
        Promise::from_future(async move {
            let _waiting = waiting;
            loop {
                let h = { *cursor.lock().unwrap() };
                // Head first, then body: see `BlockSubServer::next` for the
                // race the other order has. The body and the head commit in
                // one batch.
                let head = Meta::new(&db).head_height();
                if h > head {
                    let waiter = notify.notified();
                    tokio::pin!(waiter);
                    waiter.await;
                    continue;
                }
                if let Some(bytes) = Blocks::new(&db).get(h) {
                    let block: Block = bincode::deserialize(&bytes)
                        .map_err(|e| capnp::Error::failed(format!("decode block: {e}")))?;

                    // Re-encode as Cap'n Proto. write_message_to_words returns
                    // a framed (segment-table-prefixed) wire message, which is
                    // exactly what the replica feeds into capnp::serialize::
                    // read_message on the other side.
                    let mut msg = capnp::message::Builder::new_default();
                    {
                        let root = msg.init_root::<cb_schema::block_capnp::block::Builder>();
                        conv::block_to_capnp(root, &block);
                    }
                    let capnp_bytes = capnp::serialize::write_message_to_words(&msg);

                    let header = &block.header;
                    let block_hash = header.hash();
                    let tx_count = u32::try_from(block.txs.len()).unwrap_or(u32::MAX);

                    let mut env = results.get().init_env();
                    env.set_bytes(&capnp_bytes);
                    env.set_height(header.height);
                    env.set_hash(&block_hash.0[..]);
                    env.set_state_root(&header.state_root.0[..]);
                    env.set_parent_qc(&header.parent_qc);
                    env.set_view(header.view);
                    env.set_tx_count(tx_count);
                    env.set_blocktime(header.timestamp);

                    *cursor.lock().unwrap() = h + 1;
                    return Ok(());
                }
                // At or below the head and no bytes: pruned out from under
                // us. Fail loudly so the client doesn't spin forever waiting
                // on a height the leader will never re-emit.
                return Err(capnp::Error::failed(format!(
                    "block at height {h} missing from Blocks CF (pruned?)"
                )));
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Refused transactions
// ---------------------------------------------------------------------------

/// One transaction the sequencer refused, as `streamRejected` serves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedEntry {
    /// Position in the log, strictly increasing. A subscriber resumes from
    /// the last one it saw.
    pub seq: u64,
    /// The block being built when the transaction was refused, or 0 when
    /// every transaction drained was refused and no block came of it.
    pub height: u64,
    pub hash: H256,
    pub sender: Address,
    pub nonce: u64,
    /// The executor's error, rendered.
    pub reason: String,
}

/// The sequencer's recent refusals, kept in memory for `streamRejected`.
///
/// Under the sequencer scheme a refused transaction is dropped from the block
/// rather than recorded on chain (`cb_block::BuildResult::rejected`), so
/// nothing on disk says it died. A client told "accepted" at admission would
/// otherwise wait for a block that never carries its transaction; a gateway
/// left every such order `pending` until a later nonce proved the point. The
/// log is bounded: a subscriber further behind than `capacity` entries misses
/// the oldest, which it can detect from a gap in `seq`. It does not survive a
/// restart, and need not: whatever was in flight at the restart is reconciled
/// from nonces, as before.
pub struct RejectedLog {
    inner: Mutex<RejectedInner>,
    notify: Notify,
}

struct RejectedInner {
    next_seq: u64,
    entries: VecDeque<RejectedEntry>,
    capacity: usize,
}

impl RejectedLog {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(RejectedInner {
                next_seq: 1,
                entries: VecDeque::new(),
                capacity: capacity.max(1),
            }),
            notify: Notify::new(),
        })
    }

    /// Record a batch's refusals and wake every subscriber.
    pub fn push(&self, height: u64, rejected: &[RejectedTx]) {
        if rejected.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            for r in rejected {
                let seq = inner.next_seq;
                inner.next_seq += 1;
                inner.entries.push_back(RejectedEntry {
                    seq,
                    height,
                    hash: r.hash,
                    sender: r.sender,
                    nonce: r.nonce,
                    reason: r.error.to_string(),
                });
                while inner.entries.len() > inner.capacity {
                    inner.entries.pop_front();
                }
            }
        }
        self.notify.notify_waiters();
    }

    /// Every retained entry with `seq >= from`, oldest first, and the seq
    /// the next call should start from.
    pub fn since(&self, from: u64) -> (Vec<RejectedEntry>, u64) {
        let inner = self.inner.lock().unwrap();
        let entries = inner
            .entries
            .iter()
            .filter(|e| e.seq >= from)
            .cloned()
            .collect();
        (entries, inner.next_seq)
    }

    /// The seq the next refusal will get: where a fresh subscriber starts.
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().unwrap().next_seq
    }
}

/// Server impl for `RejectedSub.next()`.
pub struct RejectedSubServer {
    pub log: Arc<RejectedLog>,
    pub cursor: Arc<Mutex<u64>>,
}

impl rpc_capnp::rejected_sub::Server for RejectedSubServer {
    fn next(
        &mut self,
        _params: rpc_capnp::rejected_sub::NextParams,
        mut results: rpc_capnp::rejected_sub::NextResults,
    ) -> Promise<(), capnp::Error> {
        let log = self.log.clone();
        let cursor = self.cursor.clone();
        let waiting = crate::client::Waiting::begin();
        Promise::from_future(async move {
            let _waiting = waiting;
            loop {
                // Register interest before looking, so a push that lands
                // between the look and the wait is not missed.
                let waiter = log.notify.notified();
                tokio::pin!(waiter);
                waiter.as_mut().enable();
                let from = { *cursor.lock().unwrap() };
                let (entries, next) = log.since(from);
                if !entries.is_empty() {
                    {
                        let mut out = results.get().init_entries(entries.len() as u32);
                        for (i, e) in entries.iter().enumerate() {
                            conv::rejected_entry_to_capnp(out.reborrow().get(i as u32), e);
                        }
                    }
                    results.get().set_next_seq(next);
                    *cursor.lock().unwrap() = next;
                    return Ok(());
                }
                waiter.await;
            }
        })
    }
}

#[cfg(test)]
mod rejected_log_tests {
    use super::*;
    use cb_block::ExecError;

    fn refused(n: u8) -> RejectedTx {
        RejectedTx {
            hash: H256([n; 32]),
            sender: Address([n; 20]),
            nonce: n as u64,
            error: ExecError::SelfTrade,
        }
    }

    #[test]
    fn entries_are_numbered_retained_and_served_from_a_seq() {
        let log = RejectedLog::new(3);
        assert_eq!(log.next_seq(), 1, "a fresh log starts at one");
        log.push(7, &[refused(1), refused(2)]);
        let (all, next) = log.since(1);
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].seq, all[1].seq), (1, 2));
        assert_eq!(all[0].height, 7);
        assert_eq!(all[0].reason, ExecError::SelfTrade.to_string());
        assert_eq!(next, 3);
        let (later, _) = log.since(2);
        assert_eq!(later.len(), 1, "a resumed subscriber skips what it saw");
    }

    #[test]
    fn the_log_is_bounded_and_the_gap_shows_in_seq() {
        let log = RejectedLog::new(2);
        log.push(0, &[refused(1), refused(2), refused(3)]);
        let (kept, next) = log.since(1);
        assert_eq!(kept.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(next, 4);
    }
}
