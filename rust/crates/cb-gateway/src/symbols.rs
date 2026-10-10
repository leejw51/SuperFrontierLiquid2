//! Symbols, and the chain handles and scales behind them.
//!
//! **Everything the gateway knows about turning `"BTC-PERP"` into something
//! the chain understands lives here.** Symbol handling is deliberately not
//! scattered through the handlers: `cb-types` is growing its own symbol module
//! alongside the equity market universe, and when it lands this file is the
//! only thing that has to change.
//!
//! ## Scales come from `cb_types::scales`, not from here
//!
//! There are two quantity scales on this chain and they agree only for BTC:
//! spot quantities are in the asset's own base units, derivative quantities in
//! a flat `10^8` contract unit. The spot wire price scale is
//! `quote_decimals + 8 - base_decimals`, which follows from the executor's
//! `notional = price * qty / 1e8`. Getting either wrong is a hundred-fold
//! error in someone's order, so this module asks `cb_types::scales` rather
//! than restating the arithmetic.
//!
//! ## ETH spot is not listable, and that is upstream
//!
//! ETH's 18 decimals against the fixed `1e8` divisor give a *negative* price
//! exponent: one whole ETH would cost less than one wire-price unit, so no
//! human price is representable. `cb_types::scales::spot_price_scale_exp`
//! reports this as an error rather than rounding it away. The gateway
//! therefore omits any such market from `/markets` and answers `404` for it,
//! instead of publishing a market whose prices it cannot state. Every asset
//! listed since was chosen with `decimals <= 8` precisely so it cannot land
//! there.

use cb_types::scales::{self, ScaleError};
use cb_types::{AssetClass, AssetId, DerivId, MarketId, CONTRACT_QTY_DECIMALS};
use std::collections::BTreeMap;

/// Which on-chain book a symbol refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarketRef {
    /// A spot CLOB, addressed by the closed `MarketId` enum.
    Spot(MarketId),
    /// A perpetual or dated future, addressed by a `DerivId`.
    Deriv(DerivId),
}

/// Instrument family, as the contract's `kind` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKind {
    /// Spot.
    Spot,
    /// Perpetual.
    Perp,
    /// Dated future.
    Future,
}

impl InstrumentKind {
    /// The wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perp => "perp",
            Self::Future => "future",
        }
    }
}

/// The fixed-point scales one market trades in, as decimal exponents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketScales {
    /// Decimal places in the on-chain price integer.
    pub price_decimals: u32,
    /// Decimal places in the on-chain quantity integer.
    pub qty_decimals: u32,
    /// Decimal places in the quote asset's balance integer.
    pub quote_decimals: u32,
}

impl MarketScales {
    /// Scales for a spot market, or the reason it cannot be quoted.
    pub fn spot(market: MarketId) -> Result<Self, ScaleError> {
        Ok(Self {
            price_decimals: scales::market_price_scale_exp(market)? as u32,
            qty_decimals: market.base().decimals() as u32,
            quote_decimals: market.quote().decimals() as u32,
        })
    }

    /// Scales for a perpetual or dated future. Flat, and the same for every
    /// underlying: `q8` prices, `1e8` contract quantities, micro-USD notional.
    pub fn deriv() -> Self {
        Self {
            price_decimals: 8,
            qty_decimals: CONTRACT_QTY_DECIMALS as u32,
            quote_decimals: AssetId::Usd.decimals() as u32,
        }
    }
}

/// How many decimal places an asset balance is shown with by default.
///
/// A floor, not a truncation: [`crate::decimal::format_scaled`] keeps any
/// finer precision the value carries.
pub fn asset_display_decimals(asset: AssetId) -> u32 {
    match asset.class() {
        AssetClass::Fiat => 2,
        _ => (asset.decimals() as u32).min(8),
    }
}

/// The wire name of an asset, uppercase.
pub fn asset_symbol(asset: AssetId) -> &'static str {
    asset.symbol()
}

/// Parse an asset name, case-insensitively.
pub fn parse_asset(symbol: &str) -> Option<AssetId> {
    let upper = symbol.to_ascii_uppercase();
    // The chain's native denom is configurable and commonly reads "ULD"; it is
    // the same on-chain asset as USD, so both names resolve.
    if upper == "ULD" {
        return Some(AssetId::Usd);
    }
    AssetId::ALL.into_iter().find(|a| a.symbol() == upper)
}

/// The contract symbol for a spot market: `{BASE}-{QUOTE}`.
pub fn spot_symbol(market: MarketId) -> String {
    format!("{}-{}", market.base().symbol(), market.quote().symbol())
}

/// The contract symbol for a perpetual: `{BASE}-PERP`.
pub fn perp_symbol(underlying: AssetId) -> String {
    format!("{}-PERP", underlying.symbol())
}

