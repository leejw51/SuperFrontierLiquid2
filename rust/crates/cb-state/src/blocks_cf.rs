//! Blocks CF: stores raw block bytes by height; secondary index by block hash.

use crate::db::{Database, DbBatch, DbError, CF_BLOCKS, CF_BLOCK_HASH};
use cb_types::H256;

pub struct Blocks<'a> {
    db: &'a Database,
}

impl<'a> Blocks<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, height: u64, hash: H256, bytes: &[u8]) {
        self.db
            .put(CF_BLOCKS, &height.to_be_bytes(), bytes)
            .expect("rocks");
        self.db
            .put(CF_BLOCK_HASH, &hash.0, &height.to_be_bytes())
            .expect("rocks");
    }

    /// Same two writes, added to a caller-owned batch instead of issued
    /// directly. Block commit uses this so the block bytes, the hash index,
    /// the state and the head pointer are one atomic write: a crash can then
    /// leave the node at height N or at N-1, never holding a block whose
    /// state was never applied (or state whose block was never stored).
    /// A block's body and hash-index rows as `(column family, key, value)`,
    /// exactly as [`Blocks::put_into_batch`] writes them.
    pub fn rows(height: u64, hash: H256, bytes: Vec<u8>) -> [(&'static str, Vec<u8>, Vec<u8>); 2] {
        [
            (CF_BLOCKS, height.to_be_bytes().to_vec(), bytes),
            (
                CF_BLOCK_HASH,
                hash.0.to_vec(),
                height.to_be_bytes().to_vec(),
            ),
        ]
    }

    pub fn put_into_batch(
        &self,
        batch: &mut DbBatch<'_>,
        height: u64,
        hash: H256,
        bytes: &[u8],
    ) -> Result<(), DbError> {
        batch.put(CF_BLOCKS, &height.to_be_bytes(), bytes)?;
        batch.put(CF_BLOCK_HASH, &hash.0, &height.to_be_bytes())
    }

    pub fn get(&self, height: u64) -> Option<Vec<u8>> {
        self.db
            .get(CF_BLOCKS, &height.to_be_bytes())
            .expect("rocks")
    }

    pub fn height_of(&self, hash: &H256) -> Option<u64> {
        self.db
            .get(CF_BLOCK_HASH, &hash.0)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
    }

    /// Atomically remove the block bytes at `height` and its `hash → height` index
    /// entry. Caller supplies `hash` (it knows it from the block it just decoded) so
    /// `Blocks` stays decoupled from `cb_block::Block`. Pruning never touches state,
    /// trie nodes, or meta.
    /// The lowest height still stored, or `None` when nothing is.
    ///
    /// Keys are the height big-endian, so the first key a forward iterator
    /// yields is the lowest: one seek, whatever the chain's size. Pruning
    /// uses it to start where the blocks actually begin instead of at one.
    pub fn lowest_height(&self) -> Option<u64> {
        let prefix: [u8; 0] = [];
        let mut it = self.db.iter_prefix(CF_BLOCKS, &prefix).ok()?;
        let (k, _) = it.next()?.ok()?;
        if k.len() != 8 {
            return None;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&k);
        Some(u64::from_be_bytes(b))
    }

    pub fn prune_at(&self, height: u64, hash: H256) {
        let mut batch = self.db.batch();
        batch
            .delete(CF_BLOCKS, &height.to_be_bytes())
            .expect("rocks");
        batch.delete(CF_BLOCK_HASH, &hash.0).expect("rocks");
        self.db.write_batch(batch).expect("rocks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prune_removes_bytes_and_hash_index() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let blocks = Blocks::new(&db);
        let h = H256([7; 32]);
        blocks.put(42, h, b"block-bytes");
        assert!(blocks.get(42).is_some());
        assert_eq!(blocks.height_of(&h), Some(42));

        blocks.prune_at(42, h);
        assert!(blocks.get(42).is_none());
        assert_eq!(blocks.height_of(&h), None);
    }

    #[test]
    fn prune_missing_height_is_a_noop() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Blocks::new(&db).prune_at(99, H256([0; 32]));
    }
}
