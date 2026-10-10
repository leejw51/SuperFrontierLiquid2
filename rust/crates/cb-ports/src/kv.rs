//! Ordered key-value storage, as used for consensus state.
//!
//! The default implementation is RocksDB. The trait exists so that the merkle
//! trie and column-family layers can be tested against an in-memory map, and
//! so a deployment can substitute another ordered store.
//!
//! **Ordering is load-bearing.** [`KvStore::scan_prefix`] must yield keys in
//! ascending lexicographic byte order. The state root depends on it; an
//! implementation that iterates in another order will fork the chain.

use crate::error::PortResult;

/// A single write in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Set `key` to `value`.
    Put {
        /// Key to write.
        key: Vec<u8>,
        /// Value to store.
        value: Vec<u8>,
    },
    /// Remove `key` if present.
    Delete {
        /// Key to remove.
        key: Vec<u8>,
    },
}

/// An atomic group of writes.
#[derive(Debug, Clone, Default)]
pub struct KvBatch {
    ops: Vec<WriteOp>,
}

impl KvBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a put.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(WriteOp::Put {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Queue a delete.
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(WriteOp::Delete { key: key.into() });
        self
    }

    /// The queued operations, in insertion order.
    pub fn ops(&self) -> &[WriteOp] {
        &self.ops
    }

    /// Number of queued operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// An ordered, namespaced key-value store.
///
/// `namespace` maps to a RocksDB column family, a table prefix, or whatever
/// the backend uses to keep key spaces apart.
pub trait KvStore: Send + Sync + 'static {
    /// Read one key. `Ok(None)` means absent, which is not an error.
    fn get(&self, namespace: &str, key: &[u8]) -> PortResult<Option<Vec<u8>>>;

    /// Apply a batch atomically: all writes land, or none do.
    fn write_batch(&self, namespace: &str, batch: KvBatch) -> PortResult<()>;

    /// Every key-value pair whose key starts with `prefix`, in ascending
    /// lexicographic key order.
    fn scan_prefix(&self, namespace: &str, prefix: &[u8]) -> PortResult<Vec<(Vec<u8>, Vec<u8>)>>;

    /// Flush buffered writes to durable storage.
    fn flush(&self) -> PortResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_preserves_insertion_order() {
        let mut b = KvBatch::new();
        b.put(b"a".to_vec(), b"1".to_vec())
            .delete(b"b".to_vec())
            .put(b"c".to_vec(), b"3".to_vec());
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
        assert_eq!(
            b.ops()[0],
            WriteOp::Put {
                key: b"a".to_vec(),
                value: b"1".to_vec()
            }
        );
        assert_eq!(b.ops()[1], WriteOp::Delete { key: b"b".to_vec() });
    }

    #[test]
    fn empty_batch_reports_empty() {
        assert!(KvBatch::new().is_empty());
    }
}
