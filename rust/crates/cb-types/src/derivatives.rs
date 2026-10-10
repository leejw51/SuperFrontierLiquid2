//! Derivative-market identifiers, plus the on-chain types that describe
//! perpetuals and dated futures.
//!
//! `DerivId` is a 4-byte canonical identifier — `[kind: u8][index_le: u16][reserved: u8]` —
//! used as a CF key and embedded in every perp/futures tx. The legacy `MarketId` enum
//! remains for spot markets (the protocol design pins it that way); derivatives grow
//! freely via `DerivId` without needing schema bumps.
//!
//! All structured records (`Position`, `PerpMarket`, …) live here so that `cb-state`,
//! `cb-tx`, `cb-rpc`, and `cb-engine` can all import them without circular deps.

use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

use crate::address::Address;
use crate::hash::H256;
use crate::ids::{AssetId, Side, Tif};
use crate::signed::Signed;

/// Discriminator for a `DerivId`. Spot markets stay on `MarketId` and don't appear here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketKind {
    Perp,
    Future,
}

/// A 4-byte derivative-market identifier.
///
/// Layout: `[kind: u8][index_lo: u8][index_hi: u8][reserved: u8 = 0]`.
/// `kind == 1` means perp, `kind == 2` means dated future. The reserved trailing byte
/// is always zero today — we keep it so the encoding has a sliver of room for future
/// flag bits (e.g. an "options" discriminator) without breaking on-disk keys.
///
/// We deliberately do NOT embed `DerivId` into the `MarketId` enum. A closed enum is
/// great for spot's small fixed set, but derivatives grow over time (new contracts ship
/// via genesis or governance) and we don't want a schema bump every listing.
///
/// The byte layout is its own canonical encoding: equal `DerivId`s produce equal
/// keys, and ordering is total for use as a CF prefix.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct DerivId(pub [u8; 4]);

const KIND_PERP: u8 = 1;
const KIND_FUTURE: u8 = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DerivIdError {
    #[error("invalid DerivId kind byte {0}: expected 1 (perp) or 2 (future)")]
    InvalidKind(u8),
    #[error("DerivId reserved byte must be zero, got {0}")]
    ReservedNonzero(u8),
}

impl DerivId {
    /// Build a perp id with the given index.
    pub const fn perp(index: u16) -> Self {
        let b = index.to_le_bytes();
        DerivId([KIND_PERP, b[0], b[1], 0])
    }

    /// Build a dated-futures id with the given index.
    pub const fn future(index: u16) -> Self {
        let b = index.to_le_bytes();
        DerivId([KIND_FUTURE, b[0], b[1], 0])
    }

    /// Parse a `DerivId` from raw bytes, rejecting unknown kinds and a non-zero reserved.
    pub fn from_bytes(b: [u8; 4]) -> Result<Self, DerivIdError> {
        if b[3] != 0 {
            return Err(DerivIdError::ReservedNonzero(b[3]));
        }
        match b[0] {
            KIND_PERP | KIND_FUTURE => Ok(DerivId(b)),
            other => Err(DerivIdError::InvalidKind(other)),
        }
    }

    /// Discriminator. Construction is validated, so this never panics on a value
    /// that came through `perp` / `future` / `from_bytes`. We still match defensively
    /// for callers that constructed `DerivId([…])` literally.
    pub fn kind(self) -> Result<MarketKind, DerivIdError> {
        match self.0[0] {
            KIND_PERP => Ok(MarketKind::Perp),
            KIND_FUTURE => Ok(MarketKind::Future),
            other => Err(DerivIdError::InvalidKind(other)),
        }
    }

    /// Numeric index (the `N` in `perp(N)` / `future(N)`).
    pub fn index(self) -> u16 {
        u16::from_le_bytes([self.0[1], self.0[2]])
    }

    /// Raw bytes for use as a CF key.
    pub fn as_bytes(self) -> [u8; 4] {
        self.0
    }
}

