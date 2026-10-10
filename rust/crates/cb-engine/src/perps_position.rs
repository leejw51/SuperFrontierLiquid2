//! Position-delta math for perpetual fills.
//!
//! Pure: takes a buyer/seller pre-state pair plus a `Fill` and returns the
//! post-fill `(buyer, seller, realized_pnl_buyer, realized_pnl_seller)` tuple.
//! No I/O, no margin checks — M4 owns the equity gate. Pre-M4 a fill always
//! succeeds and may produce negative-equity positions.
//!
//! ## Sign convention (mirrors `Position`)
//!
//! `Position` is sign-magnitude: `side` is direction, `qty` is non-negative
//! magnitude. We treat `qty == 0` as "flat" regardless of `side`. The matcher
//! always passes signed deltas through this module rather than letting the
//! executor reach in and mutate `qty` directly — this is where the 5 cases
//! converge:
//!
//!   1. Open from flat. `qty == 0` → set side = trade direction, qty = trade qty,
//!      entry = trade price, **entry funding index = the market's current
//!      cumulative index**.
//!   2. Add to existing same-side position. New entry = volume-weighted average
//!      of old and new (rounded toward zero). The entry *funding* index is
//!      volume-weighted the same way — see `vwap_funding_index`.
//!   3. Reduce existing opposite-side position (smaller trade). Entry unchanged
//!      (we kept the older lots' basis). Realised PnL = trade qty *
//!      (trade price - entry) with sign by side.
//!   4. Flip existing opposite-side position (larger trade). Realised PnL on
//!      the closed lots; remainder opens fresh on the other side at trade price.
//!   5. Exact close. Realised PnL on full qty; position becomes flat (qty 0).
//!
//! ## The entry funding index
//!
//! `settle_market_funding` charges `qty * (cumulative - entry_funding_index)`,
//! so `entry_funding_index` is the "as of" stamp that says which slice of the
//! market's funding history this position is liable for. It is therefore
//! **not** a field that may be inherited from whatever happened to be in the
//! previous row: a position opened at cumulative index `C` that is stamped
//! with `0` is charged for every block of funding that accrued before it
//! existed, with nobody on the other side of the transfer. That mints or burns
//! collateral outright and breaks both conservation and funding neutrality.
//!
//! Hence `current_funding_index` is a required argument, and every case that
//! creates or grows a position sets the stamp from it:
//!
//! * **Open from flat** and **flip** stamp the current index directly. Both
//!   are brand-new lots; there is no earlier liability to carry. Open-from-flat
//!   deliberately ignores the previous row's snapshot, which also disposes of
//!   the stale value left behind on a `qty == 0` sentinel row.
//! * **Add to an existing position** volume-weights the old and new stamps,
//!   exactly as the entry *price* is weighted. That is what keeps funding
//!   zero-sum when one side of a market adds to a position that another side
//!   is opening fresh: the weighted stamp makes
//!   `qty_new * (C - idx_new) == qty_old * (C - idx_old) + qty_add * (C - C_add)`
//!   hold to within integer truncation, so the added lots are liable for
//!   funding from the moment they were added and not a block earlier.
//! * **Reduce** and **exact close** leave the stamp alone: the surviving lots
//!   keep their own basis, which is the same rule the entry price follows.
//!
//! Callers that have no funding index at all — dated futures, which never tick
//! funding — pass `Signed::ZERO`.
//!
//! Rounding: PnL math is `i128` (we widen `u128` at the boundary and saturate
//! on overflow — no panics in consensus). Average-entry math rounds toward
//! zero (favors the protocol on the IM/MM side; matches the spec's stated
//! convention in §2.5 of perps-trading.md).

use cb_types::{Address, DerivId, Position, Side, Signed};

