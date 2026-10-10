//! Instrument reference data.
//!
//! Equity instruments need facts nobody derives from a price: the listing
//! venue, the lot size, the trading calendar, corporate actions. In this
//! project the data is a static fixture; in production it comes from a vendor.

use crate::error::PortResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// Static facts about one tradable instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstrumentRef {
    /// Exchange symbol, such as `AAPL`.
    pub symbol: String,
    /// Human-facing name, such as `Apple Inc.`.
    pub name: String,
    /// Asset class: `crypto` or `equity`.
    pub asset_class: String,
    /// Listing venue, for equities. `None` for crypto.
    pub venue: Option<String>,
    /// Settlement currency.
    pub currency: String,
    /// Smallest tradable increment of quantity.
    pub lot_size: Decimal,
    /// Smallest price increment.
    pub tick_size: Decimal,
    /// Whether the instrument is currently tradable.
    pub tradable: bool,
    /// When this record was last refreshed from the source.
    pub as_of: DateTime<Utc>,
}

/// A source of instrument reference data.
#[async_trait]
pub trait EquityReferenceData: Send + Sync + 'static {
    /// Look one instrument up.
    async fn instrument(&self, symbol: &str) -> PortResult<InstrumentRef>;

    /// Every instrument this source carries.
    async fn all_instruments(&self) -> PortResult<Vec<InstrumentRef>>;

    /// Whether the venue is open for `symbol` at `at`.
    ///
    /// Crypto is always open. Equities follow a session calendar — which is
    /// exactly why this is a method and not a constant. A simulator may choose
    /// to trade equity perpetuals around the clock regardless; that policy
    /// belongs to the caller, not to this port.
    async fn is_open(&self, symbol: &str, at: DateTime<Utc>) -> PortResult<bool>;

    /// Whether the source is reachable.
    async fn health_check(&self) -> PortResult<()>;
}
