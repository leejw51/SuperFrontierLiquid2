//! The local order book, and the rules for keeping it honest.
//!
//! The contract is short and every clause in it matters:
//!
//! * the gateway sends a fresh **snapshot** on every subscribe and every
//!   reconnect, and the snapshot replaces the book wholesale;
//! * a **delta** level whose quantity is `"0"` removes that price level;
//! * a client that misses a sequence must resubscribe — which, since the only
//!   sequence number on the wire is a block height and blocks without book
//!   changes carry no delta, is honoured by resubscribing on every reconnect
//!   rather than by reacting to a jump in height.
//!
//! Getting any of those wrong produces a book that looks plausible and is wrong,
//! which is worse than a book that is visibly broken: somebody trades on it. So
//! this type is deliberately dumb and deliberately strict. It refuses deltas
//! that arrive before the snapshot it is waiting for, it ignores deltas from a
//! height it has already applied, and it reports a gap rather than papering over
//! one.
//!
//! No Leptos, no `web_sys`: this module is tested on the host and under
//! `wasm-bindgen-test` from the same source.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::api::types::{Level, OrderBookPayload};

/// One rendered book level, with the cumulative total behind it.
///
/// The cumulative total — everything resting between this price and the top of
/// the book — is the number that tells you what a market order would actually
/// cost, which is why the depth bar is sized by it rather than by the level's
/// own quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookRow {
    /// The price of this level.
    pub price: Decimal,
    /// The quantity resting at this price.
    pub qty: Decimal,
    /// Everything resting between the touch and this level, inclusive.
    pub cumulative: Decimal,
}

/// What happened when a delta was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOutcome {
    /// The delta was applied.
    Applied,
    /// The delta arrived before the snapshot this book is waiting for, and was
    /// dropped. Applying it would silently corrupt the book the snapshot is
    /// about to install.
    DroppedAwaitingSnapshot,
    /// The delta is for a height already applied, and was ignored.
    Stale,
    /// The delta skipped at least one height. It was applied anyway.
    ///
    /// Advisory. Heights are block heights, not message sequence numbers, so a
    /// jump means only that some block did not move this book.
    Gap,
}

/// A two-sided aggregated order book for one market.
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    height: u64,
    awaiting_snapshot: bool,
}

impl OrderBook {
    /// A book that has nothing in it and is waiting for its first snapshot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            height: 0,
            awaiting_snapshot: true,
        }
    }

    /// Marks the book as awaiting a fresh snapshot.
    ///
    /// Called on every connect and reconnect, before resubscribing. Until the
    /// snapshot lands, deltas are dropped rather than applied: the gateway is
    /// about to replace this book wholesale, and patching it in the meantime
    /// would produce levels that never existed.
    pub fn expect_snapshot(&mut self) {
        self.awaiting_snapshot = true;
    }

    /// True while a snapshot is outstanding.
    #[must_use]
    pub const fn is_awaiting_snapshot(&self) -> bool {
        self.awaiting_snapshot
    }

    /// True when a snapshot has landed and there is something to draw.
    ///
    /// Kept for callers that only want "is there a book", but it is **not** the
    /// question a panel should ask before drawing a skeleton: it is false both
    /// while a snapshot is outstanding and after an empty one has arrived, and
    /// those are different states that must not share a picture. Use
    /// [`Self::is_awaiting_snapshot`] and [`Self::is_empty`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        !self.awaiting_snapshot && !self.is_empty()
    }

    /// True when the book that landed has nothing in it.
    ///
    /// On a fresh chain this is the overwhelmingly likely state, and it is a
    /// fact rather than a delay.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// The height this book state belongs to.
    #[must_use]
    pub const fn height(&self) -> u64 {
        self.height
    }

    /// Replaces the whole book. This is what a snapshot does, always.
    pub fn apply_snapshot(&mut self, payload: &OrderBookPayload) {
        self.bids = payload
            .bids
            .iter()
            .filter(|level| level.qty() > Decimal::ZERO)
            .map(|level| (level.price(), level.qty()))
            .collect();
        self.asks = payload
            .asks
            .iter()
            .filter(|level| level.qty() > Decimal::ZERO)
            .map(|level| (level.price(), level.qty()))
            .collect();
        self.height = payload.height;
        self.awaiting_snapshot = false;
    }

    /// Applies a delta, honouring every clause of the contract.
    pub fn apply_delta(&mut self, payload: &OrderBookPayload) -> DeltaOutcome {
        if self.awaiting_snapshot {
            return DeltaOutcome::DroppedAwaitingSnapshot;
        }
        if payload.height != 0 && payload.height <= self.height {
            return DeltaOutcome::Stale;
        }
        let gapped = payload.height > self.height.saturating_add(1) && self.height != 0;

        Self::patch(&mut self.bids, &payload.bids);
        Self::patch(&mut self.asks, &payload.asks);
        if payload.height != 0 {
            self.height = payload.height;
        }

        if gapped {
            DeltaOutcome::Gap
        } else {
            DeltaOutcome::Applied
        }
    }

    /// Applies one side of a delta. A quantity of zero removes the level.
    fn patch(side: &mut BTreeMap<Decimal, Decimal>, levels: &[Level]) {
        for level in levels {
            if level.qty() <= Decimal::ZERO {
                side.remove(&level.price());
            } else {
                side.insert(level.price(), level.qty());
            }
        }
    }

    /// The highest bid.
    #[must_use]
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.keys().next_back().copied()
    }

    /// The lowest ask.
    #[must_use]
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.keys().next().copied()
    }

    /// The distance between the touch prices.
    #[must_use]
    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()? - self.best_bid()?)
    }

    /// The spread as a fraction of the mid price, in basis points.
    ///
    /// Two decimal places, because on a market this tight the figure is a
    /// fraction of a basis point and rounding it to one place would print `0.1`
    /// for every book the exchange ever shows.
    #[must_use]
    pub fn spread_bps(&self) -> Option<Decimal> {
        let (bid, ask) = (self.best_bid()?, self.best_ask()?);
        let mid = (bid + ask) / Decimal::TWO;
        if mid.is_zero() {
            return None;
        }
        Some(((ask - bid) / mid * Decimal::from(10_000)).round_dp(2))
    }

    /// The best `depth` bids, best first, with cumulative totals.
    #[must_use]
    pub fn bids(&self, depth: usize, aggregate: Option<Decimal>) -> Vec<BookRow> {
        let levels: Vec<(Decimal, Decimal)> =
            self.bids.iter().rev().map(|(p, q)| (*p, *q)).collect();
        accumulate(&aggregate_levels(&levels, aggregate, true), depth)
    }

    /// The best `depth` asks, best first, with cumulative totals.
    ///
    /// The component renders these in reverse, because asks sit above the spread
    /// at every breakpoint: position carries side before colour does.
    #[must_use]
    pub fn asks(&self, depth: usize, aggregate: Option<Decimal>) -> Vec<BookRow> {
        let levels: Vec<(Decimal, Decimal)> = self.asks.iter().map(|(p, q)| (*p, *q)).collect();
        accumulate(&aggregate_levels(&levels, aggregate, false), depth)
    }

    /// The largest cumulative total across both sides of a rendered window.
    ///
    /// Depth bars are scaled against this, so the two sides are directly
    /// comparable and the book reads as one silhouette rather than two.
    #[must_use]
    pub fn scale(bids: &[BookRow], asks: &[BookRow]) -> Decimal {
        let largest = bids
            .last()
            .map(|row| row.cumulative)
            .unwrap_or_default()
            .max(asks.last().map(|row| row.cumulative).unwrap_or_default());
        if largest.is_zero() {
            Decimal::ONE
        } else {
            largest
        }
    }
}

