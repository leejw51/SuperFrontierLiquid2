//! `CF_PERP_BOOK_INDEX`: `(deriv_id, side, price_q8, seq) -> order_id`.
//!
//! Big-endian price/seq give correct lex order for price-time-priority traversal
//! when prefix-iterating with `[deriv_id][side]`. Auxiliary, not in the trie.

use crate::db::{Database, CF_PERP_BOOK_INDEX};
use crate::perps::keys::{perp_book_index_key, perp_book_index_prefix};
use cb_types::{DerivId, Side, H256};

pub struct PerpBookIndex<'a> {
    db: &'a Database,
}

impl<'a> PerpBookIndex<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, market: &DerivId, side: Side, price_q8: u128, seq: u64, order_id: &H256) {
        let k = perp_book_index_key(market, side, price_q8, seq);
        self.db
            .put(CF_PERP_BOOK_INDEX, &k, &order_id.0)
            .expect("rocks");
    }

    pub fn get(&self, market: &DerivId, side: Side, price_q8: u128, seq: u64) -> Option<H256> {
        let k = perp_book_index_key(market, side, price_q8, seq);
        self.db
            .get(CF_PERP_BOOK_INDEX, &k)
            .expect("rocks")
            .map(|b| {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            })
    }

    pub fn delete(&self, market: &DerivId, side: Side, price_q8: u128, seq: u64) {
        let k = perp_book_index_key(market, side, price_q8, seq);
        self.db.delete(CF_PERP_BOOK_INDEX, &k).expect("rocks");
    }

    /// Walk a market's side in lex order (= ascending price, ascending seq
    /// within price). Caller iterates ascending or reverses for descending.
    pub fn list_side(&self, market: &DerivId, side: Side) -> Vec<(u128, u64, H256)> {
        let prefix = perp_book_index_prefix(market, side);
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_PERP_BOOK_INDEX, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) || k.len() != 29 || v.len() != 32 {
                if !k.starts_with(&prefix) {
                    break;
                }
                continue;
            }
            let mut px = [0u8; 16];
            px.copy_from_slice(&k[5..21]);
            let mut sq = [0u8; 8];
            sq.copy_from_slice(&k[21..]);
            let mut id = [0u8; 32];
            id.copy_from_slice(&v);
            out.push((u128::from_be_bytes(px), u64::from_be_bytes(sq), H256(id)));
        }
        out
    }

    /// Top-N entries on a side in price-time priority order without
    /// materialising the whole side. Used by depth-of-book RPC and by the
    /// matcher's hot path when M3+ wires it in (today the matcher still
    /// pulls the full snapshot — `best_n` is the seam for the next round).
    ///
    /// Ordering, by side:
    /// - **Sell (asks).** Ascending price, ascending seq within a price.
    ///   The CF lex order already encodes this: forward prefix scan, take
    ///   the first N rows.
    /// - **Buy (bids).** Descending price, **ascending seq** within a
    ///   price. The CF lex order gives ascending(price, seq), so a naive
    ///   reverse iterator yields descending(price, seq) — which violates
    ///   time priority within a level. We use the reverse iterator to
    ///   discover the top-N price levels cheaply, then re-order each
    ///   level's entries by seq ascending before appending.
    ///
    /// Either way the read is O(N + distinct_top_prices), not O(book).
    pub fn best_n(&self, market: &DerivId, side: Side, n: usize) -> Vec<(u128, u64, H256)> {
        if n == 0 {
            return Vec::new();
        }
        match side {
            Side::Sell => self.best_n_ascending(market, side, n),
            Side::Buy => self.best_n_descending(market, side, n),
        }
    }

    /// Asks: forward prefix iteration, stop after N rows. CF-native ordering
    /// = price-time priority for the sell side.
    fn best_n_ascending(&self, market: &DerivId, side: Side, n: usize) -> Vec<(u128, u64, H256)> {
        let prefix = perp_book_index_prefix(market, side);
        let mut out: Vec<(u128, u64, H256)> = Vec::with_capacity(n);
        let it = match self.db.iter_prefix(CF_PERP_BOOK_INDEX, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            if out.len() >= n {
                break;
            }
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() != 29 || v.len() != 32 {
                continue;
            }
            let mut px = [0u8; 16];
            px.copy_from_slice(&k[5..21]);
            let mut sq = [0u8; 8];
            sq.copy_from_slice(&k[21..]);
            let mut id = [0u8; 32];
            id.copy_from_slice(&v);
            out.push((u128::from_be_bytes(px), u64::from_be_bytes(sq), H256(id)));
        }
        out
    }

    /// Bids: walk the prefix in reverse to surface descending prices, but
    /// re-sort each price level by seq ascending so time priority holds.
    /// We seek to the upper-bound key (just past the prefix) and iterate
    /// `Reverse`, gathering one whole price level at a time.
    fn best_n_descending(&self, market: &DerivId, side: Side, n: usize) -> Vec<(u128, u64, H256)> {
        let prefix = perp_book_index_prefix(market, side);
        let upper_bound = next_prefix(&prefix);
        let mut out: Vec<(u128, u64, H256)> = Vec::with_capacity(n);
        let it = match self
            .db
            .iter_prefix_reverse(CF_PERP_BOOK_INDEX, &upper_bound)
        {
            Ok(it) => it,
            Err(_) => return out,
        };

        // Buffer the current top price level. Reverse iteration gives us
        // descending(price, seq), so as we pop entries we accumulate a
        // single price's rows then flush them sorted by seq ascending
        // before moving to the next-lower price.
        let mut level_price: Option<u128> = None;
        let mut level_buf: Vec<(u64, H256)> = Vec::new();

        let flush_level = |out: &mut Vec<(u128, u64, H256)>,
                           level_price: &mut Option<u128>,
                           level_buf: &mut Vec<(u64, H256)>,
                           cap: usize| {
            if let Some(px) = level_price.take() {
                level_buf.sort_by_key(|(seq, _)| *seq);
                for (sq, id) in level_buf.drain(..) {
                    if out.len() >= cap {
                        break;
                    }
                    out.push((px, sq, id));
                }
            }
        };

        for kv in it {
            if out.len() >= n {
                break;
            }
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() != 29 || v.len() != 32 {
                continue;
            }
            let mut px = [0u8; 16];
            px.copy_from_slice(&k[5..21]);
            let mut sq = [0u8; 8];
            sq.copy_from_slice(&k[21..]);
            let mut id = [0u8; 32];
            id.copy_from_slice(&v);
            let price = u128::from_be_bytes(px);
            let seq = u64::from_be_bytes(sq);
            let h = H256(id);
            match level_price {
                Some(cur) if cur == price => {
                    level_buf.push((seq, h));
                }
                _ => {
                    flush_level(&mut out, &mut level_price, &mut level_buf, n);
                    if out.len() >= n {
                        return out;
                    }
                    level_price = Some(price);
                    level_buf.push((seq, h));
                }
            }
        }
        // Drain any partially-buffered top level.
        flush_level(&mut out, &mut level_price, &mut level_buf, n);
        out
    }
}

