//! `CF_BOOK_INDEX`: `(market_id, side, price, seq) -> order_id`.
//!
//! Big-endian price + seq give correct lex order for price-time-priority
//! traversal under a `[market || side]` prefix scan. Auxiliary, not in the
//! trie (mirrors the perp orderbook choice — see `crate::compute_state_root`).
//!
//! Layout, value: 32-byte `order_id`. Key layout pinned in `keys.rs`.

use crate::db::{Database, CF_BOOK_INDEX};
use crate::spot::keys::{
    book_index_key, book_index_prefix, BOOK_INDEX_KEY_LEN, BOOK_INDEX_PREFIX_LEN, MARKET_ID_BYTES,
    PRICE_BYTES, SIDE_BYTES,
};
use crate::spot::orders::{Orders, SpotOrder};
use cb_types::{MarketId, Side, H256};

/// Offset of the price field inside a book-index key.
const PRICE_OFFSET: usize = MARKET_ID_BYTES + SIDE_BYTES;
/// Offset of the seq field inside a book-index key.
const SEQ_OFFSET: usize = PRICE_OFFSET + PRICE_BYTES;

pub struct BookIndex<'a> {
    db: &'a Database,
}

impl<'a> BookIndex<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, market: MarketId, side: Side, price: u128, seq: u64, order_id: &H256) {
        let k = book_index_key(market, side, price, seq);
        self.db.put(CF_BOOK_INDEX, &k, &order_id.0).expect("rocks");
    }

    pub fn get(&self, market: MarketId, side: Side, price: u128, seq: u64) -> Option<H256> {
        let k = book_index_key(market, side, price, seq);
        self.db.get(CF_BOOK_INDEX, &k).expect("rocks").map(|b| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&b);
            H256(h)
        })
    }

    pub fn delete(&self, market: MarketId, side: Side, price: u128, seq: u64) {
        let k = book_index_key(market, side, price, seq);
        self.db.delete(CF_BOOK_INDEX, &k).expect("rocks");
    }

    /// Walk one side of one market in CF lex order — i.e. ascending price,
    /// ascending seq within a price. Caller flips to descending for the bid
    /// side via `top_n` or by reversing the output.
    pub fn list_side(&self, market: MarketId, side: Side) -> Vec<(u128, u64, H256)> {
        let prefix = book_index_prefix(market, side);
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_BOOK_INDEX, &prefix) {
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
            if let Some(parsed) = parse_row(&k, &v) {
                out.push(parsed);
            }
        }
        out
    }

    /// Every order resting on `market`, read through this index instead of by
    /// walking `CF_ORDERS`.
    ///
    /// # Why this exists
    ///
    /// [`Orders::list_for_market`] scans the orders CF with an empty prefix —
    /// every row of every market, bincode-decoded and sorted — and then throws
    /// away the rows belonging to other markets. `BlockState::spot_book_mut`
    /// calls it once per market per block, so a node producing ten blocks a
    /// second on two markets pays twenty whole-CF scans a second. A ten-second
    /// sample of a loaded sequencer put 70% of the process's CPU inside that
    /// scan while fewer than a hundred orders were actually resting: the cost
    /// tracked how many orders had ever been filled, not how many were live,
    /// because the iterator still has to walk past every key the fills
    /// deleted.
    ///
    /// This index is keyed `market ‖ side ‖ price ‖ seq`, so one prefix scan
    /// per side sees only this market's live rows, and each is fetched by a
    /// point read. Cost becomes O(resting depth on this market) instead of
    /// O(every order the chain has ever written).
    ///
    /// # Why the two agree
    ///
    /// Every executor path that writes `CF_ORDERS` writes this index in the
    /// same breath, and every path that deletes from one deletes from the
    /// other; a partial fill changes `qty_remaining` only, which is not part of
    /// the index key. The result is sorted by `order_id` so it is identical,
    /// element for element, to what the full scan returns.
    /// `index_matches_full_scan` is the test that pins the two together.
    pub fn list_orders_for_market(&self, orders: &Orders<'_>, market: MarketId) -> Vec<SpotOrder> {
        let mut out = Vec::new();
        for side in [Side::Buy, Side::Sell] {
            for (_price, _seq, order_id) in self.list_side(market, side) {
                if let Some(o) = orders.get(&order_id) {
                    out.push(o);
                }
            }
        }
        out.sort_by_key(|a| a.order_id.0);
        out
    }

    /// Aggregate the top-`depth` price levels for `(market, side)`, summing
    /// the `qty_remaining` of every resting order at each level. Used by the
    /// S4 RPC `getOrderBook` to produce the depth-of-book response.
    ///
    /// Ordering, by side:
    /// - **Sell (asks).** Best ask first → ascending price. CF lex order is
    ///   already ascending(price, seq); we forward-iterate, group by price,
    ///   and stop after `depth` distinct levels.
    /// - **Buy (bids).** Best bid first → descending price. We reverse-iterate
    ///   from the upper bound of the prefix and group by price the same way.
    ///
    /// The returned `Vec` carries one `(price, qty_aggregated)` row per
    /// level, in the natural client-facing order for that side. Orders
    /// missing from `CF_ORDERS` (e.g. mid-flight delete with the index row
    /// not yet cleaned up — should be impossible, but defensive) are skipped
    /// without contributing to the qty sum.
    pub fn top_n(
        &self,
        orders: &Orders<'_>,
        market: MarketId,
        side: Side,
        depth: usize,
    ) -> Vec<(u128, u128)> {
        if depth == 0 {
            return Vec::new();
        }
        match side {
            Side::Sell => self.top_n_ascending(orders, market, side, depth),
            Side::Buy => self.top_n_descending(orders, market, side, depth),
        }
    }

    fn top_n_ascending(
        &self,
        orders: &Orders<'_>,
        market: MarketId,
        side: Side,
        depth: usize,
    ) -> Vec<(u128, u128)> {
        let prefix = book_index_prefix(market, side);
        let mut out: Vec<(u128, u128)> = Vec::with_capacity(depth);
        let mut current_price: Option<u128> = None;
        let mut current_qty: u128 = 0;

        let it = match self.db.iter_prefix(CF_BOOK_INDEX, &prefix) {
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
            let (price, _seq, oid) = match parse_row(&k, &v) {
                Some(r) => r,
                None => continue,
            };
            // Crossing into a new price level: flush the previous one.
            if Some(price) != current_price {
                if let Some(p) = current_price {
                    out.push((p, current_qty));
                    if out.len() >= depth {
                        return out;
                    }
                }
                current_price = Some(price);
                current_qty = 0;
            }
            if let Some(o) = orders.get(&oid) {
                current_qty = current_qty.saturating_add(o.qty_remaining);
            }
        }
        if let Some(p) = current_price {
            if out.len() < depth {
                out.push((p, current_qty));
            }
        }
        out
    }

    fn top_n_descending(
        &self,
        orders: &Orders<'_>,
        market: MarketId,
        side: Side,
        depth: usize,
    ) -> Vec<(u128, u128)> {
        let prefix = book_index_prefix(market, side);
        let upper_bound = next_prefix(&prefix);
        let mut out: Vec<(u128, u128)> = Vec::with_capacity(depth);
        let mut current_price: Option<u128> = None;
        let mut current_qty: u128 = 0;

        let it = match self.db.iter_prefix_reverse(CF_BOOK_INDEX, &upper_bound) {
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
            let (price, _seq, oid) = match parse_row(&k, &v) {
                Some(r) => r,
                None => continue,
            };
            if Some(price) != current_price {
                if let Some(p) = current_price {
                    out.push((p, current_qty));
                    if out.len() >= depth {
                        return out;
                    }
                }
                current_price = Some(price);
                current_qty = 0;
            }
            if let Some(o) = orders.get(&oid) {
                current_qty = current_qty.saturating_add(o.qty_remaining);
            }
        }
        if let Some(p) = current_price {
            if out.len() < depth {
                out.push((p, current_qty));
            }
        }
        out
    }
}

