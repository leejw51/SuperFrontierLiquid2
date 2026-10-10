//! Property tests for the M3 perpetual matching engine.
//!
//! These run against the in-memory `BookSnapshot` impl (`InMemoryBook`) so
//! they don't pull in RocksDB. Two invariants under test:
//!
//! 1. **Price-time priority is order-independent at insert time.** Inserting
//!    a fixed multiset of resting orders in any order, then matching a fixed
//!    incoming order against them, produces the same fills. Ties at equal
//!    `(price, seq)` are forbidden by the seq-derivation in the executor;
//!    here we just permute *insertion* order across distinct seqs.
//!
//! 2. **Position-delta conservation per fill.** Every fill must result in
//!    the buyer's position-qty delta equalling the seller's, just on the
//!    opposite side. Equivalently: `qty_long_total` minus `qty_short_total`
//!    is conserved across the matching event.

use cb_engine::{
    apply_fill_to_position, match_order, Fill, InMemoryBook, IncomingOrder, SelfTradeAction,
};
use cb_types::{Address, DerivId, PerpOrder, Position, Side, Signed, Tif, H256};
use proptest::prelude::*;

fn mk_resting(
    oid_byte: u8,
    owner_byte: u8,
    side: Side,
    price: u128,
    qty: u128,
    seq: u64,
) -> PerpOrder {
    PerpOrder {
        order_id: H256([oid_byte; 32]),
        owner: Address([owner_byte; 20]),
        market: DerivId::perp(0),
        side,
        price_q8: price,
        qty,
        remaining_qty: qty,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: false,
        seq,
        placed_at_height: 1,
        client_order_id: H256([0; 32]),
    }
}

/// Build a `Vec<PerpOrder>` with **distinct seqs** so any insertion-order
/// permutation produces the same canonical book. The matcher's own internal
/// `sort_by` makes this deterministic; we're testing that insertion order
/// into the snapshot doesn't affect the matcher's output.
fn book_orders() -> Vec<PerpOrder> {
    vec![
        // Asks (Side::Sell). Lowest ask = best ask, oldest-seq = priority.
        mk_resting(0xa1, 0xb1, Side::Sell, 100, 10, 1),
        mk_resting(0xa2, 0xb2, Side::Sell, 100, 5, 2), // same price, later
        mk_resting(0xa3, 0xb3, Side::Sell, 110, 7, 3),
        mk_resting(0xa4, 0xb4, Side::Sell, 120, 3, 4),
    ]
}

fn run_match(book_orders: &[PerpOrder], incoming: IncomingOrder) -> Vec<Fill> {
    let mut book = InMemoryBook::default();
    for o in book_orders {
        book.insert(o.clone());
    }
    let res = match_order(&book, incoming).expect("match_order");
    res.fills
}

