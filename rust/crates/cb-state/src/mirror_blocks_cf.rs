//! Fast-mirror block log.
//!
//! A replica running in fast-mirror mode is a block-bytes backup: it does not
//! re-execute transactions, does not maintain accounts / orders / trie state,
//! and never recomputes a `state_root`. The only consensus-relevant artifacts
//! it persists are the four fields the operator named — height, block hash,
//! blocktime, and the raw block bytes — plus a head pointer in `CF_META`.
//!
//! All writes go through [`MirrorBlocks::put_with_meta`], which packs those
//! fields into a single value and bundles the head-pointer updates into one
//! `WriteBatch` so a crash either leaves the replica at height N (clean) or at
//! height N-1 (clean). There is no half-applied state.
//!
//! Value layout for `CF_MIRROR_BLOCKS`, key = `height.to_be_bytes()`:
//! ```text
//!   bytes[0..32]   block hash (H256)
//!   bytes[32..40]  blocktime, big-endian u64 (seconds since epoch)
//!   bytes[40..]    Cap'n Proto-serialised Block (framed message)
//! ```
//! Big-endian throughout so `iterator_cf` walks heights in numeric order.

use crate::db::{Database, DbError, CF_META, CF_MIRROR_BLOCKS};
use cb_types::H256;

const HEAD_HEIGHT: &[u8] = b"head_height";
const HEAD_HASH: &[u8] = b"head_hash";

/// Width of the fixed prefix (`hash || blocktime`) before the variable-length
/// Cap'n Proto payload.
const PREFIX_LEN: usize = 32 + 8;

pub struct MirrorBlocks<'a> {
    db: &'a Database,
}

impl<'a> MirrorBlocks<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Atomic per-block commit. Writes
    ///   * `(CF_MIRROR_BLOCKS, height_be) → hash || blocktime_be || capnp_bytes`
    ///   * `(CF_META, "head_height") → height_be`
    ///   * `(CF_META, "head_hash") → head_hash_target`
    ///
    /// All in one `WriteBatch`. No `state_root`, no `parent_qc`, no hash-index
    /// CF — fast-mirror replicas do not need them.
    pub fn put_with_meta(
        &self,
        height: u64,
        hash: H256,
        blocktime: u64,
        capnp_bytes: &[u8],
        head_hash_target: H256,
    ) -> Result<(), DbError> {
        // Single contiguous buffer so RocksDB's WriteBatch can copy it once.
        let mut value = Vec::with_capacity(PREFIX_LEN + capnp_bytes.len());
        value.extend_from_slice(&hash.0);
        value.extend_from_slice(&blocktime.to_be_bytes());
        value.extend_from_slice(capnp_bytes);

        let key = height.to_be_bytes();
        let mut batch = self.db.batch();
        batch.put(CF_MIRROR_BLOCKS, &key, &value)?;
        batch.put(CF_META, HEAD_HEIGHT, &key)?;
        batch.put(CF_META, HEAD_HASH, &head_hash_target.0)?;
        self.db.write_batch(batch)
    }

    /// Read back the parts. Returns `None` if no row at that height.
    pub fn get(&self, height: u64) -> Option<(H256, u64, Vec<u8>)> {
        let raw = self
            .db
            .get(CF_MIRROR_BLOCKS, &height.to_be_bytes())
            .expect("rocks")?;
        if raw.len() < PREFIX_LEN {
            return None;
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&raw[..32]);
        let mut bt = [0u8; 8];
        bt.copy_from_slice(&raw[32..40]);
        let blocktime = u64::from_be_bytes(bt);
        let capnp_bytes = raw[PREFIX_LEN..].to_vec();
        Some((H256(hash), blocktime, capnp_bytes))
    }

    /// Highest stored height (last entry, since keys are big-endian u64s).
    /// Drop the mirrored body at `height`. The head pointer and the hash
    /// index the mirror keeps are untouched; this only reclaims the bytes.
    pub fn prune_at(&self, height: u64) {
        self.db
            .delete(CF_MIRROR_BLOCKS, &height.to_be_bytes())
            .expect("rocks");
    }

    /// The lowest height still mirrored, or `None` when nothing is. One seek:
    /// keys are the height big-endian.
    pub fn lowest(&self) -> Option<u64> {
        let prefix: [u8; 0] = [];
        let mut it = self.db.iter_prefix(CF_MIRROR_BLOCKS, &prefix).ok()?;
        let (k, _) = it.next()?.ok()?;
        if k.len() != 8 {
            return None;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&k);
        Some(u64::from_be_bytes(b))
    }

    pub fn highest(&self) -> Option<u64> {
        let it = self.db.iter_prefix(CF_MIRROR_BLOCKS, &[]).expect("rocks");
        let mut max: Option<u64> = None;
        for kv in it {
            let (k, _) = kv.ok()?;
            if k.len() == 8 {
                let mut a = [0u8; 8];
                a.copy_from_slice(&k);
                let h = u64::from_be_bytes(a);
                max = Some(max.map_or(h, |cur| cur.max(h)));
            }
        }
        max
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Meta;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_and_meta_pointers() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mb = MirrorBlocks::new(&db);
        let h = H256([7; 32]);
        mb.put_with_meta(42, h, 1_700_000_000, b"capnp-bytes", h)
            .unwrap();

        let (got_hash, got_bt, got_bytes) = mb.get(42).unwrap();
        assert_eq!(got_hash, h);
        assert_eq!(got_bt, 1_700_000_000);
        assert_eq!(got_bytes, b"capnp-bytes");

        // Meta head pointers updated in the same atomic write.
        let meta = Meta::new(&db);
        assert_eq!(meta.head_height(), 42);
        assert_eq!(meta.head_hash(), h);
    }

    #[test]
    fn highest_walks_heights_in_be_order() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mb = MirrorBlocks::new(&db);
        for h in [1u64, 7, 3, 10, 5] {
            mb.put_with_meta(h, H256([h as u8; 32]), 0, &[], H256([h as u8; 32]))
                .unwrap();
        }
        assert_eq!(mb.highest(), Some(10));
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mb = MirrorBlocks::new(&db);
        assert!(mb.get(999).is_none());
    }
}
