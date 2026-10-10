//! Liquidation math (M4): trigger eligibility, liquidation-order generation,
//! and the 0.5%-of-notional fee split.
//!
//! Pure: same inputs always produce the same outputs. The keeper-side flow
//! (`LiquidationTrigger` tx → engine drains the queue at end-of-block) lives
//! in `cb-block`'s executor; this module is the math it calls.
//!
//! ## Trigger condition
//!
//! Per `docs/design/perps-trading.md §5.1`, an account is liquidatable when
//! `equity < maintenance_margin_required`. The check uses **mark** prices,
//! not last-trade or limit, exactly so a transient one-sided book wiggle
//! doesn't cause a liquidation cascade — the median-clamp construction in
//! `cb-engine::mark` already protects mark from manipulation.
//!
//! ## Liquidation-order generation
//!
//! Per the same doc §5.2, the engine submits **reduce-only IOCs** at
//! `mark ± 50 bps adverse`, widening up to a bankruptcy band when the book
//! is thin. v1 ships a deterministic ladder:
//!
//! | Tranche | Adverse offset (bps) | Fraction of remaining qty |
//! |---------|----------------------|---------------------------|
//! | 1       | 50                   | 100%                      |
//! | 2       | 100                  | 100%                      |
//! | 3       | 200                  | 100%                      |
//!
//! Each tranche is a reduce-only IOC; the matcher fills what crosses, the
//! remainder rolls into the next tranche. We don't go beyond 200 bps in v1
//! — if even that fails to close, the residual gets absorbed by the
//! insurance fund and the market enters reduce-only halt (`§5.4`).
//!
//! "Adverse" means the price walks *toward* the order book the closer
//! direction the position must trade in: a long is closed by selling, so the
//! liquidation order sells at a **lower** price than mark; a short is
//! closed by buying, so the liquidation order buys at a **higher** price.
//!
//! ## Fee split
//!
//! 0.5% of liquidated notional total: 0.3% to insurance, 0.2% to the keeper.
//! Both halves are micro-USD `u128`. Rounding is fixed to round-down on the
//! keeper bounty and round-up on insurance (favors the protocol's solvency
//! pool).

use cb_types::{DerivId, MarginAccount, Position, Side};

use crate::margin::{maintenance_margin_required, position_unrealized_pnl_micro_usd, LeverageTier};

/// Width of the adverse-price ladder (in bps from mark) used by
/// `compute_liquidation_orders` when the book is too thin to fill at the
/// first tranche's price.
const LIQUIDATION_LADDER_BPS: &[u16] = &[50, 100, 200];

/// Liquidation-fee total in bps of liquidated notional. 0.5%.
pub const LIQUIDATION_FEE_TOTAL_BPS: u16 = 50;

/// Insurance share of `LIQUIDATION_FEE_TOTAL_BPS`: 0.3%.
pub const LIQUIDATION_FEE_INSURANCE_BPS: u16 = 30;

/// Keeper share of `LIQUIDATION_FEE_TOTAL_BPS`: 0.2%.
pub const LIQUIDATION_FEE_KEEPER_BPS: u16 = 20;

/// One reduce-only IOC tranche emitted by `compute_liquidation_orders`. The
/// executor consumes these in order, sending each through `match_order` until
/// the position is closed or the ladder runs out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiquidationOrder {
    pub market: DerivId,
    pub owner: cb_types::Address,
    /// Direction the liquidation order trades in. For a long position this
    /// is `Sell` (the position must sell to close); for a short it is `Buy`.
    pub side: Side,
    /// Limit price of the IOC, q8. Picked at `mark ± offset_bps` adverse.
    pub price_q8: u128,
    /// Quantity to attempt at this tranche. Always equals the remaining
    /// position size at the time of generation; partial fills feed back into
    /// the next tranche's `qty` via the executor.
    pub qty: u128,
    /// Diagnostic: the bps offset from mark used for this tranche.
    pub adverse_offset_bps: u16,
}

