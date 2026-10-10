//! Third-party market data, mocked.
//!
//! An exchange needs an external view of price: to mark perpetual positions,
//! to compute funding against an index, and to settle dated futures. In this
//! project every such source is simulated, but the boundary is real — a
//! deployment swaps in exchange feeds or a vendor API without the mark-price
//! or funding logic noticing.

use crate::error::PortResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// One observation of an instrument's price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceTick {
    /// Instrument symbol, such as `BTC-USD` or `AAPL-USD`.
    pub symbol: String,
    /// Mid or last price in quote units.
    pub price: Decimal,
    /// Best bid, when the source publishes one.
    pub bid: Option<Decimal>,
    /// Best ask, when the source publishes one.
    pub ask: Option<Decimal>,
    /// Source-reported observation time.
    pub at: DateTime<Utc>,
    /// Identifier of the publishing source, for provenance in audit logs.
    pub source: String,
}

impl PriceTick {
    /// The full bid-ask spread as a fraction of mid, `(ask - bid) / mid`,
    /// when both sides exist and mid is not zero. A 99 / 101 quote is `0.02`.
    ///
    /// Negative for a crossed quote, which a caller comparing spreads should
    /// treat as the anomaly it is rather than as a very tight market.
    pub fn spread_fraction(&self) -> Option<Decimal> {
        let (bid, ask) = (self.bid?, self.ask?);
        let mid = (bid + ask) / Decimal::TWO;
        if mid.is_zero() {
            return None;
        }
        Some((ask - bid) / mid)
    }
}

/// A live price stream. Dropping it unsubscribes.
#[async_trait]
pub trait FeedSubscription: Send + Sync {
    /// The next tick, or `None` once the feed closes.
    async fn next(&mut self) -> PortResult<Option<PriceTick>>;
}

/// A source of external prices.
#[async_trait]
pub trait MarketDataFeed: Send + Sync + 'static {
    /// Stable identifier for this source, recorded alongside every price it
    /// contributes so an oracle dispute can be traced.
    fn source_id(&self) -> &str;

    /// The most recent price for one symbol.
    ///
    /// Returns [`crate::PortError::NotFound`] for a symbol this source does
    /// not carry, which is distinct from a source that is merely down.
    async fn latest(&self, symbol: &str) -> PortResult<PriceTick>;

    /// The most recent price for several symbols in one round trip.
    ///
    /// Symbols the source does not carry are omitted rather than erroring, so
    /// one unknown symbol does not fail the batch.
    async fn latest_batch(&self, symbols: &[String]) -> PortResult<Vec<PriceTick>>;

    /// Stream ticks for the given symbols.
    async fn subscribe(&self, symbols: &[String]) -> PortResult<Box<dyn FeedSubscription>>;

    /// Symbols this source can serve.
    async fn supported_symbols(&self) -> PortResult<Vec<String>>;

    /// Whether the source is reachable and publishing fresh data.
    async fn health_check(&self) -> PortResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn tick(bid: Option<&str>, ask: Option<&str>) -> PriceTick {
        PriceTick {
            symbol: "BTC-USD".into(),
            price: Decimal::from_str("100").unwrap(),
            bid: bid.map(|b| Decimal::from_str(b).unwrap()),
            ask: ask.map(|a| Decimal::from_str(a).unwrap()),
            at: Utc::now(),
            source: "mock".into(),
        }
    }

    #[test]
    fn spread_fraction_needs_both_sides() {
        assert!(tick(None, Some("101")).spread_fraction().is_none());
        assert!(tick(Some("99"), None).spread_fraction().is_none());
        assert_eq!(
            tick(Some("99"), Some("101")).spread_fraction(),
            Some(Decimal::from_str("0.02").unwrap())
        );
    }

    #[test]
    fn spread_fraction_is_relative_to_mid() {
        let t = tick(Some("99"), Some("101"));
        // spread 2 over mid 100
        assert_eq!(
            t.spread_fraction().unwrap(),
            Decimal::from_str("0.02").unwrap()
        );
    }

    #[test]
    fn zero_mid_does_not_divide_by_zero() {
        let t = tick(Some("0"), Some("0"));
        assert!(t.spread_fraction().is_none());
    }
}
