//! The pending-transaction queue: `tx_hash -> raw signed tx bytes`.
//!
//! # Why this is not a column family any more
//!
//! It used to be `CF_MEMPOOL` in RocksDB, and `drain` read it with
//! `iter_prefix(CF_MEMPOOL, &[]).take(n)`. Taking only `n` items sounds
//! bounded, and it is — in items yielded. It is not bounded in keys visited.
//! Every drained transaction is deleted straight after, and a deleted key
//! stays in the LSM tree as a delete marker until compaction removes it. Hashes
//! are uniformly distributed, so the surviving entries sit scattered among all
//! those markers, and the iterator has to step over every one of them to reach
//! the next live row. The cost of one drain therefore grew with the number of
//! transactions the node had ever processed, not with the number waiting.
//!
//! A ten-second sample of a sequencer under load put **61% of the whole
//! process's CPU** inside `Mempool::drain`, in
//! `rocksdb::DBIter::FindNextUserEntryInternal` — the function that walks past
//! entries an iterator must skip. It was the single largest cost in the node,
//! and it is why blocks carried about 120 transactions against a ceiling of
//! ten thousand: the block loop spent its interval scanning instead of
//! draining.
//!
//! A mempool is node-local, ephemeral and not part of any state root, so it
//! does not need to be durable and nothing but this node ever reads it. A
//! `BTreeMap` keyed by the raw 32-byte hash gives the same ordering the CF gave
//! — RocksDB's lexicographic order over the same bytes — so which transactions
//! a capped drain selects is unchanged, while put, delete and drain all become
//! operations on live entries only.
//!
//! The queue hangs off the `Database` handle rather than off a global, so two
//! databases in one process (the sequencer and a replica in the p2p tests) keep
//! separate queues, and every clone of a handle — including the staged handle
//! a block executes against — shares the one queue.
//!
//! **Behaviour change:** transactions admitted but not yet in a block no
//! longer survive a node restart. That matches how a mempool is treated
//! elsewhere in this node — the admission nonce ledger in `cb-rpc` is already
//! in memory and repopulates from the chain — and a client that cares learns
//! the transaction never landed by the nonce not advancing.
//!
//! `CF_MEMPOOL` still exists in `ALL_CFS` so databases written by older builds
//! still open; nothing reads or writes it.

use crate::db::Database;
use cb_types::{Address, H256};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// One queued transaction: its hash (what the sequencer deletes by), its
/// nonce, and its bytes.
#[derive(Clone, Debug)]
struct Queued {
    hash: H256,
    nonce: u64,
    bytes: Vec<u8>,
}

/// The queue itself. Per sender, in nonce order; plus a hash index so a
/// drained transaction can be deleted by the hash the sequencer holds.
#[derive(Default, Debug)]
pub struct MempoolQueue {
    by_sender: BTreeMap<[u8; 20], VecDeque<Queued>>,
    by_hash: HashMap<[u8; 32], ([u8; 20], u64)>,
    limits: MempoolLimits,
    /// Hashes of the last [`RECENT_COMMITTED`] transactions committed on
    /// this node, so a copy that arrives after its commit — a forwarded
    /// transaction that lost the race against the leader's proposal, or a
    /// client resubmitting — is recognised before any signature work and
    /// never queued. Without this a validator whose forwarded copies came
    /// late recovered every proposal's signatures itself and then, when the
    /// copies arrived, recovered them again and queued them as dead entries:
    /// under load its mempool filled to the cap with transactions already
    /// on the chain.
    recent_committed: std::collections::HashSet<[u8; 32]>,
    recent_order: VecDeque<[u8; 32]>,
}

/// How many committed hashes are remembered: a few seconds of full load.
pub const RECENT_COMMITTED: usize = 1 << 19;

