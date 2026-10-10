//! Asset and spot-market identifiers, order flags, and the **single
//! authoritative asset table** ([`ASSET_SPECS`]).
//!
//! # Why these enums are closed, and why they may only grow at the end
//!
//! [`AssetId`] and [`MarketId`] are consensus-load-bearing in three separate
//! ways, and all three key off the *declaration order* of the variants:
//!
//! 1. **Transaction signatures.** `cb-tx::Transaction` is signed over its
//!    `bincode` encoding, and `bincode` encodes an enum as its zero-based
//!    variant index. Reordering `AssetId` would change the sighash of every
//!    historical transfer.
//! 2. **Account balances.** [`AssetId::as_u8`] is the map key in
//!    `cb_state::AccountState::balances` / `::locked`, which is `bincode`-encoded
//!    into the accounts trie. Renumbering an asset silently moves someone's
//!    balance to a different asset.
//! 3. **Order-book keys.** [`MarketId::as_u8`] is widened to 8 big-endian bytes
//!    as the `CF_BOOK_INDEX` key prefix (`cb_state::spot::keys`).
//!
//! The Cap'n Proto enums in `cb-schema/schema/types.capnp` mirror these
//! ordinals one-for-one. **Append only. Never reorder, never renumber, never
//! remove.** `variant_indices_match_as_u8` and the capnp ordinal test in
//! `cb-rpc::conv` pin the invariant so a careless insertion fails CI rather
//! than corrupting a chain.
//!
//! # The asset table
//!
//! [`ASSET_SPECS`] is the one place that knows an asset's decimals, class and
//! display symbol. Nothing else in the workspace should hard-code either. A
//! wrong decimal is a silent hundred-fold error in someone's balance, so the
//! table is indexed by `as_u8()` and a test asserts the index agrees with the
//! ordinal.
//!
//! # Base units vs. contract units — read this before writing any conversion
//!
//! There are **two different quantity scales** on this chain and they do not
//! agree for every asset:
//!
//! - **Spot quantities** are in the asset's own base units, i.e. scaled by
//!   `10^decimals()`. One AAPL share is `1_000_000` spot units.
//! - **Derivative quantities** are in *contract units*, scaled by a fixed
//!   `10^`[`CONTRACT_QTY_DECIMALS`] **for every underlying**. One AAPL share of
//!   AAPL-PERP exposure is `100_000_000` contract units.
//!
//! They coincide for BTC (8 decimals) and for nothing else. See
//! [`crate::scales`] for the named helpers that keep the two apart.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// Quantity scale used by **every** derivative contract, regardless of the
/// underlying's spot decimals: perp and future `qty` fields count units of
/// `10^-8` of the underlying.
///
/// This is a protocol constant, not a property of the asset. The perp notional
/// formula (`qty * mark_q8 / 1e10`, see `cb_engine::margin`) and the spot
/// notional formula (`price * qty / 1e8`, see `cb_block::executor`) both bake
/// it in, so it cannot move without a consensus change.
///
/// Re-exported from [`crate::scales`] for discoverability next to
/// [`AssetId::decimals`], which is the *other* scale and the one people reach
/// for by mistake.
pub const CONTRACT_QTY_DECIMALS: u8 = 8;

/// Broad classification of an asset. Drives policy, never arithmetic.
///
/// The distinction that matters here is [`AssetClass::Equity`]: an equity on
/// this chain is a **cash-settled synthetic**. It is priced off a mocked
/// reference feed and settles in USD. Nothing settles into a real share, no
/// dividend or corporate action is modelled, and no share is ever custodied.
/// See [`crate::calendar`] for the session-hours policy that follows from this.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetClass {
    /// A native crypto asset. Balances are (notionally) bridged 1:1 from the
    /// asset's own chain, so the decimals match that chain's base unit.
    Crypto,
    /// The quote currency. Exactly one asset is `Fiat` today (`USD`).
    Fiat,
    /// A cash-settled synthetic equity. Never a real share — see above.
    Equity,
}

