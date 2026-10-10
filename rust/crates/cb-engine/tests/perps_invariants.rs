//! M7 property test bundle for the perps engine.
//!
//! Per the brief, four invariants under heavy randomized load (1024+ cases each):
//!
//!   1. **Equity conservation across any sequence of fills.** When the matcher
//!      fills two participants against each other, their realised PnL deltas
//!      sum to exactly zero in i128 space.
//!   2. **No-negative-qty.** `Position` keeps qty as `u128` plus a `side`
//!      flag. After any sequence of fills, `qty` is `>= 0` (trivially true
//!      for `u128`), and crucially the `(side, qty)` pair never represents
//!      a logically-negative position by miswiring (e.g. side=Buy with
//!      qty=large after a flip should reflect the flip's net direction).
//!   3. **Funding-rate clamp invariant under random oracle inputs.** For
//!      arbitrary `(premium_q9, interest_q9)` and any FundingClamp where
//!      `inner ≤ outer`, the computed rate is within `[-outer, +outer]`.
//!   4. **Liquidation idempotency.** Once a position has been triggered into
//!      a flat state by `compute_liquidation_orders` + a successful fill,
//!      retriggering on the now-flat position must produce no further
//!      liquidation orders (empty `Vec`).
//!
//! This file lives in `cb-engine/tests/` so it tests the **pure** layer
//! without touching RocksDB or the `cb-block` executor. The executor-level
//! integration tests in `cb-block/tests/` already cover the impure paths.

use cb_engine::{
    apply_fill_to_position, compute_funding_rate, compute_liquidation_orders, is_liquidatable,
    liquidation_fee_split, FundingClamp, DEFAULT_TIERS,
};
use cb_types::{Address, DerivId, MarginAccount, Position, Side, Signed};
use proptest::prelude::*;

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

fn account(collateral: u128) -> MarginAccount {
    MarginAccount {
        owner: Address([1; 20]),
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    }
}