/// Buckets levels onto an aggregation tick, keeping exact cumulative totals.
///
/// Bids round down onto the tick and asks round up, so that an aggregated level
/// never claims liquidity at a better price than actually exists.
fn aggregate_levels(
    levels: &[(Decimal, Decimal)],
    tick: Option<Decimal>,
    is_bid: bool,
) -> Vec<(Decimal, Decimal)> {
    let Some(tick) = tick.filter(|tick| *tick > Decimal::ZERO) else {
        return levels.to_vec();
    };
    let mut out: Vec<(Decimal, Decimal)> = Vec::with_capacity(levels.len());
    for (price, qty) in levels {
        let buckets = *price / tick;
        let bucket = if is_bid {
            buckets.floor() * tick
        } else {
            buckets.ceil() * tick
        };
        match out.last_mut() {
            Some((last_price, last_qty)) if *last_price == bucket => *last_qty += *qty,
            _ => out.push((bucket, *qty)),
        }
    }
    out
}

/// Turns `(price, qty)` pairs into rows carrying a running cumulative total.
fn accumulate(levels: &[(Decimal, Decimal)], depth: usize) -> Vec<BookRow> {
    let mut cumulative = Decimal::ZERO;
    levels
        .iter()
        .take(depth)
        .map(|(price, qty)| {
            cumulative += *qty;
            BookRow {
                price: *price,
                qty: *qty,
                cumulative,
            }
        })
        .collect()
}

