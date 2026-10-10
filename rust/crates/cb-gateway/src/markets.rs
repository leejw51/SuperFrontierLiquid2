//! The market catalogue: what is listed, and everything the frontend needs to
//! validate an order before sending it.
//!
//! ## Where each fact comes from
//!
//! Derivative markets are dynamic and the node lists them
//! (`listPerpMarkets` / `listFutureMarkets`), so tick size, lot size, margin
//! requirements and fees are read from the chain and are authoritative.
//!
//! Spot markets are read the same way (`listSpotMarkets`): tick, minimum
//! quantity and fee are the chain's own `MarketConfig`, in the wire scales the
//! executor checks `price % tick_size` and `qty % min_qty` in. Before that
//! method existed those three fields came from [`SpotDefaults`], and a
//! gateway whose guess was finer than genesis rounded every market order's
//! protective price to a tick the chain did not accept; the executor refused
//! each one and the order sat `pending`. The defaults remain as the floor the
//! catalogue serves before the node has answered, and as the fallback should
//! the node list nothing. `min_notional` has no chain-side counterpart and
//! stays the operator's.
//!
//! ## Block heights and wall-clock time
//!
//! A dated future expires at a *block height*. The contract publishes an
//! expiry *timestamp*, because a browser cannot reason about heights. The
//! conversion needs an assumed block time, which configuration supplies: the
//! result is an estimate and drifts if the chain's cadence does, which is
//! acceptable for a countdown and would not be for settlement.

use crate::decimal::{format_scaled, to_port_decimal};
use crate::node::NodeClient;
use crate::symbols::{
    asset_class_of, future_symbol, perp_symbol, InstrumentKind, MarketEntry, MarketRef,
    MarketScales, SymbolTable,
};
use cb_ports::{AppDatabase, Clock, MarketMeta, PortResult};
use chrono::{DateTime, Duration, Utc};
use parking_lot::RwLock;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Market facts the chain will not tell us. See the module docs.
///
/// The spot fields are the chain's own `MarketConfig`, which has no RPC in
/// front of it. `min_notional` applies to every market: the chain has no
/// minimum-order-size parameter at all, only a maximum position notional, so
/// the floor below which the gateway refuses an order is the operator's.
#[derive(Debug, Clone)]
pub struct SpotDefaults {
    /// Minimum price increment, in wire price units.
    pub tick_size: u128,
    /// Minimum quantity increment, in wire quantity units.
    pub lot_size: u128,
    /// Smallest permitted order notional, in micro-USD.
    pub min_notional: u128,
    /// Taker fee, basis points.
    pub taker_fee_bps: i32,
    /// Maker fee, basis points; negative is a rebate.
    pub maker_fee_bps: i32,
}

impl Default for SpotDefaults {
    fn default() -> Self {
        Self {
            tick_size: 1,
            lot_size: 1,
            // $10, matching the contract's example and the derivative markets.
            min_notional: 10_000_000,
            taker_fee_bps: 10,
            maker_fee_bps: 0,
        }
    }
}

/// An immutable view of the market universe.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Symbol resolution.
    pub symbols: SymbolTable,
    /// Published metadata, by symbol.
    pub metas: BTreeMap<String, MarketMeta>,
    /// When the catalogue was last rebuilt from the node.
    pub refreshed_at: Option<DateTime<Utc>>,
}

impl Catalog {
    /// Metadata for one symbol, case-insensitively.
    pub fn meta(&self, symbol: &str) -> Option<&MarketMeta> {
        self.metas.get(&symbol.to_ascii_uppercase())
    }

    /// Resolve a symbol to its chain handle and scales.
    pub fn entry(&self, symbol: &str) -> Option<&MarketEntry> {
        self.symbols.get(symbol)
    }

    /// Every symbol, in order.
    pub fn symbols_list(&self) -> Vec<String> {
        self.metas.keys().cloned().collect()
    }
}

