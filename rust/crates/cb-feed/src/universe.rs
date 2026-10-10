//! The instruments this feed carries, and the scenarios it can be put through.
//!
//! Every number here is a named constant with a comment, because a magic
//! `0.65` in a price model is unreviewable: nobody can tell whether it is a
//! daily volatility, an annual one, or a typo. All volatilities and drifts in
//! this module are **annualised**, the convention every published figure uses,
//! and are converted to per-tick quantities in one place ([`crate::sim`]).
//!
//! The figures are plausible rather than fitted. What matters for the
//! simulator is that the *relationships* hold — crypto moves several times as
//! much as equities, a broad index moves less than a single name, and a
//! high-beta name moves more than a mega-cap — because those are what a trader
//! forms intuitions about.

use rust_decimal::Decimal;

/// Whether an instrument trades continuously or on a session calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetClass {
    /// Trades 24/7.
    Crypto,
    /// Trades during exchange hours only.
    Equity,
}

impl AssetClass {
    /// The string the ports use for this class.
    pub fn as_str(&self) -> &'static str {
        match self {
            AssetClass::Crypto => "crypto",
            AssetClass::Equity => "equity",
        }
    }
}

/// Everything the simulator needs to know about one underlying.
#[derive(Debug, Clone, Copy)]
pub struct AssetSpec {
    /// Feed symbol, quoted in USD.
    pub symbol: &'static str,
    /// Bare ticker, as reference data names it.
    pub ticker: &'static str,
    /// Human-facing name.
    pub name: &'static str,
    /// Continuous or session-bound.
    pub asset_class: AssetClass,
    /// Listing venue, for equities.
    pub venue: Option<&'static str>,
    /// Price at step zero, in USD.
    pub start_price: f64,
    /// Annualised volatility, as a fraction (0.65 is 65%).
    pub annual_volatility: f64,
    /// Annualised drift, as a fraction.
    pub annual_drift: f64,
    /// Decimal places the price is quoted to.
    pub price_decimals: u32,
    /// Typical half-spread in basis points at the asset's baseline volatility.
    pub base_spread_bps: f64,
    /// Smallest quantity increment, as a decimal literal.
    pub lot_size: &'static str,
    /// Smallest price increment, as a decimal literal.
    pub tick_size: &'static str,
}

impl AssetSpec {
    /// The tick size as a decimal.
    ///
    /// # Panics
    ///
    /// Panics if a constant in this module is not a valid decimal literal,
    /// which is a compile-time-shaped bug caught at first use.
    pub fn tick(&self) -> Decimal {
        Decimal::from_str_exact(self.tick_size)
            .expect("every tick_size constant in the universe is a valid decimal literal")
    }

    /// The lot size as a decimal.
    ///
    /// # Panics
    ///
    /// Panics if a constant in this module is not a valid decimal literal.
    pub fn lot(&self) -> Decimal {
        Decimal::from_str_exact(self.lot_size)
            .expect("every lot_size constant in the universe is a valid decimal literal")
    }
}

// Volatility figures, annualised. Collected here so the ordering between them
// is visible at a glance — that ordering is the property the simulator's feel
// depends on, and a test asserts it.
//
// Crypto: majors sit in the 60–80% range; a smaller, higher-beta chain runs
// well above that.
const VOL_BTC: f64 = 0.65;
const VOL_ETH: f64 = 0.80;
const VOL_SOL: f64 = 1.10;
// Equities: a broad index is the calmest thing listed, a mega-cap sits in the
// twenties, an AI-cycle semiconductor name higher, and a retail-favourite
// high-beta name is the most volatile equity here — and still below the
// calmest crypto.
const VOL_SPY: f64 = 0.15;
const VOL_AAPL: f64 = 0.25;
const VOL_NVDA: f64 = 0.42;
const VOL_TSLA: f64 = 0.50;

// Baseline drift. Small and positive: a simulated market that trends hard by
// default would make every long look clever, which is the wrong lesson for a
// trading simulator to teach. Scenarios are how a trend gets requested.
const DRIFT_CRYPTO: f64 = 0.15;
const DRIFT_EQUITY: f64 = 0.08;

// Half-spreads at baseline volatility, in basis points. A liquid perp quotes
// inside a basis point; a single equity name quotes wider; an index ETF is
// the tightest instrument on the list.
const SPREAD_BPS_BTC: f64 = 1.0;
const SPREAD_BPS_ETH: f64 = 1.5;
const SPREAD_BPS_SOL: f64 = 4.0;
const SPREAD_BPS_SPY: f64 = 0.8;
const SPREAD_BPS_AAPL: f64 = 1.2;
const SPREAD_BPS_NVDA: f64 = 1.8;
const SPREAD_BPS_TSLA: f64 = 2.5;

