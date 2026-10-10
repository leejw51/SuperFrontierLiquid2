//! Order-book deltas and derived trade prints.
//!
//! ## There is no trade stream, so trades are derived
//!
//! The RPC schema declares `streamMarket`, and the node answers it with
//! `streamMarket not implemented`. Nothing on the chain reports a fill with an
//! owner, a quantity and a price together. What the gateway can see is:
//!
//! * **every block**, and therefore every transaction and its sender;
//! * **the depth of every book**, on request;
//! * **`last_trade_px_q8` and `last_trade_height`** on each derivative market.
//!
//! Trade prints are reconstructed from those. On a derivative market the price
//! is exact — the chain publishes it — and the quantity is estimated from the
//! depth that disappeared at or through that price since the previous block.
//! On a spot market neither is published, so a print is inferred from the
//! depth that vanished on one side in a block that also carried a matching
//! order, and both figures are estimates.
//!
//! **This is stated in the API contract's gaps, not hidden.** Candle volume
//! inherits the estimate. Nothing financial — margin, PnL, fees — is computed
//! from it; those come from account state, which is authoritative.
//!
//! ## Fills are not derived from here
//!
//! A trade print says nothing about whose order it was. Attributing one by
//! guesswork would eventually show a user somebody else's trade, so fills come
//! from the user's *own* account and position deltas — see
//! [`crate::watcher`] — and never from this module.

use crate::node::{BookLevel, BookSnapshot, Trade};
use cb_types::Side;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};

/// A change to a book since the previous snapshot.
///
/// A level whose quantity is zero has been removed, which is what the API
/// contract means by a delta entry with quantity `"0"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookDelta {
    /// Height the new snapshot was taken at.
    pub height: u64,
    /// Changed bid levels.
    pub bids: Vec<BookLevel>,
    /// Changed ask levels.
    pub asks: Vec<BookLevel>,
}

impl BookDelta {
    /// Whether anything actually moved.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }
}

/// Compute the delta between two snapshots of one side.
///
/// Levels are emitted best-price first on the side's own ordering, so a client
/// applying them in order never sees a book that crosses itself.
fn diff_side(previous: &[BookLevel], current: &[BookLevel], descending: bool) -> Vec<BookLevel> {
    let before: BTreeMap<u128, u128> = previous.iter().map(|l| (l.price, l.qty)).collect();
    let after: BTreeMap<u128, u128> = current.iter().map(|l| (l.price, l.qty)).collect();

    let mut changed: Vec<BookLevel> = Vec::new();
    for (price, qty) in &after {
        if before.get(price) != Some(qty) {
            changed.push(BookLevel {
                price: *price,
                qty: *qty,
            });
        }
    }
    for price in before.keys() {
        if !after.contains_key(price) {
            // Quantity zero is the contract's "this level is gone".
            changed.push(BookLevel {
                price: *price,
                qty: 0,
            });
        }
    }
    if descending {
        changed.sort_by_key(|a| std::cmp::Reverse(a.price));
    } else {
        changed.sort_by_key(|a| a.price);
    }
    changed
}

/// Compute the delta between two book snapshots.
pub fn diff(previous: &BookSnapshot, current: &BookSnapshot) -> BookDelta {
    BookDelta {
        height: current.height,
        bids: diff_side(&previous.bids, &current.bids, true),
        asks: diff_side(&previous.asks, &current.asks, false),
    }
}

/// Total resting quantity at or through `price` on the side that would fill a
/// taker coming in on `taker_side`.
fn depth_through(book: &BookSnapshot, taker_side: Side, price: u128) -> u128 {
    match taker_side {
        // A buyer lifts asks at or below their price.
        Side::Buy => book
            .asks
            .iter()
            .filter(|l| l.price <= price)
            .map(|l| l.qty)
            .sum(),
        // A seller hits bids at or above their price.
        Side::Sell => book
            .bids
            .iter()
            .filter(|l| l.price >= price)
            .map(|l| l.qty)
            .sum(),
    }
}