/// An operator's override of a market's published state.
///
/// Halting and changing risk parameters are gateway-side: the chain has no
/// transaction for either, and `PerpMarket.paused` is set in genesis. An
/// override therefore stops the *gateway* accepting orders and changes what
/// the gateway publishes; resting orders on the chain are untouched, which is
/// exactly what the contract asks for — halting must still allow cancellation.
///
/// Overrides survive a catalogue refresh. They do not survive a restart, which
/// is recorded as a gap rather than hidden: a halt is an incident response and
/// an operator needs to know it is not durable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketOverride {
    /// Replacement status.
    pub status: Option<String>,
    /// Replacement maximum leverage.
    pub max_leverage: Option<Decimal>,
    /// Replacement initial margin, basis points.
    pub initial_margin_bps: Option<u32>,
    /// Replacement maintenance margin, basis points.
    pub maintenance_margin_bps: Option<u32>,
}

impl MarketOverride {
    /// Whether this override changes anything.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Apply the override to a market's published metadata.
    pub fn apply(&self, meta: &mut MarketMeta) {
        if let Some(status) = &self.status {
            meta.status = status.clone();
        }
        if let Some(value) = self.max_leverage {
            meta.max_leverage = Some(value);
        }
        if let Some(value) = self.initial_margin_bps {
            meta.initial_margin_bps = Some(value);
        }
        if let Some(value) = self.maintenance_margin_bps {
            meta.maintenance_margin_bps = Some(value);
        }
    }
}

/// Keeps [`Catalog`] up to date from the node.
pub struct MarketCatalog {
    node: Arc<dyn NodeClient>,
    db: Arc<dyn AppDatabase>,
    clock: Arc<dyn Clock>,
    block_time_ms: u64,
    spot_defaults: SpotDefaults,
    /// The catalogue exactly as the chain reports it, before any operator
    /// override. Kept so that lifting an override restores the chain's own
    /// state rather than whatever the override replaced.
    base: RwLock<Arc<Catalog>>,
    /// What callers see: the base with the overrides applied.
    current: RwLock<Arc<Catalog>>,
    overrides: RwLock<BTreeMap<String, MarketOverride>>,
}

impl std::fmt::Debug for MarketCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketCatalog")
            .field("markets", &self.current.read().metas.len())
            .finish_non_exhaustive()
    }
}

impl MarketCatalog {
    /// Build a catalogue. It starts holding only the spot markets, which need
    /// no round trip, so the gateway serves `/markets` before the node answers.
    pub fn new(
        node: Arc<dyn NodeClient>,
        db: Arc<dyn AppDatabase>,
        clock: Arc<dyn Clock>,
        block_time_ms: u64,
        spot_defaults: SpotDefaults,
    ) -> Self {
        let catalog = Self {
            node,
            db,
            clock,
            block_time_ms: block_time_ms.max(1),
            spot_defaults,
            base: RwLock::new(Arc::new(Catalog::default())),
            current: RwLock::new(Arc::new(Catalog::default())),
            overrides: RwLock::new(BTreeMap::new()),
        };
        let spot_only = Arc::new(catalog.spot_only());
        *catalog.base.write() = Arc::clone(&spot_only);
        *catalog.current.write() = spot_only;
        catalog
    }

    /// The current view. Cheap: an `Arc` clone.
    pub fn snapshot(&self) -> Arc<Catalog> {
        Arc::clone(&self.current.read())
    }