/// Decide whether `account` is liquidatable given its open positions and the
/// current mark prices. Pure check — same inputs always yield the same bool.
///
/// The condition: `equity < maintenance_margin_required`, where equity =
/// `collateral + Σ unrealized_pnl(side, qty, entry, mark)` and MM is the
/// piecewise sum across the supplied tier table.
///
/// Special cases:
///   * No open positions → not liquidatable (MM = 0; equity ≥ 0 trivially
///     covers it).
///   * A position whose market lacks a mark in `marks` is **skipped** — same
///     defensive convention as `compute_equity`. The caller (executor) is
///     responsible for collecting all relevant marks before calling.
pub fn is_liquidatable(
    account: &MarginAccount,
    positions: &[Position],
    marks: &[(DerivId, u128)],
    tiers: &[LeverageTier],
) -> bool {
    let mm = maintenance_margin_required(positions, marks, tiers);
    if mm == 0 {
        return false;
    }
    // Compute equity inline so the comparison is straight i128 vs u128.
    // `bad_debt` is subtracted for the same reason `compute_equity` subtracts
    // it: an account that owes the protocol money has negative equity, not
    // zero, and must be liquidatable on that basis alone. (QA F4)
    let mut equity: i128 = (account.collateral as i128).saturating_sub(account.bad_debt as i128);
    for p in positions {
        if p.qty == 0 {
            continue;
        }
        let mark_q8 = match marks.iter().find(|(m, _)| m == &p.market).map(|(_, x)| *x) {
            Some(x) => x,
            None => continue,
        };
        let pnl = position_unrealized_pnl_micro_usd(p.side, p.qty, p.entry_px_q8, mark_q8);
        equity = equity.saturating_add(pnl.to_i128());
    }
    // Liquidatable iff equity < mm. Negative equity is always < mm (mm ≥ 0).
    if equity < 0 {
        return true;
    }
    (equity as u128) < mm
}

/// Return a list of reduce-only IOCs that will close `position` at mark ± 50
/// / 100 / 200 bps adverse. The matcher consumes these one at a time; each
/// tranche carries the **full** remaining qty of the position so the next
/// matcher call can fill against fresh book depth without the executor
/// having to track partial-fill bookkeeping inline.
///
/// `min_price_q8` and `max_price_q8` are sanity bounds — used by the
/// executor to keep the limit price within the market's tick range. Pass
/// `(0, u128::MAX)` from tests where bounds don't matter.
///
/// Returns an empty Vec if `position.qty == 0`.
pub fn compute_liquidation_orders(
    position: &Position,
    mark_q8: u128,
    market: DerivId,
) -> Vec<LiquidationOrder> {
    if position.qty == 0 {
        return Vec::new();
    }
    // Liquidation orders trade in the **opposite** direction of the position:
    // a long sells, a short buys.
    let liq_side = match position.side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    };
    let mut out = Vec::with_capacity(LIQUIDATION_LADDER_BPS.len());
    for &offset in LIQUIDATION_LADDER_BPS {
        let price_q8 = adverse_price_q8(mark_q8, liq_side, offset);
        // Skip degenerate tranches (price would underflow to 0). A 0 price
        // wouldn't pass the matcher's static check; we'd rather fail closed
        // than loud-panic here.
        if price_q8 == 0 {
            continue;
        }
        out.push(LiquidationOrder {
            market,
            owner: position.owner,
            side: liq_side,
            price_q8,
            qty: position.qty,
            adverse_offset_bps: offset,
        });
    }
    out
}

