//! The authoritative mapping between **wire symbols** and **chain identifiers**.
//!
//! Every surface that speaks to a human or to another system — the gateway's
//! REST and WebSocket API, the simulator's bot configuration, the CLI, the web
//! frontend — needs to turn `"BTC-PERP"` into a [`DerivId`] and back. This module
//! is where that happens, once. A second implementation anywhere else is a bug:
//! two tables that agree today will disagree after the next listing, and the
//! disagreement shows up as orders routed to the wrong market.
//!
//! # Symbol grammar
//!
//! | Form | Example | Resolves to |
//! |------|---------|-------------|
//! | `{BASE}-USD` | `BTC-USD` | a spot [`MarketId`] |
//! | `{BASE}-PERP` | `AAPL-PERP` | a perpetual [`DerivId`] |
//! | `{BASE}-{YYMMDD}` | `ETH-260626` | a dated-future [`DerivId`] |
//!
//! Parsing is **table lookup, not pattern matching**. `"ETH-260626"` resolves
//! because that exact string is in [`FUTURES`], not because a `YYMMDD` parser
//! accepted six digits. A grammar-driven parser would happily invent
//! `DerivId::future(n)` for a contract the chain has never listed, and the
//! caller would only find out when the node answered `MarketNotFound` — or
//! worse, when index `n` turned out to be some *other* live contract. Unknown
//! symbols are rejected, which is what `unknown_symbols_are_rejected` pins.
//!
//! # This table describes the genesis-full universe
//!
//! [`DerivId`] indices are assigned by genesis line order, so what
//! `DerivId::perp(2)` *means* is a property of the chain, not of this crate. The
//! tables here describe the universe declared by `rust/genesis-full.jsonl`, and
//! `cb-genesis`'s `full_genesis` test asserts the file and these tables agree —
//! that cross-check is what makes it safe to have the listing written down in
//! two places.
//!
//! On a chain booted from one of the older genesis files the mapping is
//! narrower but compatible where it overlaps: `genesis.jsonl` and
//! `genesis-perps.jsonl` both put BTC-PERP at `perp(0)`, ETH-PERP at `perp(1)`,
//! and an ETH-dated future at `future(0)`, which is exactly why those indices
//! are pinned here. Their futures carry different expiry labels, so a
//! `"ETH-260626"` lookup against such a chain resolves to the right *index* and
//! the node's market record is authoritative for the actual expiry.
//!
//! # Expiry labels are nominal
//!
//! The `YYMMDD` in a futures symbol is a label. The contract's real expiry is
//! `FutureMarket::expiry_height`, a block height set in genesis, and **nothing
//! here derives one from the other**. Block time is a deployment property; a
//! chain booted a year late would have every derived date wrong, and silently.
//! [`FutureSpec::expiry_label`] carries the human date for display only.

use std::fmt;

use thiserror::Error;

use crate::derivatives::{DerivId, MarketKind};
use crate::ids::{AssetId, MarketId};

/// Suffix marking a perpetual contract.
const PERP_SUFFIX: &str = "PERP";
/// Suffix marking a USD-quoted spot market.
const SPOT_SUFFIX: &str = "USD";

/// A resolved instrument: what a wire symbol names on this chain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Instrument {
    /// A spot order book.
    Spot(MarketId),
    /// A perpetual contract. The `DerivId` always has `kind == Perp`.
    Perp(DerivId),
    /// A dated cash-settled future. The `DerivId` always has `kind == Future`.
    Future(DerivId),
}

impl Instrument {
    /// The underlying asset traded, for any instrument form.
    pub fn underlying(self) -> Option<AssetId> {
        match self {
            Instrument::Spot(m) => Some(m.base()),
            Instrument::Perp(d) => perp_spec(d).map(|s| s.underlying),
            Instrument::Future(d) => future_spec(d).map(|s| s.underlying),
        }
    }

    /// `true` when the underlying is a cash-settled synthetic equity.
    pub fn is_equity(self) -> bool {
        self.underlying().is_some_and(|a| a.is_equity())
    }