    /// Rebuild from the node and persist through [`cb_ports::MarketMetaRepository`].
    ///
    /// A failure leaves the previous catalogue in place. Serving a slightly
    /// stale market list beats serving none because the node blinked.
    pub async fn refresh(&self, head_height: u64) -> PortResult<Arc<Catalog>> {
        let now = self.clock.now();
        let mut catalog = self.spot_only();

        // The chain's spot configuration overrides the configured defaults.
        // A market the node lists but the symbol table does not (one whose
        // prices are not representable, see `SymbolTable::with_spot_markets`)
        // is skipped rather than invented.
        for market in self.node.spot_markets().await? {
            let Some(entry) = catalog.symbols.by_market(MarketRef::Spot(market.id)) else {
                continue;
            };
            let (symbol, scales) = (entry.symbol.clone(), entry.scales);
            if let Some(meta) = catalog.metas.get_mut(&symbol) {
                meta.tick_size = decimal_of(market.tick_size, scales.price_decimals);
                meta.lot_size = decimal_of(market.min_qty, scales.qty_decimals);
                meta.price_decimals = display_decimals(market.tick_size, scales.price_decimals, 2);
                meta.size_decimals = display_decimals(market.min_qty, scales.qty_decimals, 0);
                meta.taker_fee_bps = i32::from(market.fee_bps);
            }
        }

        for market in self.node.perp_markets().await? {
            let symbol = perp_symbol(market.underlying);
            let scales = MarketScales::deriv();
            catalog.symbols.insert(MarketEntry {
                symbol: symbol.clone(),
                market: MarketRef::Deriv(market.id),
                kind: InstrumentKind::Perp,
                base: market.underlying,
                quote: cb_types::AssetId::Usd,
                scales,
                asset_class: asset_class_of(market.underlying),
                display_name: format!("{} Perpetual", market.underlying.name()),
            });
            let status = if market.paused {
                "halted"
            } else if market.reduce_only_mode {
                // The insurance fund is exhausted and the market takes closing
                // flow only. "settling" is the contract value that says so.
                "settling"
            } else {
                "trading"
            };
            catalog.metas.insert(
                symbol.clone(),
                MarketMeta {
                    symbol,
                    kind: "perp".into(),
                    asset_class: asset_class_of(market.underlying).into(),
                    base: market.underlying.symbol().into(),
                    quote: "USD".into(),
                    display_name: format!("{} Perpetual", market.underlying.name()),
                    status: status.into(),
                    tick_size: decimal_of(market.tick_size, scales.price_decimals),
                    lot_size: decimal_of(market.lot_size, scales.qty_decimals),
                    min_notional: decimal_of(
                        self.min_notional(market.lot_size, market.tick_size),
                        6,
                    ),
                    price_decimals: display_decimals(market.tick_size, scales.price_decimals, 2),
                    size_decimals: display_decimals(market.lot_size, scales.qty_decimals, 0),
                    max_leverage: Some(
                        Decimal::from(market.max_leverage_x100) / Decimal::from(100),
                    ),
                    initial_margin_bps: Some(market.initial_margin_bps as u32),
                    maintenance_margin_bps: Some(market.maintenance_margin_bps as u32),
                    funding_interval_seconds: Some(
                        market.funding_interval_blocks as u64 * self.block_time_ms / 1000,
                    ),
                    expiry: None,
                    maker_fee_bps: market.maker_fee_bps as i32,
                    taker_fee_bps: market.taker_fee_bps as i32,
                },
            );
        }

        for market in self.node.future_markets().await? {
            let expiry = self.height_to_time(head_height, market.expiry_height, now);
            let symbol = future_symbol(market.underlying, expiry);
            let scales = MarketScales::deriv();
            catalog.symbols.insert(MarketEntry {
                symbol: symbol.clone(),
                market: MarketRef::Deriv(market.id),
                kind: InstrumentKind::Future,
                base: market.underlying,
                quote: cb_types::AssetId::Usd,
                scales,
                asset_class: asset_class_of(market.underlying),
                display_name: format!("{} Future", market.underlying.name()),
            });
            let status = if market.final_settlement_px_q8.is_some() {
                "expired"
            } else if market.paused {
                "halted"
            } else if head_height >= market.expiry_height {
                "settling"
            } else {
                "trading"
            };
            catalog.metas.insert(
                symbol.clone(),
                MarketMeta {
                    symbol,
                    kind: "future".into(),
                    asset_class: asset_class_of(market.underlying).into(),
                    base: market.underlying.symbol().into(),
                    quote: "USD".into(),
                    display_name: format!("{} Future", market.underlying.name()),
                    status: status.into(),
                    tick_size: decimal_of(market.tick_size, scales.price_decimals),
                    lot_size: decimal_of(market.lot_size, scales.qty_decimals),
                    min_notional: decimal_of(
                        self.min_notional(market.lot_size, market.tick_size),
                        6,
                    ),
                    price_decimals: display_decimals(market.tick_size, scales.price_decimals, 2),
                    size_decimals: display_decimals(market.lot_size, scales.qty_decimals, 0),
                    max_leverage: Some(
                        Decimal::from(market.max_leverage_x100) / Decimal::from(100),
                    ),
                    initial_margin_bps: Some(market.initial_margin_bps as u32),
                    maintenance_margin_bps: Some(market.maintenance_margin_bps as u32),
                    // Futures do not pay funding; they converge by expiry.
                    funding_interval_seconds: None,
                    expiry: Some(expiry),
                    maker_fee_bps: market.maker_fee_bps as i32,
                    taker_fee_bps: market.taker_fee_bps as i32,
                },
            );
        }

        catalog.refreshed_at = Some(now);
        *self.base.write() = Arc::new(catalog.clone());
        self.apply_overrides(&mut catalog);
        let metas: Vec<MarketMeta> = catalog.metas.values().cloned().collect();
        // Persisting is a cache for restarts, not the source of truth, so a
        // storage failure must not discard a catalogue we successfully read.
        if let Err(e) = self.db.markets().replace_all(&metas).await {
            tracing::warn!(target: "cb_gateway::markets", error = %e, "market cache not persisted");
        }

        let catalog = Arc::new(catalog);
        *self.current.write() = Arc::clone(&catalog);
        Ok(catalog)
    }