/// Compute the lex-next byte string after `prefix`. Used as the seek target
/// for reverse iteration: we want the iterator to start at the largest key
/// strictly less than `next_prefix(prefix)` so it lands on the highest key
/// that still belongs to `prefix`. If `prefix` is empty or all `0xff`,
/// returns an empty Vec (which rocksdb interprets as "before everything",
/// useful as a no-op upper bound).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use proptest::prelude::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_delete_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        let oid = H256([9; 32]);
        bi.put(&m, Side::Buy, 100, 0, &oid);
        assert_eq!(bi.get(&m, Side::Buy, 100, 0).unwrap(), oid);
        bi.delete(&m, Side::Buy, 100, 0);
        assert!(bi.get(&m, Side::Buy, 100, 0).is_none());
    }

    #[test]
    fn list_side_returns_lex_order() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        // Insert out of order; should come back ordered by (price, seq).
        bi.put(&m, Side::Buy, 200, 0, &H256([1; 32]));
        bi.put(&m, Side::Buy, 100, 1, &H256([2; 32]));
        bi.put(&m, Side::Buy, 100, 0, &H256([3; 32]));
        // Different side should not appear.
        bi.put(&m, Side::Sell, 100, 0, &H256([4; 32]));

        let buys = bi.list_side(&m, Side::Buy);
        assert_eq!(buys.len(), 3);
        assert_eq!(buys[0].0, 100);
        assert_eq!(buys[0].1, 0);
        assert_eq!(buys[1].0, 100);
        assert_eq!(buys[1].1, 1);
        assert_eq!(buys[2].0, 200);

        let sells = bi.list_side(&m, Side::Sell);
        assert_eq!(sells.len(), 1);
    }

    // M12 ----------------------------------------------------------------

    #[test]
    fn best_n_zero_returns_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        bi.put(&m, Side::Buy, 100, 0, &H256([1; 32]));
        assert!(bi.best_n(&m, Side::Buy, 0).is_empty());
    }

    #[test]
    fn best_n_empty_book_returns_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        assert!(bi.best_n(&m, Side::Buy, 5).is_empty());
        assert!(bi.best_n(&m, Side::Sell, 5).is_empty());
    }

    #[test]
    fn best_n_asks_returns_lowest_price_first_then_seq() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        // Asks: ascending price, ascending seq within price.
        bi.put(&m, Side::Sell, 105, 0, &H256([1; 32]));
        bi.put(&m, Side::Sell, 100, 2, &H256([2; 32]));
        bi.put(&m, Side::Sell, 100, 0, &H256([3; 32]));
        bi.put(&m, Side::Sell, 100, 1, &H256([4; 32]));
        bi.put(&m, Side::Sell, 110, 0, &H256([5; 32]));
        // Different market should not leak in.
        bi.put(&DerivId::perp(7), Side::Sell, 100, 0, &H256([9; 32]));

        let top3 = bi.best_n(&m, Side::Sell, 3);
        assert_eq!(top3.len(), 3);
        assert_eq!(top3[0], (100, 0, H256([3; 32])));
        assert_eq!(top3[1], (100, 1, H256([4; 32])));
        assert_eq!(top3[2], (100, 2, H256([2; 32])));

        let top5 = bi.best_n(&m, Side::Sell, 5);
        assert_eq!(top5.len(), 5);
        assert_eq!(top5[3].0, 105);
        assert_eq!(top5[4].0, 110);

        // Asking for more than exists returns all.
        assert_eq!(bi.best_n(&m, Side::Sell, 100).len(), 5);
    }

    #[test]
    fn best_n_bids_returns_highest_price_first_then_lowest_seq() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        // Bids: descending price, ascending seq within price.
        bi.put(&m, Side::Buy, 95, 0, &H256([1; 32]));
        bi.put(&m, Side::Buy, 100, 2, &H256([2; 32]));
        bi.put(&m, Side::Buy, 100, 0, &H256([3; 32]));
        bi.put(&m, Side::Buy, 100, 1, &H256([4; 32]));
        bi.put(&m, Side::Buy, 90, 0, &H256([5; 32]));
        // Sell-side same market shouldn't leak.
        bi.put(&m, Side::Sell, 100, 0, &H256([7; 32]));

        let top3 = bi.best_n(&m, Side::Buy, 3);
        assert_eq!(top3.len(), 3);
        // Highest price = 100; within 100, ascending seq.
        assert_eq!(top3[0], (100, 0, H256([3; 32])));
        assert_eq!(top3[1], (100, 1, H256([4; 32])));
        assert_eq!(top3[2], (100, 2, H256([2; 32])));

        let top5 = bi.best_n(&m, Side::Buy, 5);
        assert_eq!(top5.len(), 5);
        // After the 100s, next highest is 95 then 90.
        assert_eq!(top5[3].0, 95);
        assert_eq!(top5[4].0, 90);
    }

    #[test]
    fn best_n_bids_partial_top_level() {
        // Top level has 5 entries at price 100; ask for 3 — must return the
        // three lowest-seq orders at 100, never spilling to a lower price.
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let bi = PerpBookIndex::new(&db);
        let m = DerivId::perp(0);
        for s in 0u64..5 {
            bi.put(&m, Side::Buy, 100, s, &H256([s as u8; 32]));
        }
        bi.put(&m, Side::Buy, 90, 0, &H256([42; 32]));

        let top3 = bi.best_n(&m, Side::Buy, 3);
        assert_eq!(top3.len(), 3);
        assert_eq!(top3[0], (100, 0, H256([0; 32])));
        assert_eq!(top3[1], (100, 1, H256([1; 32])));
        assert_eq!(top3[2], (100, 2, H256([2; 32])));
        // Lower price never appears because we capped at n=3.
        assert!(top3.iter().all(|(p, _, _)| *p == 100));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// For any randomly-populated book, `best_n(.., n)` is the prefix
        /// of `list_side` after re-sorting by price-time priority. We
        /// derive the canonical answer in-memory and assert agreement.
        #[test]
        fn proptest_best_n_matches_canonical_priority(
            seed in 0u64..1024,
            n in 0usize..32,
            entries in proptest::collection::vec((0u128..1000, 0u64..1000), 0..50)
        ) {
            // Set up a fresh DB per case; tempfile is cheap and the prop
            // engine runs each case in-process.
            let dir = tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            let bi = PerpBookIndex::new(&db);
            let m = DerivId::perp((seed % 4) as u16);

            // Dedup (price, seq) pairs — the CF stores one row per such pair.
            let mut by_key: std::collections::BTreeMap<(u128, u64), H256> =
                std::collections::BTreeMap::new();
            for (i, (px, sq)) in entries.iter().enumerate() {
                by_key.insert((*px, *sq), H256([(i as u8).wrapping_add(1); 32]));
            }

            // Choose a side based on seed.
            let side = if seed % 2 == 0 { Side::Buy } else { Side::Sell };
            for ((px, sq), h) in &by_key {
                bi.put(&m, side, *px, *sq, h);
            }

            // Canonical: sort by side-specific priority and take first n.
            let mut canonical: Vec<(u128, u64, H256)> = by_key
                .iter()
                .map(|((p, s), h)| (*p, *s, *h))
                .collect();
            match side {
                Side::Sell => canonical.sort_by(
                    |(pa, sa, _), (pb, sb, _)| pa.cmp(pb).then(sa.cmp(sb)),
                ),
                Side::Buy => canonical.sort_by(
                    |(pa, sa, _), (pb, sb, _)| pb.cmp(pa).then(sa.cmp(sb)),
                ),
            }
            canonical.truncate(n);

            let got = bi.best_n(&m, side, n);
            prop_assert_eq!(got, canonical);
        }
    }

    #[test]
    fn next_prefix_increments_last_byte() {
        assert_eq!(super::next_prefix(&[0x01, 0x02]), vec![0x01, 0x03]);
    }

    #[test]
    fn next_prefix_handles_carry() {
        assert_eq!(super::next_prefix(&[0x01, 0xff]), vec![0x02]);
        assert_eq!(super::next_prefix(&[0xff, 0xff]), Vec::<u8>::new());
    }
}
