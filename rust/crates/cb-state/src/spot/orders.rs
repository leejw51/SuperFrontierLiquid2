//! `CF_ORDERS`: `order_id -> SpotOrder`.
//!
//! Auxiliary index — spot orders are not committed to the trie (mirroring the
//! perp choice; see `crate::compute_state_root`). The bincode-encoded
//! `SpotOrder` lives in this CF only; the matcher (S2) and executor (S3) read
//! through `Orders` and write back via the same wrapper.

use crate::db::{Database, CF_ORDERS};
use cb_types::{Address, MarketId, Side, Tif, H256};
use serde::{Deserialize, Serialize};

/// Persistent record for one resting (or in-flight) spot order.
///
/// Field semantics (mirrors `PerpOrder` shape with spot-specific
/// adaptations):
/// - `price` is in 1e8 quote-per-base units, same convention as perp
///   `price_q8`. Spot does not need a separate naming convention because
///   matching uses the integer directly.
/// - `qty_remaining` decreases as fills land; `qty_total` is immutable.
/// - `seq` is the executor-assigned monotonic sequence used for price-time
///   tie-breaking. Wider type than strictly needed (8 bytes) so the
///   `CF_BOOK_INDEX` key can use big-endian seq for free.
/// - `tif` / `post_only` / `client_order_id` mirror perp semantics.
/// - `created_height` is the block height at admission, useful for client UX
///   ("order placed at block N") and for any future TTL logic.
///
/// Spot does not carry `reduce_only` (rejected at admission per S3 plan).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotOrder {
    pub order_id: H256,
    pub market: MarketId,
    pub owner: Address,
    pub side: Side,
    pub price: u128,
    pub qty_remaining: u128,
    pub qty_total: u128,
    pub seq: u64,
    pub tif: Tif,
    pub post_only: bool,
    pub client_order_id: H256,
    pub created_height: u64,
}

pub struct Orders<'a> {
    db: &'a Database,
}

impl<'a> Orders<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, o: &SpotOrder) {
        self.db
            .put(
                CF_ORDERS,
                &o.order_id.0,
                &bincode::serialize(o).expect("SpotOrder encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, order_id: &H256) -> Option<SpotOrder> {
        self.db
            .get(CF_ORDERS, &order_id.0)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("SpotOrder decode"))
    }

    pub fn delete(&self, order_id: &H256) {
        self.db.delete(CF_ORDERS, &order_id.0).expect("rocks");
    }

    /// Walk every persisted spot order. The CF is keyed by `order_id` (no
    /// built-in market index), so this is an O(N) scan over the whole CF.
    /// Output is sorted by `order_id` so any subsequent trie writes the
    /// caller drives are deterministic. Mirror of `PerpOrders::list_for_market`
    /// but spot's market is a tiny enum so we expose the unfiltered iterator
    /// and a per-market filter on top.
    pub fn iter(&self) -> Vec<SpotOrder> {
        let prefix: [u8; 0] = [];
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_ORDERS, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(o) = bincode::deserialize::<SpotOrder>(&v) {
                out.push(o);
            }
        }
        out.sort_by_key(|a| a.order_id.0);
        out
    }

    /// `iter()` filtered to one market. Same O(N) full-scan; v1 scale is
    /// fine — when the order CF outgrows it we add a per-market secondary
    /// index, matching the perp side.
    pub fn list_for_market(&self, market: MarketId) -> Vec<SpotOrder> {
        self.iter()
            .into_iter()
            .filter(|o| o.market == market)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use tempfile::tempdir;

    fn sample(market: MarketId, side: Side, oid: u8) -> SpotOrder {
        SpotOrder {
            order_id: H256([oid; 32]),
            market,
            owner: Address([1; 20]),
            side,
            price: 5_000_000_000,
            qty_remaining: 1_000,
            qty_total: 1_000,
            seq: 7,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([2; 32]),
            created_height: 42,
        }
    }

    #[test]
    fn bincode_roundtrip_preserves_every_field() {
        let o = sample(MarketId::BtcUsd, Side::Buy, 7);
        let bytes = bincode::serialize(&o).unwrap();
        let back: SpotOrder = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, o);
    }

    #[test]
    fn put_get_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let orders = Orders::new(&db);
        let o = sample(MarketId::BtcUsd, Side::Buy, 7);
        orders.put(&o);
        assert_eq!(orders.get(&o.order_id).unwrap(), o);
        orders.delete(&o.order_id);
        assert!(orders.get(&o.order_id).is_none());
    }

    #[test]
    fn delete_then_iter_is_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let orders = Orders::new(&db);
        let a = sample(MarketId::BtcUsd, Side::Buy, 1);
        let b = sample(MarketId::BtcUsd, Side::Sell, 2);
        orders.put(&a);
        orders.put(&b);
        assert_eq!(orders.iter().len(), 2);
        orders.delete(&a.order_id);
        orders.delete(&b.order_id);
        assert!(orders.iter().is_empty());
        assert!(orders.get(&a.order_id).is_none());
        assert!(orders.get(&b.order_id).is_none());
    }

    #[test]
    fn list_for_market_filters() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let orders = Orders::new(&db);
        let a = sample(MarketId::BtcUsd, Side::Buy, 1);
        let b = sample(MarketId::EthUsd, Side::Sell, 2);
        orders.put(&a);
        orders.put(&b);
        let btc = orders.list_for_market(MarketId::BtcUsd);
        assert_eq!(btc.len(), 1);
        assert_eq!(btc[0].order_id, a.order_id);
    }
}
