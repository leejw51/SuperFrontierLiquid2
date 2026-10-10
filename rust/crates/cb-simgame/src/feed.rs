//! The scenario engine, exposed as a [`cb_ports::MarketDataFeed`].
//!
//! The in-process world reads prices from [`crate::scenario::PriceEngine`]
//! directly: the block loop is synchronous and hermetic, and routing it
//! through an async trait would buy nothing. The networked mode is different
//! — there the simulation stands in for an external price source, and the
//! port is the seam a production deployment swaps for a real exchange feed.
//! This adapter is what lets the same scripted timeline drive either one.
//!
//! The feed publishes a two-sided quote around the scripted mid so consumers
//! that care about spread (`PriceTick::spread_fraction`) have something to
//! read.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cb_ports::{FeedSubscription, MarketDataFeed, PortError, PortResult, PriceTick};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::scenario::{PriceEngine, ScenarioTimeline};
use crate::universe::{Underlying, Q8};

/// Symbol published for an underlying.
pub fn symbol_for(u: Underlying) -> String {
    format!("{}-USD", u.label())
}

/// A [`MarketDataFeed`] backed by the scenario engine.
///
/// Advancing the clock is explicit: [`ScenarioFeed::step`] moves every price
/// one block. Nothing advances on its own, so a test can pin the feed and
/// assert on a boundary.
pub struct ScenarioFeed {
    inner: Arc<Mutex<FeedState>>,
    source_id: String,
    /// Half-spread quoted around the mid, in basis points.
    half_spread_bps: i64,
}

struct FeedState {
    engine: PriceEngine,
    offset: u64,
    at: DateTime<Utc>,
}

impl ScenarioFeed {
    /// A feed running `timeline` from `seed`, with its clock starting at `at`.
    pub fn new(seed: u64, timeline: ScenarioTimeline, at: DateTime<Utc>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FeedState {
                engine: PriceEngine::new(seed, timeline),
                offset: 0,
                at,
            })),
            source_id: "cb-simgame-scenario".to_string(),
            half_spread_bps: 5,
        }
    }

    /// Advance every price by one block, and the clock by one second.
    pub fn step(&self) {
        let mut g = self.inner.lock().expect("scenario feed poisoned");
        let offset = g.offset;
        g.engine.step(offset);
        g.offset += 1;
        g.at += chrono::Duration::seconds(1);
    }

    fn tick_for(&self, u: Underlying) -> PriceTick {
        let g = self.inner.lock().expect("scenario feed poisoned");
        let px = g.engine.price_q8(u);
        let bid = crate::universe::bump_bps(px, -self.half_spread_bps);
        let ask = crate::universe::bump_bps(px, self.half_spread_bps);
        PriceTick {
            symbol: symbol_for(u),
            price: q8_to_decimal(px),
            bid: Some(q8_to_decimal(bid)),
            ask: Some(q8_to_decimal(ask)),
            at: g.at,
            source: self.source_id.clone(),
        }
    }

    fn parse_symbol(sym: &str) -> Option<Underlying> {
        Underlying::ALL
            .iter()
            .copied()
            .find(|u| symbol_for(*u) == sym)
    }
}

#[async_trait]
impl MarketDataFeed for ScenarioFeed {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    async fn latest(&self, symbol: &str) -> PortResult<PriceTick> {
        match Self::parse_symbol(symbol) {
            Some(u) => Ok(self.tick_for(u)),
            None => Err(PortError::NotFound(format!(
                "scenario feed does not carry {symbol}"
            ))),
        }
    }

    async fn latest_batch(&self, symbols: &[String]) -> PortResult<Vec<PriceTick>> {
        // Unknown symbols are omitted, not an error: one bad symbol must not
        // fail the batch.
        Ok(symbols
            .iter()
            .filter_map(|s| Self::parse_symbol(s))
            .map(|u| self.tick_for(u))
            .collect())
    }

    async fn subscribe(&self, symbols: &[String]) -> PortResult<Box<dyn FeedSubscription>> {
        let wanted: Vec<Underlying> = symbols
            .iter()
            .filter_map(|s| Self::parse_symbol(s))
            .collect();
        if wanted.is_empty() {
            return Err(PortError::NotFound(
                "no known symbols in subscription".to_string(),
            ));
        }
        Ok(Box::new(ScenarioSubscription {
            feed: ScenarioFeed {
                inner: Arc::clone(&self.inner),
                source_id: self.source_id.clone(),
                half_spread_bps: self.half_spread_bps,
            },
            wanted,
            cursor: 0,
        }))
    }

    async fn supported_symbols(&self) -> PortResult<Vec<String>> {
        Ok(Underlying::ALL.iter().copied().map(symbol_for).collect())
    }

    async fn health_check(&self) -> PortResult<()> {
        Ok(())
    }
}

/// A subscription that yields one tick per symbol per [`ScenarioFeed::step`].
struct ScenarioSubscription {
    feed: ScenarioFeed,
    wanted: Vec<Underlying>,
    cursor: usize,
}

#[async_trait]
impl FeedSubscription for ScenarioSubscription {
    async fn next(&mut self) -> PortResult<Option<PriceTick>> {
        if self.wanted.is_empty() {
            return Ok(None);
        }
        if self.cursor >= self.wanted.len() {
            self.cursor = 0;
            self.feed.step();
        }
        let u = self.wanted[self.cursor];
        self.cursor += 1;
        Ok(Some(self.feed.tick_for(u)))
    }
}

fn q8_to_decimal(px_q8: u128) -> Decimal {
    // q8 is at most ~1e14 in this simulation, well inside i64.
    Decimal::new(px_q8 as i64, 8) / Decimal::new(Q8 as i64, 8) * Decimal::new(Q8 as i64, 8)
        / Decimal::new(Q8 as i64, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;

    #[tokio::test]
    async fn unknown_symbol_is_not_found_not_a_zero_price() {
        let feed = ScenarioFeed::new(1, ScenarioTimeline::single(Scenario::Calm), Utc::now());
        assert!(matches!(
            feed.latest("DOGE-USD").await,
            Err(PortError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn batch_omits_unknown_symbols_rather_than_failing() {
        let feed = ScenarioFeed::new(1, ScenarioTimeline::single(Scenario::Calm), Utc::now());
        let got = feed
            .latest_batch(&["BTC-USD".to_string(), "DOGE-USD".to_string()])
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "BTC-USD");
    }

    #[tokio::test]
    async fn feed_quotes_two_sides() {
        let feed = ScenarioFeed::new(1, ScenarioTimeline::single(Scenario::Calm), Utc::now());
        let tick = feed.latest("BTC-USD").await.unwrap();
        assert!(tick.bid.unwrap() < tick.ask.unwrap());
        assert!(tick.spread_fraction().is_some());
    }

    #[tokio::test]
    async fn stepping_moves_the_price() {
        let feed = ScenarioFeed::new(9, ScenarioTimeline::single(Scenario::Crash), Utc::now());
        let before = feed.latest("BTC-USD").await.unwrap().price;
        for _ in 0..20 {
            feed.step();
        }
        let after = feed.latest("BTC-USD").await.unwrap().price;
        assert!(after < before, "a crash must move the published price down");
    }
}
