//! Deterministic binary Merkle trie over an arbitrary KV store.

pub mod node;
pub mod trie;

use cb_types::H256;
use std::collections::HashMap;
use thiserror::Error;

pub use trie::MerkleTrie;

#[derive(Debug, Error)]
pub enum TrieError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("decode error: {0}")]
    Decode(String),
}

pub trait KvStore: Send + Sync {
    fn get(&self, key: &H256) -> Result<Option<Vec<u8>>, TrieError>;
    fn put(&mut self, key: H256, value: Vec<u8>) -> Result<(), TrieError>;
    fn delete(&mut self, key: &H256) -> Result<(), TrieError>;

    /// Batched read. Default impl loops over `get`; backends with native
    /// batched primitives (rocksdb's `multi_get_cf`) override this to save
    /// one round-trip per key. The trie's `prefetch_paths` calls this once
    /// per BFS depth level, collapsing a per-key 32-deep walk into a
    /// per-level batch.
    ///
    /// Output length must equal input length, with `None` for missing keys.
    fn multi_get(&self, keys: &[H256]) -> Result<Vec<Option<Vec<u8>>>, TrieError> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            out.push(self.get(k)?);
        }
        Ok(out)
    }

    /// Hint that the implementation should pre-populate any cache it owns
    /// with the values for these keys. The default impl is a no-op (the
    /// node will be fetched on demand). `BufferedKv` overrides this to do
    /// a single batched read and stash everything in its in-memory map.
    fn prefetch(&mut self, _keys: &[H256]) -> Result<(), TrieError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemKv(HashMap<H256, Vec<u8>>);

impl KvStore for MemKv {
    fn get(&self, key: &H256) -> Result<Option<Vec<u8>>, TrieError> {
        Ok(self.0.get(key).cloned())
    }
    fn put(&mut self, key: H256, value: Vec<u8>) -> Result<(), TrieError> {
        self.0.insert(key, value);
        Ok(())
    }
    fn delete(&mut self, key: &H256) -> Result<(), TrieError> {
        self.0.remove(key);
        Ok(())
    }
}
