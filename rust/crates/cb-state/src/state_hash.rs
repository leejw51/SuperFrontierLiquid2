//! The chain's state commitment: a hash chain over each block's changes.
//!
//! ## What it replaces, and why
//!
//! The state root used to be `keccak(accounts_root ‖ positions_root ‖
//! markets_root ‖ margin_root)`, four binary merkle tries keyed by
//! `keccak(key)`. Every changed key cost about log₂N node hashes plus the
//! RocksDB reads of its path. At 100,000 accounts that commit step was 91% of
//! the block loop in `make orderbench` and held the engine to 13,334
//! orders/s (ADR 0007). Nothing ever used what a merkle tree adds over a plain
//! hash: no RPC returns an inclusion proof, and there is no light client. The
//! root had one consumer, a replica comparing its re-execution against the
//! header.
//!
//! A hash chain serves that consumer at a cost proportional to what changed:
//!
//! ```text
//! diff_N  = sha256( entry* )           every committed key block N changed, in (cf, key) order
//! root_N  = sha256( DOMAIN ‖ root_{N−1} ‖ diff_N )
//! root_0  = sha256( DOMAIN ‖ 0³² ‖ sha256( every committed row at genesis ) )
//! ```
//!
//! Two replicas that agree on `root_{N−1}` and on every byte block N wrote
//! agree on `root_N`; one differing value anywhere gives a different root at
//! that block, which is where the merkle root caught divergence too. What is
//! given up: a proof of one key against a root, and checking a snapshot
//! without replaying. Neither was used.
//!
//! The chain also commits to more than the tries did. The order books, funding
//! indices, oracle state and liquidation queue were "auxiliary" and outside
//! the root; every committed column family below is in the diff, so a replica
//! whose book diverged is now caught at the block that diverged it.
//!
//! ## The rule that keeps leader and replica in step
//!
//! `diff_N` must be the same set on every node, so it contains **writes**, not
//! reads. The executor's writes are a deterministic function of the block. Its
//! reads are not: a replica prefetches accounts, and the benchmark loads
//! accounts it never changes. Accounts are therefore hashed only when their
//! value at the end of the block differs from their value at its start
//! (`BlockState::commit_into`). Everything else enters through the write stage,
//! which only the executor's `put` and `delete` reach.
//!
//! SHA-256 rather than keccak: the diff is hundreds of kilobytes a block, and
//! `sha2` uses the CPU's SHA extensions where they exist. The chain step is 96
//! bytes a block and could be either.

use crate::db::{Database, DbError, ALL_CFS};
use cb_types::H256;
use sha2::{Digest, Sha256};

/// Identifies this commitment scheme. Stored in `Meta` at genesis; a data
/// directory built under another scheme is refused at startup rather than
/// producing roots no peer will agree with.
pub const STATE_HASH_SCHEME: u32 = 2;

/// Domain separator for the chain step.
const DOMAIN: &[u8] = b"cb-state-hash-v2";

/// Column families whose contents are consensus state. Blocks, the hash index,
/// `meta` (head pointer, view, roots), the mempool, quorum certificates and
/// the fast-mirror copy are either the chain itself or node-local, and the
/// trie nodes of the scheme this replaced are no longer written.
pub const COMMITTED_CFS: &[&str] = &[
    crate::db::CF_ACCOUNTS,
    crate::db::CF_ORDERS,
    crate::db::CF_BOOK_INDEX,
    crate::db::CF_MARKETS,
    crate::db::CF_VALIDATORS,
    crate::db::CF_POSITIONS,
    crate::db::CF_MARGIN_ACCOUNTS,
    crate::db::CF_FUNDING_INDICES,
    crate::db::CF_PERP_MARKETS,
    crate::db::CF_FUTURES_MARKETS,
    crate::db::CF_ORACLE_STATE,
    crate::db::CF_LIQUIDATION_QUEUE,
    crate::db::CF_PERP_ORDERS,
    crate::db::CF_PERP_BOOK_INDEX,
    crate::db::CF_PERP_ORDERS_BY_OWNER,
];

/// Whether the column family at `idx` in [`ALL_CFS`] is committed.
pub fn is_committed(idx: usize) -> bool {
    COMMITTED_CFS.contains(&ALL_CFS[idx])
}

/// Accumulates one block's changes, in `(column family, key)` order.
///
/// Each entry is framed so no two different change sets can produce the same
/// byte stream: the column family's index in [`ALL_CFS`], the key's length and
/// bytes, then a tag (`0` delete, `1` put) and, for a put, the value's length
/// and bytes.
pub struct DiffHasher {
    inner: Sha256,
    entries: u64,
}