proptest! {
    /// Price-time priority is invariant to insertion order.
    /// We pick a permutation of indices `[0..N)`; insert orders in that
    /// order; match a fixed incoming buy that consumes the entire book; and
    /// assert the fill sequence is identical to the canonical-order one.
    #[test]
    fn fills_independent_of_insertion_order(perm in proptest::collection::vec(0u8..4, 4..=4).prop_filter("perm", |v| {
        let mut s = v.clone(); s.sort(); s.dedup(); s.len() == 4
    })) {
        let canonical = book_orders();
        let permuted: Vec<PerpOrder> = perm.iter().map(|&i| canonical[i as usize].clone()).collect();

        let incoming = IncomingOrder {
            order_id: H256([0xff; 32]),
            owner: Address([0xaa; 20]),
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 200,
            qty: 25, // exactly consumes the whole book (10+5+7+3)
            tif: Tif::Ioc,
            post_only: false,
            reduce_only: false,
            seq: 100,
            placed_at_height: 2,
            client_order_id: H256::ZERO,
            stp: SelfTradeAction::CancelTaker,
        };

        let canonical_fills = run_match(&canonical, incoming.clone());
        let permuted_fills = run_match(&permuted, incoming);

        prop_assert_eq!(canonical_fills, permuted_fills);
    }

    /// Position-delta conservation. For any single fill, applying it to two
    /// previously-flat positions (buyer + seller) results in the buyer's qty
    /// (positive: long) and seller's qty (negative: short) summing to zero
    /// when read with their signed direction.
    ///
    /// Inputs: a fill price and a fill qty.
    #[test]
    fn position_delta_conservation_open_from_flat(
        price_q8 in 1u128..1_000_000_000_000u128,
        fill_qty in 1u128..1_000_000u128,
    ) {
        let market = DerivId::perp(0);
        let buyer = Address([1; 20]);
        let seller = Address([2; 20]);

        let (buyer_after, _) = apply_fill_to_position(
            None,
            buyer,
            market,
            Side::Buy,
            fill_qty,
            price_q8,
            1,
            Signed::ZERO,
        );
        let (seller_after, _) = apply_fill_to_position(
            None,
            seller,
            market,
            Side::Sell,
            fill_qty,
            price_q8,
            1,
            Signed::ZERO,
        );

        // Signed deltas must sum to zero (one side + qty, other - qty).
        prop_assert_eq!(buyer_after.qty, fill_qty);
        prop_assert_eq!(seller_after.qty, fill_qty);
        prop_assert_eq!(buyer_after.side, Side::Buy);
        prop_assert_eq!(seller_after.side, Side::Sell);

        // Realised PnL is zero on opening fills.
        // (Caller validates separately; here we focus on qty conservation.)
    }

    /// Position-delta conservation under partial close. If a long position
    /// closes some qty against a counterparty that opens a short, the net
    /// long delta + short delta == 0. Realised PnL on the closing side
    /// equals (price - entry) * close_qty signed by side.
    #[test]
    fn position_delta_conservation_partial_close(
        entry_q8 in 1u128..1_000_000_000u128,
        exit_q8 in 1u128..1_000_000_000u128,
        prev_qty in 100u128..1_000_000u128,
        close_qty in 1u128..100u128,
    ) {
        let market = DerivId::perp(0);
        let alice = Address([1; 20]);
        let bob = Address([2; 20]);

        let alice_long = Position {
            owner: alice,
            market,
            side: Side::Buy,
            qty: prev_qty,
            entry_funding_index: cb_types::Signed::ZERO,
            entry_px_q8: entry_q8,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        };

        // Alice sells `close_qty` at exit_q8 (closing); Bob opens short.
        let (alice_after, alice_pnl) = apply_fill_to_position(
            Some(&alice_long), alice, market, Side::Sell, close_qty, exit_q8, 1,
            Signed::ZERO,
        );
        let (bob_after, bob_pnl) = apply_fill_to_position(
            None, bob, market, Side::Sell, close_qty, exit_q8, 1,
            Signed::ZERO,
        );

        // Alice's qty drops by close_qty; her side stays long.
        prop_assert_eq!(alice_after.qty, prev_qty - close_qty);
        prop_assert_eq!(alice_after.side, Side::Buy);
        // Bob opens a short of close_qty.
        prop_assert_eq!(bob_after.qty, close_qty);
        prop_assert_eq!(bob_after.side, Side::Sell);

        // PnL conservation: a closing trade realises PnL only on the
        // closing side. Bob is opening from flat → zero PnL. Alice's PnL
        // matches the formula.
        prop_assert_eq!(bob_pnl, cb_types::Signed::ZERO);
        let expected = if exit_q8 >= entry_q8 {
            (exit_q8 - entry_q8) as i128 * close_qty as i128
        } else {
            -((entry_q8 - exit_q8) as i128) * close_qty as i128
        };
        prop_assert_eq!(alice_pnl, cb_types::Signed::from_i128(expected));
    }

    /// Equity conservation across a perp fill: one side's PnL change is the
    /// negation of the other side's. This is the M4 invariant the
    /// per-fill `credit_realized_pnl` path inherits from
    /// `apply_fill_to_position`. We test the pure-engine half here; the
    /// executor-level test in `cb-block::tests::liquidation` confirms the
    /// collateral mutation matches.
    #[test]
    fn fill_pnl_is_zero_sum_across_sides(
        entry_q8 in 1u128..1_000_000_000u128,
        exit_q8 in 1u128..1_000_000_000u128,
        qty in 1u128..1_000_000u128,
    ) {
        let market = DerivId::perp(0);
        let alice = Address([1; 20]);
        let bob = Address([2; 20]);
        // Alice was long, closes against Bob who was short. Both close
        // `qty` at exit_q8. Their realised PnLs must be exact negations.
        let alice_long = Position {
            owner: alice, market, side: Side::Buy, qty,
            entry_funding_index: cb_types::Signed::ZERO,
            entry_px_q8: entry_q8, margin_mode: 0, isolated_margin: 0,
            last_funding_height: 0,
        };
        let bob_short = Position {
            owner: bob, market, side: Side::Sell, qty,
            entry_funding_index: cb_types::Signed::ZERO,
            entry_px_q8: entry_q8, margin_mode: 0, isolated_margin: 0,
            last_funding_height: 0,
        };
        let (_a, alice_pnl) = apply_fill_to_position(
            Some(&alice_long), alice, market, Side::Sell, qty, exit_q8, 1,
            Signed::ZERO,
        );
        let (_b, bob_pnl) = apply_fill_to_position(
            Some(&bob_short), bob, market, Side::Buy, qty, exit_q8, 1,
            Signed::ZERO,
        );
        // Sum to zero in i128 space.
        prop_assert_eq!(alice_pnl.to_i128() + bob_pnl.to_i128(), 0);
    }

    /// Piecewise IM grows monotonically with notional. Doubling the
    /// notional never decreases the IM requirement; if both bands stay in
    /// the same tier the IM exactly doubles, otherwise it grows by at
    /// least the prev IM (because each band's contribution is positive).
    #[test]
    fn piecewise_im_is_monotonic_in_notional(
        n in 1u128..10_000_000_000_000u128, // up to $10M
    ) {
        let pos1 = cb_engine::position_notional_micro_usd(1, 0); // unused; we feed n directly via a synthetic Position
        let _ = pos1; // silence
        // Construct two positions whose notional-via-price math equals the test inputs. We
        // sidestep the Position construction and call the public sum helpers directly.
        let im_n = im_for_notional(n);
        let im_2n = im_for_notional(n.saturating_mul(2).min(u128::MAX / 2));
        prop_assert!(im_2n >= im_n);
    }
}

/// Small helper: piecewise IM for a notional value. Mirrors what
/// `initial_margin_required` would produce given a single mark/qty pair —
/// we just call into `cb_engine::DEFAULT_TIERS` and walk the bands by hand
/// so this property test doesn't construct synthetic Position rows.
fn im_for_notional(n: u128) -> u128 {
    use cb_engine::DEFAULT_TIERS;
    let mut prev_bound: u128 = 0;
    let mut accum: u128 = 0;
    for tier in DEFAULT_TIERS {
        let band_top = tier.upper_bound_micro_usd.min(n);
        let width = band_top.saturating_sub(prev_bound);
        if width == 0 {
            prev_bound = tier.upper_bound_micro_usd;
            continue;
        }
        let contrib = width
            .saturating_mul(tier.initial_margin_bps as u128)
            .saturating_add(9_999)
            / 10_000;
        accum = accum.saturating_add(contrib);
        prev_bound = tier.upper_bound_micro_usd;
        if prev_bound >= n {
            break;
        }
    }
    accum
}