impl fmt::Debug for DerivId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            Ok(MarketKind::Perp) => write!(f, "DerivId(perp:{})", self.index()),
            Ok(MarketKind::Future) => write!(f, "DerivId(future:{})", self.index()),
            Err(_) => write!(f, "DerivId(invalid:0x{})", hex::encode(self.0)),
        }
    }
}

impl fmt::Display for DerivId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind() {
            Ok(MarketKind::Perp) => write!(f, "perp:{}", self.index()),
            Ok(MarketKind::Future) => write!(f, "future:{}", self.index()),
            Err(_) => write!(f, "invalid_deriv:0x{}", hex::encode(self.0)),
        }
    }
}

/// Underlying spot asset for a derivative — what the contract is priced on. v1
/// only lists derivatives backed by existing spot assets.
pub type Underlying = AssetId;

/// A trader's open position on a single derivative market. One row per
/// `(owner, market)`. `qty == 0` is the closed sentinel; we keep the row briefly
/// so the funding-index snapshot survives until end-of-block GC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub owner: Address,
    pub market: DerivId,
    /// Direction. `qty == 0` overrides — the position is flat regardless of `side`.
    pub side: Side,
    /// Size in **contract units**: a fixed `10^`[`crate::CONTRACT_QTY_DECIMALS`]
    /// (= 1e8) per whole unit of the underlying, *for every underlying*.
    ///
    /// This is deliberately NOT the underlying's spot
    /// [`AssetId::decimals`](crate::AssetId::decimals). One AAPL share of
    /// exposure is `100_000_000` here, while one AAPL share of *spot* balance
    /// is `1_000_000`, because AAPL has 6 decimals. The two scales coincide for
    /// BTC and for nothing else, which is what makes mixing them up easy and
    /// expensive. Build this value with
    /// [`contract_qty_from_whole`](crate::scales::contract_qty_from_whole),
    /// never with `spot_qty_from_whole`.
    pub qty: u128,
    /// Cumulative-funding index snapshot at last position update.
    pub entry_funding_index: Signed,
    /// Volume-weighted entry price, quote-per-base, scaled by 1e8.
    pub entry_px_q8: u128,
    /// `0` = cross (uses MarginAccount.collateral), `1` = isolated.
    pub margin_mode: u8,
    /// 0 unless `margin_mode == 1`.
    pub isolated_margin: u128,
    /// Last block height the position participated in funding. Used so funding
    /// is applied lazily on every position-mutating tx.
    pub last_funding_height: u64,
}

/// Per-owner USD collateral pool that backs every cross-margin position. PnL
/// bookkeeping is stored sign-magnitude.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginAccount {
    pub owner: Address,
    /// Posted USD collateral, 1e6-base units (matches `AssetId::Usd`).
    pub collateral: u128,
    /// Running unrealized PnL across cross positions. Recomputed on mark
    /// updates touching the account.
    pub unrealized_pnl: Signed,
    /// Realized losses awaiting socialization (insurance fund debit).
    pub bad_debt: u128,
}

impl MarginAccount {
    pub fn new(owner: Address) -> Self {
        Self {
            owner,
            collateral: 0,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        }
    }
}

/// Per-perp cumulative funding index plus the latest 1h rate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingIndex {
    pub market: DerivId,
    /// Cumulative funding. Convention: positive => longs paid shorts.
    pub cumulative: Signed,
    /// Last block height funding ticked at.
    pub last_tick_height: u64,
    /// Latest 1-hour funding rate, fixed-point at 1e9 denom (i.e. `rate_q9 / 1e9`).
    pub rate_q9: i64,
}

impl FundingIndex {
    pub fn new(market: DerivId) -> Self {
        Self {
            market,
            cumulative: Signed::ZERO,
            last_tick_height: 0,
            rate_q9: 0,
        }
    }
}

/// Oracle price. Posted by the per-market authority; the chain stores it
/// verbatim for the engine to use as input to mark-price computation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OraclePrice {
    pub market: DerivId,
    /// Quote-per-base, 1e8 scale.
    pub px_q8: u128,
    /// Block height the authority signed at.
    pub source_height: u64,
    /// Authority that signed (must equal `oracle_authority` of the market).
    pub authority: Address,
}