/// Every instrument this feed carries, ordered so that
/// [`crate::SimulatedFeed::supported_symbols`] is stable across runs.
pub const UNIVERSE: &[AssetSpec] = &[
    AssetSpec {
        symbol: "BTC-USD",
        ticker: "BTC",
        name: "Bitcoin",
        asset_class: AssetClass::Crypto,
        venue: None,
        start_price: 64_000.0,
        annual_volatility: VOL_BTC,
        annual_drift: DRIFT_CRYPTO,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_BTC,
        lot_size: "0.00001",
        tick_size: "0.01",
    },
    AssetSpec {
        symbol: "ETH-USD",
        ticker: "ETH",
        name: "Ether",
        asset_class: AssetClass::Crypto,
        venue: None,
        start_price: 3_200.0,
        annual_volatility: VOL_ETH,
        annual_drift: DRIFT_CRYPTO,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_ETH,
        lot_size: "0.0001",
        tick_size: "0.01",
    },
    AssetSpec {
        symbol: "SOL-USD",
        ticker: "SOL",
        name: "Solana",
        asset_class: AssetClass::Crypto,
        venue: None,
        start_price: 145.0,
        annual_volatility: VOL_SOL,
        annual_drift: DRIFT_CRYPTO,
        price_decimals: 3,
        base_spread_bps: SPREAD_BPS_SOL,
        lot_size: "0.01",
        tick_size: "0.001",
    },
    AssetSpec {
        symbol: "AAPL-USD",
        ticker: "AAPL",
        name: "Apple Inc.",
        asset_class: AssetClass::Equity,
        venue: Some("NASDAQ"),
        start_price: 225.0,
        annual_volatility: VOL_AAPL,
        annual_drift: DRIFT_EQUITY,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_AAPL,
        lot_size: "1",
        tick_size: "0.01",
    },
    AssetSpec {
        symbol: "TSLA-USD",
        ticker: "TSLA",
        name: "Tesla, Inc.",
        asset_class: AssetClass::Equity,
        venue: Some("NASDAQ"),
        start_price: 340.0,
        annual_volatility: VOL_TSLA,
        annual_drift: DRIFT_EQUITY,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_TSLA,
        lot_size: "1",
        tick_size: "0.01",
    },
    AssetSpec {
        symbol: "NVDA-USD",
        ticker: "NVDA",
        name: "NVIDIA Corporation",
        asset_class: AssetClass::Equity,
        venue: Some("NASDAQ"),
        start_price: 128.0,
        annual_volatility: VOL_NVDA,
        annual_drift: DRIFT_EQUITY,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_NVDA,
        lot_size: "1",
        tick_size: "0.01",
    },
    AssetSpec {
        symbol: "SPY-USD",
        ticker: "SPY",
        name: "SPDR S&P 500 ETF Trust",
        asset_class: AssetClass::Equity,
        venue: Some("NYSE ARCA"),
        start_price: 560.0,
        annual_volatility: VOL_SPY,
        annual_drift: DRIFT_EQUITY,
        price_decimals: 2,
        base_spread_bps: SPREAD_BPS_SPY,
        lot_size: "1",
        tick_size: "0.01",
    },
];

/// Look an instrument up by feed symbol (`BTC-USD`) or bare ticker (`BTC`),
/// case-insensitively.
///
/// Accepting both spellings is deliberate: the market-data port names things
/// `BTC-USD`, while the reference-data port names the same instrument `BTC`.
/// Making callers translate between the two would put a lookup table in every
/// caller instead of one in here.
pub fn lookup(symbol: &str) -> Option<&'static AssetSpec> {
    UNIVERSE.iter().find(|spec| {
        spec.symbol.eq_ignore_ascii_case(symbol) || spec.ticker.eq_ignore_ascii_case(symbol)
    })
}

/// How many steps a scenario's opening shock is spread over.
///
/// Delivering a crash as one enormous step would be a single outlier that any
/// sane risk system would reject as bad data. Spreading it over a handful of
/// ticks produces something a liquidation engine actually has to survive: a
/// fast, continuous slide.
pub const SHOCK_STEPS: u32 = 8;

/// Total move applied by a crash's opening shock, as a multiplier.
///
/// A 25% drawdown inside a few ticks is a genuine crash: deep enough to
/// liquidate a 4x long, shallow enough that the market still exists
/// afterwards.
const CRASH_SHOCK_TOTAL: f64 = 0.75;

/// Total move applied by a squeeze's opening shock, as a multiplier.
const SQUEEZE_SHOCK_TOTAL: f64 = 1.30;

/// A scripted market regime.
///
/// A scenario changes drift and volatility, and two of them also inject an
/// opening shock. The distinction matters: drift alone, over the short
/// horizons a simulation session covers, is indistinguishable from noise —
/// which is why [`Scenario::Crash`] carries a shock rather than merely a large
/// negative drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scenario {
    /// Baseline drift, suppressed volatility. The market ticks quietly.
    #[default]
    Calm,
    /// A sustained bid. Strong positive drift, normal volatility.
    TrendUp,
    /// A sustained offer. Strong negative drift, slightly elevated volatility.
    TrendDown,
    /// A sharp drawdown: an opening shock, then continued selling in high
    /// volatility.
    Crash,
    /// A sharp rally: the mirror of [`Scenario::Crash`].
    Squeeze,
    /// Directionless and violent. No drift, high volatility, and a pull back
    /// towards where the scenario started, so the price thrashes inside a
    /// range instead of wandering off.
    Chop,
}