/// How much the mempool holds before [`Mempool::try_put`] refuses.
///
/// # Why there is a bound
///
/// The queue used to grow without limit. Admission checks the nonce, so one
/// sender cannot queue two transactions with the same nonce, but it can
/// queue a contiguous run of any length — a million pipelined transfers is a
/// million entries — and any number of fresh keys can each do the same. The
/// only thing standing between a client and the node's memory was the
/// verification queue, which bounds how fast transactions arrive, not how
/// many stay.
///
/// The defaults are far above what the chain can use: at the largest block
/// (65,535 transactions) the total is four blocks' worth, and one sender can
/// pipeline a quarter of a large block. A refusal is `mempoolFull`, which the
/// gateway already answers as "rate limited" and the simulator as "re-read
/// the nonce", so a bound that is hit costs a retry, not a stuck client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MempoolLimits {
    /// Most transactions held across all senders.
    pub max_total: usize,
    /// Most transactions held for one sender.
    pub max_per_sender: usize,
}

impl MempoolLimits {
    pub const DEFAULT_MAX_TOTAL: usize = 262_144;
    pub const DEFAULT_MAX_PER_SENDER: usize = 16_384;
}

impl Default for MempoolLimits {
    fn default() -> Self {
        Self {
            max_total: Self::DEFAULT_MAX_TOTAL,
            max_per_sender: Self::DEFAULT_MAX_PER_SENDER,
        }
    }
}

/// Why [`Mempool::try_put`] refused a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MempoolFull {
    #[error(
        "mempool is full ({held} transactions waiting, limit {limit}); retry after the next block"
    )]
    Total { held: usize, limit: usize },
    #[error("this sender has {held} transactions waiting (limit {limit}); wait for a block before sending more")]
    Sender { held: usize, limit: usize },
}

impl MempoolQueue {
    fn mark_committed(&mut self, hashes: &[H256]) {
        for h in hashes {
            if self.recent_committed.insert(h.0) {
                self.recent_order.push_back(h.0);
            }
        }
        while self.recent_order.len() > RECENT_COMMITTED {
            if let Some(old) = self.recent_order.pop_front() {
                self.recent_committed.remove(&old);
            }
        }
    }

    fn is_known(&self, hash: &H256) -> bool {
        self.by_hash.contains_key(&hash.0) || self.recent_committed.contains(&hash.0)
    }

    fn insert(&mut self, hash: H256, sender: Address, nonce: u64, bytes: &[u8]) {
        if self.by_hash.contains_key(&hash.0) || self.recent_committed.contains(&hash.0) {
            return;
        }
        self.by_hash.insert(hash.0, (sender.0, nonce));
        let queue = self.by_sender.entry(sender.0).or_default();
        let item = Queued {
            hash,
            nonce,
            bytes: bytes.to_vec(),
        };
        // Admission delivers nonces in order, so this is an append; the
        // fallback keeps the queue sorted if a caller ever does not.
        match queue.back() {
            Some(last) if last.nonce >= nonce => {
                let pos = queue.partition_point(|x| x.nonce < nonce);
                queue.insert(pos, item);
            }
            _ => queue.push_back(item),
        }
    }

    fn remove(&mut self, hash: &H256) {
        let Some((sender, nonce)) = self.by_hash.remove(&hash.0) else {
            return;
        };
        let remove_sender = if let Some(queue) = self.by_sender.get_mut(&sender) {
            // Drained entries are at the front, so this is usually O(1). Two
            // entries may share a nonce (a replacement, or a forged copy);
            // the hash picks the one being removed.
            if let Some(pos) = queue
                .iter()
                .position(|x| x.nonce == nonce && x.hash == *hash)
            {
                queue.remove(pos);
            }
            queue.is_empty()
        } else {
            false
        };
        if remove_sender {
            self.by_sender.remove(&sender);
        }
    }
}

pub struct Mempool<'a> {
    db: &'a Database,
}

