//! SuperFrontierLiquid execution engine — the **pure** layer.
//!
//! No file in this crate touches I/O. Every entry point is a function of its
//! inputs; the same inputs always produce the same outputs. This is the
//! contract that lets `cb-block`'s impure executor produce bit-for-bit
//! reproducible state transitions across the sequencer and every replica.
//!
//! ## Layout (M2 + M3)
//!
//! - `oracle` — oracle freshness + authority policy (M2).
//! - `mark` — mark-price computation (median-of-three, clamped) (M2).
//! - `funding` — funding-rate clamp + cumulative index accrual (M2).
//! - `book` — perpetual CLOB matcher (M3). Consumes a `BookSnapshot`
//!   (read-only view of `CF_PERP_ORDERS` + `CF_PERP_BOOK_INDEX`) and an
//!   `IncomingOrder`, produces a `MatchResult` describing fills and CF
//!   mutations to apply.
//! - `perps_position` — pure position-delta math for fills (M3). Same
//!   convention used by both the matching path (M3) and liquidation /
//!   settlement (M4 / M5).
//!
//! ## Fixed-point conventions
//!
//! Two distinct fixed-point scales appear in this crate:
//!
//! * **Price (`q8`)** — quote-per-base prices are stored as `u128` with an
//!   implicit `1e8` denominator. Matches `OraclePrice.px_q8`,
//!   `MarkPrice.px_q8`, `Position.entry_px_q8`. Picked at M1 because BTC
//!   already uses 8 decimals on the spot side, and 1e8 keeps a 50_000 USD/BTC
//!   price within `u128` head-room out to ~10^28 — plenty for notional math.
//!
//! * **Rate (`q9`)** — funding rates and funding-index deltas are signed
//!   `i64` with an implicit `1e9` denominator. So `rate_q9 = 5_000_000`
//!   represents 0.5% (= 5_000_000 / 1e9). Matches `FundingIndex.rate_q9`
//!   and `MarkPrice.premium_ema_q9`. The spec calls for ±0.005 (50 bps/h)
//!   and ±0.0005 (5 bps/h) clamps — both fit comfortably in `i64` at q9
//!   scale, with `2^63 ≈ 9.2e18` of head-room.
//!
//! Both scales are consensus-load-bearing. Bumping them is a hard fork.
//!
//! ## Modules
//!
//! - `margin`, `liquidation` — M4.
//! - `dated` — M5 dated-futures pure helpers (settlement price, K-shrink,
//!   per-position settlement PnL).

pub mod book;
pub mod dated;
pub mod funding;
pub mod liquidation;
pub mod margin;
pub mod mark;
pub mod oracle;
pub mod perps_position;
pub mod spot_book;

pub use book::{
    best_bid_ask, derivative_price_band, last_trade_price, match_order, within_price_band,
    BookError, BookMutation, BookSnapshot, Fill, InMemoryBook, IncomingOrder, MatchOutcome,
    MatchResult, SelfTradeAction, DERIVATIVE_PRICE_BAND_BPS,
};
pub use dated::{
    compute_settlement_pnl, compute_settlement_price, dated_mark_clamp_k,
    MarkClampK as DatedMarkClampK, FINAL_HOUR_BLOCKS, MARK_HISTORY_BLOCKS,
    SETTLEMENT_METHOD_ORACLE, SETTLEMENT_METHOD_TWAP,
};
pub use funding::{
    accrue_index_delta, compute_funding_rate, FundingClamp, FUNDING_PRICE_UNIT_Q8, RATE_DEN_Q9,
};
pub use liquidation::{
    compute_liquidation_orders, is_liquidatable, liquidation_fee_split, LiquidationOrder,
    LIQUIDATION_FEE_INSURANCE_BPS, LIQUIDATION_FEE_KEEPER_BPS, LIQUIDATION_FEE_TOTAL_BPS,
};
pub use margin::{
    compute_equity, initial_margin_required, maintenance_margin_required, passes_initial_margin,
    passes_initial_margin_with_exposure, passes_initial_margin_worst_case,
    position_notional_micro_usd, position_unrealized_pnl_micro_usd,
    total_position_notional_micro_usd, LeverageTier, DEFAULT_TIERS,
};
pub use mark::{compute_mark_price, MarkClampK};
pub use perps_position::apply_fill_to_position;