/// Apply a single fill to one side's position. Returns `(updated, realized_pnl)`.
/// The caller invokes this twice per fill (once for buyer, once for seller).
///
/// `side_of_fill` is the direction in which THIS owner is moving — i.e. for
/// the taker's side it equals `Fill.taker_side`; for the maker's side it's the
/// opposite. The caller knows which is which.
///
/// `realized_pnl` is signed. Convention: positive means the owner gained.
///
/// `current_funding_index` is the market's live `FundingIndex.cumulative` at
/// the height this fill lands. See the module docs for why it is a required
/// argument rather than something inherited from `pre`. Markets that do not
/// tick funding (dated futures) pass `Signed::ZERO`.
#[allow(clippy::too_many_arguments)]
pub fn apply_fill_to_position(
    pre: Option<&Position>,
    owner: Address,
    market: DerivId,
    side_of_fill: Side,
    fill_qty: u128,
    fill_price_q8: u128,
    block_height: u64,
    current_funding_index: Signed,
) -> (Position, Signed) {
    // Zero-quantity fills are rejected by the matcher (`BookError::ZeroQty`)
    // and by `cb_tx::validate_static`, and the executor's admission block
    // rejects them before the margin gate simulates anything. Returning the
    // pre-state unchanged keeps this a total function rather than relying on a
    // `debug_assert` that a Byzantine proposer could turn into a replica abort.
    if fill_qty == 0 {
        let unchanged = pre.cloned().unwrap_or(Position {
            owner,
            market,
            side: side_of_fill,
            qty: 0,
            entry_funding_index: current_funding_index,
            entry_px_q8: 0,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: block_height,
        });
        return (unchanged, Signed::ZERO);
    }

    // Materialize previous state. Use `unwrap_or_default`-shaped path: a
    // missing `Position` is "flat".
    let (prev_side, prev_qty, prev_entry_q8, prev_funding_idx, margin_mode, isolated_margin) =
        if let Some(p) = pre {
            (
                p.side,
                p.qty,
                p.entry_px_q8,
                p.entry_funding_index,
                p.margin_mode,
                p.isolated_margin,
            )
        } else {
            (
                Side::Buy, // arbitrary; only matters if qty > 0
                0u128,
                0u128,
                Signed::ZERO,
                0u8,
                0u128,
            )
        };

    // Case 1: flat → open. The stamp comes from the market, never from `pre`:
    // a `qty == 0` sentinel row left behind by an earlier close carries a
    // snapshot that is now arbitrarily stale.
    if prev_qty == 0 {
        let updated = Position {
            owner,
            market,
            side: side_of_fill,
            qty: fill_qty,
            entry_funding_index: current_funding_index,
            entry_px_q8: fill_price_q8,
            margin_mode,
            isolated_margin,
            last_funding_height: block_height,
        };
        return (updated, Signed::ZERO);
    }

    // Case 2: same side → add to position. VWAP entry, no realised PnL.
    if prev_side == side_of_fill {
        let new_qty = prev_qty.saturating_add(fill_qty);
        // VWAP: (prev_qty * prev_px + fill_qty * fill_px) / new_qty.
        let new_entry = vwap_entry(prev_qty, prev_entry_q8, fill_qty, fill_price_q8, new_qty);
        // The funding stamp is weighted on the same volumes as the entry
        // price. Keeping `prev_funding_idx` would charge the added lots for
        // funding that accrued before they existed; adopting
        // `current_funding_index` outright would forgive the original lots the
        // funding they already owe. The weighted average is the only choice
        // that leaves the market's total liability unchanged.
        let new_funding_idx = vwap_funding_index(
            prev_qty,
            prev_funding_idx,
            fill_qty,
            current_funding_index,
            new_qty,
        );
        let updated = Position {
            owner,
            market,
            side: side_of_fill,
            qty: new_qty,
            entry_funding_index: new_funding_idx,
            entry_px_q8: new_entry,
            margin_mode,
            isolated_margin,
            last_funding_height: block_height,
        };
        return (updated, Signed::ZERO);
    }

    // Cases 3, 4, 5: opposite side. We're closing some or all of the prior
    // position, possibly flipping.
    let close_qty = std::cmp::min(prev_qty, fill_qty);
    let pnl = realised_pnl_q8(prev_side, close_qty, prev_entry_q8, fill_price_q8);

    if fill_qty < prev_qty {
        // Case 3: partial close, prior side intact.
        let updated = Position {
            owner,
            market,
            side: prev_side,
            qty: prev_qty - fill_qty,
            entry_funding_index: prev_funding_idx,
            entry_px_q8: prev_entry_q8, // remaining lots keep their basis
            margin_mode,
            isolated_margin,
            last_funding_height: block_height,
        };
        return (updated, pnl);
    }

    if fill_qty == prev_qty {
        // Case 5: exact close. Position is flat. We retain the funding-index
        // snapshot for the GC window — a follow-up tx within the same block
        // can re-open without losing it.
        let updated = Position {
            owner,
            market,
            side: prev_side, // immaterial; qty is 0
            qty: 0,
            entry_funding_index: prev_funding_idx,
            entry_px_q8: 0,
            margin_mode,
            isolated_margin,
            last_funding_height: block_height,
        };
        return (updated, pnl);
    }

    // Case 4: flip. Close the full prior position; open the remainder on the
    // new side at fill price. The remainder is a brand-new lot, so it is
    // stamped with the market's current index exactly as case 1 is.
    let new_qty = fill_qty - prev_qty;
    let updated = Position {
        owner,
        market,
        side: side_of_fill,
        qty: new_qty,
        entry_funding_index: current_funding_index,
        entry_px_q8: fill_price_q8,
        margin_mode,
        isolated_margin,
        last_funding_height: block_height,
    };
    (updated, pnl)
}

