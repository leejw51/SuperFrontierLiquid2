//! `CF_LIQUIDATION_QUEUE`: `(height, seq) -> LiquidationEntry`.
//!
//! Trigger txs append to this queue; the engine drains it at end-of-block in
//! `(height, seq)` order. M1 ships only the codec — drain logic is M4.

use crate::db::{Database, CF_LIQUIDATION_QUEUE};
use crate::perps::keys::liquidation_queue_key;
use cb_types::LiquidationEntry;

pub struct LiquidationQueue<'a> {
    db: &'a Database,
}

impl<'a> LiquidationQueue<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, e: &LiquidationEntry) {
        let k = liquidation_queue_key(e.triggered_at_height, e.seq);
        self.db
            .put(
                CF_LIQUIDATION_QUEUE,
                &k,
                &bincode::serialize(e).expect("LiquidationEntry encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, height: u64, seq: u64) -> Option<LiquidationEntry> {
        let k = liquidation_queue_key(height, seq);
        self.db
            .get(CF_LIQUIDATION_QUEUE, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("LiquidationEntry decode"))
    }

    pub fn delete(&self, height: u64, seq: u64) {
        let k = liquidation_queue_key(height, seq);
        self.db.delete(CF_LIQUIDATION_QUEUE, &k).expect("rocks");
    }

    /// Drain entries at exactly `height` in `seq` order. Used by M4's
    /// liquidation processor. Empty if no entries at that height.
    pub fn list_at_height(&self, height: u64) -> Vec<LiquidationEntry> {
        let prefix = height.to_be_bytes();
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_LIQUIDATION_QUEUE, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if let Ok(e) = bincode::deserialize::<LiquidationEntry>(&v) {
                out.push(e);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::{Address, DerivId};
    use tempfile::tempdir;

    fn entry(height: u64, seq: u64) -> LiquidationEntry {
        LiquidationEntry {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            triggered_by: Address([2; 20]),
            triggered_at_height: height,
            mark_at_trigger_q8: 5_000_000_000_000,
            seq,
        }
    }

    #[test]
    fn put_get_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let q = LiquidationQueue::new(&db);
        q.put(&entry(100, 0));
        assert_eq!(q.get(100, 0).unwrap(), entry(100, 0));
        q.delete(100, 0);
        assert!(q.get(100, 0).is_none());
    }

    #[test]
    fn list_at_height_returns_in_seq_order() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let q = LiquidationQueue::new(&db);
        // Insert out of order; should come back sorted by seq.
        q.put(&entry(100, 2));
        q.put(&entry(100, 0));
        q.put(&entry(101, 0));
        q.put(&entry(100, 1));
        let at_100 = q.list_at_height(100);
        assert_eq!(at_100.len(), 3);
        assert_eq!(at_100[0].seq, 0);
        assert_eq!(at_100[1].seq, 1);
        assert_eq!(at_100[2].seq, 2);
        let at_101 = q.list_at_height(101);
        assert_eq!(at_101.len(), 1);
    }
}
