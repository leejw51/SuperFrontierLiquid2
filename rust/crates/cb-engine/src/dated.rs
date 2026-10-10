//! Dated-futures pure helpers (M5).
//!
//! Three functions live here, each a pure function of its inputs:
//!
//! - [`dated_mark_clamp_k`] — the per-market clamp factor `K` for the mark
//!   formula. Returns 1% in the final hour before expiry, 5% otherwise. This
//!   matches `docs/design/perps-trading.md` §6.5 ("in the final hour, the mark
//!   clamp K shrinks linearly from 5% to 1%"), simplified to a step function:
//!   the spec calls for a linear shrink, but a step at the 1-hour boundary is
//!   simpler to reason about and falls strictly inside the trader's
//!   protection band (we move *to the tighter* clamp earlier than a linear
//!   schedule would). When traders want the exact linear behavior, the
//!   helper's signature is stable and we can replace the body in-place.
//!
//! - [`compute_settlement_price`] — final settlement price. Method 0 is TWAP
//!   over the last `MARK_HISTORY_BLOCKS` (= 30 min @ 1s) before `expiry_height`.
//!   Method 1 is the oracle price at `expiry_height`. Both are pure: they
//!   take pre-collected inputs and return a `u128`.
//!
//! - [`compute_settlement_pnl`] — final realised PnL for a single position
//!   when the contract settles at `settle_px`. Same sign convention as
//!   `apply_fill_to_position`: positive `Signed` means the owner gained.
//!
//! ## Mark-history buffer length
//!
//! `MARK_HISTORY_BLOCKS = 1800` (30 minutes at 1s blocktime). That's the
//! exact TWAP window §6.4 requires; longer would be wasted storage and
//! shorter would silently truncate the spec's averaging window. The buffer
//! is per-market and lives in `oracle_state` (kind = 2) — see
//! `cb-state::oracle_state`. Pruning happens at write-time: when sample N
//! lands, sample N - MARK_HISTORY_BLOCKS gets deleted if present.

use cb_types::{FutureMarket, Position, Side, Signed};

/// Number of mark samples kept per dated future for TWAP settlement. Sized
/// to exactly the TWAP window from `docs/design/perps-trading.md` §6.4
/// (30 min). At a 1s blocktime that's 1800 blocks. If the chain blocktime
/// changes this value MUST move with it (a hard fork, since settlement
/// price would otherwise differ across replicas with mismatched constants).
pub const MARK_HISTORY_BLOCKS: u64 = 1800;

/// Final-hour window used by [`dated_mark_clamp_k`]. 1 hour at 1s blocks =
/// 3600 blocks. Same hard-fork caveat as `MARK_HISTORY_BLOCKS`.
pub const FINAL_HOUR_BLOCKS: u64 = 3600;

/// Settlement-method discriminators (mirror `FutureMarket.settlement_method`).
pub const SETTLEMENT_METHOD_TWAP: u8 = 0;
pub const SETTLEMENT_METHOD_ORACLE: u8 = 1;

/// Dated-future clamp factor. Mirrors `MarkClampK` in `mark.rs` shape so the
/// caller can substitute it into [`compute_mark_price`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkClampK {
    pub bps: u16,
}

impl MarkClampK {
    /// 1% — used in the final hour before expiry.
    pub const TIGHT: Self = Self { bps: 100 };
    /// 5% — used during the bulk of the contract's life.
    pub const WIDE: Self = Self { bps: 500 };
}

/// Return the mark clamp `K` for a dated future at the given block height.
///
/// - Returns `TIGHT` (1%) when `current_height` is within `FINAL_HOUR_BLOCKS`
///   of `market.expiry_height` (inclusive of the boundary).
/// - Returns `WIDE` (5%) at any earlier height.
/// - Already-expired (post-settlement) markets keep `TIGHT` — defensive: any
///   late mark recompute on a settled market should err on the side of
///   tighter clamping.
///
/// The function takes a `FutureMarket` directly (not just an expiry height)
/// so future K-shrink schedules can read other knobs (e.g. per-market
/// shrink window) without breaking the call site.
pub fn dated_mark_clamp_k(market: &FutureMarket, current_height: u64) -> MarkClampK {
    if current_height >= market.expiry_height {
        return MarkClampK::TIGHT;
    }
    let blocks_to_expiry = market.expiry_height - current_height;
    if blocks_to_expiry <= FINAL_HOUR_BLOCKS {
        MarkClampK::TIGHT
    } else {
        MarkClampK::WIDE
    }
}