/// Volume-weighted average entry price, rounded toward zero. Operates in
/// `u128` throughout — at v1 scales (`qty <= 1e18`, `price_q8 <= 1e36`) the
/// product fits in `u256`-sized math; we collapse to `u128` via saturating
/// ops and accept a 1-unit rounding loss in extreme worst cases. Tests below
/// pin the boundary.
fn vwap_entry(prev_qty: u128, prev_px: u128, add_qty: u128, add_px: u128, new_qty: u128) -> u128 {
    // (prev_qty * prev_px + add_qty * add_px) / new_qty
    let lhs = prev_qty.saturating_mul(prev_px);
    let rhs = add_qty.saturating_mul(add_px);
    let num = lhs.saturating_add(rhs);
    if new_qty == 0 {
        return 0;
    }
    num / new_qty
}

/// Volume-weighted average of two funding-index stamps, the signed mirror of
/// [`vwap_entry`].
///
/// Runs in `i128` because `FundingIndex.cumulative` is signed: a market whose
/// shorts have been paying longs carries a negative cumulative index, and a
/// position opened during that stretch must be stamped with a negative value.
/// Rounds toward zero, matching `vwap_entry`; the residue is bounded by one
/// index unit (`1e-9` of a rate) per add and is inside the dust budget the
/// conservation check allows.
fn vwap_funding_index(
    prev_qty: u128,
    prev_idx: Signed,
    add_qty: u128,
    add_idx: Signed,
    new_qty: u128,
) -> Signed {
    if new_qty == 0 {
        return add_idx;
    }
    let lhs = (prev_qty as i128).saturating_mul(prev_idx.to_i128());
    let rhs = (add_qty as i128).saturating_mul(add_idx.to_i128());
    Signed::from_i128(lhs.saturating_add(rhs) / (new_qty as i128))
}