    /// The canonical wire symbol, or `None` for a `DerivId` outside the listed
    /// universe.
    pub fn symbol(self) -> Option<&'static str> {
        format_symbol(self)
    }
}

impl fmt::Display for Instrument {
    /// The wire symbol when the instrument is listed, otherwise a diagnostic
    /// form — never a guess that could be mistaken for a real symbol.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.symbol() {
            Some(s) => f.write_str(s),
            None => match self {
                Instrument::Spot(m) => write!(f, "<unlisted spot {m}>"),
                Instrument::Perp(d) | Instrument::Future(d) => write!(f, "<unlisted {d}>"),
            },
        }
    }
}

/// Margin parameters for a derivative market.
///
/// `initial_margin_bps` is the reciprocal of the market's maximum leverage
/// (`bps = 1_000_000 / max_leverage_x100`), and `maintenance_margin_bps` is half
/// of it. The relationship is asserted by `risk_params_are_internally_consistent`
/// — a market whose stated leverage and stated margin disagree is a market that
/// will liquidate at a price nobody expects.
///
/// # These are not yet enforced per market
///
/// The executor currently gates every derivative order against
/// `cb_engine::margin::DEFAULT_TIERS`, a piecewise notional-banded table, rather
/// than against the per-market values recorded here and in genesis. That
/// predates this work and wiring it up would change margin behaviour on
/// already-listed markets, so it is left alone deliberately. Until it is wired,
/// treat these fields as the *declared* risk profile of each listing: they are
/// persisted on the market record, served over RPC, and displayed — but the
/// liquidation engine reads the default tiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RiskParams {
    /// Maximum leverage, times 100. `5000` is 50×.
    pub max_leverage_x100: u32,
    /// Initial margin requirement in basis points.
    pub initial_margin_bps: u16,
    /// Maintenance margin requirement in basis points.
    pub maintenance_margin_bps: u16,
}

impl RiskParams {
    /// Build a risk profile from a leverage cap, deriving both margins.
    ///
    /// `initial = 1_000_000 / leverage_x100` (the reciprocal), `maintenance =
    /// initial / 2`. Deriving rather than hand-writing the pair is what keeps a
    /// typo from producing a market that claims 5× and margins like 50×.
    const fn from_leverage(max_leverage_x100: u32) -> Self {
        let initial_margin_bps = (1_000_000 / max_leverage_x100) as u16;
        Self {
            max_leverage_x100,
            initial_margin_bps,
            maintenance_margin_bps: initial_margin_bps / 2,
        }
    }
}

/// A listed perpetual contract.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PerpSpec {
    /// Wire symbol, e.g. `"BTC-PERP"`.
    pub symbol: &'static str,
    /// Genesis registration index: this contract is `DerivId::perp(index)`.
    pub index: u16,
    /// Asset the contract is priced on.
    pub underlying: AssetId,
    /// Declared margin profile. See [`RiskParams`].
    pub risk: RiskParams,
}

impl PerpSpec {
    /// The chain identifier for this contract.
    pub const fn deriv_id(&self) -> DerivId {
        DerivId::perp(self.index)
    }
}

/// A listed dated future.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FutureSpec {
    /// Wire symbol, e.g. `"ETH-260626"`.
    pub symbol: &'static str,
    /// Genesis registration index: this contract is `DerivId::future(index)`.
    pub index: u16,
    /// Asset the contract is priced on.
    pub underlying: AssetId,
    /// Declared margin profile. See [`RiskParams`].
    pub risk: RiskParams,
    /// Human-readable nominal expiry, e.g. `"2026-06-26"`. **Display only** —
    /// the contract's real expiry is a block height in genesis. See the module
    /// docs.
    pub expiry_label: &'static str,
}

impl FutureSpec {
    /// The chain identifier for this contract.
    pub const fn deriv_id(&self) -> DerivId {
        DerivId::future(self.index)
    }
}