/// Mark price, derived deterministically by the engine each block from oracle
/// + book mid + last trade. M1 only stores it; computation lives in M2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkPrice {
    pub market: DerivId,
    pub px_q8: u128,
    pub computed_at_height: u64,
    /// EMA over the (mid - oracle) premium. Fixed-point 1e9, signed.
    pub premium_ema_q9: i64,
}

/// Per-market parameters for a perpetual contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerpMarket {
    /// Always `kind == Perp`.
    pub id: DerivId,
    pub underlying: Underlying,
    /// Price tick in 1e8 units.
    pub tick_size: u128,
    /// Quantity tick in base units.
    pub lot_size: u128,
    /// e.g. 5000 == 50x.
    pub max_leverage_x100: u32,
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub taker_fee_bps: u16,
    /// Negative = rebate.
    pub maker_fee_bps_signed: i16,
    /// Funding cadence in blocks.
    pub funding_interval_blocks: u32,
    /// Cap on `|rate_q9|` per tick.
    pub funding_rate_cap_q9: i64,
    /// Authority allowed to sign `OracleUpdate` for this market.
    pub oracle_authority: Address,
    /// Per-account open-interest cap, in quote units (1e6).
    pub max_position_notional: u128,
    pub paused: bool,
    /// Last executed trade price on this market, 1e8 scale. `0` until the
    /// engine prints a fill; M3 writes it on every fill so M2's mark-price
    /// recompute can read it without scanning the book or replaying trades.
    /// Persisted on the market record (instead of a parallel CF) because it
    /// changes at the same cadence as the orderbook and the market record is
    /// the natural home for "current state of trading on this market".
    #[serde(default)]
    pub last_trade_px_q8: u128,
    /// Block height at which `last_trade_px_q8` was written. `0` until first
    /// fill. Used by M2's staleness logic on mid/last fallback for mark price.
    #[serde(default)]
    pub last_trade_height: u64,
    /// Reduce-only halt flag (M4). When `true`, `PlacePerpOrder` requires
    /// `reduce_only=true`; new opening / increasing positions are rejected.
    /// Triggered when insurance-fund coverage runs out during a liquidation
    /// (`docs/design/perps-trading.md` §5.4 / §7.4). Cleared by governance —
    /// no automatic recovery in v1.
    ///
    /// `#[serde(default)]` so genesis blobs and pre-M4 persisted markets
    /// decode cleanly to `false`.
    #[serde(default)]
    pub paused_reduce_only_only: bool,
}

/// Per-market parameters for a dated cash-settled future. Differs from `PerpMarket`
/// by carrying expiry + settlement-method + final settlement price.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FutureMarket {
    /// Always `kind == Future`.
    pub id: DerivId,
    pub underlying: Underlying,
    pub tick_size: u128,
    pub lot_size: u128,
    pub max_leverage_x100: u32,
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub taker_fee_bps: u16,
    pub maker_fee_bps_signed: i16,
    /// Block height after which the market is frozen except for `FuturesSettle`.
    pub expiry_height: u64,
    /// `0` = TWAP of last N mark snapshots, `1` = oracle spot at `expiry_height`.
    pub settlement_method: u8,
    /// N for the TWAP method.
    pub settlement_window_blocks: u32,
    pub oracle_authority: Address,
    pub paused: bool,
    /// Set by the chain at settlement.
    pub final_settlement_px_q8: Option<u128>,
    /// Last executed trade price on this market, 1e8 scale. Same shape as
    /// `PerpMarket.last_trade_px_q8` so the matcher's `BookSnapshot::last_trade_price`
    /// works uniformly across perps and futures (M5).
    ///
    /// `#[serde(default)]` so older persisted markets and capnp-encoded
    /// messages without this field decode cleanly to `0` (= "no trade yet").
    #[serde(default)]
    pub last_trade_px_q8: u128,
    /// Reduce-only halt, the futures twin of `PerpMarket::paused_reduce_only_only`.
    /// Set by the executor when a settlement or fill loss on this market
    /// exhausts the insurance fund; opening orders are refused until governance
    /// lifts it. Until core review L4 a future had no such flag, so a market
    /// whose losses had emptied the fund kept accepting new risk.
    #[serde(default)]
    pub paused_reduce_only_only: bool,
    /// Block height at which `last_trade_px_q8` was written. `0` until first fill.
    #[serde(default)]
    pub last_trade_height: u64,
}