    /// Replace an operator override and republish the catalogue.
    pub fn set_override(&self, symbol: &str, patch: MarketOverride) {
        let symbol = symbol.to_ascii_uppercase();
        {
            let mut overrides = self.overrides.write();
            if patch.is_empty() {
                overrides.remove(&symbol);
            } else {
                overrides.insert(symbol, patch);
            }
        }
        self.republish();
    }

    /// Merge a partial override onto whatever is already set.
    pub fn amend_override(&self, symbol: &str, amend: impl FnOnce(&mut MarketOverride)) {
        let symbol = symbol.to_ascii_uppercase();
        {
            let mut overrides = self.overrides.write();
            let entry = overrides.entry(symbol.clone()).or_default();
            amend(entry);
            if entry.is_empty() {
                overrides.remove(&symbol);
            }
        }
        self.republish();
    }

    /// The override on a market, if any.
    pub fn override_for(&self, symbol: &str) -> Option<MarketOverride> {
        self.overrides
            .read()
            .get(&symbol.to_ascii_uppercase())
            .cloned()
    }

    /// Re-apply the overrides to the catalogue the chain reported.
    ///
    /// Always from the base, never from the published copy: applying to the
    /// published copy would bake an override in permanently, and lifting it
    /// would leave the market in whatever state the override had put it.
    fn republish(&self) {
        let mut catalog = (**self.base.read()).clone();
        self.apply_overrides(&mut catalog);
        *self.current.write() = Arc::new(catalog);
    }

    fn apply_overrides(&self, catalog: &mut Catalog) {
        let overrides = self.overrides.read();
        for (symbol, patch) in overrides.iter() {
            if let Some(meta) = catalog.metas.get_mut(symbol) {
                patch.apply(meta);
            }
        }
    }

    /// The catalogue as it looks with spot markets alone.
    fn spot_only(&self) -> Catalog {
        let symbols = SymbolTable::with_spot_markets();
        let mut metas = BTreeMap::new();
        for entry in symbols.entries() {
            metas.insert(
                entry.symbol.clone(),
                MarketMeta {
                    symbol: entry.symbol.clone(),
                    kind: "spot".into(),
                    asset_class: entry.asset_class.into(),
                    base: entry.base.symbol().into(),
                    quote: entry.quote.symbol().into(),
                    display_name: entry.display_name.clone(),
                    status: "trading".into(),
                    tick_size: decimal_of(
                        self.spot_defaults.tick_size,
                        entry.scales.price_decimals,
                    ),
                    lot_size: decimal_of(self.spot_defaults.lot_size, entry.scales.qty_decimals),
                    min_notional: decimal_of(self.spot_defaults.min_notional, 6),
                    price_decimals: display_decimals(
                        self.spot_defaults.tick_size,
                        entry.scales.price_decimals,
                        2,
                    ),
                    size_decimals: display_decimals(
                        self.spot_defaults.lot_size,
                        entry.scales.qty_decimals,
                        0,
                    ),
                    // Spot is unlevered, so the contract wants these null.
                    max_leverage: None,
                    initial_margin_bps: None,
                    maintenance_margin_bps: None,
                    funding_interval_seconds: None,
                    expiry: None,
                    maker_fee_bps: self.spot_defaults.maker_fee_bps,
                    taker_fee_bps: self.spot_defaults.taker_fee_bps,
                },
            );
        }
        Catalog {
            symbols,
            metas,
            refreshed_at: None,
        }
    }

