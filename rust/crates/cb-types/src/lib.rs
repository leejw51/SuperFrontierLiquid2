//! Primitive types shared across SuperFrontierLiquid crates.
//!
//! # Where the market universe is defined
//!
//! Three modules together own what is tradable and at what scale. Nothing
//! outside them should hard-code an asset's decimals, a market's symbol, or a
//! contract's index:
//!
//! - [`ids`] — the [`AssetId`] / [`MarketId`] enums and [`ASSET_SPECS`], the
//!   single authoritative table of per-asset decimals and classification.
//! - [`scales`] — the conversions between human numbers and on-chain integers,
//!   including the two *different* quantity scales (spot base units vs.
//!   derivative contract units) that differ by 100× on an equity.
//! - [`symbols`] — the wire-symbol mapping (`"BTC-PERP"` ↔ [`DerivId`]) and the
//!   declared risk parameters of every listed derivative.
//!
//! [`calendar`] holds the equity trading-session policy, which is a product
//! decision rather than a type; read its module docs before changing it.

pub mod address;
pub mod amount;
pub mod calendar;
pub mod derivatives;
pub mod hash;
pub mod ids;
pub mod scales;
pub mod signed;
pub mod symbols;

pub use address::Address;
pub use calendar::{SessionCalendar, TradingPolicy, EQUITY_TRADING_POLICY};
pub use derivatives::{
    DerivId, DerivIdError, FundingIndex, FutureMarket, LiquidationEntry, MarginAccount, MarkPrice,
    MarketKind, OraclePrice, PerpMarket, PerpOrder, Position, Underlying,
};
pub use hash::H256;
pub use ids::{
    AssetClass, AssetId, AssetIdError, AssetSpec, MarketId, MarketIdError, Side, Tif, ASSET_SPECS,
    CONTRACT_QTY_DECIMALS,
};
pub use scales::{ScaleError, SPOT_NOTIONAL_DIVISOR_DECIMALS};
pub use signed::{Signed, SignedError};
pub use symbols::{
    parse_symbol, spot_symbol, FutureSpec, Instrument, PerpSpec, RiskParams, SymbolError, FUTURES,
    PERPS,
};