impl Default for DiffHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffHasher {
    pub fn new() -> Self {
        Self {
            inner: Sha256::new(),
            entries: 0,
        }
    }

    /// Feed one change. The caller supplies entries in ascending
    /// `(cf_idx, key)` order; the hash is order-sensitive by design.
    pub fn entry(&mut self, cf_idx: usize, key: &[u8], value: Option<&[u8]>) {
        self.inner.update([cf_idx as u8]);
        self.inner.update((key.len() as u32).to_be_bytes());
        self.inner.update(key);
        match value {
            None => self.inner.update([0u8]),
            Some(v) => {
                self.inner.update([1u8]);
                self.inner.update((v.len() as u32).to_be_bytes());
                self.inner.update(v);
            }
        }
        self.entries += 1;
    }

    /// Number of entries fed so far.
    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn finish(self) -> H256 {
        H256(self.inner.finalize().into())
    }
}

/// One link of the chain: the root after a block, from the root before it and
/// the block's diff hash.
pub fn chain(prev: H256, diff: H256) -> H256 {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update(prev.0);
    h.update(diff.0);
    H256(h.finalize().into())
}

/// One column family's worth of pending writes: `Some` a put, `None` a
/// delete. The write stage, the pending-write overlay and a block's sealed
/// batch all use this shape, indexed by position in [`ALL_CFS`].
pub type CfMap = std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// The diff hash of one block's writes, given as one map per column family in
/// [`ALL_CFS`] order: every entry of every committed family, in `(cf, key)`
/// order. Returns the hash and the number of entries hashed.
///
/// This is the only function that computes a block's diff. The leader's
/// pipelined seal and a replica's synchronous commit both call it on the same
/// maps, which is what keeps their roots byte-identical.
pub fn diff_hash(cfs: &[CfMap]) -> (H256, u64) {
    let mut h = DiffHasher::new();
    for (idx, map) in cfs.iter().enumerate() {
        if !is_committed(idx) {
            continue;
        }
        for (k, v) in map {
            h.entry(idx, k, v.as_deref());
        }
    }
    let n = h.entries();
    (h.finish(), n)
}

/// The genesis root: every committed row, in `(cf, key)` order, as one diff
/// against an empty chain. Genesis is the one place the whole state is hashed.
pub fn genesis_state_hash(db: &Database) -> Result<H256, DbError> {
    let mut h = DiffHasher::new();
    for (idx, cf) in ALL_CFS.iter().enumerate() {
        if !is_committed(idx) {
            continue;
        }
        for item in db.iter_prefix(cf, &[])? {
            let (k, v) = item?;
            h.entry(idx, &k, Some(&v));
        }
    }
    Ok(chain(H256::ZERO, h.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn digest(entries: &[(usize, &[u8], Option<&[u8]>)]) -> H256 {
        let mut h = DiffHasher::new();
        for (cf, k, v) in entries {
            h.entry(*cf, k, *v);
        }
        h.finish()
    }

    #[test]
    fn a_changed_value_changes_the_diff() {
        assert_ne!(
            digest(&[(0, b"a", Some(b"1"))]),
            digest(&[(0, b"a", Some(b"2"))])
        );
    }

    #[test]
    fn a_delete_is_not_an_empty_put() {
        assert_ne!(digest(&[(0, b"a", None)]), digest(&[(0, b"a", Some(b""))]));
    }

    #[test]
    fn framing_keeps_key_and_value_boundaries() {
        // "ab"+"c" and "a"+"bc" must not collide.
        assert_ne!(
            digest(&[(0, b"ab", Some(b"c"))]),
            digest(&[(0, b"a", Some(b"bc"))])
        );
        assert_ne!(
            digest(&[(0, b"a", Some(b"1"))]),
            digest(&[(1, b"a", Some(b"1"))])
        );
    }

    #[test]
    fn the_chain_depends_on_the_previous_root() {
        let d = digest(&[(0, b"a", Some(b"1"))]);
        assert_ne!(chain(H256::ZERO, d), chain(H256([1; 32]), d));
        assert_eq!(chain(H256::ZERO, d), chain(H256::ZERO, d));
    }

    #[test]
    fn committed_families_exclude_the_chain_and_node_local_state() {
        for cf in [
            crate::db::CF_BLOCKS,
            crate::db::CF_BLOCK_HASH,
            crate::db::CF_META,
            crate::db::CF_MEMPOOL,
            crate::db::CF_TRIE_NODES,
            crate::db::CF_QC,
            crate::db::CF_MIRROR_BLOCKS,
        ] {
            assert!(!COMMITTED_CFS.contains(&cf), "{cf} must not be committed");
        }
        assert_eq!(COMMITTED_CFS.len() + 7, ALL_CFS.len());
    }
}