/// Every listed perpetual, in genesis registration order.
///
/// # Leverage, and why each underlying gets the cap it does
///
/// | Symbol | Max leverage | IM | MM |
/// |--------|--------------|----|----|
/// | BTC-PERP | 50× | 2.0% | 1.0% |
/// | ETH-PERP | 25× | 4.0% | 2.0% |
/// | SOL-PERP | 20× | 5.0% | 2.5% |
/// | AAPL / TSLA / NVDA / SPY -PERP | 5× | 20.0% | 10.0% |
///
/// The ordering follows liquidity and the depth of book a liquidation can
/// actually be worked through. BTC's book is the deepest, so a position can be
/// closed near the mark and a thin 1% maintenance buffer is enough to cover the
/// slippage; ETH's is shallower, SOL's shallower again, and each step down halves
/// or nearly halves the cap.
///
/// **Equities get 5×, far below any crypto market, for a reason specific to what
/// they are here.** An equity on this chain is a cash-settled synthetic marked
/// against a *mocked* reference feed (see [`crate::ids::AssetClass::Equity`]).
/// Two things follow. First, the reference price is the only thing tethering the
/// contract to anything, and a synthetic with no arbitrage path to a real share
/// can decouple from its mark much further than a crypto perp whose spot book
/// sits on the same venue. Second, the underlying itself gaps: a real equity can
/// open several percent away from its previous close on an earnings print, and a
/// 50×-margined position with a 1% buffer is already insolvent before any
/// liquidation can be triggered. 20% initial margin means a position survives a
/// move that would be unremarkable for a single-name equity. SPY, an index, is
/// genuinely less gappy than the three single names, but it shares their cap:
/// the difference is not large enough to be worth a special case, and a uniform
/// equity tier is one less thing for a trader to get wrong.
///
/// Indices 0 and 1 are pinned to BTC and ETH because both pre-existing genesis
/// files already register them there; changing either would silently repoint
/// every stored position on an existing chain.
pub const PERPS: [PerpSpec; 7] = [
    PerpSpec {
        symbol: "BTC-PERP",
        index: 0,
        underlying: AssetId::Btc,
        risk: RiskParams::from_leverage(5_000),
    },
    PerpSpec {
        symbol: "ETH-PERP",
        index: 1,
        underlying: AssetId::Eth,
        risk: RiskParams::from_leverage(2_500),
    },
    PerpSpec {
        symbol: "SOL-PERP",
        index: 2,
        underlying: AssetId::Sol,
        risk: RiskParams::from_leverage(2_000),
    },
    PerpSpec {
        symbol: "AAPL-PERP",
        index: 3,
        underlying: AssetId::Aapl,
        risk: RiskParams::from_leverage(500),
    },
    PerpSpec {
        symbol: "TSLA-PERP",
        index: 4,
        underlying: AssetId::Tsla,
        risk: RiskParams::from_leverage(500),
    },
    PerpSpec {
        symbol: "NVDA-PERP",
        index: 5,
        underlying: AssetId::Nvda,
        risk: RiskParams::from_leverage(500),
    },
    PerpSpec {
        symbol: "SPY-PERP",
        index: 6,
        underlying: AssetId::Spy,
        risk: RiskParams::from_leverage(500),
    },
];