/// Estimate the quantity a trade at `price` consumed.
///
/// The estimate is the depth that was resting at or through the trade price
/// before the block and is no longer there. Cancellations in the same block
/// inflate it and hidden replenishment deflates it, which is why this is an
/// estimate and is documented as one everywhere it surfaces.
pub fn estimate_trade_qty(
    previous: &BookSnapshot,
    current: &BookSnapshot,
    taker_side: Side,
    price: u128,
) -> u128 {
    let before = depth_through(previous, taker_side, price);
    let after = depth_through(current, taker_side, price);
    before.saturating_sub(after)
}

/// Which way an aggressor must have come, from where the price sits in the
/// previous book.
///
/// A print at or above the previous best ask was a buyer lifting offers; at or
/// below the previous best bid, a seller hitting bids. Inside the spread it is
/// genuinely ambiguous, and the side that lost depth breaks the tie.
pub fn infer_taker_side(previous: &BookSnapshot, current: &BookSnapshot, price: u128) -> Side {
    if previous.asks.first().is_some_and(|a| price >= a.price) {
        return Side::Buy;
    }
    if previous.bids.first().is_some_and(|b| price <= b.price) {
        return Side::Sell;
    }
    let ask_loss = estimate_trade_qty(previous, current, Side::Buy, price);
    let bid_loss = estimate_trade_qty(previous, current, Side::Sell, price);
    if ask_loss >= bid_loss {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// How many recent trades and funding observations to keep per market.
///
/// The chain publishes neither history, so what the gateway can serve is what
/// it has watched go past since it started. A ring keeps that bounded.
const HISTORY_RING: usize = 1_000;

/// One funding observation, kept because the node has no funding history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingPoint {
    /// When it was observed.
    pub at: DateTime<Utc>,
    /// Interval rate, `1e9`-scaled.
    pub rate_q9: i64,
    /// Mark price at the tick, `1e8`-scaled.
    pub mark_px_q8: u128,
    /// Index price at the tick, `1e8`-scaled.
    pub index_px_q8: u128,
    /// Height of the funding tick.
    pub height: u64,
}

/// The last state the gateway saw for one market.
#[derive(Debug, Clone, Default)]
struct MarketState {
    book: BookSnapshot,
    last_trade_height: u64,
    last_funding_height: u64,
    trades: std::collections::VecDeque<Trade>,
    funding: std::collections::VecDeque<FundingPoint>,
}

/// Remembers each market's last book so deltas and trades can be derived.
///
/// The cached book is also what a new WebSocket subscriber gets as its
/// snapshot: serving the same book the deltas are computed against makes
/// snapshot-then-delta ordering correct by construction, and costs the node
/// nothing however many browsers connect.
#[derive(Default)]
pub struct MarketDataCache {
    states: Mutex<HashMap<String, MarketState>>,
}

impl std::fmt::Debug for MarketDataCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketDataCache")
            .field("markets", &self.states.lock().len())
            .finish_non_exhaustive()
    }
}

