//! Static instrument reference data.

use crate::calendar::is_us_equity_session_open;
use crate::universe::{lookup, AssetClass, AssetSpec, UNIVERSE};
use async_trait::async_trait;
use cb_ports::clock::{Clock, SystemClock};
use cb_ports::refdata::{EquityReferenceData, InstrumentRef};
use cb_ports::{PortError, PortResult};
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// Reference data from a compiled-in fixture.
///
/// In production this port is a vendor feed that can be stale, partial or
/// down. Here it is a constant, which means every caller's *happy* path is
/// exercised and none of its failure paths are — see [`crate::FaultyFeed`] for
/// the market-data equivalent, and treat the absence of failures here as a gap
/// to close when a real vendor is wired in.
#[derive(Clone)]
pub struct StaticRefData {
    /// When the fixture was "refreshed". Fixed at construction so that two
    /// reads of the same instrument return equal records — a caller caching on
    /// `as_of` would otherwise see a change on every call.
    as_of: DateTime<Utc>,
    specs: &'static [AssetSpec],
}

impl std::fmt::Debug for StaticRefData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticRefData")
            .field("as_of", &self.as_of)
            .field("instruments", &self.specs.len())
            .finish()
    }
}

impl Default for StaticRefData {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticRefData {
    /// Reference data for the whole universe, stamped from the system clock.
    pub fn new() -> Self {
        Self::with_clock(&SystemClock)
    }

    /// Reference data stamped from an injected clock, so a replay produces
    /// identical records.
    pub fn with_clock(clock: &dyn Clock) -> Self {
        Self {
            as_of: clock.now(),
            specs: UNIVERSE,
        }
    }

    /// Reference data stamped from a shared clock handle.
    pub fn with_clock_handle(clock: Arc<dyn Clock>) -> Self {
        Self::with_clock(clock.as_ref())
    }

    fn to_ref(&self, spec: &AssetSpec) -> InstrumentRef {
        InstrumentRef {
            symbol: spec.ticker.to_string(),
            name: spec.name.to_string(),
            asset_class: spec.asset_class.as_str().to_string(),
            venue: spec.venue.map(str::to_string),
            currency: "USD".to_string(),
            lot_size: spec.lot(),
            tick_size: spec.tick(),
            tradable: true,
            as_of: self.as_of,
        }
    }
}

#[async_trait]
impl EquityReferenceData for StaticRefData {
    async fn instrument(&self, symbol: &str) -> PortResult<InstrumentRef> {
        lookup(symbol)
            .filter(|spec| self.specs.iter().any(|s| s.symbol == spec.symbol))
            .map(|spec| self.to_ref(spec))
            .ok_or_else(|| {
                PortError::NotFound(format!("no reference data for instrument `{symbol}`"))
            })
    }

    async fn all_instruments(&self) -> PortResult<Vec<InstrumentRef>> {
        Ok(self.specs.iter().map(|spec| self.to_ref(spec)).collect())
    }

    /// Crypto is always open; equities follow the US regular session.
    ///
    /// This answers what the *venue* is doing, not what the exchange in this
    /// project permits. The simulator deliberately keeps equity perpetuals
    /// trading around the clock — a 24/7 game whose markets close for sixteen
    /// hours a day is not a game anyone plays — and that policy belongs to the
    /// caller. What this method provides is the fact the caller needs in order
    /// to mark a position at a stale reference price and say so.
    async fn is_open(&self, symbol: &str, at: DateTime<Utc>) -> PortResult<bool> {
        let spec = lookup(symbol).ok_or_else(|| {
            PortError::NotFound(format!("no reference data for instrument `{symbol}`"))
        })?;

        Ok(match spec.asset_class {
            AssetClass::Crypto => true,
            AssetClass::Equity => is_us_equity_session_open(at),
        })
    }

    async fn health_check(&self) -> PortResult<()> {
        Ok(())
    }
}
