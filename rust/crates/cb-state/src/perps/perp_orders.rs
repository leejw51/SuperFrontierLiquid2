//! `CF_PERP_ORDERS`: `order_id -> PerpOrder`.
//!
//! Auxiliary index — perp orders are not committed to the trie (Hyperliquid-style:
//! ephemeral matching state recomputes on replay). `PerpOrder` itself lives in
//! `cb-types` so the M3 matching engine in `cb-engine` can operate over it
//! without pulling RocksDB into the engine; this module is a thin storage codec.

use crate::db::{Database, CF_PERP_ORDERS, CF_PERP_ORDERS_BY_OWNER};
use cb_types::{Address, PerpOrder, H256};

/// `(owner: 20 || order_id: 32)`.
fn by_owner_key(owner: &Address, order_id: &H256) -> [u8; 52] {
    let mut k = [0u8; 52];
    k[..20].copy_from_slice(&owner.0);
    k[20..].copy_from_slice(&order_id.0);
    k
}

pub struct PerpOrders<'a> {
    db: &'a Database,
}

impl<'a> PerpOrders<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Write the order and its `(owner, order_id)` index entry. This is the
    /// only door through which an order reaches either CF, so the index
    /// cannot drift from the rows it indexes.
    pub fn put(&self, o: &PerpOrder) {
        self.db
            .put(
                CF_PERP_ORDERS,
                &o.order_id.0,
                &bincode::serialize(o).expect("PerpOrder encode"),
            )
            .expect("rocks");
        self.db
            .put(
                CF_PERP_ORDERS_BY_OWNER,
                &by_owner_key(&o.owner, &o.order_id),
                &[],
            )
            .expect("rocks");
    }

    pub fn get(&self, order_id: &H256) -> Option<PerpOrder> {
        self.db
            .get(CF_PERP_ORDERS, &order_id.0)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("PerpOrder decode"))
    }

    /// Delete the order and its owner-index entry. The row is read first for
    /// the owner; a delete of an order that does not exist touches nothing.
    pub fn delete(&self, order_id: &H256) {
        if let Some(o) = self.get(order_id) {
            self.db
                .delete(CF_PERP_ORDERS_BY_OWNER, &by_owner_key(&o.owner, order_id))
                .expect("rocks");
        }
        self.db.delete(CF_PERP_ORDERS, &order_id.0).expect("rocks");
    }

    /// Every resting order belonging to `owner`, in `order_id` byte order.
    /// One prefix scan over the owner index plus one point read per order;
    /// never a scan of the order CF. (QA F11: the margin gate needs this on
    /// every placement, and without an index every placement paid a full
    /// scan — which is why resting orders reserved no collateral at all.)
    pub fn list_for_owner(&self, owner: &Address) -> Vec<PerpOrder> {
        let prefix = owner.0;
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_PERP_ORDERS_BY_OWNER, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (k, _) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() != 52 {
                continue;
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&k[20..]);
            if let Some(o) = self.get(&H256(id)) {
                out.push(o);
            }
        }
        out
    }

    /// Walk every resting order belonging to `market`. The CF is keyed by
    /// `order_id` (no built-in market index), so we full-scan and filter.
    /// Output is sorted by `order_id` so any subsequent trie writes the
    /// caller drives are deterministic.
    ///
    /// O(N) over the entire order CF — same shape as `list_for_market` on
    /// positions. At v1 scale (a handful of markets, tens of thousands of
    /// resting orders) this is fine. M-future could add a per-market index.
    pub fn list_for_market(&self, market: &cb_types::DerivId) -> Vec<PerpOrder> {
        let prefix: [u8; 0] = [];
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_PERP_ORDERS, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(o) = bincode::deserialize::<PerpOrder>(&v) {
                if o.market == *market {
                    out.push(o);
                }
            }
        }
        out.sort_by_key(|a| a.order_id.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::{Address, DerivId, Side, Tif};
    use tempfile::tempdir;

    fn sample() -> PerpOrder {
        PerpOrder {
            order_id: H256([7; 32]),
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 5_000_000_000_000,
            qty: 1_000,
            remaining_qty: 1_000,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            seq: 1,
            placed_at_height: 42,
            client_order_id: H256([2; 32]),
        }
    }

    #[test]
    fn put_get_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = PerpOrders::new(&db);
        let o = sample();
        os.put(&o);
        assert_eq!(os.get(&o.order_id).unwrap(), o);
        os.delete(&o.order_id);
        assert!(os.get(&o.order_id).is_none());
    }
}