/// The contract symbol for a dated future: `{BASE}-{YYMMDD}`.
pub fn future_symbol(underlying: AssetId, expiry: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Datelike;
    format!(
        "{}-{:02}{:02}{:02}",
        underlying.symbol(),
        expiry.year().rem_euclid(100),
        expiry.month(),
        expiry.day()
    )
}

/// Whether a string is shaped like a symbol the contract defines.
///
/// Lets the HTTP layer tell "that market does not exist" (`404`) apart from
/// "that is not a symbol" (`400`).
pub fn is_well_formed(symbol: &str) -> bool {
    let Some((base, suffix)) = symbol.split_once('-') else {
        return false;
    };
    if base.is_empty()
        || !base
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return false;
    }
    if suffix.is_empty() {
        return false;
    }
    if suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    suffix.bytes().all(|b| b.is_ascii_uppercase())
}

/// A resolved market: symbol, chain handle, scales and the facts the contract
/// publishes about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketEntry {
    /// The contract symbol.
    pub symbol: String,
    /// The chain handle to address it by.
    pub market: MarketRef,
    /// Instrument family.
    pub kind: InstrumentKind,
    /// Base asset.
    pub base: AssetId,
    /// Quote asset.
    pub quote: AssetId,
    /// Fixed-point scales.
    pub scales: MarketScales,
    /// `crypto` or `equity`, as the contract's `asset_class`.
    pub asset_class: &'static str,
    /// Human-facing name.
    pub display_name: String,
}

/// The `asset_class` value the contract publishes for an underlying.
pub fn asset_class_of(asset: AssetId) -> &'static str {
    match asset.class() {
        AssetClass::Equity => "equity",
        _ => "crypto",
    }
}

/// The gateway's symbol table.
///
/// Built from the chain's closed spot enum plus whatever derivative markets
/// the node reports, and refreshed when the market list changes. Lookups go
/// both ways because REST speaks symbols and RPC speaks `MarketId`/`DerivId`.
#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    by_symbol: BTreeMap<String, MarketEntry>,
    by_ref: BTreeMap<MarketRef, String>,
}