/// Compute the price at `mark ± offset_bps` adverse for the supplied side.
/// "Adverse" = walks toward the bid (selling) or toward the ask (buying).
///
/// Formula: `price = mark * (10_000 ± offset) / 10_000`. Saturating to keep
/// us inside u128 even at extreme bp values.
fn adverse_price_q8(mark_q8: u128, side: Side, offset_bps: u16) -> u128 {
    let bps = offset_bps as u128;
    match side {
        // Selling (closing a long) → walk the price down: mark * (10_000 - bps) / 10_000.
        Side::Sell => {
            let mult = 10_000u128.saturating_sub(bps);
            mark_q8.saturating_mul(mult) / 10_000
        }
        // Buying (closing a short) → walk the price up: mark * (10_000 + bps) / 10_000.
        Side::Buy => {
            let mult = 10_000u128.saturating_add(bps);
            mark_q8.saturating_mul(mult) / 10_000
        }
    }
}

/// Split a liquidation fee on `notional_micro_usd` between the insurance fund
/// and the keeper. Returns `(insurance_fee, keeper_fee)` in micro-USD.
///
/// 0.5% total = 0.3% insurance + 0.2% keeper. Insurance rounds **up** (favors
/// the protocol's solvency pool); keeper rounds **down** (any 1-unit
/// rounding stays with the protocol). Total `insurance + keeper` therefore
/// equals `ceil(notional * 0.3%)` + `floor(notional * 0.2%)`, which is at
/// most 1 unit greater than `notional * 0.5%`.
pub fn liquidation_fee_split(notional_micro_usd: u128) -> (u128, u128) {
    let insurance = ceil_div(
        notional_micro_usd.saturating_mul(LIQUIDATION_FEE_INSURANCE_BPS as u128),
        10_000,
    );
    let keeper = notional_micro_usd.saturating_mul(LIQUIDATION_FEE_KEEPER_BPS as u128) / 10_000;
    (insurance, keeper)
}