/// Immutable per-asset facts. One row per [`AssetId`], indexed by
/// [`AssetId::as_u8`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetSpec {
    /// The asset this row describes. Must equal `ASSET_SPECS[i].id.as_u8() == i`.
    pub id: AssetId,
    /// Upper-case wire symbol, e.g. `"BTC"`, `"AAPL"`. Used to build market
    /// symbols in [`crate::symbols`].
    pub symbol: &'static str,
    /// Number of decimal places in the asset's base unit: an on-chain balance
    /// of `n` represents `n / 10^decimals` whole units.
    pub decimals: u8,
    /// Policy classification. Never used in arithmetic.
    pub class: AssetClass,
    /// Human-readable name for display surfaces.
    pub name: &'static str,
}

/// The authoritative asset table. **The only place decimals are defined.**
///
/// Ordering is the enum's ordinal order and is consensus-load-bearing; see the
/// module docs. Decimal choices, with reasoning:
///
/// | Asset | Decimals | Why |
/// |-------|----------|-----|
/// | BTC   | 8  | Satoshis. Frozen: the genesis chain already stores balances at this scale. |
/// | ETH   | 18 | Wei. Frozen for the same reason — see the caveat below. |
/// | USD   | 6  | Micro-USD. The quote unit; `MarginAccount::collateral` is in these units. |
/// | SOL   | 9  | Lamports — SOL's native base unit, so a future bridge maps 1:1 with no rescaling step to get wrong. |
/// | AAPL / TSLA / NVDA / SPY | 6 | Micro-shares. |
///
/// **Why 6 for equities.** Two reasons. First, fractional-share trading needs
/// sub-share granularity but not much of it: `1e-6` of an AAPL share is a
/// fraction of a cent, comfortably below any economically meaningful size, and
/// `1e-6` is where retail fractional-share venues actually stop. Second, and
/// more importantly, picking the *same* scale as USD makes the spot wire-price
/// scale come out at exactly `1e8` — see [`crate::scales::spot_price_scale_exp`]
/// — which is the same `q8` scale used by every oracle, mark and perp price on
/// the chain. Equities are the bulk of the newly listed markets, so choosing
/// the decimals that collapse two scales into one removes the largest available
/// class of conversion bugs.
///
/// **The ETH caveat.** ETH's 18 decimals predate this table and cannot be
/// changed without rewriting consensus state. Combined with the fixed `1e8`
/// spot lot scale it makes ETH's spot wire price sub-unit — see
/// [`crate::scales::spot_price_scale_exp`], which reports this as an error
/// rather than silently truncating. New assets are deliberately chosen with
/// `decimals <= 8` so they never land in that hole.
pub const ASSET_SPECS: [AssetSpec; 8] = [
    AssetSpec {
        id: AssetId::Btc,
        symbol: "BTC",
        decimals: 8,
        class: AssetClass::Crypto,
        name: "Bitcoin",
    },
    AssetSpec {
        id: AssetId::Eth,
        symbol: "ETH",
        decimals: 18,
        class: AssetClass::Crypto,
        name: "Ether",
    },
    AssetSpec {
        id: AssetId::Usd,
        symbol: "USD",
        decimals: 6,
        class: AssetClass::Fiat,
        name: "US Dollar",
    },
    AssetSpec {
        id: AssetId::Sol,
        symbol: "SOL",
        decimals: 9,
        class: AssetClass::Crypto,
        name: "Solana",
    },
    AssetSpec {
        id: AssetId::Aapl,
        symbol: "AAPL",
        decimals: 6,
        class: AssetClass::Equity,
        name: "Apple Inc. (synthetic)",
    },
    AssetSpec {
        id: AssetId::Tsla,
        symbol: "TSLA",
        decimals: 6,
        class: AssetClass::Equity,
        name: "Tesla Inc. (synthetic)",
    },
    AssetSpec {
        id: AssetId::Nvda,
        symbol: "NVDA",
        decimals: 6,
        class: AssetClass::Equity,
        name: "NVIDIA Corp. (synthetic)",
    },
    AssetSpec {
        id: AssetId::Spy,
        symbol: "SPY",
        decimals: 6,
        class: AssetClass::Equity,
        name: "S&P 500 ETF (synthetic)",
    },
];