/// Realised PnL on closing `close_qty` of a position that was opened on
/// `prev_side` at `entry_q8`, against an exit at `exit_q8`. Returned as a
/// `Signed` so the persisted shape stays `(sign: i8, abs: u128)`.
///
/// Formula:
///   long  closed: pnl = close_qty * (exit - entry)
///   short closed: pnl = close_qty * (entry - exit)
fn realised_pnl_q8(prev_side: Side, close_qty: u128, entry_q8: u128, exit_q8: u128) -> Signed {
    if close_qty == 0 {
        return Signed::ZERO;
    }
    let (gain, abs_diff) = match prev_side {
        Side::Buy => {
            if exit_q8 >= entry_q8 {
                (true, exit_q8 - entry_q8)
            } else {
                (false, entry_q8 - exit_q8)
            }
        }
        Side::Sell => {
            if entry_q8 >= exit_q8 {
                (true, entry_q8 - exit_q8)
            } else {
                (false, exit_q8 - entry_q8)
            }
        }
    };
    let abs = close_qty.saturating_mul(abs_diff);
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

    fn pos(side: Side, qty: u128, entry: u128) -> Position {
        Position {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
            side,
            qty,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: entry,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        }
    }

    #[test]
    fn open_from_flat_sets_side_qty_entry() {
        let (p, pnl) = apply_fill_to_position(
            None,
            Address([2; 20]),
            DerivId::perp(0),
            Side::Buy,
            10,
            100,
            42,
            Signed::ZERO,
        );
        assert_eq!(p.side, Side::Buy);
        assert_eq!(p.qty, 10);
        assert_eq!(p.entry_px_q8, 100);
        assert_eq!(p.last_funding_height, 42);
        assert_eq!(pnl, Signed::ZERO);
    }

    #[test]
    fn add_same_side_uses_vwap_entry() {
        let prev = pos(Side::Buy, 10, 100);
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            10,
            200,
            1,
            Signed::ZERO,
        );
        assert_eq!(p.qty, 20);
        // VWAP = (10*100 + 10*200) / 20 = 150
        assert_eq!(p.entry_px_q8, 150);
        assert_eq!(pnl, Signed::ZERO);
    }

    #[test]
    fn partial_close_realises_pnl_keeps_basis() {
        // Long 10 @ 100. Sell 4 @ 150. PnL = 4 * (150 - 100) = 200.
        // Remaining: long 6 @ 100.
        let prev = pos(Side::Buy, 10, 100);
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            4,
            150,
            1,
            Signed::ZERO,
        );
        assert_eq!(p.side, Side::Buy);
        assert_eq!(p.qty, 6);
        assert_eq!(p.entry_px_q8, 100);
        assert_eq!(pnl, Signed::from_i128(200));
    }

    #[test]
    fn exact_close_zeroes_position_realises_pnl() {
        let prev = pos(Side::Buy, 10, 100);
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            10,
            150,
            1,
            Signed::ZERO,
        );
        assert_eq!(p.qty, 0);
        assert_eq!(pnl, Signed::from_i128(500));
    }

    #[test]
    fn flip_realises_full_prev_pnl_then_opens_remainder() {
        // Long 10 @ 100. Sell 25 @ 150. PnL = 10 * (150 - 100) = 500.
        // Remainder: short 15 @ 150.
        let prev = pos(Side::Buy, 10, 100);
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            25,
            150,
            1,
            Signed::ZERO,
        );
        assert_eq!(p.side, Side::Sell);
        assert_eq!(p.qty, 15);
        assert_eq!(p.entry_px_q8, 150);
        assert_eq!(pnl, Signed::from_i128(500));
    }

    #[test]
    fn short_pnl_signed_correctly() {
        // Short 10 @ 100. Cover at 80 → PnL = 10 * (100 - 80) = +200 (gain).
        let prev = pos(Side::Sell, 10, 100);
        let (_p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            10,
            80,
            1,
            Signed::ZERO,
        );
        assert_eq!(pnl, Signed::from_i128(200));

        // Short 10 @ 100. Cover at 130 → PnL = 10 * (100 - 130) = -300 (loss).
        let prev = pos(Side::Sell, 10, 100);
        let (_p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            10,
            130,
            1,
            Signed::ZERO,
        );
        assert_eq!(pnl, Signed::from_i128(-300));
    }

    #[test]
    fn closing_below_entry_on_long_is_loss() {
        let prev = pos(Side::Buy, 10, 100);
        let (_p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            10,
            70,
            1,
            Signed::ZERO,
        );
        assert_eq!(pnl, Signed::from_i128(-300));
    }

    // -----------------------------------------------------------------------
    // Entry funding index (QA finding F2).
    //
    // `settle_market_funding` charges `qty * (cumulative - entry_funding_index)`.
    // If the stamp is wrong the charge is wrong, and because the two sides of a
    // market carry different stamps the error does not cancel — it moves
    // collateral with nobody on the other side. These pin the stamp on every
    // case that creates or grows a position.
    // -----------------------------------------------------------------------

    #[test]
    fn opening_from_flat_stamps_the_markets_current_funding_index() {
        let cur = Signed::from_i128(4_200);
        let (p, _) = apply_fill_to_position(
            None,
            Address([2; 20]),
            DerivId::perp(0),
            Side::Buy,
            10,
            100,
            42,
            cur,
        );
        assert_eq!(
            p.entry_funding_index, cur,
            "a new position is liable only for funding from the moment it opened"
        );
    }

    #[test]
    fn reopening_over_a_closed_sentinel_row_ignores_its_stale_stamp() {
        // An exact close leaves a `qty == 0` row behind, carrying whatever
        // stamp the closed lots had. `settle_market_funding` skips zero-qty
        // rows, so that value goes stale — it must not be inherited.
        let mut sentinel = pos(Side::Buy, 0, 0);
        sentinel.entry_funding_index = Signed::from_i128(-999);
        let cur = Signed::from_i128(7_000);
        let (p, _) = apply_fill_to_position(
            Some(&sentinel),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            5,
            777,
            10,
            cur,
        );
        assert_eq!(p.entry_funding_index, cur);
    }

    #[test]
    fn flipping_stamps_the_current_index_on_the_new_side() {
        let mut prev = pos(Side::Buy, 10, 100);
        prev.entry_funding_index = Signed::from_i128(1_000);
        let cur = Signed::from_i128(9_000);
        let (p, _) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            25,
            150,
            1,
            cur,
        );
        assert_eq!(p.side, Side::Sell);
        assert_eq!(
            p.entry_funding_index, cur,
            "the remainder is a fresh short; it owes nothing for the long's history"
        );
    }

    #[test]
    fn adding_to_a_position_volume_weights_the_funding_stamp() {
        // 10 lots stamped at 100, plus 30 lots stamped at 200:
        //   (10*100 + 30*200) / 40 = 175.
        let mut prev = pos(Side::Buy, 10, 100);
        prev.entry_funding_index = Signed::from_i128(100);
        let (p, _) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            30,
            100,
            1,
            Signed::from_i128(200),
        );
        assert_eq!(p.qty, 40);
        assert_eq!(p.entry_funding_index, Signed::from_i128(175));
    }

    #[test]
    fn the_weighted_stamp_preserves_the_total_funding_liability() {
        // This is the property the weighting exists for. Liability at a future
        // cumulative index C is `qty * (C - stamp)`. Weighting must leave it
        // equal to the sum of the two parcels' liabilities.
        let mut prev = pos(Side::Buy, 400, 100);
        prev.entry_funding_index = Signed::from_i128(30);
        let add_idx = Signed::from_i128(110);
        let (p, _) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            600,
            100,
            1,
            add_idx,
        );
        let c = 500i128;
        let combined = (p.qty as i128) * (c - p.entry_funding_index.to_i128());
        let separate = 400 * (c - 30) + 600 * (c - 110);
        assert_eq!(combined, separate);
    }

    #[test]
    fn a_negative_cumulative_index_is_stamped_and_weighted_signed() {
        // Shorts paying longs drives `cumulative` negative. The stamp must
        // follow it; an unsigned average would wrap or clamp to zero.
        let mut prev = pos(Side::Sell, 10, 100);
        prev.entry_funding_index = Signed::from_i128(-1_000);
        let (p, _) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            10,
            100,
            1,
            Signed::from_i128(-500),
        );
        assert_eq!(p.entry_funding_index, Signed::from_i128(-750));
    }

    #[test]
    fn reducing_a_position_leaves_the_surviving_lots_stamp_alone() {
        let mut prev = pos(Side::Buy, 10, 100);
        prev.entry_funding_index = Signed::from_i128(55);
        let (p, _) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            4,
            150,
            1,
            Signed::from_i128(900),
        );
        assert_eq!(p.qty, 6);
        assert_eq!(
            p.entry_funding_index,
            Signed::from_i128(55),
            "the remaining lots keep their own basis, exactly as the entry price does"
        );
    }

    #[test]
    fn a_zero_quantity_fill_is_a_total_function_not_an_abort() {
        // A Byzantine proposer can put a zero-qty order in a block. The engine
        // must return, not fire a debug assertion that aborts the replica.
        let prev = pos(Side::Buy, 10, 100);
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            0,
            150,
            1,
            Signed::from_i128(5),
        );
        assert_eq!(p, prev, "state is unchanged");
        assert_eq!(pnl, Signed::ZERO);

        let (flat, pnl) = apply_fill_to_position(
            None,
            Address([1; 20]),
            DerivId::perp(0),
            Side::Buy,
            0,
            150,
            1,
            Signed::from_i128(5),
        );
        assert_eq!(flat.qty, 0);
        assert_eq!(pnl, Signed::ZERO);
    }

    #[test]
    fn vwap_with_zero_prev_qty_just_uses_new_price() {
        // Edge case: a flat position passed as Some(prev) with qty=0 should
        // behave the same as None — open from flat at fill price.
        let mut prev = pos(Side::Buy, 0, 0);
        prev.qty = 0;
        let (p, pnl) = apply_fill_to_position(
            Some(&prev),
            Address([1; 20]),
            DerivId::perp(0),
            Side::Sell,
            5,
            777,
            10,
            Signed::ZERO,
        );
        assert_eq!(p.qty, 5);
        assert_eq!(p.side, Side::Sell);
        assert_eq!(p.entry_px_q8, 777);
        assert_eq!(pnl, Signed::ZERO);
    }
}