    /// The smallest order the gateway will accept on a market.
    ///
    /// The chain has no minimum-order parameter, so this is the larger of what
    /// the market can physically represent — one lot at one tick — and the
    /// operator's configured floor. Without the floor, a market with a fine
    /// tick would accept economically meaningless dust.
    fn min_notional(&self, lot_size: u128, tick_size: u128) -> u128 {
        min_notional_of(lot_size, tick_size).max(self.spot_defaults.min_notional)
    }

    /// Estimate the wall-clock time of a future block height.
    fn height_to_time(&self, head: u64, target: u64, now: DateTime<Utc>) -> DateTime<Utc> {
        let delta_blocks = target as i64 - head as i64;
        now + Duration::milliseconds(delta_blocks.saturating_mul(self.block_time_ms as i64))
    }
}

/// A wire integer at `decimals` places, as the `Decimal` the port model wants.
///
/// A value too large for `Decimal` is stored as zero rather than a rounded
/// lie; a tick size that does not fit is a configuration error, and zero is
/// visibly wrong where a rounded value would not be.
fn decimal_of(value: u128, decimals: u32) -> Decimal {
    to_port_decimal(value, decimals).unwrap_or_default()
}

/// How many decimal places to display, from the increment's own precision.
///
/// A tick of `0.10` has one significant place; the contract's example shows
/// two, because prices look wrong with fewer than the quote currency's own
/// places. `floor` is the increment's precision and `min_places` the floor
/// under it.
fn display_decimals(increment: u128, scale: u32, min_places: u32) -> u32 {
    let text = format_scaled(increment.max(1), scale, 0);
    let places = text
        .split_once('.')
        .map(|(_, frac)| frac.len() as u32)
        .unwrap_or(0);
    places.max(min_places)
}