impl Scenario {
    /// Every scenario, for tests and for a UI that offers all of them.
    pub const ALL: [Scenario; 6] = [
        Scenario::Calm,
        Scenario::TrendUp,
        Scenario::TrendDown,
        Scenario::Crash,
        Scenario::Squeeze,
        Scenario::Chop,
    ];

    /// Parse the wire name used by the simulation control API.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "calm" => Some(Scenario::Calm),
            "trend_up" => Some(Scenario::TrendUp),
            "trend_down" => Some(Scenario::TrendDown),
            "crash" => Some(Scenario::Crash),
            "squeeze" => Some(Scenario::Squeeze),
            "chop" => Some(Scenario::Chop),
            _ => None,
        }
    }

    /// The wire name used by the simulation control API.
    pub fn as_str(&self) -> &'static str {
        match self {
            Scenario::Calm => "calm",
            Scenario::TrendUp => "trend_up",
            Scenario::TrendDown => "trend_down",
            Scenario::Crash => "crash",
            Scenario::Squeeze => "squeeze",
            Scenario::Chop => "chop",
        }
    }

    /// Annualised drift *added* to the asset's baseline.
    ///
    /// Additive rather than replacing, so an asset's own character survives
    /// the scenario: SOL still drifts faster than BTC in a rally.
    pub fn extra_annual_drift(&self) -> f64 {
        match self {
            Scenario::Calm => 0.0,
            Scenario::TrendUp => 0.60,
            Scenario::TrendDown => -0.60,
            // Large enough that the slide continues after the opening shock,
            // rather than the price simply stabilising at the new level.
            Scenario::Crash => -3.0,
            Scenario::Squeeze => 3.0,
            Scenario::Chop => 0.0,
        }
    }

    /// Multiplier applied to the asset's baseline volatility.
    pub fn volatility_multiplier(&self) -> f64 {
        match self {
            Scenario::Calm => 0.6,
            Scenario::TrendUp => 1.0,
            Scenario::TrendDown => 1.1,
            Scenario::Crash => 3.0,
            Scenario::Squeeze => 2.5,
            Scenario::Chop => 2.0,
        }
    }

    /// Whether the scenario suppresses its own drift in favour of pulling back
    /// to where it started.
    pub fn is_mean_reverting(&self) -> bool {
        matches!(self, Scenario::Chop)
    }

    /// Per-step multiplier for the scenario's opening shock, and how many
    /// steps it lasts. `None` for scenarios that open with no shock.
    pub fn opening_shock(&self) -> Option<(f64, u32)> {
        let total = match self {
            Scenario::Crash => CRASH_SHOCK_TOTAL,
            Scenario::Squeeze => SQUEEZE_SHOCK_TOTAL,
            _ => return None,
        };
        Some((total.powf(1.0 / f64::from(SHOCK_STEPS)), SHOCK_STEPS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenarios_round_trip_through_their_wire_names() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::parse(s.as_str()), Some(s));
        }
        assert_eq!(Scenario::parse("moon"), None);
    }

    /// The relationship the simulator's feel depends on. Asserted rather than
    /// trusted, because it is easy to nudge one constant and silently make a
    /// mega-cap move like a small-cap token.
    #[test]
    fn every_equity_is_markedly_calmer_than_every_crypto() {
        let loudest_equity = UNIVERSE
            .iter()
            .filter(|s| s.asset_class == AssetClass::Equity)
            .map(|s| s.annual_volatility)
            .fold(f64::MIN, f64::max);
        let quietest_crypto = UNIVERSE
            .iter()
            .filter(|s| s.asset_class == AssetClass::Crypto)
            .map(|s| s.annual_volatility)
            .fold(f64::MAX, f64::min);

        assert!(
            loudest_equity < quietest_crypto,
            "the most volatile equity ({loudest_equity}) must sit below the \
             least volatile crypto ({quietest_crypto})"
        );
    }

    #[test]
    fn the_universe_covers_the_documented_markets_and_looks_up_both_spellings() {
        for ticker in ["BTC", "ETH", "SOL", "AAPL", "TSLA", "NVDA", "SPY"] {
            let by_ticker = lookup(ticker).unwrap_or_else(|| panic!("{ticker} is carried"));
            let by_symbol = lookup(&format!("{ticker}-USD"))
                .unwrap_or_else(|| panic!("{ticker}-USD is carried"));
            assert_eq!(by_ticker.symbol, by_symbol.symbol);
            assert_eq!(
                lookup(&ticker.to_lowercase()).map(|s| s.symbol),
                Some(by_ticker.symbol),
                "lookup is case-insensitive"
            );
        }
        assert!(lookup("DOGE").is_none());
    }

    #[test]
    fn a_crash_shock_compounds_to_the_intended_drawdown() {
        let (per_step, steps) = Scenario::Crash
            .opening_shock()
            .expect("a crash has an opening shock");
        let total = per_step.powi(steps as i32);
        assert!(
            (total - CRASH_SHOCK_TOTAL).abs() < 1e-12,
            "the per-step factor must compound to exactly the intended total, \
             got {total}"
        );
        assert!(Scenario::Calm.opening_shock().is_none());
    }
}