/// A level's depth-bar width, as a whole percentage of the rendered scale.
///
/// Computed in [`Decimal`] and emitted as an integer because it is a CSS width.
/// No quantity is rounded on the way: the integer is the last step.
#[must_use]
pub fn depth_pct(cumulative: Decimal, scale: Decimal) -> u32 {
    if scale <= Decimal::ZERO {
        return 0;
    }
    let hundred = Decimal::from(100);
    ((cumulative / scale) * hundred)
        .clamp(Decimal::ZERO, hundred)
        .round()
        .try_into()
        .unwrap_or(0u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap_or_default()
    }

    fn level(price: &str, qty: &str) -> Level {
        Level(d(price), d(qty))
    }

    fn snapshot() -> OrderBookPayload {
        OrderBookPayload {
            height: 184_203,
            bids: vec![
                level("64250.00", "1.2500"),
                level("64249.50", "3.0000"),
                level("64249.10", "0.8400"),
            ],
            asks: vec![level("64251.00", "0.8020"), level("64251.40", "1.0700")],
        }
    }

    #[test]
    fn a_snapshot_replaces_the_book_wholesale() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        assert_eq!(book.best_bid(), Some(d("64250.00")));
        assert_eq!(book.best_ask(), Some(d("64251.00")));

        book.apply_snapshot(&OrderBookPayload {
            height: 184_300,
            bids: vec![level("100.00", "1.0")],
            asks: vec![],
        });
        assert_eq!(book.bids(10, None).len(), 1);
        assert_eq!(book.asks(10, None).len(), 0, "old asks must not survive");
    }

    #[test]
    fn a_delta_quantity_of_zero_removes_the_level() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        let outcome = book.apply_delta(&OrderBookPayload {
            height: 184_204,
            bids: vec![level("64250.00", "0"), level("64249.00", "2.0000")],
            asks: vec![],
        });
        assert_eq!(outcome, DeltaOutcome::Applied);
        assert_eq!(
            book.best_bid(),
            Some(d("64249.50")),
            "the removed level must be gone, not zeroed in place"
        );
        let prices: Vec<Decimal> = book.bids(10, None).iter().map(|row| row.price).collect();
        assert!(!prices.contains(&d("64250.00")));
        assert!(prices.contains(&d("64249.00")));
    }

    #[test]
    fn a_delta_before_the_snapshot_is_dropped() {
        let mut book = OrderBook::new();
        let outcome = book.apply_delta(&OrderBookPayload {
            height: 184_204,
            bids: vec![level("64250.00", "1.0")],
            asks: vec![],
        });
        assert_eq!(outcome, DeltaOutcome::DroppedAwaitingSnapshot);
        assert_eq!(
            book.best_bid(),
            None,
            "nothing may be applied before a snapshot"
        );
    }

    #[test]
    fn a_reconnect_drops_deltas_until_the_fresh_snapshot_lands() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        book.expect_snapshot();
        assert_eq!(
            book.apply_delta(&OrderBookPayload {
                height: 184_400,
                bids: vec![level("64250.00", "0")],
                asks: vec![],
            }),
            DeltaOutcome::DroppedAwaitingSnapshot
        );
        assert_eq!(
            book.best_bid(),
            Some(d("64250.00")),
            "the frozen book stays on screen until the snapshot replaces it"
        );
    }

    #[test]
    fn a_stale_delta_is_ignored_and_a_gap_is_reported() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        assert_eq!(
            book.apply_delta(&OrderBookPayload {
                height: 184_203,
                bids: vec![level("1.00", "1.0")],
                asks: vec![],
            }),
            DeltaOutcome::Stale
        );
        assert_eq!(
            book.apply_delta(&OrderBookPayload {
                height: 184_210,
                bids: vec![level("64248.00", "1.0")],
                asks: vec![],
            }),
            DeltaOutcome::Gap
        );
        assert_eq!(book.height(), 184_210);
    }

    #[test]
    fn cumulative_totals_run_from_the_touch_outward() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        let bids = book.bids(3, None);
        assert_eq!(bids[0].cumulative, d("1.2500"));
        assert_eq!(bids[1].cumulative, d("4.2500"));
        assert_eq!(bids[2].cumulative, d("5.0900"));
        let asks = book.asks(2, None);
        assert_eq!(asks[0].cumulative, d("0.8020"));
        assert_eq!(asks[1].cumulative, d("1.8720"));
    }

    #[test]
    fn depth_percentages_are_relative_to_the_bigger_side() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        let bids = book.bids(3, None);
        let asks = book.asks(2, None);
        let scale = OrderBook::scale(&bids, &asks);
        assert_eq!(scale, d("5.0900"));
        assert_eq!(depth_pct(bids[2].cumulative, scale), 100);
        assert_eq!(depth_pct(asks[0].cumulative, scale), 16);
        assert_eq!(depth_pct(Decimal::ZERO, scale), 0);
    }

    #[test]
    fn aggregation_buckets_without_losing_quantity() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        let bids = book.bids(10, Some(d("1")));
        assert_eq!(bids.len(), 2, "64250.00 alone, then 64249.50 with 64249.10");
        assert_eq!(bids[0].price, d("64250"));
        assert_eq!(bids[1].price, d("64249"));
        assert_eq!(bids[1].qty, d("3.8400"));
        // The aggregated cumulative is still the exact resting total.
        assert_eq!(bids[1].cumulative, d("5.0900"));
    }

    #[test]
    fn the_spread_matches_the_reference() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&snapshot());
        assert_eq!(book.spread(), Some(d("1.00")));
        let mut tight = OrderBook::new();
        tight.apply_snapshot(&OrderBookPayload {
            height: 1,
            bids: vec![level("64250.00", "1.25")],
            asks: vec![level("64250.50", "0.80")],
        });
        assert_eq!(tight.spread(), Some(d("0.50")));
        assert_eq!(tight.spread_bps(), Some(d("0.08")));
    }
}