/// Raised when a byte or string does not name a known asset.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AssetIdError {
    /// The byte is outside the range of defined ordinals.
    #[error("unknown asset ordinal {0}")]
    UnknownOrdinal(u8),
    /// The string does not match any asset's symbol.
    #[error("unknown asset name `{0}`")]
    UnknownName(String),
}

/// An asset that can be held as a balance and traded on a spot market.
///
/// **Append-only.** See the module docs before touching this enum.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetId {
    /// Bitcoin, 8 decimals. Ordinal 0.
    Btc,
    /// Ether, 18 decimals. Ordinal 1.
    Eth,
    /// US Dollar, 6 decimals. The quote asset of every listed market. Ordinal 2.
    Usd,
    /// Solana, 9 decimals. Ordinal 3.
    Sol,
    /// Synthetic Apple Inc. equity, 6 decimals. Ordinal 4.
    Aapl,
    /// Synthetic Tesla Inc. equity, 6 decimals. Ordinal 5.
    Tsla,
    /// Synthetic NVIDIA Corp. equity, 6 decimals. Ordinal 6.
    Nvda,
    /// Synthetic S&P 500 ETF equity, 6 decimals. Ordinal 7.
    Spy,
}

impl AssetId {
    /// Every asset, in ordinal order.
    pub const ALL: [AssetId; 8] = [
        AssetId::Btc,
        AssetId::Eth,
        AssetId::Usd,
        AssetId::Sol,
        AssetId::Aapl,
        AssetId::Tsla,
        AssetId::Nvda,
        AssetId::Spy,
    ];

    /// The authoritative [`AssetSpec`] row for this asset.
    pub fn spec(self) -> &'static AssetSpec {
        // Safe by construction: `variant_indices_match_as_u8` and
        // `asset_specs_are_indexed_by_ordinal` pin the invariant.
        &ASSET_SPECS[self.as_u8() as usize]
    }

    /// Decimal places in this asset's base unit. The **only** supported way to
    /// ask; never hard-code a scale at a call site.
    pub fn decimals(self) -> u8 {
        self.spec().decimals
    }

    /// `10^decimals()` — the number of base units in one whole unit.
    ///
    /// Fits in `u128` for every listed asset (`10^18` is the largest).
    pub fn one_whole(self) -> u128 {
        10u128.pow(self.decimals() as u32)
    }

    /// Policy classification. See [`AssetClass`].
    pub fn class(self) -> AssetClass {
        self.spec().class
    }

    /// `true` for a cash-settled synthetic equity. Nothing settles into a real
    /// share — see [`AssetClass::Equity`].
    pub fn is_equity(self) -> bool {
        self.class() == AssetClass::Equity
    }

    /// Upper-case wire symbol, e.g. `"BTC"`.
    pub fn symbol(self) -> &'static str {
        self.spec().symbol
    }

    /// Lower-case short name, e.g. `"btc"` — the spelling genesis files and CLI
    /// output use.
    ///
    /// Returns `&'static str` rather than an allocated lower-casing of
    /// [`AssetId::symbol`] so display paths stay allocation-free.
    /// `lower_symbol_matches_symbol` pins the two in sync.
    pub fn lower_symbol(self) -> &'static str {
        match self {
            AssetId::Btc => "btc",
            AssetId::Eth => "eth",
            AssetId::Usd => "usd",
            AssetId::Sol => "sol",
            AssetId::Aapl => "aapl",
            AssetId::Tsla => "tsla",
            AssetId::Nvda => "nvda",
            AssetId::Spy => "spy",
        }
    }

    /// Human-readable display name.
    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// Consensus ordinal. Matches the `bincode` variant index and the
    /// Cap'n Proto enumerant value.
    pub fn as_u8(self) -> u8 {
        match self {
            AssetId::Btc => 0,
            AssetId::Eth => 1,
            AssetId::Usd => 2,
            AssetId::Sol => 3,
            AssetId::Aapl => 4,
            AssetId::Tsla => 5,
            AssetId::Nvda => 6,
            AssetId::Spy => 7,
        }
    }

    /// Inverse of [`AssetId::as_u8`]. Rejects unknown ordinals rather than
    /// falling back to a default — a silent fallback would misattribute a
    /// balance read off disk.
    pub fn from_u8(b: u8) -> Result<AssetId, AssetIdError> {
        AssetId::ALL
            .get(b as usize)
            .copied()
            .ok_or(AssetIdError::UnknownOrdinal(b))
    }
}