/// The smallest order the chain will accept, expressed as a notional.
///
/// One lot at one tick is the smallest non-zero notional representable on the
/// market, which is the honest floor when the chain publishes no explicit
/// minimum of its own.
fn min_notional_of(lot_size: u128, tick_size: u128) -> u128 {
    lot_size
        .saturating_mul(tick_size)
        .checked_div(10u128.pow(10))
        .unwrap_or(0)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryDatabase;
    use crate::node::{FutureMarketSnapshot, MockNode, PerpMarketSnapshot, SpotMarketSnapshot};
    use cb_ports::SystemClock;
    use cb_types::{AssetId, DerivId, MarketId};

    fn perp(index: u16, underlying: AssetId) -> PerpMarketSnapshot {
        PerpMarketSnapshot {
            id: DerivId::perp(index),
            underlying,
            tick_size: 10_000_000, // 0.10 at q8
            lot_size: 10_000,      // 0.0001 at 1e8
            max_leverage_x100: 5_000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 4,
            maker_fee_bps: -1,
            funding_interval_blocks: 36_000,
            max_position_notional: 0,
            paused: false,
            reduce_only_mode: false,
            last_trade_px_q8: 0,
            last_trade_height: 0,
        }
    }

    fn catalog_with(node: Arc<MockNode>) -> MarketCatalog {
        MarketCatalog::new(
            node,
            InMemoryDatabase::shared(),
            Arc::new(SystemClock),
            100,
            SpotDefaults::default(),
        )
    }

    #[tokio::test]
    async fn the_catalogue_serves_spot_markets_before_the_node_answers() {
        let catalog = catalog_with(Arc::new(MockNode::new()));
        let snapshot = catalog.snapshot();
        assert!(snapshot.meta("BTC-USD").is_some());
        assert!(snapshot.meta("BTC-PERP").is_none(), "not read yet");
        assert!(snapshot.refreshed_at.is_none());
    }

    #[tokio::test]
    async fn spot_metadata_comes_from_the_chain_once_the_node_answers() {
        // genesis-full.jsonl's btc_usd: tick 100000 and min_qty 1000 in the
        // wire scales, which BTC-USD (price 6 places, quantity 8) shows as
        // 0.1 and 0.00001. The default before the node answers is one wire
        // unit, which is what every market order used to be rounded to.
        let node = Arc::new(MockNode::new());
        node.edit(|state| {
            state.spot_markets = vec![SpotMarketSnapshot {
                id: MarketId::BtcUsd,
                base: AssetId::Btc,
                quote: AssetId::Usd,
                tick_size: 100_000,
                min_qty: 1_000,
                fee_bps: 10,
            }]
        });
        let catalog = catalog_with(node);

        let before = catalog.snapshot();
        let meta = before.meta("BTC-USD").expect("listed from the start");
        assert_eq!(
            meta.tick_size.to_string(),
            "0.000001",
            "the configured default"
        );

        let snapshot = catalog.refresh(100).await.expect("refresh");
        let meta = snapshot.meta("BTC-USD").expect("listed");
        assert_eq!(meta.tick_size.to_string(), "0.1");
        assert_eq!(meta.lot_size.to_string(), "0.00001");
        assert_eq!(meta.price_decimals, 2);
        assert_eq!(meta.size_decimals, 5);
        assert_eq!(meta.taker_fee_bps, 10);
        assert_eq!(
            meta.min_notional.to_string(),
            "10",
            "the operator's floor; the chain has none"
        );
    }

    #[tokio::test]
    async fn perp_metadata_matches_the_contracts_example_shape() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| state.perp_markets = vec![perp(0, AssetId::Btc)]);
        let catalog = catalog_with(node);
        let snapshot = catalog.refresh(100).await.expect("refresh");

        let meta = snapshot.meta("BTC-PERP").expect("listed");
        assert_eq!(meta.kind, "perp");
        assert_eq!(meta.asset_class, "crypto");
        assert_eq!(meta.status, "trading");
        assert_eq!(meta.tick_size.to_string(), "0.1");
        assert_eq!(meta.lot_size.to_string(), "0.0001");
        assert_eq!(
            meta.min_notional.to_string(),
            "10",
            "the operator's floor, not the market's dust limit"
        );
        assert_eq!(meta.price_decimals, 2);
        assert_eq!(meta.size_decimals, 4);
        assert_eq!(meta.max_leverage.expect("levered").to_string(), "50");
        assert_eq!(meta.initial_margin_bps, Some(200));
        assert_eq!(meta.maker_fee_bps, -1);
        assert_eq!(meta.taker_fee_bps, 4);
        assert_eq!(meta.funding_interval_seconds, Some(3_600));
        assert!(meta.expiry.is_none());
    }

    #[tokio::test]
    async fn spot_metadata_leaves_the_derivative_fields_null() {
        let catalog = catalog_with(Arc::new(MockNode::new()));
        let snapshot = catalog.snapshot();
        let meta = snapshot.meta("BTC-USD").expect("listed");
        assert_eq!(meta.kind, "spot");
        assert!(meta.max_leverage.is_none());
        assert!(meta.initial_margin_bps.is_none());
        assert!(meta.maintenance_margin_bps.is_none());
        assert!(meta.funding_interval_seconds.is_none());
        assert!(meta.expiry.is_none());
    }

    #[tokio::test]
    async fn an_equity_perp_is_classified_as_equity() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| state.perp_markets = vec![perp(1, AssetId::Tsla)]);
        let catalog = catalog_with(node);
        let snapshot = catalog.refresh(1).await.expect("refresh");
        let meta = snapshot.meta("TSLA-PERP").expect("listed");
        assert_eq!(meta.asset_class, "equity");
        assert_eq!(meta.base, "TSLA");
    }

    #[tokio::test]
    async fn a_dated_future_gets_a_dated_symbol_and_an_expiry() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| {
            state.future_markets = vec![FutureMarketSnapshot {
                id: DerivId::future(0),
                underlying: AssetId::Btc,
                tick_size: 10_000_000,
                lot_size: 10_000,
                max_leverage_x100: 1_000,
                initial_margin_bps: 1_000,
                maintenance_margin_bps: 500,
                taker_fee_bps: 5,
                maker_fee_bps: 0,
                expiry_height: 1_000_000,
                paused: false,
                reduce_only_mode: false,
                final_settlement_px_q8: None,
                last_trade_px_q8: 0,
                last_trade_height: 0,
            }]
        });
        let catalog = catalog_with(node);
        let snapshot = catalog.refresh(1).await.expect("refresh");
        let symbol = snapshot
            .symbols_list()
            .into_iter()
            .find(|s| s.starts_with("BTC-") && s != "BTC-USD")
            .expect("a dated symbol");
        let meta = snapshot.meta(&symbol).expect("listed");
        assert_eq!(meta.kind, "future");
        assert!(meta.expiry.is_some());
        assert!(meta.funding_interval_seconds.is_none());
        assert_eq!(meta.status, "trading");
    }

    #[tokio::test]
    async fn a_settled_future_reports_expired() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| {
            state.future_markets = vec![FutureMarketSnapshot {
                id: DerivId::future(0),
                underlying: AssetId::Btc,
                tick_size: 1,
                lot_size: 1,
                max_leverage_x100: 100,
                initial_margin_bps: 1,
                maintenance_margin_bps: 1,
                taker_fee_bps: 0,
                maker_fee_bps: 0,
                expiry_height: 10,
                paused: false,
                reduce_only_mode: false,
                final_settlement_px_q8: Some(1),
                last_trade_px_q8: 0,
                last_trade_height: 0,
            }]
        });
        let catalog = catalog_with(node);
        let snapshot = catalog.refresh(20).await.expect("refresh");
        assert!(snapshot.metas.values().any(|m| m.status == "expired"));
    }

    #[tokio::test]
    async fn a_paused_market_reports_halted_and_reduce_only_reports_settling() {
        let node = Arc::new(MockNode::new());
        let mut paused = perp(0, AssetId::Btc);
        paused.paused = true;
        let mut winding_down = perp(1, AssetId::Eth);
        winding_down.reduce_only_mode = true;
        node.edit(|state| state.perp_markets = vec![paused, winding_down]);
        let catalog = catalog_with(node);
        let snapshot = catalog.refresh(1).await.expect("refresh");
        assert_eq!(snapshot.meta("BTC-PERP").expect("listed").status, "halted");
        assert_eq!(
            snapshot.meta("ETH-PERP").expect("listed").status,
            "settling"
        );
    }

    #[tokio::test]
    async fn an_operator_override_halts_a_market_and_survives_a_refresh() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| state.perp_markets = vec![perp(0, AssetId::Btc)]);
        let catalog = catalog_with(node);
        catalog.refresh(1).await.expect("refresh");
        assert_eq!(
            catalog.snapshot().meta("BTC-PERP").expect("listed").status,
            "trading"
        );

        catalog.amend_override("btc-perp", |patch| patch.status = Some("halted".into()));
        assert_eq!(
            catalog.snapshot().meta("BTC-PERP").expect("listed").status,
            "halted"
        );

        // A refresh must not quietly un-halt the market.
        catalog.refresh(2).await.expect("refresh");
        assert_eq!(
            catalog.snapshot().meta("BTC-PERP").expect("listed").status,
            "halted"
        );

        // Lifting the override restores the chain's own state.
        catalog.amend_override("BTC-PERP", |patch| patch.status = None);
        assert_eq!(
            catalog.snapshot().meta("BTC-PERP").expect("listed").status,
            "trading"
        );
        assert!(catalog.override_for("BTC-PERP").is_none());
    }

    #[tokio::test]
    async fn a_risk_override_changes_what_the_gateway_publishes() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| state.perp_markets = vec![perp(0, AssetId::Btc)]);
        let catalog = catalog_with(node);
        catalog.refresh(1).await.expect("refresh");

        catalog.amend_override("BTC-PERP", |patch| {
            patch.max_leverage = Some(Decimal::from(20));
            patch.initial_margin_bps = Some(500);
            patch.maintenance_margin_bps = Some(250);
        });
        let meta = catalog.snapshot().meta("BTC-PERP").expect("listed").clone();
        assert_eq!(meta.max_leverage, Some(Decimal::from(20)));
        assert_eq!(meta.initial_margin_bps, Some(500));
        assert_eq!(meta.maintenance_margin_bps, Some(250));
        assert_eq!(
            meta.status, "trading",
            "risk changes do not halt the market"
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_previous_catalogue() {
        let node = Arc::new(MockNode::new());
        node.edit(|state| state.perp_markets = vec![perp(0, AssetId::Btc)]);
        let catalog = catalog_with(node.clone());
        catalog.refresh(1).await.expect("first refresh");
        assert!(catalog.snapshot().meta("BTC-PERP").is_some());

        node.take_offline("restarting");
        assert!(catalog.refresh(2).await.is_err());
        assert!(
            catalog.snapshot().meta("BTC-PERP").is_some(),
            "a stale catalogue beats no catalogue"
        );
    }
}