/// Every listed dated future, in genesis registration order.
///
/// | Symbol | Underlying | Nominal expiry | Max leverage | IM | MM |
/// |--------|------------|----------------|--------------|----|----|
/// | ETH-260626 | ETH | 2026-06-26 | 10× | 10% | 5% |
/// | BTC-261225 | BTC | 2026-12-25 | 25× | 4% | 2% |
/// | SOL-270326 | SOL | 2027-03-26 | 10× | 10% | 5% |
///
/// **Dated futures carry roughly half the leverage of the perpetual on the same
/// underlying.** A perp can be closed at any time against a continuously
/// converging mark; a future has a fixed settlement date, so a position taken
/// against it is exposed to basis — the gap between the futures price and the
/// underlying — which does not have to converge until expiry and can widen
/// sharply near it. Halving the cap prices in that extra exposure. BTC keeps the
/// widest cap of the three for the same liquidity reason it does on the perp
/// side.
///
/// Only crypto underlyings have dated futures; equities list spot and perpetual
/// only, per `docs/PLAN.md` §3.
///
/// **Index 0 is an ETH contract** because both pre-existing genesis files put an
/// ETH-dated future there. Their expiry labels differ from this one's — the
/// index is what is compatible, not the date. See the module docs.
pub const FUTURES: [FutureSpec; 3] = [
    FutureSpec {
        symbol: "ETH-260626",
        index: 0,
        underlying: AssetId::Eth,
        risk: RiskParams::from_leverage(1_000),
        expiry_label: "2026-06-26",
    },
    FutureSpec {
        symbol: "BTC-261225",
        index: 1,
        underlying: AssetId::Btc,
        risk: RiskParams::from_leverage(2_500),
        expiry_label: "2026-12-25",
    },
    FutureSpec {
        symbol: "SOL-270326",
        index: 2,
        underlying: AssetId::Sol,
        risk: RiskParams::from_leverage(1_000),
        expiry_label: "2027-03-26",
    },
];

/// Why a symbol could not be resolved.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SymbolError {
    /// The symbol does not name any listed instrument.
    #[error("unknown symbol `{0}`")]
    Unknown(String),
    /// The symbol was empty or had no `-` separator.
    #[error("malformed symbol `{0}`: expected `BASE-USD`, `BASE-PERP` or `BASE-YYMMDD`")]
    Malformed(String),
}

/// Resolve a wire symbol to an instrument.
///
/// Case-insensitive. Surrounding whitespace is *not* trimmed: a symbol arriving
/// with stray whitespace means the caller's framing is wrong, and quietly
/// accepting it hides that.
///
/// ```
/// use cb_types::symbols::{parse_symbol, Instrument};
/// use cb_types::{DerivId, MarketId};
///
/// assert_eq!(parse_symbol("BTC-USD").unwrap(), Instrument::Spot(MarketId::BtcUsd));
/// assert_eq!(parse_symbol("AAPL-PERP").unwrap(), Instrument::Perp(DerivId::perp(3)));
/// assert_eq!(parse_symbol("ETH-260626").unwrap(), Instrument::Future(DerivId::future(0)));
/// assert!(parse_symbol("DOGE-PERP").is_err());
/// ```
pub fn parse_symbol(s: &str) -> Result<Instrument, SymbolError> {
    if s.is_empty() {
        return Err(SymbolError::Malformed(s.to_string()));
    }
    let upper = s.to_ascii_uppercase();
    let (base, suffix) = upper
        .split_once('-')
        .ok_or_else(|| SymbolError::Malformed(s.to_string()))?;
    if base.is_empty() || suffix.is_empty() {
        return Err(SymbolError::Malformed(s.to_string()));
    }

    // Spot and perp are resolved through the market/asset tables; dated futures
    // are resolved by exact symbol, because their labels are data, not grammar.
    if suffix == SPOT_SUFFIX {
        return MarketId::ALL
            .iter()
            .copied()
            .find(|m| m.base().symbol() == base)
            .map(Instrument::Spot)
            .ok_or_else(|| SymbolError::Unknown(s.to_string()));
    }
    if suffix == PERP_SUFFIX {
        return PERPS
            .iter()
            .find(|p| p.underlying.symbol() == base)
            .map(|p| Instrument::Perp(p.deriv_id()))
            .ok_or_else(|| SymbolError::Unknown(s.to_string()));
    }
    FUTURES
        .iter()
        .find(|f| f.symbol == upper)
        .map(|f| Instrument::Future(f.deriv_id()))
        .ok_or_else(|| SymbolError::Unknown(s.to_string()))
}