/// One row of the liquidation queue. Trigger txs append; the engine drains the
/// queue at end-of-block in `(height, seq)` order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationEntry {
    pub owner: Address,
    pub market: DerivId,
    pub triggered_by: Address,
    pub triggered_at_height: u64,
    /// Snapshot of the mark price at trigger time. Defends against an
    /// adversarial trigger frontrunning a price improvement.
    pub mark_at_trigger_q8: u128,
    /// Monotonic, unique within a height.
    pub seq: u64,
}

/// Resting / in-flight perp order. Mirrors the spot order shape with two
/// additions: `reduce_only` and a `DerivId` market. Lives here (not in
/// `cb-state`) so the matching engine can operate over its fields without
/// pulling RocksDB into `cb-engine`. The CF wrapper in `cb-state` stays a
/// thin codec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerpOrder {
    pub order_id: H256,
    pub owner: Address,
    pub market: DerivId,
    pub side: Side,
    pub price_q8: u128,
    pub qty: u128,
    pub remaining_qty: u128,
    pub tif: Tif,
    pub post_only: bool,
    pub reduce_only: bool,
    /// Sequence number assigned by the book at insert time; ties price-time
    /// priority for orders at the same price.
    pub seq: u64,
    /// Block height the order was placed at — used for the maker-rebate
    /// "rested for >= 1 block" wash-trade guard.
    pub placed_at_height: u64,
    pub client_order_id: H256,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    >(
        v: &T,
    ) {
        let bytes = bincode::serialize(v).unwrap();
        let back: T = bincode::deserialize(&bytes).unwrap();
        assert_eq!(&back, v);
    }

    // ---------------- DerivId ----------------

    #[test]
    fn deriv_id_perp_round_trip() {
        let d = DerivId::perp(7);
        assert_eq!(d.kind().unwrap(), MarketKind::Perp);
        assert_eq!(d.index(), 7);
        assert_eq!(d.as_bytes(), [1, 7, 0, 0]);
        let back = DerivId::from_bytes(d.as_bytes()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn deriv_id_future_round_trip() {
        let d = DerivId::future(0xBEEF);
        assert_eq!(d.kind().unwrap(), MarketKind::Future);
        assert_eq!(d.index(), 0xBEEF);
        assert_eq!(d.as_bytes(), [2, 0xEF, 0xBE, 0]);
    }

    #[test]
    fn deriv_id_rejects_invalid_kind() {
        assert_eq!(
            DerivId::from_bytes([9, 0, 0, 0]).unwrap_err(),
            DerivIdError::InvalidKind(9)
        );
    }

    #[test]
    fn deriv_id_rejects_nonzero_reserved() {
        assert_eq!(
            DerivId::from_bytes([1, 0, 0, 1]).unwrap_err(),
            DerivIdError::ReservedNonzero(1)
        );
    }

    #[test]
    fn deriv_id_total_order_matches_byte_order() {
        // CF keys rely on lex byte ordering.
        let p0 = DerivId::perp(0);
        let p1 = DerivId::perp(1);
        let f0 = DerivId::future(0);
        assert!(p0 < p1);
        assert!(p1 < f0); // perp kind = 1 < future kind = 2
    }

    #[test]
    fn the_largest_index_round_trips_and_the_reserved_byte_stays_clear() {
        // Genesis caps each table at u16::MAX records; the top of that range
        // must encode without spilling into the reserved byte.
        for d in [DerivId::perp(u16::MAX), DerivId::future(u16::MAX)] {
            assert_eq!(d.index(), u16::MAX);
            assert_eq!(d.as_bytes()[3], 0);
            assert_eq!(DerivId::from_bytes(d.as_bytes()).unwrap(), d);
        }
    }

    #[test]
    fn byte_order_is_kind_then_little_endian_index_not_numeric_order() {
        // The CF prefix orders by kind and then by the index's *low* byte, so
        // perp 256 sorts before perp 1. Nothing may rely on a numeric scan
        // order; pinned so a range query written on that assumption fails here.
        assert!(DerivId::perp(256) < DerivId::perp(1));
        assert!(DerivId::perp(u16::MAX) < DerivId::future(0));
    }

    #[test]
    fn deriv_id_serde_round_trip() {
        round_trip(&DerivId::perp(42));
        round_trip(&DerivId::future(1));
    }

    #[test]
    fn deriv_id_display_kinds() {
        assert_eq!(format!("{}", DerivId::perp(3)), "perp:3");
        assert_eq!(format!("{}", DerivId::future(9)), "future:9");
    }

    // ---------------- Position ----------------

    fn sample_position() -> Position {
        Position {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            side: Side::Buy,
            qty: 100_000_000, // 1.00 BTC
            entry_funding_index: Signed::from_i128(-12345),
            entry_px_q8: 50_000 * 100_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 42,
        }
    }

    #[test]
    fn position_round_trip() {
        round_trip(&sample_position());
    }

    // ---------------- MarginAccount ----------------

    #[test]
    fn margin_account_round_trip() {
        let m = MarginAccount {
            owner: Address([2; 20]),
            collateral: 1_000_000_000,
            unrealized_pnl: Signed::from_i128(-5_000_000),
            bad_debt: 0,
        };
        round_trip(&m);
    }

    // ---------------- FundingIndex ----------------

    #[test]
    fn funding_index_round_trip() {
        let f = FundingIndex {
            market: DerivId::perp(2),
            cumulative: Signed::from_i128(98_765),
            last_tick_height: 7_200,
            rate_q9: 250_000,
        };
        round_trip(&f);
    }

    // ---------------- OraclePrice / MarkPrice ----------------

    #[test]
    fn oracle_and_mark_round_trip() {
        let o = OraclePrice {
            market: DerivId::perp(0),
            px_q8: 50_000 * 100_000_000,
            source_height: 1234,
            authority: Address([7; 20]),
        };
        round_trip(&o);
        let m = MarkPrice {
            market: DerivId::perp(0),
            px_q8: 50_000 * 100_000_000,
            computed_at_height: 1234,
            premium_ema_q9: 12_345,
        };
        round_trip(&m);
    }

    // ---------------- PerpMarket / FutureMarket ----------------

    #[test]
    fn perp_market_round_trip() {
        let p = PerpMarket {
            id: DerivId::perp(0),
            underlying: AssetId::Btc,
            tick_size: 100,
            lot_size: 1000,
            max_leverage_x100: 5000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: 3600,
            funding_rate_cap_q9: 1_000_000,
            oracle_authority: Address([3; 20]),
            max_position_notional: 100_000_000_000_000,
            paused: false,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        };
        round_trip(&p);
    }

    #[test]
    fn future_market_round_trip() {
        let f = FutureMarket {
            id: DerivId::future(0),
            underlying: AssetId::Eth,
            tick_size: 100,
            lot_size: 1000,
            max_leverage_x100: 2500,
            initial_margin_bps: 400,
            maintenance_margin_bps: 200,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            expiry_height: 2_592_000,
            settlement_method: 0,
            settlement_window_blocks: 600,
            oracle_authority: Address([4; 20]),
            paused: false,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        };
        round_trip(&f);
        let mut g = f;
        g.final_settlement_px_q8 = Some(3_200 * 100_000_000);
        round_trip(&g);
    }

    // ---------------- LiquidationEntry ----------------

    #[test]
    fn liquidation_entry_round_trip() {
        let e = LiquidationEntry {
            owner: Address([5; 20]),
            market: DerivId::perp(1),
            triggered_by: Address([6; 20]),
            triggered_at_height: 999,
            mark_at_trigger_q8: 45_000 * 100_000_000,
            seq: 0,
        };
        round_trip(&e);
    }
}