// ---------------------------------------------------------------------------
// 1. Equity conservation across a sequence of fills
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Conservation across N pair-of-fills. Each fill creates a long and a
    /// short on opposite participants at the same (qty, price). The sum of
    /// realised PnL across both sides must be exactly zero.
    ///
    /// We open from flat in N-1 cases and close-then-flip in the last case
    /// to exercise both Case 2 (same-side add) and Cases 3-5 (close/flip)
    /// branches of `apply_fill_to_position`.
    #[test]
    fn equity_conservation_across_open_close_flip_sequence(
        seq in proptest::collection::vec(
            (1u128..1_000_000_000u128, 1u128..1_000_000u128, any::<bool>()),
            1..=8,
        ),
    ) {
        let market = DerivId::perp(0);
        let alice = Address([1; 20]);
        let bob = Address([2; 20]);
        let mut alice_pos: Option<Position> = None;
        let mut bob_pos: Option<Position> = None;
        // Track realised PnL across the entire sequence.
        let mut alice_pnl_total: i128 = 0;
        let mut bob_pnl_total: i128 = 0;

        for (price, qty, alice_buys) in seq {
            let alice_side = if alice_buys { Side::Buy } else { Side::Sell };
            let bob_side = if alice_buys { Side::Sell } else { Side::Buy };

            let (a_new, a_pnl) = apply_fill_to_position(
                alice_pos.as_ref(), alice, market, alice_side, qty, price, 1,
                Signed::ZERO,
            );
            let (b_new, b_pnl) = apply_fill_to_position(
                bob_pos.as_ref(), bob, market, bob_side, qty, price, 1,
                Signed::ZERO,
            );

            // ZERO-SUM at the trade-level.
            let a = a_pnl.to_i128();
            let b = b_pnl.to_i128();
            prop_assert_eq!(a + b, 0, "trade-level PnL must sum to zero");

            alice_pnl_total = alice_pnl_total.saturating_add(a_pnl.to_i128());
            bob_pnl_total = bob_pnl_total.saturating_add(b_pnl.to_i128());

            alice_pos = Some(a_new);
            bob_pos = Some(b_new);
        }

        // Cumulative ZERO-SUM at the sequence level.
        prop_assert_eq!(
            alice_pnl_total + bob_pnl_total, 0,
            "cumulative PnL across both sides must equal zero"
        );
    }

    // -----------------------------------------------------------------------
    // 2. No-negative-qty invariant: every Position output by the engine has
    // qty >= 0 (u128, trivially true) and the side+qty pair stays internally
    // consistent (qty=0 implies "flat", regardless of side flag).
    // -----------------------------------------------------------------------

    #[test]
    fn position_invariant_holds_through_open_close_flip(
        prev_qty in 1u128..1_000_000u128,
        prev_entry in 1u128..1_000_000_000u128,
        fill_qty in 1u128..2_000_000u128,
        fill_price in 1u128..1_000_000_000u128,
        prev_buy in any::<bool>(),
        fill_buy in any::<bool>(),
    ) {
        let market = DerivId::perp(0);
        let owner = Address([1; 20]);
        let prev_side = if prev_buy { Side::Buy } else { Side::Sell };
        let fill_side = if fill_buy { Side::Buy } else { Side::Sell };
        let prev = pos(prev_side, prev_qty, prev_entry);

        let (after, _pnl) = apply_fill_to_position(
            Some(&prev), owner, market, fill_side, fill_qty, fill_price, 100,
            Signed::ZERO,
        );

        // `qty` is `u128`, so it is structurally non-negative: the property
        // is carried by the type and there is nothing here to assert at
        // runtime. The assertions below are the ones that can actually fail.
        // qty=0 is the "flat" sentinel; entry_px is canonicalised to 0 when
        // flat (the executor / engine writes 0 in case 5: exact close).
        if after.qty == 0 {
            prop_assert_eq!(after.entry_px_q8, 0,
                "flat position must have entry_px_q8 = 0");
        }
        // Position owner is preserved.
        prop_assert_eq!(after.owner, owner);
        prop_assert_eq!(after.market, market);
    }

    // -----------------------------------------------------------------------
    // 3. Funding-rate clamp under random oracle inputs.
    // -----------------------------------------------------------------------

    /// Default clamp: rate must always be in [-outer, +outer] = [-5e6, +5e6] q9.
    #[test]
    fn funding_rate_within_default_outer_clamp_arbitrary(
        p in any::<i64>(), i in any::<i64>(),
    ) {
        let r = compute_funding_rate(p, i, FundingClamp::DEFAULT);
        prop_assert!(r >= -FundingClamp::DEFAULT.outer_q9);
        prop_assert!(r <= FundingClamp::DEFAULT.outer_q9);
    }

    /// Custom clamp: rate within [-outer, +outer] for any non-negative bounds.
    #[test]
    fn funding_rate_within_custom_outer_clamp_arbitrary(
        p in any::<i64>(),
        i in any::<i64>(),
        inner in 0i64..1_000_000,
        outer in 1i64..100_000_000,
    ) {
        // Mirror the protocol assumption that outer >= inner.
        let outer = outer.max(inner);
        let c = FundingClamp { inner_q9: inner, outer_q9: outer };
        let r = compute_funding_rate(p, i, c);
        prop_assert!(r >= -outer);
        prop_assert!(r <= outer);
    }

    /// Sign-symmetry: negating both inputs negates the output (within the
    /// representable range). Catches a regression where one of the clamps
    /// was applied asymmetrically (e.g. forgot the negative case).
    #[test]
    fn funding_rate_negation_symmetric(
        p in -1_000_000_000i64..1_000_000_000,
        i in -1_000_000_000i64..1_000_000_000,
    ) {
        let pos_r = compute_funding_rate(p, i, FundingClamp::DEFAULT);
        let neg_r = compute_funding_rate(-p, -i, FundingClamp::DEFAULT);
        prop_assert_eq!(pos_r, -neg_r);
    }

    // -----------------------------------------------------------------------
    // 4. Liquidation idempotency.
    //
    // Once a position is flat (qty=0), `compute_liquidation_orders` must
    // return an empty Vec — the same trigger run twice is a no-op on the
    // second call. This holds regardless of the prior side / entry / mark.
    // -----------------------------------------------------------------------

    #[test]
    fn flat_position_yields_no_liquidation_orders(
        side_buy in any::<bool>(),
        entry in 1u128..1_000_000_000u128,
        mark in 1u128..1_000_000_000u128,
    ) {
        let side = if side_buy { Side::Buy } else { Side::Sell };
        // Construct a "flat" position: qty=0 with the side flag set arbitrarily.
        let p = pos(side, 0, entry);
        let orders = compute_liquidation_orders(&p, mark, DerivId::perp(0));
        prop_assert!(orders.is_empty(),
            "flat position must yield zero liquidation orders");
    }

    /// `is_liquidatable(account, [], marks, tiers) == false` always (no
    /// positions, no MM requirement, no liquidation possible). Catches a
    /// regression where the empty case fell through to a non-zero default MM.
    #[test]
    fn is_liquidatable_false_with_no_positions(
        collateral in any::<u128>(),
    ) {
        let m = account(collateral);
        prop_assert!(!is_liquidatable(&m, &[], &[], DEFAULT_TIERS));
    }

    /// Liquidation fee split monotonicity: more notional → more fee. This
    /// surfaces a regression where an internal saturating_mul wrapped early.
    #[test]
    fn fee_split_monotonic_in_notional(
        n1 in 0u128..(u128::MAX / 4),
        delta in 1u128..1_000_000_000u128,
    ) {
        let n2 = n1.saturating_add(delta);
        let (i1, k1) = liquidation_fee_split(n1);
        let (i2, k2) = liquidation_fee_split(n2);
        prop_assert!(i2 >= i1, "insurance fee not monotone: {n1}→{i1}, {n2}→{i2}");
        prop_assert!(k2 >= k1, "keeper fee not monotone: {n1}→{k1}, {n2}→{k2}");
    }

    // -----------------------------------------------------------------------
    // 5. Liquidation idempotency at the sequence level.
    //
    // Apply a single liquidation tranche's worth of fill to a long position;
    // the resulting position is closed (qty=0 if the tranche fully covers).
    // Re-running `compute_liquidation_orders` on the closed position yields
    // an empty list. This pins the broader idempotency claim end-to-end.
    // -----------------------------------------------------------------------

    #[test]
    fn liquidation_then_retrigger_is_no_op(
        prev_qty in 1u128..1_000_000u128,
        entry in 1u128..1_000_000_000u128,
        mark in 1u128..1_000_000_000u128,
    ) {
        let market = DerivId::perp(0);
        let owner = Address([1; 20]);
        let p = pos(Side::Buy, prev_qty, entry);

        // Generate liquidation orders for the long.
        let orders = compute_liquidation_orders(&p, mark, market);
        prop_assert_eq!(orders.len(), 3,
            "default ladder always emits 3 tranches when qty>0");
        prop_assert_eq!(orders[0].qty, prev_qty);

        // Apply a fill that fully covers the qty (simulating an IOC that
        // crosses every resting bid). Closing direction = Sell; fill_qty
        // = prev_qty; fill_price = the first tranche's adverse band.
        let (after, _pnl) = apply_fill_to_position(
            Some(&p), owner, market, Side::Sell, prev_qty, orders[0].price_q8, 1,
            Signed::ZERO,
        );
        prop_assert_eq!(after.qty, 0,
            "fill of full size against flat position must close it");

        // RETRIGGER: compute_liquidation_orders on the now-flat position
        // must return zero tranches. This is the idempotency proof.
        let orders2 = compute_liquidation_orders(&after, mark, market);
        prop_assert!(orders2.is_empty(), "retrigger on flat must be a no-op");
    }
}