/// Compute the settlement price for a dated future.
///
/// `oracle_price` is `Some` only when the latest oracle price is available
/// at `expiry_height` (engine reads `OraclePrice.px_q8` from `oracle_state`).
/// `mark_history` is a slice of `(height, mark_q8)` samples sorted ascending
/// by `height`; the engine prunes anything older than
/// `expiry_height - MARK_HISTORY_BLOCKS` before calling.
///
/// Settlement methods:
/// - `0` (TWAP): unweighted arithmetic mean of `mark_q8` over samples in
///   `[expiry_height - MARK_HISTORY_BLOCKS, expiry_height)`. The exclusive
///   upper bound matches §8.3 ("the settling block's mark doesn't feed
///   into its own settlement price"). Empty history falls back to oracle
///   if available; if both are absent we return `0` and the caller is
///   responsible for not settling.
/// - `1` (oracle): the supplied `oracle_price`. Falls back to TWAP if oracle
///   is absent — the engine is permitted to use a slightly stale oracle but
///   we should not return zero if a TWAP is available.
///
/// Pure integer math: TWAP is `sum(mark_q8) / count`, rounded toward zero.
/// `count` is a `u128` for the divide; sum saturates so abnormal histories
/// (history of 1800 entries with each mark near `u128::MAX`) don't panic.
pub fn compute_settlement_price(
    market: &FutureMarket,
    oracle_price: Option<u128>,
    mark_history: &[(u64, u128)],
) -> u128 {
    match market.settlement_method {
        SETTLEMENT_METHOD_ORACLE => match oracle_price {
            Some(px) if px > 0 => px,
            _ => twap_or_zero(market, mark_history),
        },
        // Default branch (covers SETTLEMENT_METHOD_TWAP and any future value;
        // genesis already validates the discriminant so we won't see anything
        // else in production).
        _ => match twap(market, mark_history) {
            Some(px) => px,
            None => oracle_price.unwrap_or(0),
        },
    }
}

/// TWAP helper: returns the unweighted mean of `mark_history` samples whose
/// height is in `[expiry - MARK_HISTORY_BLOCKS, expiry)`. Returns `None` if
/// no samples qualify.
fn twap(market: &FutureMarket, mark_history: &[(u64, u128)]) -> Option<u128> {
    let lo = market.expiry_height.saturating_sub(MARK_HISTORY_BLOCKS);
    let hi = market.expiry_height; // exclusive
    let mut sum: u128 = 0;
    let mut count: u128 = 0;
    for (h, px) in mark_history {
        if *h >= lo && *h < hi {
            sum = sum.saturating_add(*px);
            count = count.saturating_add(1);
        }
    }
    // `checked_div` rather than a guarded `/`: an empty window is a real case
    // (a market listed inside its own settlement window has no samples yet) and
    // the `None` it produces is the caller's signal to fall back to the oracle.
    sum.checked_div(count)
}

fn twap_or_zero(market: &FutureMarket, mark_history: &[(u64, u128)]) -> u128 {
    twap(market, mark_history).unwrap_or(0)
}