/// Render an instrument as its canonical wire symbol.
///
/// Returns `None` for a `DerivId` that is not part of the listed universe —
/// deliberately, rather than fabricating a plausible-looking symbol for a market
/// nobody can trade.
pub fn format_symbol(i: Instrument) -> Option<&'static str> {
    match i {
        Instrument::Spot(m) => MarketId::ALL
            .iter()
            .position(|x| *x == m)
            .map(|_| spot_symbol(m)),
        Instrument::Perp(d) => perp_spec(d).map(|s| s.symbol),
        Instrument::Future(d) => future_spec(d).map(|s| s.symbol),
    }
}

/// The wire symbol for a spot market, e.g. `"BTC-USD"`.
///
/// Every `MarketId` is listed, so unlike the derivative forms this is total.
pub fn spot_symbol(m: MarketId) -> &'static str {
    match m {
        MarketId::BtcUsd => "BTC-USD",
        MarketId::EthUsd => "ETH-USD",
        MarketId::SolUsd => "SOL-USD",
        MarketId::AaplUsd => "AAPL-USD",
        MarketId::TslaUsd => "TSLA-USD",
        MarketId::NvdaUsd => "NVDA-USD",
        MarketId::SpyUsd => "SPY-USD",
    }
}

/// The [`PerpSpec`] for a `DerivId`, or `None` if it is not a listed perpetual.
pub fn perp_spec(d: DerivId) -> Option<&'static PerpSpec> {
    if d.kind().ok()? != MarketKind::Perp {
        return None;
    }
    PERPS.iter().find(|p| p.index == d.index())
}

/// The [`FutureSpec`] for a `DerivId`, or `None` if it is not a listed future.
pub fn future_spec(d: DerivId) -> Option<&'static FutureSpec> {
    if d.kind().ok()? != MarketKind::Future {
        return None;
    }
    FUTURES.iter().find(|f| f.index == d.index())
}

/// The perpetual on a given underlying, if one is listed.
pub fn perp_for_underlying(a: AssetId) -> Option<&'static PerpSpec> {
    PERPS.iter().find(|p| p.underlying == a)
}

/// Every listed instrument, spot then perps then futures.
///
/// The order is stable, so a caller rendering a market list gets the same
/// ordering on every run — which matters for snapshot tests and for a UI that
/// should not reshuffle between page loads.
pub fn all_instruments() -> Vec<Instrument> {
    let mut out: Vec<Instrument> = MarketId::ALL
        .iter()
        .copied()
        .map(Instrument::Spot)
        .collect();
    out.extend(PERPS.iter().map(|p| Instrument::Perp(p.deriv_id())));
    out.extend(FUTURES.iter().map(|f| Instrument::Future(f.deriv_id())));
    out
}