impl<'a> Mempool<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    fn with<R>(&self, f: impl FnOnce(&mut MempoolQueue) -> R) -> R {
        let mut g = self.db.mempool_map().lock().expect("mempool poisoned");
        f(&mut g)
    }

    /// Queue a transaction from `sender` carrying `nonce`.
    ///
    /// # Why the key is the sender and the nonce, not the hash
    ///
    /// The queue used to be keyed by hash and drained in hash order. That was
    /// harmless while admission was slower than block production, because
    /// every drain then took the whole queue and the block builder's ordering
    /// step sorted each sender's transactions into nonce order. Once
    /// signature recovery moved to a pool the queue could hold more than one
    /// block's worth, and a drain of the first N by hash took a random subset
    /// of each sender's run: nonces 1010, 1013 and 1023 without 1011. The
    /// executor refused everything after the gap — "bad nonce: expected 1010,
    /// got 1013" — and dropped it, the admission ledger then expected the
    /// chain's nonce while the client was hundreds ahead, and six connections
    /// collapsed to three hundred orders a second at a third refused.
    ///
    /// Keyed this way, a drain takes each sender's lowest nonces first, so
    /// whatever it includes of a sender is a contiguous run starting at that
    /// sender's next nonce. Admission already guarantees the run is
    /// contiguous from the chain's nonce.
    ///
    /// Not bounded by [`MempoolLimits`]: it is what puts back the
    /// transactions of an abandoned proposal (which were admitted under the
    /// bound once and must not be lost) and what peer gossip calls. Client
    /// admission goes through [`Mempool::try_put`].
    pub fn put(&self, hash: H256, sender: Address, nonce: u64, bytes: &[u8]) {
        self.with(|q| q.insert(hash, sender, nonce, bytes))
    }

    /// [`Mempool::put`], refused when the queue or this sender's share of it
    /// is at its limit. A hash already held is accepted without counting
    /// again: it is the same transaction.
    pub fn try_put(
        &self,
        hash: H256,
        sender: Address,
        nonce: u64,
        bytes: &[u8],
    ) -> Result<(), MempoolFull> {
        self.with(|q| {
            if q.by_hash.contains_key(&hash.0) {
                return Ok(());
            }
            let held = q.by_hash.len();
            if held >= q.limits.max_total {
                return Err(MempoolFull::Total {
                    held,
                    limit: q.limits.max_total,
                });
            }
            let mine = q.by_sender.get(&sender.0).map(|d| d.len()).unwrap_or(0);
            if mine >= q.limits.max_per_sender {
                return Err(MempoolFull::Sender {
                    held: mine,
                    limit: q.limits.max_per_sender,
                });
            }
            q.insert(hash, sender, nonce, bytes);
            Ok(())
        })
    }

    /// Replace the bounds [`Mempool::try_put`] applies. Shared by every
    /// handle on this database. Lowering them never evicts: what is held
    /// stays until it is drained.
    pub fn set_limits(&self, limits: MempoolLimits) {
        self.with(|q| q.limits = limits)
    }

    pub fn limits(&self) -> MempoolLimits {
        self.with(|q| q.limits)
    }

    /// The senders recorded for `hashes`, for those still pending. Admission
    /// recovered each of them once; a validator checking a proposal that
    /// carries them need not recover them again.
    pub fn senders_of(&self, hashes: &[H256]) -> Vec<Option<Address>> {
        self.with(|q| {
            hashes
                .iter()
                .map(|h| q.by_hash.get(&h.0).map(|(s, _)| Address(*s)))
                .collect()
        })
    }

    /// The raw bytes held for `hash`, if it is still pending.
    pub fn get(&self, hash: &H256) -> Option<Vec<u8>> {
        self.with(|q| {
            let (sender, nonce) = *q.by_hash.get(&hash.0)?;
            q.by_sender
                .get(&sender)?
                .iter()
                .find(|x| x.nonce == nonce && x.hash == *hash)
                .map(|x| x.bytes.clone())
        })
    }

    pub fn delete(&self, hash: &H256) {
        self.with(|q| q.remove(hash))
    }

    /// Delete every one of `hashes` under one acquisition of the lock.
    ///
    /// The block builder removes a whole block's transactions at once. One
    /// `delete` per transaction was ten thousand lock round trips per
    /// block, each contending with the admission coordinator's `put`.
    pub fn delete_many(&self, hashes: &[H256]) {
        if hashes.is_empty() {
            return;
        }
        self.with(|q| {
            for h in hashes {
                q.remove(h);
            }
        })
    }

    /// Remember `hashes` as committed (see `MempoolQueue::recent_committed`).
    /// Called at commit, not at selection: a selected transaction can still
    /// be requeued if its proposal is abandoned.
    pub fn mark_committed(&self, hashes: &[H256]) {
        if hashes.is_empty() {
            return;
        }
        self.with(|q| q.mark_committed(hashes))
    }

    /// Whether `hash` is queued or was recently committed: nothing admission
    /// needs to look at, let alone recover a signature for.
    pub fn is_known(&self, hash: &H256) -> bool {
        self.with(|q| q.is_known(hash))
    }

    /// Take up to `n` items without removing them: the block builder deletes
    /// what its block actually carried.
    ///
    /// Round-robin over senders in address order, one transaction from the
    /// front of each queue per pass, until `n`. Every sender included is
    /// included as a contiguous run from its lowest pending nonce, and no
    /// sender can starve the others. Deterministic: the same queue always
    /// drains the same way.
    pub fn drain(&self, n: usize) -> Vec<(H256, Vec<u8>)> {
        self.with(|q| {
            let mut out = Vec::with_capacity(n.min(q.by_hash.len()));
            if n == 0 || q.by_sender.is_empty() {
                return out;
            }
            let mut cursors: Vec<(&[u8; 20], usize, &VecDeque<Queued>)> = q
                .by_sender
                .iter()
                .map(|(s, queue)| (s, 0usize, queue))
                .collect();
            let mut progressed = true;
            while out.len() < n && progressed {
                progressed = false;
                for (_, cursor, queue) in cursors.iter_mut() {
                    if out.len() >= n {
                        break;
                    }
                    if let Some(item) = queue.get(*cursor) {
                        out.push((item.hash, item.bytes.clone()));
                        *cursor += 1;
                        progressed = true;
                    }
                }
            }
            out
        })
    }

    pub fn is_empty(&self) -> bool {
        self.with(|q| q.by_hash.is_empty())
    }

    /// How many transactions are waiting. Diagnostics and the dashboard.
    pub fn len(&self) -> usize {
        self.with(|q| q.by_hash.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn put_get_delete_and_empty() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        assert!(m.is_empty());
        m.put(H256([7; 32]), Address([1; 20]), 0, b"bytes");
        assert!(!m.is_empty());
        assert_eq!(m.get(&H256([7; 32])).as_deref(), Some(&b"bytes"[..]));
        m.delete(&H256([7; 32]));
        assert!(m.is_empty());
        assert_eq!(m.get(&H256([7; 32])), None);
    }

    /// The reproduction for the scan this queue replaces: churn it the way a
    /// loaded node churns it — far more transactions already drained and
    /// deleted than left waiting — and a drain must still see only what is
    /// waiting, in hash order, at the size asked for.
    #[test]
    fn a_drained_and_deleted_history_does_not_affect_the_next_drain() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        for i in 0..2_000u32 {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_be_bytes());
            m.put(
                H256(h),
                Address([(i % 7) as u8; 20]),
                u64::from(i / 7),
                &i.to_be_bytes(),
            );
        }
        // Drain and delete everything, twice over, as the block loop does.
        for _ in 0..2 {
            for (h, _) in m.drain(1_000) {
                m.delete(&h);
            }
        }
        assert!(m.is_empty());

        // Three latecomers. A drain must return exactly these three, in
        // ascending hash order, however much traffic came before them.
        let mut expected = Vec::new();
        for i in [9u32, 3, 6] {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_be_bytes());
            m.put(H256(h), Address([9; 20]), u64::from(i), &i.to_be_bytes());
            expected.push((i, H256(h)));
        }
        // One sender: a drain returns its nonces in ascending order.
        expected.sort_by_key(|(n, _)| *n);
        let expected: Vec<H256> = expected.into_iter().map(|(_, h)| h).collect();
        let got: Vec<H256> = m.drain(10).into_iter().map(|(h, _)| h).collect();
        assert_eq!(got, expected);
        assert_eq!(m.len(), 3);
        assert_eq!(m.drain(2).len(), 2, "a capped drain stops at the cap");
    }

    /// The reproduction for the collapse the per-sender queue fixes: with
    /// more queued than one block takes, a drain must never hand the block
    /// builder a sender's nonces with a gap, and must serve every sender.
    #[test]
    fn a_partial_drain_takes_contiguous_nonce_runs_from_every_sender() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        // Three senders, 40 nonces each, arriving interleaved.
        for nonce in 0..40u64 {
            for s in 0..3u8 {
                let mut h = [0u8; 32];
                h[0] = s;
                h[1..9].copy_from_slice(&nonce.to_be_bytes());
                // Hash bytes chosen so hash order would scramble nonces.
                h[9] = (nonce * 37 % 251) as u8;
                m.put(H256(h), Address([s + 1; 20]), nonce, b"x");
            }
        }
        let drained = m.drain(50);
        assert_eq!(drained.len(), 50);
        // Decode (sender, nonce) back out of the hash layout above.
        let mut per_sender: std::collections::BTreeMap<u8, Vec<u64>> = Default::default();
        for (h, _) in &drained {
            let mut nb = [0u8; 8];
            nb.copy_from_slice(&h.0[1..9]);
            per_sender
                .entry(h.0[0])
                .or_default()
                .push(u64::from_be_bytes(nb));
        }
        assert_eq!(per_sender.len(), 3, "every sender is served");
        for (s, nonces) in &per_sender {
            let expect: Vec<u64> = (0..nonces.len() as u64).collect();
            assert_eq!(nonces, &expect, "sender {s} was handed a gap or a reorder");
            assert!(
                nonces.len() >= 16,
                "round robin keeps shares within one of each other"
            );
        }
        // Deleting what was drained leaves the rest intact and still ordered.
        for (h, _) in &drained {
            m.delete(h);
        }
        assert_eq!(m.len(), 120 - 50);
        let next = m.drain(3);
        for (h, _) in &next {
            let mut nb = [0u8; 8];
            nb.copy_from_slice(&h.0[1..9]);
            let nonce = u64::from_be_bytes(nb);
            assert_eq!(
                nonce,
                per_sender[&h.0[0]].len() as u64,
                "continues where the drain stopped"
            );
        }
    }

    /// Two databases in one process keep separate queues — the p2p tests
    /// assert exactly this about a sequencer and a replica.
    #[test]
    fn queues_are_per_database() {
        let (_d1, a) = db();
        let (_d2, b) = db();
        Mempool::new(&a).put(H256([1; 32]), Address([1; 20]), 0, b"x");
        assert!(!Mempool::new(&a).is_empty());
        assert!(Mempool::new(&b).is_empty());
    }

    /// A block executes against a staged handle; it must see the same queue.
    #[test]
    fn a_staged_handle_shares_the_queue() {
        let (_d, db) = db();
        Mempool::new(&db).put(H256([4; 32]), Address([1; 20]), 0, b"x");
        let staged = db.staged();
        assert_eq!(Mempool::new(&staged).len(), 1);
        Mempool::new(&staged).delete(&H256([4; 32]));
        assert!(Mempool::new(&db).is_empty());
    }

    fn h(sender: u8, nonce: u64) -> H256 {
        let mut b = [0u8; 32];
        b[0] = sender;
        b[1..9].copy_from_slice(&nonce.to_be_bytes());
        H256(b)
    }

    #[test]
    fn try_put_refuses_past_the_total_and_the_per_sender_limit() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        assert_eq!(m.limits(), MempoolLimits::default());
        m.set_limits(MempoolLimits {
            max_total: 5,
            max_per_sender: 3,
        });
        for n in 0..3 {
            m.try_put(h(1, n), Address([1; 20]), n, b"a").unwrap();
        }
        // The fourth from one sender is its limit, not the pool's.
        let err = m.try_put(h(1, 3), Address([1; 20]), 3, b"a").unwrap_err();
        assert_eq!(err, MempoolFull::Sender { held: 3, limit: 3 });
        assert!(err.to_string().contains("limit 3"), "{err}");
        // A repeat of a held hash is not a new entry and is not refused.
        m.try_put(h(1, 0), Address([1; 20]), 0, b"a").unwrap();
        m.try_put(h(2, 0), Address([2; 20]), 0, b"b").unwrap();
        m.try_put(h(2, 1), Address([2; 20]), 1, b"b").unwrap();
        let err = m.try_put(h(3, 0), Address([3; 20]), 0, b"c").unwrap_err();
        assert_eq!(err, MempoolFull::Total { held: 5, limit: 5 });
        assert!(err.to_string().contains("mempool is full"), "{err}");
        assert_eq!(m.len(), 5);
        // Space frees as a block takes transactions.
        m.delete(&h(1, 0));
        m.try_put(h(3, 0), Address([3; 20]), 0, b"c").unwrap();
        // `put` (requeue, gossip) is not bounded.
        m.put(h(4, 0), Address([4; 20]), 0, b"d");
        assert_eq!(m.len(), 6);
        // Limits are shared by every handle on the database.
        assert_eq!(Mempool::new(&db.staged()).limits().max_total, 5);
    }

    #[test]
    fn delete_many_removes_exactly_the_named_hashes() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        for s in 1..=3u8 {
            for n in 0..4 {
                m.put(h(s, n), Address([s; 20]), n, &[s, n as u8]);
            }
        }
        m.delete_many(&[h(1, 0), h(1, 1), h(2, 0), H256([0xee; 32])]);
        m.delete_many(&[]);
        assert_eq!(m.len(), 12 - 3);
        assert!(m.get(&h(1, 0)).is_none());
        assert_eq!(m.get(&h(1, 2)), Some(vec![1, 2]));
        let first: Vec<H256> = m.drain(3).into_iter().map(|(x, _)| x).collect();
        assert_eq!(first, vec![h(1, 2), h(2, 1), h(3, 0)]);
    }

    /// Two entries for one sender and one nonce (a gossiped replacement next
    /// to an admitted one): deleting one by hash removes that one, not
    /// whichever the nonce finds first.
    #[test]
    fn same_nonce_twins_are_told_apart_by_hash() {
        let (_d, db) = db();
        let m = Mempool::new(&db);
        m.put(H256([1; 32]), Address([1; 20]), 0, b"first");
        m.put(H256([2; 32]), Address([1; 20]), 0, b"second");
        assert_eq!(m.get(&H256([2; 32])).as_deref(), Some(&b"second"[..]));
        m.delete(&H256([2; 32]));
        assert_eq!(m.get(&H256([1; 32])).as_deref(), Some(&b"first"[..]));
        assert_eq!(m.len(), 1);
        let d = m.drain(10);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, H256([1; 32]));
    }

    mod model {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeMap as Map;

        #[derive(Clone, Debug)]
        enum Op {
            /// Admit the sender's next nonce (or a stale/duplicate one).
            Admit { sender: u8, skew: i8 },
            /// A block takes a drain of up to `n` and deletes a prefix of it.
            Select { n: usize, keep: usize },
            /// Delete an arbitrary held entry by index.
            Remove { pick: usize },
        }

        fn op() -> impl Strategy<Value = Op> {
            prop_oneof![
                4 => (0u8..6, -1i8..2).prop_map(|(sender, skew)| Op::Admit { sender, skew }),
                2 => (0usize..12, 0usize..12).prop_map(|(n, keep)| Op::Select { n, keep }),
                1 => any::<usize>().prop_map(|pick| Op::Remove { pick }),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]

            /// The mempool against a plain model: per sender, a sorted map
            /// of nonce to hash. After every step the limits hold, nothing
            /// admitted is lost, nothing deleted survives, and a drain
            /// hands out each sender's lowest nonces in order with no entry
            /// twice and no sender more than one ahead of another.
            #[test]
            fn the_mempool_behaves_like_its_model(
                ops in prop::collection::vec(op(), 1..120),
                max_total in 1usize..24,
                max_per_sender in 1usize..8,
            ) {
                let (_d, db) = db();
                let m = Mempool::new(&db);
                m.set_limits(MempoolLimits { max_total, max_per_sender });
                let mut model: Map<u8, Map<u64, H256>> = Map::new();
                let mut next_nonce: Map<u8, u64> = Map::new();
                for op in ops {
                    match op {
                        Op::Admit { sender, skew } => {
                            let base = *next_nonce.get(&sender).unwrap_or(&0);
                            let nonce = (base as i64 + skew as i64).max(0) as u64;
                            let hash = h(sender, nonce);
                            let held_total: usize = model.values().map(|q| q.len()).sum();
                            let held_mine = model.get(&sender).map(|q| q.len()).unwrap_or(0);
                            let dup = model.get(&sender).map(|q| q.values().any(|x| *x == hash)).unwrap_or(false);
                            let r = m.try_put(hash, Address([sender; 20]), nonce, &nonce.to_le_bytes());
                            if dup {
                                prop_assert!(r.is_ok(), "a held hash is accepted again");
                            } else if held_total >= max_total {
                                let is_total = matches!(r, Err(MempoolFull::Total { .. }));
                                prop_assert!(is_total);
                            } else if held_mine >= max_per_sender {
                                let is_sender = matches!(r, Err(MempoolFull::Sender { .. }));
                                prop_assert!(is_sender);
                            } else {
                                prop_assert!(r.is_ok());
                                model.entry(sender).or_default().insert(nonce, hash);
                                if nonce >= base {
                                    next_nonce.insert(sender, nonce + 1);
                                }
                            }
                        }
                        Op::Select { n, keep } => {
                            let drained = m.drain(n);
                            let total: usize = model.values().map(|q| q.len()).sum();
                            prop_assert_eq!(drained.len(), n.min(total));
                            // No entry twice.
                            let mut seen = std::collections::HashSet::new();
                            for (x, _) in &drained {
                                prop_assert!(seen.insert(*x), "drained twice");
                            }
                            // Per sender: the model's lowest nonces, in order.
                            let mut per: Map<u8, Vec<H256>> = Map::new();
                            for (x, _) in &drained {
                                per.entry(x.0[0]).or_default().push(*x);
                            }
                            // Round robin: a sender served two fewer than
                            // another must have run out.
                            let got_of = |s: &u8| per.get(s).map(|v| v.len()).unwrap_or(0);
                            let most = model.keys().map(got_of).max().unwrap_or(0);
                            for (s, q) in &model {
                                let got = got_of(s);
                                prop_assert!(got + 1 >= most || got == q.len(), "sender {} starved", s);
                            }
                            for (s, got) in &per {
                                let want: Vec<H256> = model[s].values().take(got.len()).copied().collect();
                                prop_assert_eq!(got, &want);
                            }
                            // A block keeps a prefix; one batch delete.
                            let taken: Vec<H256> = drained.iter().take(keep).map(|(x, _)| *x).collect();
                            m.delete_many(&taken);
                            for x in &taken {
                                if let Some(q) = model.get_mut(&x.0[0]) {
                                    q.retain(|_, v| v != x);
                                }
                            }
                            model.retain(|_, q| !q.is_empty());
                        }
                        Op::Remove { pick } => {
                            let all: Vec<H256> = model.values().flat_map(|q| q.values().copied()).collect();
                            if !all.is_empty() {
                                let x = all[pick % all.len()];
                                m.delete(&x);
                                model.get_mut(&x.0[0]).unwrap().retain(|_, v| *v != x);
                                model.retain(|_, q| !q.is_empty());
                            }
                        }
                    }
                    // Invariants after every step.
                    let total: usize = model.values().map(|q| q.len()).sum();
                    prop_assert_eq!(m.len(), total, "no loss, no ghost");
                    prop_assert!(m.len() <= max_total);
                    for (s, q) in &model {
                        prop_assert!(q.len() <= max_per_sender);
                        for (nonce, x) in q {
                            prop_assert_eq!(m.get(x), Some(nonce.to_le_bytes().to_vec()));
                            let _ = s;
                        }
                    }
                }
            }
        }
    }
}