impl fmt::Display for AssetId {
    /// Lower-case short name, matching the genesis-file spelling (`"btc"`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.lower_symbol())
    }
}

impl FromStr for AssetId {
    type Err = AssetIdError;

    /// Case-insensitive match against [`AssetSpec::symbol`]. Deliberately
    /// strict: no aliases, no fuzzy matching. Chain-specific aliases (e.g. a
    /// display denom for USD) belong in the layer that owns them.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ASSET_SPECS
            .iter()
            .find(|spec| spec.symbol.eq_ignore_ascii_case(s))
            .map(|spec| spec.id)
            .ok_or_else(|| AssetIdError::UnknownName(s.to_string()))
    }
}

/// Raised when a byte or string does not name a known spot market.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MarketIdError {
    /// The byte is outside the range of defined ordinals.
    #[error("unknown spot market ordinal {0}")]
    UnknownOrdinal(u8),
    /// The string does not match any listed spot market.
    #[error("unknown spot market `{0}`")]
    UnknownName(String),
}

/// A listed spot market. Every market is quoted in [`AssetId::Usd`].
///
/// **Append-only.** See the module docs before touching this enum.
///
/// The enum stays closed on purpose (`docs/PLAN.md` §7): a dynamic spot
/// registry would put market listing on the consensus path. Derivatives have no
/// such constraint — they are keyed by [`crate::DerivId`] and grow via genesis.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketId {
    /// BTC/USD. Ordinal 0.
    BtcUsd,
    /// ETH/USD. Ordinal 1.
    EthUsd,
    /// SOL/USD. Ordinal 2.
    SolUsd,
    /// Synthetic AAPL/USD. Ordinal 3.
    AaplUsd,
    /// Synthetic TSLA/USD. Ordinal 4.
    TslaUsd,
    /// Synthetic NVDA/USD. Ordinal 5.
    NvdaUsd,
    /// Synthetic SPY/USD. Ordinal 6.
    SpyUsd,
}

impl MarketId {
    /// Every spot market, in ordinal order.
    pub const ALL: [MarketId; 7] = [
        MarketId::BtcUsd,
        MarketId::EthUsd,
        MarketId::SolUsd,
        MarketId::AaplUsd,
        MarketId::TslaUsd,
        MarketId::NvdaUsd,
        MarketId::SpyUsd,
    ];

    /// The base (traded) asset.
    pub fn base(self) -> AssetId {
        match self {
            MarketId::BtcUsd => AssetId::Btc,
            MarketId::EthUsd => AssetId::Eth,
            MarketId::SolUsd => AssetId::Sol,
            MarketId::AaplUsd => AssetId::Aapl,
            MarketId::TslaUsd => AssetId::Tsla,
            MarketId::NvdaUsd => AssetId::Nvda,
            MarketId::SpyUsd => AssetId::Spy,
        }
    }

    /// The quote asset. Every listed market is USD-quoted; kept as a method so
    /// a future non-USD listing has one place to change.
    pub fn quote(self) -> AssetId {
        AssetId::Usd
    }

    /// Consensus ordinal. Matches the `bincode` variant index, the Cap'n Proto
    /// enumerant value, and the `CF_BOOK_INDEX` key prefix.
    pub fn as_u8(self) -> u8 {
        match self {
            MarketId::BtcUsd => 0,
            MarketId::EthUsd => 1,
            MarketId::SolUsd => 2,
            MarketId::AaplUsd => 3,
            MarketId::TslaUsd => 4,
            MarketId::NvdaUsd => 5,
            MarketId::SpyUsd => 6,
        }
    }

    /// Inverse of [`MarketId::as_u8`]. Rejects unknown ordinals.
    pub fn from_u8(b: u8) -> Result<MarketId, MarketIdError> {
        MarketId::ALL
            .get(b as usize)
            .copied()
            .ok_or(MarketIdError::UnknownOrdinal(b))
    }