impl SymbolTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace an entry.
    pub fn insert(&mut self, entry: MarketEntry) {
        self.by_ref.insert(entry.market, entry.symbol.clone());
        self.by_symbol.insert(entry.symbol.clone(), entry);
    }

    /// Look a symbol up, case-insensitively.
    pub fn get(&self, symbol: &str) -> Option<&MarketEntry> {
        self.by_symbol.get(&symbol.to_ascii_uppercase())
    }

    /// Resolve a chain handle to its entry.
    pub fn by_market(&self, market: MarketRef) -> Option<&MarketEntry> {
        self.by_ref.get(&market).and_then(|s| self.by_symbol.get(s))
    }

    /// The symbol for a chain handle.
    pub fn symbol_for(&self, market: MarketRef) -> Option<&str> {
        self.by_ref.get(&market).map(String::as_str)
    }

    /// Every entry, in symbol order.
    pub fn entries(&self) -> impl Iterator<Item = &MarketEntry> {
        self.by_symbol.values()
    }

    /// How many markets are listed.
    pub fn len(&self) -> usize {
        self.by_symbol.len()
    }

    /// Whether the table is empty, which means the node has not been read yet.
    pub fn is_empty(&self) -> bool {
        self.by_symbol.is_empty()
    }

    /// Every spot market the chain's closed enum can express *and* the gateway
    /// can quote a price for.
    ///
    /// Markets whose scales are unrepresentable — ETH/USD today — are omitted
    /// rather than listed with a price the gateway would have to invent.
    pub fn with_spot_markets() -> Self {
        let mut table = Self::new();
        for market in MarketId::ALL {
            match MarketScales::spot(market) {
                Ok(scales) => table.insert(MarketEntry {
                    symbol: spot_symbol(market),
                    market: MarketRef::Spot(market),
                    kind: InstrumentKind::Spot,
                    base: market.base(),
                    quote: market.quote(),
                    scales,
                    asset_class: asset_class_of(market.base()),
                    display_name: format!("{} Spot", market.base().name()),
                }),
                Err(e) => tracing::warn!(
                    target: "cb_gateway::symbols",
                    market = %market,
                    error = %e,
                    "spot market omitted: its prices are not representable"
                ),
            }
        }
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_symbols_follow_the_base_quote_form() {
        assert_eq!(spot_symbol(MarketId::BtcUsd), "BTC-USD");
        assert_eq!(spot_symbol(MarketId::AaplUsd), "AAPL-USD");
    }

    #[test]
    fn perp_and_future_symbols_follow_the_contract_forms() {
        assert_eq!(perp_symbol(AssetId::Btc), "BTC-PERP");
        assert_eq!(perp_symbol(AssetId::Tsla), "TSLA-PERP");
        let expiry = chrono::DateTime::parse_from_rfc3339("2026-06-26T08:00:00Z")
            .expect("static timestamp")
            .with_timezone(&chrono::Utc);
        assert_eq!(future_symbol(AssetId::Eth, expiry), "ETH-260626");
    }

    #[test]
    fn spot_price_scale_is_not_the_derivative_scale() {
        // The hundred-fold bug. If BTC/USD ever reads 8 here, spot orders are
        // being sent at the derivatives scale and every lock is 100x wrong.
        let btc = MarketScales::spot(MarketId::BtcUsd).expect("BTC/USD is quotable");
        assert_eq!(btc.price_decimals, 6);
        assert_eq!(btc.qty_decimals, 8);
        let aapl = MarketScales::spot(MarketId::AaplUsd).expect("AAPL/USD is quotable");
        assert_eq!(aapl.price_decimals, 8);
        assert_eq!(aapl.qty_decimals, 6);
        assert_eq!(MarketScales::deriv().price_decimals, 8);
        assert_eq!(MarketScales::deriv().qty_decimals, 8);
    }

    #[test]
    fn eth_spot_is_unquotable_and_therefore_unlisted() {
        assert!(MarketScales::spot(MarketId::EthUsd).is_err());
        let table = SymbolTable::with_spot_markets();
        assert!(
            table.get("ETH-USD").is_none(),
            "a market whose prices cannot be stated must not be published"
        );
        assert!(table.get("BTC-USD").is_some());
    }

    #[test]
    fn spot_notional_math_agrees_with_the_executor() {
        // The executor computes `price * qty / 1e8` and credits the result to
        // the quote balance. One BTC at $64,250 must lock exactly
        // 64_250_000_000 micro-USD.
        let scales = MarketScales::spot(MarketId::BtcUsd).expect("quotable");
        let price = crate::decimal::parse_scaled("64250", scales.price_decimals).expect("price");
        let qty = crate::decimal::parse_scaled("1", scales.qty_decimals).expect("qty");
        assert_eq!(price * qty / 100_000_000u128, 64_250_000_000);

        // ...and the same for an equity, where both scales differ from BTC's.
        let scales = MarketScales::spot(MarketId::AaplUsd).expect("quotable");
        let price = crate::decimal::parse_scaled("190.25", scales.price_decimals).expect("price");
        let qty = crate::decimal::parse_scaled("10", scales.qty_decimals).expect("qty");
        assert_eq!(price * qty / 100_000_000u128, 1_902_500_000);
    }

    #[test]
    fn deriv_notional_math_agrees_with_the_margin_module() {
        let scales = MarketScales::deriv();
        let price = crate::decimal::parse_scaled("64250", scales.price_decimals).expect("price");
        let qty = crate::decimal::parse_scaled("1", scales.qty_decimals).expect("qty");
        assert_eq!(qty * price / 10u128.pow(10), 64_250_000_000);
    }

    #[test]
    fn well_formed_rejects_what_is_not_a_symbol() {
        assert!(is_well_formed("BTC-PERP"));
        assert!(is_well_formed("BTC-USD"));
        assert!(is_well_formed("ETH-260626"));
        assert!(!is_well_formed("btc-perp"));
        assert!(!is_well_formed("BTCPERP"));
        assert!(!is_well_formed("-PERP"));
        assert!(!is_well_formed("BTC-"));
    }

    #[test]
    fn the_table_resolves_both_directions() {
        let table = SymbolTable::with_spot_markets();
        let entry = table.get("btc-usd").expect("case-insensitive lookup");
        assert_eq!(entry.market, MarketRef::Spot(MarketId::BtcUsd));
        assert_eq!(
            table.symbol_for(MarketRef::Spot(MarketId::BtcUsd)),
            Some("BTC-USD")
        );
    }

    #[test]
    fn equities_are_classified_as_equities() {
        assert_eq!(asset_class_of(AssetId::Aapl), "equity");
        assert_eq!(asset_class_of(AssetId::Btc), "crypto");
        assert_eq!(asset_class_of(AssetId::Usd), "crypto");
    }

    #[test]
    fn the_native_denom_alias_resolves_to_the_same_asset() {
        assert_eq!(parse_asset("uld"), Some(AssetId::Usd));
        assert_eq!(parse_asset("USD"), Some(AssetId::Usd));
        assert_eq!(parse_asset("nvda"), Some(AssetId::Nvda));
        assert_eq!(parse_asset("DOGE"), None);
    }
}