/// Every listed wire symbol, in the same order as [`all_instruments`].
pub fn all_symbols() -> Vec<&'static str> {
    all_instruments()
        .into_iter()
        .map(|i| i.symbol().expect("every listed instrument has a symbol"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- round-trip over the whole universe ----------------

    #[test]
    fn every_market_in_the_universe_round_trips() {
        // The property the gateway and the simulator both depend on: for every
        // listed market, symbol -> instrument -> symbol is the identity.
        let all = all_instruments();
        assert_eq!(
            all.len(),
            MarketId::ALL.len() + PERPS.len() + FUTURES.len(),
            "all_instruments must cover the whole universe"
        );
        for instrument in all {
            let symbol = instrument
                .symbol()
                .unwrap_or_else(|| panic!("{instrument:?} has no symbol"));
            assert_eq!(
                parse_symbol(symbol).unwrap(),
                instrument,
                "`{symbol}` did not round trip"
            );
        }
    }

    #[test]
    fn symbols_are_unique_across_forms() {
        let mut symbols = all_symbols();
        let before = symbols.len();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(before, symbols.len(), "two instruments share a wire symbol");
    }

    #[test]
    fn parsing_is_case_insensitive() {
        for s in all_symbols() {
            assert_eq!(
                parse_symbol(&s.to_ascii_lowercase()).unwrap(),
                parse_symbol(s).unwrap()
            );
            let mixed: String = s
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    }
                })
                .collect();
            assert_eq!(parse_symbol(&mixed).unwrap(), parse_symbol(s).unwrap());
        }
    }

    // ---------------- the specific mappings named in the spec ----------------

    #[test]
    fn spec_examples_resolve_as_documented() {
        assert_eq!(
            parse_symbol("BTC-USD").unwrap(),
            Instrument::Spot(MarketId::BtcUsd)
        );
        assert_eq!(
            parse_symbol("BTC-PERP").unwrap(),
            Instrument::Perp(DerivId::perp(0))
        );
        assert_eq!(
            parse_symbol("AAPL-PERP").unwrap(),
            Instrument::Perp(DerivId::perp(3))
        );
        assert_eq!(
            parse_symbol("ETH-260626").unwrap(),
            Instrument::Future(DerivId::future(0))
        );
    }

    #[test]
    fn legacy_deriv_indices_are_pinned() {
        // Both pre-existing genesis files register these; moving them would
        // repoint stored positions on a live chain.
        assert_eq!(PERPS[0].underlying, AssetId::Btc);
        assert_eq!(PERPS[0].index, 0);
        assert_eq!(PERPS[1].underlying, AssetId::Eth);
        assert_eq!(PERPS[1].index, 1);
        assert_eq!(FUTURES[0].underlying, AssetId::Eth);
        assert_eq!(FUTURES[0].index, 0);
    }

    // ---------------- rejection, not silent mapping ----------------

    #[test]
    fn unknown_symbols_are_rejected() {
        for bad in [
            "DOGE-USD",    // unlisted asset, valid shape
            "DOGE-PERP",   // unlisted asset, perp shape
            "BTC-991231",  // valid-looking expiry that is not a listed contract
            "ETH-260627",  // one day off a real contract
            "AAPL-260626", // equities have no dated futures
            "SOL-USDT",    // wrong quote
            "BTC-SPOT",    // not the grammar
            "BTC-FUTURE",
            "USD-USD",
        ] {
            assert!(
                matches!(parse_symbol(bad), Err(SymbolError::Unknown(_))),
                "`{bad}` must be rejected as unknown, got {:?}",
                parse_symbol(bad)
            );
        }
    }

    #[test]
    fn malformed_symbols_are_rejected() {
        for bad in ["", "BTC", "BTCUSD", "-USD", "BTC-", "btc_usd"] {
            assert!(
                matches!(parse_symbol(bad), Err(SymbolError::Malformed(_))),
                "`{bad}` must be rejected as malformed, got {:?}",
                parse_symbol(bad)
            );
        }
    }

    #[test]
    fn whitespace_is_not_silently_trimmed() {
        assert!(parse_symbol(" BTC-USD").is_err());
        assert!(parse_symbol("BTC-USD ").is_err());
    }

    #[test]
    fn an_expiry_label_is_never_turned_into_an_index() {
        // The failure mode the table-lookup design exists to prevent: a grammar
        // parser would map any well-formed date onto *some* future index.
        for probe in ["ETH-260101", "ETH-991231", "ETH-000000"] {
            assert!(
                parse_symbol(probe).is_err(),
                "`{probe}` must not resolve — it is not a listed contract"
            );
        }
    }

    #[test]
    fn unlisted_deriv_ids_have_no_symbol() {
        assert_eq!(format_symbol(Instrument::Perp(DerivId::perp(99))), None);
        assert_eq!(format_symbol(Instrument::Future(DerivId::future(99))), None);
        // A kind mismatch must not resolve against the other table.
        assert_eq!(perp_spec(DerivId::future(0)), None);
        assert_eq!(future_spec(DerivId::perp(0)), None);
    }

    #[test]
    fn display_marks_unlisted_instruments_rather_than_guessing() {
        let s = format!("{}", Instrument::Perp(DerivId::perp(99)));
        assert!(s.contains("unlisted"), "got `{s}`");
        assert_eq!(
            format!("{}", Instrument::Perp(DerivId::perp(0))),
            "BTC-PERP"
        );
    }

    // ---------------- universe shape ----------------

    #[test]
    fn indices_are_dense_and_ordered() {
        // Genesis assigns `DerivId::{perp,future}(N)` by line order, so the
        // table's indices must be exactly 0..len with no gaps.
        for (i, p) in PERPS.iter().enumerate() {
            assert_eq!(p.index as usize, i, "PERPS[{i}] has index {}", p.index);
        }
        for (i, f) in FUTURES.iter().enumerate() {
            assert_eq!(f.index as usize, i, "FUTURES[{i}] has index {}", f.index);
        }
    }

    #[test]
    fn every_asset_that_can_be_traded_has_a_spot_market_and_a_perp() {
        for a in AssetId::ALL {
            if a == AssetId::Usd {
                continue; // the quote asset is not itself traded
            }
            assert!(
                MarketId::ALL.iter().any(|m| m.base() == a),
                "{a} has no spot market"
            );
            assert!(perp_for_underlying(a).is_some(), "{a} has no perpetual");
        }
    }

    #[test]
    fn dated_futures_are_crypto_only() {
        // `docs/PLAN.md` §3: equities list spot and perpetual only.
        for f in FUTURES {
            assert!(
                !f.underlying.is_equity(),
                "{} lists a dated future on an equity underlying",
                f.symbol
            );
        }
    }

    #[test]
    fn futures_have_distinct_expiries() {
        let mut labels: Vec<&str> = FUTURES.iter().map(|f| f.expiry_label).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(before, labels.len(), "two futures share an expiry label");
        assert!(
            FUTURES.len() >= 2,
            "the universe needs at least two dated futures"
        );
    }

    #[test]
    fn a_futures_symbol_matches_its_underlying_and_expiry_label() {
        for f in FUTURES {
            let (base, yymmdd) = f.symbol.split_once('-').expect("well-formed symbol");
            assert_eq!(base, f.underlying.symbol(), "{}", f.symbol);
            // "2026-06-26" -> "260626". Checked so a typo in either field is
            // caught, even though neither is derived from the other.
            let compact: String = f.expiry_label.replace('-', "")[2..].to_string();
            assert_eq!(yymmdd, compact, "{} label/symbol disagree", f.symbol);
        }
    }

    // ---------------- risk parameters ----------------

    #[test]
    fn risk_params_are_internally_consistent() {
        // IM must be the reciprocal of the leverage cap, and MM must be half of
        // IM. A market whose numbers disagree liquidates at a surprising price.
        for (symbol, risk) in PERPS
            .iter()
            .map(|p| (p.symbol, p.risk))
            .chain(FUTURES.iter().map(|f| (f.symbol, f.risk)))
        {
            assert_eq!(
                risk.initial_margin_bps as u32 * risk.max_leverage_x100,
                1_000_000,
                "{symbol}: IM {} bps does not match {}x leverage",
                risk.initial_margin_bps,
                risk.max_leverage_x100 / 100
            );
            assert_eq!(
                risk.maintenance_margin_bps * 2,
                risk.initial_margin_bps,
                "{symbol}: MM must be half of IM"
            );
            assert!(
                risk.maintenance_margin_bps < risk.initial_margin_bps,
                "{symbol}: MM must be strictly below IM or a new position opens already liquidatable"
            );
        }
    }

    #[test]
    fn leverage_caps_match_the_documented_table() {
        let expect = |sym: &str| {
            PERPS
                .iter()
                .find(|p| p.symbol == sym)
                .unwrap_or_else(|| panic!("{sym} missing"))
                .risk
                .max_leverage_x100
        };
        assert_eq!(expect("BTC-PERP"), 5_000, "BTC 50x");
        assert_eq!(expect("ETH-PERP"), 2_500, "ETH 25x");
        assert_eq!(expect("SOL-PERP"), 2_000, "SOL 20x");
        for sym in ["AAPL-PERP", "TSLA-PERP", "NVDA-PERP", "SPY-PERP"] {
            assert_eq!(expect(sym), 500, "{sym} equity 5x");
        }
    }

    #[test]
    fn every_equity_perp_is_capped_below_every_crypto_perp() {
        // The policy statement, asserted rather than only documented.
        let equity_max = PERPS
            .iter()
            .filter(|p| p.underlying.is_equity())
            .map(|p| p.risk.max_leverage_x100)
            .max()
            .expect("equity perps exist");
        let crypto_min = PERPS
            .iter()
            .filter(|p| !p.underlying.is_equity())
            .map(|p| p.risk.max_leverage_x100)
            .min()
            .expect("crypto perps exist");
        assert!(
            equity_max < crypto_min,
            "equity leverage cap {equity_max} must stay below the lowest crypto cap {crypto_min}"
        );
    }

    #[test]
    fn dated_futures_are_capped_below_the_perp_on_the_same_underlying() {
        for f in FUTURES {
            let perp = perp_for_underlying(f.underlying)
                .unwrap_or_else(|| panic!("{} has no matching perp", f.symbol));
            assert!(
                f.risk.max_leverage_x100 < perp.risk.max_leverage_x100,
                "{} at {}x should be capped below {} at {}x — basis risk",
                f.symbol,
                f.risk.max_leverage_x100 / 100,
                perp.symbol,
                perp.risk.max_leverage_x100 / 100
            );
        }
    }

    // ---------------- instrument helpers ----------------

    #[test]
    fn underlying_resolves_for_every_form() {
        assert_eq!(
            parse_symbol("SPY-USD").unwrap().underlying(),
            Some(AssetId::Spy)
        );
        assert_eq!(
            parse_symbol("SOL-PERP").unwrap().underlying(),
            Some(AssetId::Sol)
        );
        assert_eq!(
            parse_symbol("BTC-261225").unwrap().underlying(),
            Some(AssetId::Btc)
        );
        assert_eq!(Instrument::Perp(DerivId::perp(99)).underlying(), None);
    }

    #[test]
    fn is_equity_follows_the_underlying() {
        assert!(parse_symbol("NVDA-PERP").unwrap().is_equity());
        assert!(parse_symbol("TSLA-USD").unwrap().is_equity());
        assert!(!parse_symbol("BTC-PERP").unwrap().is_equity());
        assert!(!parse_symbol("ETH-260626").unwrap().is_equity());
    }

    // ---------------- uniqueness the byte-wise checks cannot see ----------------

    #[test]
    fn symbols_are_unique_even_ignoring_case() {
        // Parsing is case-insensitive, so two symbols that differed only in
        // case would be one symbol with two meanings. `symbols_are_unique_across_forms`
        // compares bytes and would not notice.
        let mut lowered: Vec<String> = all_symbols()
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let before = lowered.len();
        lowered.sort_unstable();
        lowered.dedup();
        assert_eq!(
            before,
            lowered.len(),
            "two instruments share a symbol up to case"
        );
    }

    #[test]
    fn each_underlying_has_at_most_one_perpetual() {
        // `perp_for_underlying` returns the first match, so a second listing
        // on the same underlying would be unreachable through it, and the
        // oracle would price one contract and not the other.
        let mut seen: Vec<AssetId> = PERPS.iter().map(|p| p.underlying).collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "two perpetuals list the same underlying"
        );
    }

    #[test]
    fn no_two_futures_share_an_underlying_and_an_expiry() {
        // Distinct labels overall is already pinned; this is the tighter rule
        // that the same contract cannot be listed twice under two indices.
        let mut keys: Vec<(AssetId, &str)> = FUTURES
            .iter()
            .map(|f| (f.underlying, f.expiry_label))
            .collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(
            before,
            keys.len(),
            "two futures on one underlying with the same expiry"
        );
    }
}
