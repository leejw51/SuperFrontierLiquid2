//! The market universe the simulation trades, and the fixed-point constants
//! that go with it.
//!
//! Kept in one module so a run, a test and the invariant checker can never
//! disagree about which markets exist or how a price is scaled. Other
//! engineers are concurrently growing the on-chain universe (equities are in
//! flight); nothing here assumes the chain has *only* these markets — the
//! invariant checker discovers markets by listing the column families, and
//! this module only describes what the simulation itself lists and trades.

use cb_types::{AssetId, DerivId, MarketId};

/// Price fixed-point denominator: prices are quote-per-base scaled `1e8`.
pub const Q8: u128 = 100_000_000;

/// USD fixed-point denominator: collateral and notional are micro-USD (`1e6`).
pub const MICRO: u128 = 1_000_000;

/// One unit of an 8-decimal base asset (one satoshi's worth of scale).
pub const BASE_ONE: u128 = 100_000_000;

/// `qty * price_q8 / NOTIONAL_DEN` lands in micro-USD. Mirrors
/// `cb_engine::position_notional_micro_usd`, restated here so the checker's
/// arithmetic is legible on its own.
pub const NOTIONAL_DEN: u128 = 10_000_000_000;

/// BTC perpetual.
pub const PERP_BTC: DerivId = DerivId::perp(0);
/// ETH perpetual.
pub const PERP_ETH: DerivId = DerivId::perp(1);
/// BTC dated future. Expires inside a normal run so settlement is exercised.
pub const FUT_BTC: DerivId = DerivId::future(0);
/// BTC/USD spot market.
pub const SPOT_BTC: MarketId = MarketId::BtcUsd;

/// Underlyings the scenario engine prices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Underlying {
    /// Bitcoin.
    Btc,
    /// Ether.
    Eth,
}

impl Underlying {
    /// Every underlying, in a fixed order. Never iterate a map instead.
    pub const ALL: [Underlying; 2] = [Underlying::Btc, Underlying::Eth];

    /// Starting index price in q8.
    pub fn start_price_q8(self) -> u128 {
        match self {
            Underlying::Btc => 50_000 * Q8,
            Underlying::Eth => 3_000 * Q8,
        }
    }

    /// The on-chain asset this underlying maps to.
    pub fn asset(self) -> AssetId {
        match self {
            Underlying::Btc => AssetId::Btc,
            Underlying::Eth => AssetId::Eth,
        }
    }

    /// Human name, for reports.
    pub fn label(self) -> &'static str {
        match self {
            Underlying::Btc => "BTC",
            Underlying::Eth => "ETH",
        }
    }
}

/// A derivative market the simulation lists, paired with its price source.
#[derive(Clone, Copy, Debug)]
pub struct SimMarket {
    /// On-chain id.
    pub id: DerivId,
    /// Which scenario price path drives its oracle.
    pub underlying: Underlying,
    /// Human name for reports.
    pub label: &'static str,
}

/// Every derivative market the simulation lists, in fixed order.
pub const SIM_MARKETS: [SimMarket; 3] = [
    SimMarket {
        id: PERP_BTC,
        underlying: Underlying::Btc,
        label: "BTC-PERP",
    },
    SimMarket {
        id: PERP_ETH,
        underlying: Underlying::Eth,
        label: "ETH-PERP",
    },
    SimMarket {
        id: FUT_BTC,
        underlying: Underlying::Btc,
        label: "BTC-FUT",
    },
];

/// Round `price_q8` down to the nearest multiple of `tick`.
pub fn round_to_tick(price_q8: u128, tick: u128) -> u128 {
    if tick <= 1 {
        return price_q8;
    }
    (price_q8 / tick) * tick
}

/// Apply a signed basis-point offset to a price, saturating at 1.
pub fn bump_bps(price_q8: u128, bps: i64) -> u128 {
    if bps >= 0 {
        price_q8.saturating_mul(10_000 + bps as u128) / 10_000
    } else {
        let down = (-bps) as u128;
        let mult = 10_000u128.saturating_sub(down);
        (price_q8.saturating_mul(mult) / 10_000).max(1)
    }
}