/// Parse a `(key, value)` pair pulled out of `CF_BOOK_INDEX`. Returns
/// `(price, seq, order_id)` or `None` if the row's shape doesn't match.
fn parse_row(k: &[u8], v: &[u8]) -> Option<(u128, u64, H256)> {
    if k.len() != BOOK_INDEX_KEY_LEN || v.len() != 32 {
        return None;
    }
    let mut px = [0u8; PRICE_BYTES];
    px.copy_from_slice(&k[PRICE_OFFSET..PRICE_OFFSET + PRICE_BYTES]);
    let mut sq = [0u8; 8];
    sq.copy_from_slice(&k[SEQ_OFFSET..]);
    let mut id = [0u8; 32];
    id.copy_from_slice(v);
    Some((u128::from_be_bytes(px), u64::from_be_bytes(sq), H256(id)))
}

/// Lex-next byte string after `prefix`. Used as the seek anchor for reverse
/// iteration over the bid side. Mirrors the perp helper of the same name.
fn next_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    while let Some(last) = out.last_mut() {
        if *last < 0xff {
            *last += 1;
            return out;
        }
        out.pop();
    }
    out
}

// Make sure the prefix-len constant is consumed somewhere visible to the
// linker so a future refactor doesn't silently drop it from the public
// surface. (Compiler would warn anyway, but the const is part of the layout
// contract documented in `keys.rs`.)
#[allow(dead_code)]
const _PREFIX_LEN_LIVE: usize = BOOK_INDEX_PREFIX_LEN;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spot::orders::SpotOrder;
    use crate::Database;
    use cb_types::{Address, Tif};
    use tempfile::tempdir;

    fn order_at(
        market: MarketId,
        side: Side,
        oid: u8,
        price: u128,
        seq: u64,
        qty: u128,
    ) -> SpotOrder {
        SpotOrder {
            order_id: H256([oid; 32]),
            market,
            owner: Address([1; 20]),
            side,
            price,
            qty_remaining: qty,
            qty_total: qty,
            seq,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([0; 32]),
            created_height: 1,
        }
    }

    /// The reproduction for the whole-CF scan this index replaces.
    ///
    /// The database is loaded the way a running chain loads it: two markets
    /// trading side by side, and far more orders already filled and deleted
    /// than left resting. The index path must return exactly what the full
    /// scan returns — same orders, same order — while reading only the live
    /// rows of the market asked for. If the two ever disagree, a leader
    /// hydrating through the index and a replica hydrating through the scan
    /// would build different books from the same database, so this equality is
    /// the safety property the optimisation rests on.
    #[test]
    fn index_matches_full_scan() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let orders = Orders::new(&db);
        let bi = BookIndex::new(&db);

        // 40 orders across two markets and both sides, written exactly as the
        // executor writes them: the row and its index entry together.
        let mut written = Vec::new();
        for i in 0..40u8 {
            let market = if i % 2 == 0 {
                MarketId::BtcUsd
            } else {
                MarketId::EthUsd
            };
            let side = if i % 4 < 2 { Side::Buy } else { Side::Sell };
            let o = order_at(market, side, i, 1_000 + u128::from(i), u64::from(i), 500);
            orders.put(&o);
            bi.put(o.market, o.side, o.price, o.seq, &o.order_id);
            written.push(o);
        }

        // Three quarters of them fill and are deleted, again through both.
        // These are the keys the full scan still walks and the prefix scan of
        // this index does not.
        for o in written.iter().filter(|o| o.order_id.0[0] % 4 != 0) {
            bi.delete(o.market, o.side, o.price, o.seq);
            orders.delete(&o.order_id);
        }

        // One partial fill: qty_remaining is not part of the index key, so the
        // index is untouched and the point read must still return the new
        // quantity.
        let mut partial = written
            .iter()
            .find(|o| o.order_id.0[0] == 0)
            .cloned()
            .expect("order 0 survives");
        partial.qty_remaining = 125;
        orders.put(&partial);

        for market in [MarketId::BtcUsd, MarketId::EthUsd] {
            let via_index = bi.list_orders_for_market(&orders, market);
            let via_scan = orders.list_for_market(market);
            assert_eq!(
                via_index, via_scan,
                "index and full scan disagree on {market:?}"
            );
            assert!(
                via_index.iter().all(|o| o.market == market),
                "index returned another market's order"
            );
        }

        assert_eq!(
            bi.list_orders_for_market(&orders, MarketId::BtcUsd)
                .iter()
                .find(|o| o.order_id.0[0] == 0)
                .map(|o| o.qty_remaining),
            Some(125),
            "a partial fill must be visible through the index"
        );
    }

    /// An index row whose order row is gone contributes nothing rather than
    /// panicking. The executor deletes both together, so this cannot happen
    /// today; the assertion is here so it stays a skipped row if it ever does.
    #[test]
    fn index_row_without_an_order_row_is_skipped() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let orders = Orders::new(&db);
        let bi = BookIndex::new(&db);
        let live = order_at(MarketId::BtcUsd, Side::Buy, 1, 100, 0, 10);
        orders.put(&live);
        bi.put(live.market, live.side, live.price, live.seq, &live.order_id);
        bi.put(MarketId::BtcUsd, Side::Buy, 200, 1, &H256([99; 32]));

        let got = bi.list_orders_for_market(&orders, MarketId::BtcUsd);
        assert_eq!(got, vec![live]);
    }

    #[test]
    fn put_get_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let oid = H256([9; 32]);
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 0, &oid);
        assert_eq!(bi.get(MarketId::BtcUsd, Side::Buy, 100, 0).unwrap(), oid);
        bi.delete(MarketId::BtcUsd, Side::Buy, 100, 0);
        assert!(bi.get(MarketId::BtcUsd, Side::Buy, 100, 0).is_none());
    }

    #[test]
    fn list_side_returns_lex_order_buys() {
        // Inserted out of order; CF lex iteration must surface them as
        // ascending(price, seq) regardless of the side.
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        bi.put(MarketId::BtcUsd, Side::Buy, 200, 0, &H256([1; 32]));
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 1, &H256([2; 32]));
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 0, &H256([3; 32]));
        // Different side / market should not leak in.
        bi.put(MarketId::BtcUsd, Side::Sell, 100, 0, &H256([4; 32]));
        bi.put(MarketId::EthUsd, Side::Buy, 100, 0, &H256([5; 32]));

        let buys = bi.list_side(MarketId::BtcUsd, Side::Buy);
        assert_eq!(buys.len(), 3);
        assert_eq!(buys[0], (100, 0, H256([3; 32])));
        assert_eq!(buys[1], (100, 1, H256([2; 32])));
        assert_eq!(buys[2], (200, 0, H256([1; 32])));
    }

    #[test]
    fn list_side_returns_lex_order_sells() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        bi.put(MarketId::BtcUsd, Side::Sell, 110, 0, &H256([1; 32]));
        bi.put(MarketId::BtcUsd, Side::Sell, 105, 1, &H256([2; 32]));
        bi.put(MarketId::BtcUsd, Side::Sell, 105, 0, &H256([3; 32]));
        let asks = bi.list_side(MarketId::BtcUsd, Side::Sell);
        assert_eq!(asks.len(), 3);
        assert_eq!(asks[0], (105, 0, H256([3; 32])));
        assert_eq!(asks[1], (105, 1, H256([2; 32])));
        assert_eq!(asks[2], (110, 0, H256([1; 32])));
    }

    #[test]
    fn top_n_zero_returns_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let orders = Orders::new(&db);
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 0, &H256([1; 32]));
        assert!(bi.top_n(&orders, MarketId::BtcUsd, Side::Buy, 0).is_empty());
    }

    #[test]
    fn top_n_empty_book_returns_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let orders = Orders::new(&db);
        assert!(bi.top_n(&orders, MarketId::BtcUsd, Side::Buy, 5).is_empty());
        assert!(bi
            .top_n(&orders, MarketId::BtcUsd, Side::Sell, 5)
            .is_empty());
    }

    #[test]
    fn top_n_asks_aggregates_lowest_price_first() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let orders = Orders::new(&db);
        // Ask side with two orders at price 100 (qtys 3 and 5 → 8 total)
        // and one order at 105 (qty 2), 110 (qty 1).
        let entries = [
            (Side::Sell, 100u128, 0u64, 1u8, 3u128),
            (Side::Sell, 100, 1, 2, 5),
            (Side::Sell, 105, 0, 3, 2),
            (Side::Sell, 110, 0, 4, 1),
        ];
        for (side, price, seq, oid, qty) in entries {
            let o = order_at(MarketId::BtcUsd, side, oid, price, seq, qty);
            orders.put(&o);
            bi.put(MarketId::BtcUsd, side, price, seq, &o.order_id);
        }
        // Different market should not leak in.
        let other = order_at(MarketId::EthUsd, Side::Sell, 9, 100, 0, 999);
        orders.put(&other);
        bi.put(MarketId::EthUsd, Side::Sell, 100, 0, &other.order_id);

        let top2 = bi.top_n(&orders, MarketId::BtcUsd, Side::Sell, 2);
        assert_eq!(top2, vec![(100, 8), (105, 2)]);
        let top10 = bi.top_n(&orders, MarketId::BtcUsd, Side::Sell, 10);
        assert_eq!(top10, vec![(100, 8), (105, 2), (110, 1)]);
    }

    #[test]
    fn top_n_bids_aggregates_highest_price_first() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let orders = Orders::new(&db);
        // Bids: 95 (4), 100×2 → 7, 90 (1).
        let entries = [
            (Side::Buy, 95u128, 0u64, 1u8, 4u128),
            (Side::Buy, 100, 2, 2, 2),
            (Side::Buy, 100, 0, 3, 5),
            (Side::Buy, 90, 0, 4, 1),
        ];
        for (side, price, seq, oid, qty) in entries {
            let o = order_at(MarketId::BtcUsd, side, oid, price, seq, qty);
            orders.put(&o);
            bi.put(MarketId::BtcUsd, side, price, seq, &o.order_id);
        }
        let top2 = bi.top_n(&orders, MarketId::BtcUsd, Side::Buy, 2);
        assert_eq!(top2, vec![(100, 7), (95, 4)]);
        let top10 = bi.top_n(&orders, MarketId::BtcUsd, Side::Buy, 10);
        assert_eq!(top10, vec![(100, 7), (95, 4), (90, 1)]);
    }

    #[test]
    fn top_n_caps_distinct_levels_not_orders() {
        // Top level has 5 orders at price 100; depth=1 must surface a single
        // aggregated level summing all five remaining qtys.
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        let orders = Orders::new(&db);
        let mut total: u128 = 0;
        for s in 0u64..5 {
            let qty = (s as u128 + 1) * 10;
            total += qty;
            let o = order_at(MarketId::BtcUsd, Side::Buy, s as u8, 100, s, qty);
            orders.put(&o);
            bi.put(MarketId::BtcUsd, Side::Buy, 100, s, &o.order_id);
        }
        let lower = order_at(MarketId::BtcUsd, Side::Buy, 9, 90, 0, 999);
        orders.put(&lower);
        bi.put(MarketId::BtcUsd, Side::Buy, 90, 0, &lower.order_id);

        let top1 = bi.top_n(&orders, MarketId::BtcUsd, Side::Buy, 1);
        assert_eq!(top1, vec![(100, total)]);
    }

    #[test]
    fn delete_then_iter_is_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = BookIndex::new(&db);
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 0, &H256([1; 32]));
        bi.put(MarketId::BtcUsd, Side::Buy, 100, 1, &H256([2; 32]));
        bi.put(MarketId::BtcUsd, Side::Sell, 105, 0, &H256([3; 32]));
        assert_eq!(bi.list_side(MarketId::BtcUsd, Side::Buy).len(), 2);
        assert_eq!(bi.list_side(MarketId::BtcUsd, Side::Sell).len(), 1);
        bi.delete(MarketId::BtcUsd, Side::Buy, 100, 0);
        bi.delete(MarketId::BtcUsd, Side::Buy, 100, 1);
        bi.delete(MarketId::BtcUsd, Side::Sell, 105, 0);
        assert!(bi.list_side(MarketId::BtcUsd, Side::Buy).is_empty());
        assert!(bi.list_side(MarketId::BtcUsd, Side::Sell).is_empty());
    }

    #[test]
    fn next_prefix_increments_last_byte() {
        assert_eq!(next_prefix(&[0x01, 0x02]), vec![0x01, 0x03]);
        assert_eq!(next_prefix(&[0x01, 0xff]), vec![0x02]);
        assert_eq!(next_prefix(&[0xff, 0xff]), Vec::<u8>::new());
    }
}