impl MarketDataCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last book seen for a symbol, for a subscriber's snapshot.
    pub fn book(&self, symbol: &str) -> Option<BookSnapshot> {
        self.states.lock().get(symbol).map(|s| s.book.clone())
    }

    /// Record a fresh book and return what changed.
    ///
    /// The first snapshot for a market produces no delta: there is nothing to
    /// diff against, and emitting the whole book as a delta would be a
    /// snapshot wearing the wrong label.
    pub fn observe_book(&self, symbol: &str, book: BookSnapshot) -> Option<BookDelta> {
        let mut states = self.states.lock();
        match states.get_mut(symbol) {
            Some(state) => {
                let delta = diff(&state.book, &book);
                state.book = book;
                (!delta.is_empty()).then_some(delta)
            }
            None => {
                states.insert(
                    symbol.to_string(),
                    MarketState {
                        book,
                        ..Default::default()
                    },
                );
                None
            }
        }
    }

    /// Record a derivative market's published last trade, returning a print
    /// when it has moved since the previous block.
    ///
    /// Quantity is estimated from the book the caller passes as `previous`,
    /// which must be the snapshot taken before this block.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_last_trade(
        &self,
        symbol: &str,
        price: u128,
        trade_height: u64,
        previous: &BookSnapshot,
        current: &BookSnapshot,
        at: DateTime<Utc>,
    ) -> Option<Trade> {
        if price == 0 || trade_height == 0 {
            return None;
        }
        let mut states = self.states.lock();
        let state = states.entry(symbol.to_string()).or_default();
        if trade_height <= state.last_trade_height {
            return None;
        }
        state.last_trade_height = trade_height;
        let taker_side = infer_taker_side(previous, current, price);
        let trade = Trade {
            symbol: symbol.to_string(),
            price,
            qty: estimate_trade_qty(previous, current, taker_side, price),
            taker_side,
            height: trade_height,
            at,
        };
        state.trades.push_back(trade.clone());
        while state.trades.len() > HISTORY_RING {
            state.trades.pop_front();
        }
        Some(trade)
    }

    /// Record a trade the gateway derived some other way.
    pub fn record_trade(&self, trade: Trade) {
        let mut states = self.states.lock();
        let state = states.entry(trade.symbol.clone()).or_default();
        state.trades.push_back(trade);
        while state.trades.len() > HISTORY_RING {
            state.trades.pop_front();
        }
    }

    /// Recent trades for a market, newest first.
    pub fn trades(&self, symbol: &str, limit: usize) -> Vec<Trade> {
        let states = self.states.lock();
        let Some(state) = states.get(symbol) else {
            return Vec::new();
        };
        state.trades.iter().rev().take(limit).cloned().collect()
    }

    /// The most recent trade price, for the ticker's `last_price`.
    pub fn last_price(&self, symbol: &str) -> Option<u128> {
        let states = self.states.lock();
        states.get(symbol)?.trades.back().map(|t| t.price)
    }

    /// Record a funding tick, if it is one the gateway has not seen.
    pub fn observe_funding(&self, symbol: &str, point: FundingPoint) -> Option<FundingPoint> {
        if point.height == 0 {
            return None;
        }
        let mut states = self.states.lock();
        let state = states.entry(symbol.to_string()).or_default();
        if point.height <= state.last_funding_height {
            return None;
        }
        state.last_funding_height = point.height;
        state.funding.push_back(point.clone());
        while state.funding.len() > HISTORY_RING {
            state.funding.pop_front();
        }
        Some(point)
    }

    /// Funding history for a market, newest first.
    pub fn funding(&self, symbol: &str, limit: usize) -> Vec<FundingPoint> {
        let states = self.states.lock();
        let Some(state) = states.get(symbol) else {
            return Vec::new();
        };
        state.funding.iter().rev().take(limit).cloned().collect()
    }

    /// Forget a market, for one that has been delisted.
    pub fn forget(&self, symbol: &str) {
        self.states.lock().remove(symbol);
    }

    /// How many markets are cached.
    pub fn len(&self) -> usize {
        self.states.lock().len()
    }

    /// Whether anything is cached.
    pub fn is_empty(&self) -> bool {
        self.states.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bids: &[(u128, u128)], asks: &[(u128, u128)], height: u64) -> BookSnapshot {
        BookSnapshot {
            bids: bids
                .iter()
                .map(|(price, qty)| BookLevel {
                    price: *price,
                    qty: *qty,
                })
                .collect(),
            asks: asks
                .iter()
                .map(|(price, qty)| BookLevel {
                    price: *price,
                    qty: *qty,
                })
                .collect(),
            height,
        }
    }

    #[test]
    fn an_unchanged_book_produces_no_delta() {
        let a = book(&[(100, 5)], &[(101, 5)], 1);
        let b = book(&[(100, 5)], &[(101, 5)], 2);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn a_removed_level_is_reported_with_quantity_zero() {
        let before = book(&[(100, 5), (99, 3)], &[(101, 5)], 1);
        let after = book(&[(100, 5)], &[(101, 5)], 2);
        let delta = diff(&before, &after);
        assert_eq!(delta.bids, vec![BookLevel { price: 99, qty: 0 }]);
        assert!(delta.asks.is_empty());
        assert_eq!(delta.height, 2);
    }

    #[test]
    fn a_changed_quantity_and_a_new_level_are_both_reported() {
        let before = book(&[(100, 5)], &[(101, 5)], 1);
        let after = book(&[(100, 2), (99, 7)], &[(101, 5)], 2);
        let delta = diff(&before, &after);
        assert_eq!(
            delta.bids,
            vec![
                BookLevel { price: 100, qty: 2 },
                BookLevel { price: 99, qty: 7 },
            ],
            "bids descend, best first"
        );
    }

    #[test]
    fn ask_deltas_ascend_and_bid_deltas_descend() {
        let before = book(&[], &[], 1);
        let after = book(&[(100, 1), (98, 1), (99, 1)], &[(105, 1), (103, 1)], 2);
        let delta = diff(&before, &after);
        let bid_prices: Vec<u128> = delta.bids.iter().map(|l| l.price).collect();
        let ask_prices: Vec<u128> = delta.asks.iter().map(|l| l.price).collect();
        assert_eq!(bid_prices, vec![100, 99, 98]);
        assert_eq!(ask_prices, vec![103, 105]);
    }

    #[test]
    fn the_first_book_for_a_market_is_a_snapshot_not_a_delta() {
        let cache = MarketDataCache::new();
        assert!(cache
            .observe_book("BTC-PERP", book(&[(100, 5)], &[(101, 5)], 1))
            .is_none());
        assert_eq!(cache.book("BTC-PERP").expect("cached").height, 1);
    }

    #[test]
    fn the_cache_serves_the_book_its_deltas_are_diffed_against() {
        let cache = MarketDataCache::new();
        cache.observe_book("BTC-PERP", book(&[(100, 5)], &[(101, 5)], 1));
        let delta = cache
            .observe_book("BTC-PERP", book(&[(100, 4)], &[(101, 5)], 2))
            .expect("changed");
        assert_eq!(delta.bids, vec![BookLevel { price: 100, qty: 4 }]);
        // A subscriber arriving now gets the post-delta book, so applying the
        // next delta to it is consistent.
        assert_eq!(cache.book("BTC-PERP").expect("cached").bids[0].qty, 4);
    }

    #[test]
    fn a_trade_quantity_is_estimated_from_the_depth_that_vanished() {
        let before = book(&[(100, 5)], &[(101, 8), (102, 4)], 1);
        let after = book(&[(100, 5)], &[(101, 3), (102, 4)], 2);
        // A buyer paid 101 and took five of the eight resting there.
        assert_eq!(estimate_trade_qty(&before, &after, Side::Buy, 101), 5);
        // Nothing left the bid side.
        assert_eq!(estimate_trade_qty(&before, &after, Side::Sell, 100), 0);
    }

    #[test]
    fn a_trade_through_several_levels_counts_all_of_them() {
        let before = book(&[], &[(101, 5), (102, 5), (103, 5)], 1);
        let after = book(&[], &[(103, 2)], 2);
        assert_eq!(estimate_trade_qty(&before, &after, Side::Buy, 103), 13);
    }

    #[test]
    fn the_aggressor_side_follows_where_the_price_sat_in_the_book() {
        let before = book(&[(100, 5)], &[(101, 5)], 1);
        let after = book(&[(100, 5)], &[], 2);
        assert_eq!(infer_taker_side(&before, &after, 101), Side::Buy);

        let before = book(&[(100, 5)], &[(101, 5)], 1);
        let after = book(&[], &[(101, 5)], 2);
        assert_eq!(infer_taker_side(&before, &after, 100), Side::Sell);
    }

    #[test]
    fn a_print_inside_the_spread_is_attributed_to_the_side_that_lost_depth() {
        let before = book(&[(98, 5)], &[(102, 5)], 1);
        let after = book(&[(98, 5)], &[], 2);
        assert_eq!(
            infer_taker_side(&before, &after, 100),
            Side::Buy,
            "the offers disappeared, so a buyer took them"
        );
    }

    #[test]
    fn one_trade_is_reported_once_however_often_the_market_is_polled() {
        let cache = MarketDataCache::new();
        let before = book(&[(100, 5)], &[(101, 8)], 1);
        let after = book(&[(100, 5)], &[(101, 3)], 2);
        let at = Utc::now();

        let first = cache
            .observe_last_trade("BTC-PERP", 101, 2, &before, &after, at)
            .expect("a new print");
        assert_eq!(first.price, 101);
        assert_eq!(first.qty, 5);
        assert_eq!(first.taker_side, Side::Buy);
        assert_eq!(first.height, 2);

        assert!(
            cache
                .observe_last_trade("BTC-PERP", 101, 2, &before, &after, at)
                .is_none(),
            "the same last-trade height must not print twice"
        );
    }

    #[test]
    fn a_market_that_has_never_traded_prints_nothing() {
        let cache = MarketDataCache::new();
        let empty = BookSnapshot::default();
        assert!(cache
            .observe_last_trade("BTC-PERP", 0, 0, &empty, &empty, Utc::now())
            .is_none());
        assert!(cache
            .observe_last_trade("BTC-PERP", 100, 0, &empty, &empty, Utc::now())
            .is_none());
    }

    #[test]
    fn recent_trades_come_back_newest_first_and_are_bounded() {
        let cache = MarketDataCache::new();
        for height in 1..=5u64 {
            cache.record_trade(Trade {
                symbol: "BTC-PERP".into(),
                price: 100 + height as u128,
                qty: 1,
                taker_side: Side::Buy,
                height,
                at: Utc::now(),
            });
        }
        let trades = cache.trades("BTC-PERP", 3);
        assert_eq!(trades.len(), 3);
        assert_eq!(trades[0].height, 5, "newest first");
        assert_eq!(cache.last_price("BTC-PERP"), Some(105));
        assert!(cache.trades("ETH-PERP", 10).is_empty());
    }

    #[test]
    fn a_funding_tick_is_recorded_once() {
        let cache = MarketDataCache::new();
        let point = FundingPoint {
            at: Utc::now(),
            rate_q9: 125_000,
            mark_px_q8: 6_424_810_000_000,
            index_px_q8: 6_425_100_000_000,
            height: 36_000,
        };
        assert!(cache.observe_funding("BTC-PERP", point.clone()).is_some());
        assert!(
            cache.observe_funding("BTC-PERP", point.clone()).is_none(),
            "the same tick height must not be recorded twice"
        );
        assert_eq!(cache.funding("BTC-PERP", 10).len(), 1);

        let mut later = point;
        later.height = 72_000;
        assert!(cache.observe_funding("BTC-PERP", later).is_some());
        let history = cache.funding("BTC-PERP", 10);
        assert_eq!(history[0].height, 72_000, "newest first");
    }

    #[test]
    fn forgetting_a_market_clears_its_state() {
        let cache = MarketDataCache::new();
        cache.observe_book("BTC-PERP", book(&[(100, 5)], &[], 1));
        assert_eq!(cache.len(), 1);
        cache.forget("BTC-PERP");
        assert!(cache.is_empty());
        assert!(cache.book("BTC-PERP").is_none());
    }
}