    /// Snake-case identifier as written in genesis files, e.g. `"btc_usd"`.
    ///
    /// This is the *genesis / CLI* spelling. The *wire* symbol (`"BTC-USD"`)
    /// lives in [`crate::symbols`]; the two are deliberately different strings
    /// so neither surface drifts into the other's format.
    pub fn genesis_id(self) -> &'static str {
        match self {
            MarketId::BtcUsd => "btc_usd",
            MarketId::EthUsd => "eth_usd",
            MarketId::SolUsd => "sol_usd",
            MarketId::AaplUsd => "aapl_usd",
            MarketId::TslaUsd => "tsla_usd",
            MarketId::NvdaUsd => "nvda_usd",
            MarketId::SpyUsd => "spy_usd",
        }
    }

    /// `true` when the base asset is a cash-settled synthetic equity.
    pub fn is_equity(self) -> bool {
        self.base().is_equity()
    }
}

impl fmt::Display for MarketId {
    /// The genesis spelling, e.g. `"btc_usd"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.genesis_id())
    }
}

impl FromStr for MarketId {
    type Err = MarketIdError;

    /// Accepts the genesis spelling (`"btc_usd"`) and its unseparated form
    /// (`"btcusd"`), case-insensitively. Wire symbols (`"BTC-USD"`) are parsed
    /// by [`crate::symbols::parse_symbol`], not here.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let needle = s.to_ascii_lowercase();
        MarketId::ALL
            .iter()
            .copied()
            .find(|m| {
                let id = m.genesis_id();
                needle == id || needle == id.replace('_', "")
            })
            .ok_or_else(|| MarketIdError::UnknownName(s.to_string()))
    }
}

/// Order direction.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Bid: buy base, pay quote.
    Buy,
    /// Ask: sell base, receive quote.
    Sell,
}