/// Final realised PnL for `position` if it settles at `settle_px_q8`.
///
/// Same sign convention as `apply_fill_to_position`: positive = owner
/// gained. Magnitude is in **raw `qty * delta_q8` units** — i.e. base units
/// multiplied by 1e8-scaled price difference. The caller (executor) divides
/// by 1e10 to land in micro-USD, exactly matching `credit_realized_pnl`.
///
/// `qty == 0` → `Signed::ZERO`. Closing a long at a price above entry is a
/// gain; closing a short at a price above entry is a loss. Matches the
/// formulas in `realised_pnl_q8` (which we deliberately mirror here so an
/// already-tested helper isn't invented twice).
pub fn compute_settlement_pnl(position: &Position, settle_px_q8: u128) -> Signed {
    if position.qty == 0 {
        return Signed::ZERO;
    }
    let entry = position.entry_px_q8;
    let (gain, abs_diff) = match position.side {
        Side::Buy => {
            if settle_px_q8 >= entry {
                (true, settle_px_q8 - entry)
            } else {
                (false, entry - settle_px_q8)
            }
        }
        Side::Sell => {
            if entry >= settle_px_q8 {
                (true, entry - settle_px_q8)
            } else {
                (false, settle_px_q8 - entry)
            }
        }
    };
    let abs = position.qty.saturating_mul(abs_diff);
    if abs == 0 {
        return Signed::ZERO;
    }
    Signed {
        sign: if gain { 1 } else { -1 },
        abs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::{Address, AssetId, DerivId};

    fn sample_market(expiry: u64) -> FutureMarket {
        FutureMarket {
            id: DerivId::future(0),
            underlying: AssetId::Eth,
            tick_size: 100,
            lot_size: 1000,
            max_leverage_x100: 2500,
            initial_margin_bps: 400,
            maintenance_margin_bps: 200,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            expiry_height: expiry,
            settlement_method: SETTLEMENT_METHOD_TWAP,
            settlement_window_blocks: MARK_HISTORY_BLOCKS as u32,
            oracle_authority: Address([4; 20]),
            paused: false,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        }
    }

    fn sample_position(side: Side, qty: u128, entry_q8: u128) -> Position {
        Position {
            owner: Address([1; 20]),
            market: DerivId::future(0),
            side,
            qty,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: entry_q8,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        }
    }

    // ---------------- dated_mark_clamp_k ----------------

    #[test]
    fn clamp_k_is_wide_far_from_expiry() {
        let m = sample_market(10_000);
        // 5000 blocks before expiry → > 3600 → WIDE.
        assert_eq!(dated_mark_clamp_k(&m, 5_000), MarkClampK::WIDE);
        assert_eq!(dated_mark_clamp_k(&m, 0), MarkClampK::WIDE);
    }

    #[test]
    fn clamp_k_tightens_in_final_hour() {
        let m = sample_market(10_000);
        // Exactly 3600 blocks before expiry → boundary → TIGHT (≤ FINAL_HOUR_BLOCKS).
        assert_eq!(dated_mark_clamp_k(&m, 6_400), MarkClampK::TIGHT);
        // 1 block before expiry → TIGHT.
        assert_eq!(dated_mark_clamp_k(&m, 9_999), MarkClampK::TIGHT);
        // Exactly at expiry → TIGHT.
        assert_eq!(dated_mark_clamp_k(&m, 10_000), MarkClampK::TIGHT);
    }

    #[test]
    fn clamp_k_post_expiry_stays_tight() {
        let m = sample_market(10_000);
        assert_eq!(dated_mark_clamp_k(&m, 10_001), MarkClampK::TIGHT);
        assert_eq!(dated_mark_clamp_k(&m, 99_999), MarkClampK::TIGHT);
    }

    #[test]
    fn clamp_k_step_boundary_is_at_final_hour_blocks() {
        let m = sample_market(10_000);
        // 3601 blocks before expiry: just outside the final hour → WIDE.
        assert_eq!(dated_mark_clamp_k(&m, 6_399), MarkClampK::WIDE);
        // 3600 blocks before expiry: exactly at the boundary → TIGHT.
        assert_eq!(dated_mark_clamp_k(&m, 6_400), MarkClampK::TIGHT);
    }

    // ---------------- compute_settlement_price ----------------

    #[test]
    fn twap_settlement_simple_average() {
        let m = sample_market(1000);
        // Three samples in window. Expected TWAP = (100 + 200 + 300) / 3 = 200.
        let history = vec![(998, 100u128), (999, 200), (997, 300)];
        let px = compute_settlement_price(&m, None, &history);
        assert_eq!(px, 200);
    }

    #[test]
    fn twap_excludes_settling_block_and_pre_window() {
        // Window is [expiry - 1800, expiry) — exclusive upper, inclusive lower.
        let m = sample_market(2000);
        let history = vec![
            (199, 999u128),    // pre-window — excluded.
            (200, 100),        // first eligible (= expiry - 1800).
            (1500, 200),       // mid window.
            (1999, 300),       // last eligible (= expiry - 1).
            (2000, 9_999_999), // settling block — excluded.
            (2001, 9_999_998), // post-settle — excluded (defensive).
        ];
        // Eligible: 100 + 200 + 300 = 600; mean = 200.
        let px = compute_settlement_price(&m, None, &history);
        assert_eq!(px, 200);
    }

    #[test]
    fn twap_falls_back_to_oracle_when_history_empty() {
        let m = sample_market(1000);
        let px = compute_settlement_price(&m, Some(42_000_000), &[]);
        assert_eq!(px, 42_000_000);
    }

    #[test]
    fn twap_returns_zero_when_no_inputs_available() {
        let m = sample_market(1000);
        // Both empty: no oracle, no history. Caller must check.
        let px = compute_settlement_price(&m, None, &[]);
        assert_eq!(px, 0);
    }

    #[test]
    fn oracle_method_uses_supplied_oracle_price() {
        let mut m = sample_market(1000);
        m.settlement_method = SETTLEMENT_METHOD_ORACLE;
        let history = vec![(999, 1u128)]; // would TWAP to 1, but we use oracle.
        let px = compute_settlement_price(&m, Some(99), &history);
        assert_eq!(px, 99);
    }

    #[test]
    fn oracle_method_falls_back_to_twap() {
        let mut m = sample_market(1000);
        m.settlement_method = SETTLEMENT_METHOD_ORACLE;
        // No oracle posted → TWAP fallback.
        let history = vec![(998, 50u128), (999, 150)];
        let px = compute_settlement_price(&m, None, &history);
        assert_eq!(px, 100);
    }

    // ---------------- compute_settlement_pnl ----------------

    #[test]
    fn settle_pnl_long_winner() {
        // Long 50 @ 3,150 settles at 3,200 → +50 * 50 = +2500.
        let p = sample_position(Side::Buy, 50, 3_150);
        let pnl = compute_settlement_pnl(&p, 3_200);
        assert_eq!(pnl, Signed::from_i128(2_500));
    }

    #[test]
    fn settle_pnl_short_winner() {
        // Short 50 @ 3,150 settles at 3,100 → +50 * 50 = +2500.
        let p = sample_position(Side::Sell, 50, 3_150);
        let pnl = compute_settlement_pnl(&p, 3_100);
        assert_eq!(pnl, Signed::from_i128(2_500));
    }

    #[test]
    fn settle_pnl_long_loser() {
        let p = sample_position(Side::Buy, 50, 3_150);
        let pnl = compute_settlement_pnl(&p, 3_100);
        assert_eq!(pnl, Signed::from_i128(-2_500));
    }

    #[test]
    fn settle_pnl_short_loser() {
        let p = sample_position(Side::Sell, 50, 3_150);
        let pnl = compute_settlement_pnl(&p, 3_200);
        assert_eq!(pnl, Signed::from_i128(-2_500));
    }

    #[test]
    fn settle_pnl_zero_position_is_zero() {
        let p = sample_position(Side::Buy, 0, 1_000);
        assert_eq!(compute_settlement_pnl(&p, 9_999), Signed::ZERO);
    }

    #[test]
    fn settle_pnl_at_entry_is_zero() {
        let p = sample_position(Side::Buy, 50, 3_150);
        assert_eq!(compute_settlement_pnl(&p, 3_150), Signed::ZERO);
        let p = sample_position(Side::Sell, 50, 3_150);
        assert_eq!(compute_settlement_pnl(&p, 3_150), Signed::ZERO);
    }

    /// Property: the sum of settlement PnL across long+short positions of
    /// equal qty at the same entry price is exactly zero (zero-sum). This
    /// is the conservation law that makes settlement collateral-neutral.
    #[test]
    fn settle_pnl_long_short_sum_to_zero() {
        let entry = 3_150u128;
        let qty = 50u128;
        let settle = 3_200u128;
        let long_pnl = compute_settlement_pnl(&sample_position(Side::Buy, qty, entry), settle);
        let short_pnl = compute_settlement_pnl(&sample_position(Side::Sell, qty, entry), settle);
        assert_eq!(long_pnl.to_i128() + short_pnl.to_i128(), 0);
    }
}