fn ceil_div(num: u128, den: u128) -> u128 {
    if den == 0 {
        return 0;
    }
    num.saturating_add(den - 1) / den
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::margin::DEFAULT_TIERS;
    use cb_types::{Address, Signed};

    const Q8: u128 = 100_000_000;
    const MICRO: u128 = 1_000_000;
    const ONE_BTC: u128 = 100_000_000;
    const PX_50K_Q8: u128 = 50_000 * Q8;

    fn pos(side: Side, qty: u128, entry_q8: u128) -> Position {
        Position {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            side,
            qty,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: entry_q8,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        }
    }

    fn account(collateral: u128) -> MarginAccount {
        MarginAccount {
            owner: Address([1; 20]),
            collateral,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        }
    }

    // ---------------- is_liquidatable ----------------

    #[test]
    fn healthy_position_is_not_liquidatable() {
        // 1 BTC long at 50k entry, mark 50k, $5_000 collateral:
        //   equity = 5_000 + 0 = 5_000
        //   notional = 50_000; MM tier1 1% = 500
        //   5_000 ≥ 500 → not liquidatable.
        let m = account(5_000 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), PX_50K_Q8)];
        assert!(!is_liquidatable(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn position_with_loss_below_mm_is_liquidatable() {
        // 1 BTC long entry 50k, mark 45k (uPnL = -5_000), collateral 5_400:
        //   equity = 5_400 - 5_000 = 400
        //   notional = 45_000; MM tier1 1% = 450
        //   400 < 450 → LIQ.
        let m = account(5_400 * MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), 45_000 * Q8)];
        assert!(is_liquidatable(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn negative_equity_is_always_liquidatable() {
        // Underwater position: collateral $1, mark drops to 40k → uPnL = -10_000.
        // equity = 1 - 10_000 = -9_999 → must liquidate.
        let m = account(MICRO);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let marks = [(DerivId::perp(0), 40_000 * Q8)];
        assert!(is_liquidatable(&m, &[p], &marks, DEFAULT_TIERS));
    }

    #[test]
    fn no_positions_is_not_liquidatable() {
        let m = account(1);
        assert!(!is_liquidatable(&m, &[], &[], DEFAULT_TIERS));
    }

    #[test]
    fn empty_marks_skips_position_so_not_liquidatable() {
        // Defensive: a missing mark on a position contributes no PnL or MM
        // (skipped). This means the account looks fine even though we don't
        // know the market's price. The executor must always supply marks.
        let m = account(1);
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        assert!(!is_liquidatable(&m, &[p], &[], DEFAULT_TIERS));
    }

    // ---------------- compute_liquidation_orders ----------------

    #[test]
    fn long_liquidation_walks_down_at_50_100_200_bps() {
        let p = pos(Side::Buy, ONE_BTC, PX_50K_Q8);
        let mark = PX_50K_Q8;
        let orders = compute_liquidation_orders(&p, mark, DerivId::perp(0));
        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].side, Side::Sell);
        assert_eq!(orders[0].adverse_offset_bps, 50);
        // 50_000 * 9_950 / 10_000 = 49_750.
        assert_eq!(orders[0].price_q8, 49_750 * Q8);
        // 50_000 * 9_900 / 10_000 = 49_500.
        assert_eq!(orders[1].price_q8, 49_500 * Q8);
        // 50_000 * 9_800 / 10_000 = 49_000.
        assert_eq!(orders[2].price_q8, 49_000 * Q8);
        // Each tranche carries the full remaining qty (the executor decrements).
        assert_eq!(orders[0].qty, ONE_BTC);
        assert_eq!(orders[1].qty, ONE_BTC);
        assert_eq!(orders[2].qty, ONE_BTC);
    }

    #[test]
    fn short_liquidation_walks_up_at_50_100_200_bps() {
        let p = pos(Side::Sell, ONE_BTC, PX_50K_Q8);
        let mark = PX_50K_Q8;
        let orders = compute_liquidation_orders(&p, mark, DerivId::perp(0));
        assert_eq!(orders.len(), 3);
        assert_eq!(orders[0].side, Side::Buy);
        // 50_000 * 10_050 / 10_000 = 50_250.
        assert_eq!(orders[0].price_q8, 50_250 * Q8);
        assert_eq!(orders[1].price_q8, 50_500 * Q8);
        assert_eq!(orders[2].price_q8, 51_000 * Q8);
    }

    #[test]
    fn zero_qty_position_yields_no_orders() {
        let p = pos(Side::Buy, 0, PX_50K_Q8);
        let orders = compute_liquidation_orders(&p, PX_50K_Q8, DerivId::perp(0));
        assert!(orders.is_empty());
    }

    // ---------------- fee split ----------------

    #[test]
    fn fee_split_50_30_20_on_round_notional() {
        // Notional 100_000 USD = 1e11 micro-USD.
        // Total = 0.5% = 500 USD; insurance 0.3% = 300; keeper 0.2% = 200.
        let (ins, keeper) = liquidation_fee_split(100_000 * MICRO);
        assert_eq!(ins, 300 * MICRO);
        assert_eq!(keeper, 200 * MICRO);
    }

    #[test]
    fn fee_split_zero_notional_zero_fees() {
        let (ins, keeper) = liquidation_fee_split(0);
        assert_eq!(ins, 0);
        assert_eq!(keeper, 0);
    }

    #[test]
    fn fee_split_rounds_insurance_up_keeper_down() {
        // Notional 1_001 micro-USD. Insurance 0.3% = 3.003 → 4 (ceil).
        // Keeper 0.2% = 2.002 → 2 (floor). Catches the rounding direction.
        let (ins, keeper) = liquidation_fee_split(1_001);
        assert_eq!(ins, 4);
        assert_eq!(keeper, 2);
    }

    #[test]
    fn fee_split_total_is_close_to_50_bps() {
        // For a "nice" notional of 1e10 micro-USD = $10_000:
        //   insurance = 30 USD = 30_000_000 micro
        //   keeper = 20 USD = 20_000_000 micro
        //   total = 50 USD = 50 bps of $10_000.
        let (ins, keeper) = liquidation_fee_split(10_000 * MICRO);
        assert_eq!(ins + keeper, 50 * MICRO);
    }
}