/// Time-in-force.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tif {
    /// Good-till-cancelled: rests on the book.
    Gtc,
    /// Immediate-or-cancel: fills what it can, cancels the rest.
    Ioc,
    /// Fill-or-kill: fills entirely or is rejected.
    Fok,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- decimals (pinned, one assertion per asset) ----------------
    //
    // These are the assertions that stop a hundred-fold balance error. Each one
    // is deliberately a literal, not a lookup: a test that reads the same table
    // it is checking proves nothing.

    #[test]
    fn btc_has_eight_decimals() {
        assert_eq!(AssetId::Btc.decimals(), 8);
    }

    #[test]
    fn eth_has_eighteen_decimals() {
        assert_eq!(AssetId::Eth.decimals(), 18);
    }

    #[test]
    fn usd_has_six_decimals() {
        assert_eq!(AssetId::Usd.decimals(), 6);
    }

    #[test]
    fn sol_has_nine_decimals() {
        assert_eq!(AssetId::Sol.decimals(), 9);
    }

    #[test]
    fn aapl_has_six_decimals() {
        assert_eq!(AssetId::Aapl.decimals(), 6);
    }

    #[test]
    fn tsla_has_six_decimals() {
        assert_eq!(AssetId::Tsla.decimals(), 6);
    }

    #[test]
    fn nvda_has_six_decimals() {
        assert_eq!(AssetId::Nvda.decimals(), 6);
    }

    #[test]
    fn spy_has_six_decimals() {
        assert_eq!(AssetId::Spy.decimals(), 6);
    }

    #[test]
    fn every_equity_uses_the_same_decimals() {
        // A mixed-decimal equity class would be a trap for anyone writing a
        // generic equity code path.
        let equity_decimals: Vec<u8> = AssetId::ALL
            .iter()
            .filter(|a| a.is_equity())
            .map(|a| a.decimals())
            .collect();
        assert_eq!(equity_decimals, vec![6, 6, 6, 6]);
    }

    #[test]
    fn one_whole_matches_decimals() {
        assert_eq!(AssetId::Btc.one_whole(), 100_000_000);
        assert_eq!(AssetId::Usd.one_whole(), 1_000_000);
        assert_eq!(AssetId::Sol.one_whole(), 1_000_000_000);
        assert_eq!(AssetId::Aapl.one_whole(), 1_000_000);
        assert_eq!(AssetId::Eth.one_whole(), 1_000_000_000_000_000_000);
    }

    // ---------------- ordinal stability ----------------

    #[test]
    fn legacy_asset_ordinals_are_frozen() {
        // These three predate the widened universe and are written into every
        // historical signature and account row. They can never move.
        assert_eq!(AssetId::Btc.as_u8(), 0);
        assert_eq!(AssetId::Eth.as_u8(), 1);
        assert_eq!(AssetId::Usd.as_u8(), 2);
    }

    #[test]
    fn legacy_market_ordinals_are_frozen() {
        assert_eq!(MarketId::BtcUsd.as_u8(), 0);
        assert_eq!(MarketId::EthUsd.as_u8(), 1);
    }

    #[test]
    fn new_asset_ordinals_are_appended() {
        assert_eq!(AssetId::Sol.as_u8(), 3);
        assert_eq!(AssetId::Aapl.as_u8(), 4);
        assert_eq!(AssetId::Tsla.as_u8(), 5);
        assert_eq!(AssetId::Nvda.as_u8(), 6);
        assert_eq!(AssetId::Spy.as_u8(), 7);
    }

    #[test]
    fn new_market_ordinals_are_appended() {
        assert_eq!(MarketId::SolUsd.as_u8(), 2);
        assert_eq!(MarketId::AaplUsd.as_u8(), 3);
        assert_eq!(MarketId::TslaUsd.as_u8(), 4);
        assert_eq!(MarketId::NvdaUsd.as_u8(), 5);
        assert_eq!(MarketId::SpyUsd.as_u8(), 6);
    }

    #[test]
    fn all_arrays_are_in_ordinal_order_and_complete() {
        for (i, a) in AssetId::ALL.iter().enumerate() {
            assert_eq!(a.as_u8() as usize, i, "AssetId::ALL out of order at {i}");
        }
        for (i, m) in MarketId::ALL.iter().enumerate() {
            assert_eq!(m.as_u8() as usize, i, "MarketId::ALL out of order at {i}");
        }
    }

    #[test]
    fn variant_indices_match_as_u8() {
        // `bincode` writes an enum as a u32 variant index. Transaction
        // signatures commit to those bytes, and `AccountState` keys balances by
        // `as_u8()`. If the two ever disagree, a transfer signed for one asset
        // credits another. This test is the tripwire.
        for a in AssetId::ALL {
            let encoded = bincode::serialize(&a).expect("asset encodes");
            assert_eq!(
                encoded,
                (a.as_u8() as u32).to_le_bytes().to_vec(),
                "bincode variant index for {a:?} must equal as_u8()"
            );
        }
        for m in MarketId::ALL {
            let encoded = bincode::serialize(&m).expect("market encodes");
            assert_eq!(
                encoded,
                (m.as_u8() as u32).to_le_bytes().to_vec(),
                "bincode variant index for {m:?} must equal as_u8()"
            );
        }
    }

    #[test]
    fn asset_specs_are_indexed_by_ordinal() {
        for (i, spec) in ASSET_SPECS.iter().enumerate() {
            assert_eq!(
                spec.id.as_u8() as usize,
                i,
                "ASSET_SPECS row {i} describes {:?}, whose ordinal is {}",
                spec.id,
                spec.id.as_u8()
            );
        }
        assert_eq!(ASSET_SPECS.len(), AssetId::ALL.len());
    }

    // ---------------- round-trips ----------------

    #[test]
    fn asset_ordinal_round_trips() {
        for a in AssetId::ALL {
            assert_eq!(AssetId::from_u8(a.as_u8()).unwrap(), a);
        }
    }

    #[test]
    fn market_ordinal_round_trips() {
        for m in MarketId::ALL {
            assert_eq!(MarketId::from_u8(m.as_u8()).unwrap(), m);
        }
    }

    #[test]
    fn unknown_asset_ordinal_is_rejected_not_defaulted() {
        assert_eq!(
            AssetId::from_u8(8).unwrap_err(),
            AssetIdError::UnknownOrdinal(8)
        );
        assert_eq!(
            AssetId::from_u8(255).unwrap_err(),
            AssetIdError::UnknownOrdinal(255)
        );
    }

    #[test]
    fn unknown_market_ordinal_is_rejected_not_defaulted() {
        assert_eq!(
            MarketId::from_u8(7).unwrap_err(),
            MarketIdError::UnknownOrdinal(7)
        );
    }

    #[test]
    fn lower_symbol_matches_symbol() {
        for a in AssetId::ALL {
            assert_eq!(a.lower_symbol(), a.symbol().to_ascii_lowercase());
            assert_eq!(a.to_string(), a.lower_symbol());
        }
    }

    #[test]
    fn asset_name_round_trips_through_from_str() {
        for a in AssetId::ALL {
            assert_eq!(a.symbol().parse::<AssetId>().unwrap(), a);
            assert_eq!(a.to_string().parse::<AssetId>().unwrap(), a);
        }
    }

    #[test]
    fn unknown_asset_name_is_rejected() {
        assert_eq!(
            "doge".parse::<AssetId>().unwrap_err(),
            AssetIdError::UnknownName("doge".into())
        );
        assert!("".parse::<AssetId>().is_err());
    }

    #[test]
    fn market_genesis_id_round_trips() {
        for m in MarketId::ALL {
            assert_eq!(m.genesis_id().parse::<MarketId>().unwrap(), m);
            assert_eq!(
                m.genesis_id().replace('_', "").parse::<MarketId>().unwrap(),
                m
            );
            assert_eq!(
                m.genesis_id()
                    .to_ascii_uppercase()
                    .parse::<MarketId>()
                    .unwrap(),
                m
            );
        }
    }

    #[test]
    fn unknown_market_name_is_rejected() {
        assert!("doge_usd".parse::<MarketId>().is_err());
        // A wire symbol must NOT parse here — that surface is `symbols`.
        assert!("BTC-USD".parse::<MarketId>().is_err());
    }

    // ---------------- structure ----------------

    #[test]
    fn market_components_are_consistent() {
        assert_eq!(MarketId::BtcUsd.base(), AssetId::Btc);
        assert_eq!(MarketId::BtcUsd.quote(), AssetId::Usd);
        assert_eq!(MarketId::EthUsd.base(), AssetId::Eth);
        assert_eq!(MarketId::SolUsd.base(), AssetId::Sol);
        assert_eq!(MarketId::AaplUsd.base(), AssetId::Aapl);
        assert_eq!(MarketId::SpyUsd.base(), AssetId::Spy);
    }

    #[test]
    fn every_market_quotes_in_usd_and_has_a_distinct_base() {
        let mut bases: Vec<AssetId> = MarketId::ALL.iter().map(|m| m.base()).collect();
        let before = bases.len();
        bases.sort();
        bases.dedup();
        assert_eq!(bases.len(), before, "two spot markets share a base asset");
        for m in MarketId::ALL {
            assert_eq!(m.quote(), AssetId::Usd);
            assert_ne!(m.base(), m.quote(), "{m} would trade against itself");
        }
    }

    #[test]
    fn asset_classes_are_as_expected() {
        assert_eq!(AssetId::Btc.class(), AssetClass::Crypto);
        assert_eq!(AssetId::Sol.class(), AssetClass::Crypto);
        assert_eq!(AssetId::Usd.class(), AssetClass::Fiat);
        for a in [AssetId::Aapl, AssetId::Tsla, AssetId::Nvda, AssetId::Spy] {
            assert!(a.is_equity(), "{a} should be an equity");
        }
        assert!(!AssetId::Btc.is_equity());
        assert!(MarketId::AaplUsd.is_equity());
        assert!(!MarketId::BtcUsd.is_equity());
    }

    #[test]
    fn contract_qty_decimals_is_eight() {
        // Pinned: the derivative notional formulas divide by 1e10 = 1e8 (price)
        // * 1e2 (this scale vs. USD's 6). Changing it is a consensus change.
        assert_eq!(CONTRACT_QTY_DECIMALS, 8);
    }
}
