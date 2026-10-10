//! Property tests for the pure perpetual layer that the existing bundles
//! (`properties.rs`, `perps_invariants.rs`) leave open:
//!
//! 1. **Reduce-only is enforced by the matcher, on both sides of a fill.**
//!    Random books mixing ordinary and reduce-only makers, owned by accounts
//!    holding random positions, are hit by random takers (reduce-only or not,
//!    and — deliberately — with reduce-only quantities *larger* than the
//!    position, which admission would refuse but a Byzantine block could
//!    carry). After the fills are applied, no reduce-only participant's
//!    |position| has grown or flipped, positions net to the same total, every
//!    fill is inside both parties' limits, nobody trades with themselves, and
//!    the book left behind is not crossed.
//! 2. **Funding accrual pro-rates by the interval and rounds toward zero.**
//!    `accrue_index_delta` at the $100 par mark is exactly
//!    `rate * elapsed / interval` truncated, scales with the mark,
//!    is odd in the rate, and a whole interval accrued in `k` ticks loses at
//!    most `k - 1` index units to truncation — never gains.
//! 3. **The worst-case margin gate is monotone** in the ways its callers rely
//!    on: more collateral never turns a pass into a fail, more exposure or a
//!    worse fill never turns a fail into a pass, an order that cannot rest is
//!    never judged more harshly than one that can, and negative equity passes
//!    only a zero requirement.
//! 4. **`is_liquidatable` agrees with `compute_equity` and the maintenance
//!    requirement**, which is what lets the executor use one for the trigger
//!    re-check and the other in the gate without the two drifting.

use cb_engine::{
    accrue_index_delta, apply_fill_to_position, compute_equity, compute_liquidation_orders,
    initial_margin_required, is_liquidatable, maintenance_margin_required, match_order,
    passes_initial_margin_worst_case, BookSnapshot, InMemoryBook, IncomingOrder, MatchOutcome,
    SelfTradeAction, DEFAULT_TIERS, FUNDING_PRICE_UNIT_Q8 as PAR,
};
use cb_types::{Address, DerivId, MarginAccount, PerpOrder, Position, Side, Signed, Tif, H256};
use proptest::prelude::*;
use std::collections::BTreeMap;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const OWNERS: u8 = 5;

fn addr(b: u8) -> Address {
    Address([b + 1; 20])
}

fn opposite(s: Side) -> Side {
    match s {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

/// Signed size: long positive, short negative.
fn signed(side: Side, qty: u128) -> i128 {
    match side {
        Side::Buy => qty as i128,
        Side::Sell => -(qty as i128),
    }
}

#[derive(Clone, Debug)]
struct Maker {
    owner: u8,
    side: Side,
    price: u128,
    qty: u128,
    reduce_only: bool,
}

fn maker_strategy() -> impl Strategy<Value = Maker> {
    (
        0..OWNERS,
        prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        1u128..=10,
        1u128..=20,
        prop::bool::weighted(0.35),
    )
        .prop_map(|(owner, side, off, qty, reduce_only)| {
            // Bids at or below 100, asks at or above 101: the book the test
            // starts from is uncrossed, as a real one is.
            let price = match side {
                Side::Buy => 101 - off,
                Side::Sell => 100 + off,
            };
            Maker {
                owner,
                side,
                price,
                qty,
                reduce_only,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant 1 of the module header.
    #[test]
    fn reduce_only_never_grows_a_position_on_either_side_of_a_fill(
        makers in prop::collection::vec(maker_strategy(), 0..24),
        positions in prop::collection::vec((prop_oneof![Just(Side::Buy), Just(Side::Sell)], 0u128..40), OWNERS as usize),
        taker in 0..OWNERS,
        taker_side in prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        taker_price in 90u128..=111,
        taker_qty in 1u128..=80,
        taker_tif in prop_oneof![Just(Tif::Gtc), Just(Tif::Ioc), Just(Tif::Fok)],
        taker_ro in any::<bool>(),
    ) {
        let mut book = InMemoryBook::default();
        let mut pos: BTreeMap<u8, Position> = BTreeMap::new();
        for (b, (side, qty)) in positions.iter().enumerate() {
            let b = b as u8;
            book.set_position(addr(b), MARKET, *side, *qty);
            let (p, _) = apply_fill_to_position(None, addr(b), MARKET, *side, *qty, 100 * Q8, 1, Signed::ZERO);
            pos.insert(b, p);
        }
        for (i, m) in makers.iter().enumerate() {
            book.insert(PerpOrder {
                order_id: H256([i as u8 + 1; 32]),
                owner: addr(m.owner),
                market: MARKET,
                side: m.side,
                price_q8: m.price,
                qty: m.qty,
                remaining_qty: m.qty,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: m.reduce_only,
                seq: i as u64 + 1,
                placed_at_height: 1,
                client_order_id: H256::default(),
            });
        }
        let before = book.clone();
        let net_before: i128 = pos.values().map(|p| signed(p.side, p.qty)).sum();

        let incoming = IncomingOrder {
            order_id: H256([0xEE; 32]),
            owner: addr(taker),
            market: MARKET,
            side: taker_side,
            price_q8: taker_price,
            qty: taker_qty,
            tif: taker_tif,
            post_only: false,
            reduce_only: taker_ro,
            seq: 10_000,
            placed_at_height: 2,
            client_order_id: H256::default(),
            stp: SelfTradeAction::CancelTaker,
        };
        let res = match_order(&book, incoming.clone()).expect("valid order");
        book.apply_all(&res.mutations);

        // Fills: inside both limits, never self, never zero.
        for f in &res.fills {
            let maker = &before.orders[&f.maker_order_id];
            prop_assert_eq!(f.price_q8, maker.price_q8);
            prop_assert_ne!(f.maker, f.taker);
            prop_assert!(f.qty > 0 && f.qty <= maker.remaining_qty);
            match taker_side {
                Side::Buy => prop_assert!(f.price_q8 <= taker_price),
                Side::Sell => prop_assert!(f.price_q8 >= taker_price),
            }
        }
        let filled: u128 = res.fills.iter().map(|f| f.qty).sum();
        prop_assert_eq!(filled, res.filled_qty);
        if taker_tif == Tif::Fok && res.outcome != MatchOutcome::KilledFOK && res.outcome != MatchOutcome::RejectedSelfTrade {
            prop_assert_eq!(filled, taker_qty, "an FOK that was not killed filled in full");
        }

        // Apply the fills to positions in the matcher's plan order, checking
        // each reduce-only fill as it lands: the participant whose order is
        // reduce-only must come out of that fill no larger and on the same
        // side (or flat). The property is per fill, not per owner — an owner
        // with a plain order and a reduce-only order both filled can end up
        // flipped, legitimately, by the plain one.
        for f in &res.fills {
            let mk = &before.orders[&f.maker_order_id];
            let mb = (0..OWNERS).find(|b| addr(*b) == f.maker).unwrap();
            for (who, side, ro) in [(taker, taker_side, taker_ro), (mb, opposite(taker_side), mk.reduce_only)] {
                let p0 = pos[&who].clone();
                let (p1, _) = apply_fill_to_position(Some(&p0), addr(who), MARKET, side, f.qty, f.price_q8, 2, Signed::ZERO);
                if ro {
                    prop_assert!(p1.qty <= p0.qty && p0.qty > 0,
                        "owner {}'s reduce-only fill of {} grew |position| {} -> {}", who, f.qty, p0.qty, p1.qty);
                    if p1.qty > 0 {
                        prop_assert_eq!(p1.side, p0.side, "owner {}'s reduce-only fill flipped the position", who);
                    }
                }
                pos.insert(who, p1);
            }
        }
        let net_after: i128 = pos.values().map(|p| signed(p.side, p.qty)).sum();
        prop_assert_eq!(net_before, net_after, "fills move size between accounts, never create it");

        // The taker's reduce-only remainder never rests beyond what it could
        // still close, and the book left behind is not crossed.
        if taker_ro {
            if let Some(rest) = book.orders.get(&incoming.order_id) {
                let (side0, qty0) = positions[taker as usize];
                let cap = if side0 != taker_side { qty0 } else { 0 };
                prop_assert!(rest.remaining_qty <= cap.saturating_sub(filled));
            }
        }
        let bid = book.list_side(&MARKET, Side::Buy).iter().map(|(p, _, _)| *p).max();
        let ask = book.list_side(&MARKET, Side::Sell).iter().map(|(p, _, _)| *p).min();
        if let (Some(b), Some(a)) = (bid, ask) {
            prop_assert!(b < a, "book crossed after the match: bid {} ask {}", b, a);
        }
    }

    /// Invariant 2: the accrual formula, exactly.
    #[test]
    fn funding_accrual_is_rate_times_elapsed_over_interval_truncated(
        start in -1_000_000_000_000i128..1_000_000_000_000,
        rate in -5_000_000i64..=5_000_000,
        elapsed in 0u64..100_000,
        interval in 0u64..10_000,
    ) {
        let got = accrue_index_delta(Signed::from_i128(start), rate, PAR, elapsed, interval).to_i128();
        let i = interval.max(1) as i128;
        // Rust's `/` on i128 truncates toward zero, which is the documented
        // rounding; written out so a change to either side shows up.
        let delta = (rate as i128 * elapsed as i128) / i;
        prop_assert_eq!(got, start + delta);
        // Odd in the rate: longs and shorts see mirror-image indices.
        let neg = accrue_index_delta(Signed::ZERO, -rate, PAR, elapsed, interval).to_i128();
        let pos = accrue_index_delta(Signed::ZERO, rate, PAR, elapsed, interval).to_i128();
        prop_assert_eq!(neg, -pos);
        // Never more than one full rate per interval elapsed (rounded up).
        prop_assert!(pos.abs() <= (rate as i128).abs() * (elapsed as i128 + i - 1) / i);
    }

    /// Invariant 2, the price half: the index is funding owed per contract
    /// unit, so it is the par accrual scaled by mark / par, truncated — and
    /// a position of `qty` settles `notional * rate` to within truncation.
    #[test]
    fn funding_accrual_scales_with_the_mark(
        rate in -5_000_000i64..=5_000_000,
        mark in 1u128..100_000_000_000_000,
        elapsed in 0u64..100_000,
        interval in 1u64..10_000,
    ) {
        let got = accrue_index_delta(Signed::ZERO, rate, mark, elapsed, interval).to_i128();
        let want = rate as i128 * mark as i128 * elapsed as i128 / (interval as i128 * PAR as i128);
        prop_assert_eq!(got, want);
        // One BTC-sized position: qty * Δindex / 1e9 against notional * rate
        // computed the long way, notional = qty * mark / 1e10 micro-USD.
        let qty: i128 = 100_000_000;
        let paid = qty * got / 1_000_000_000;
        let exact = qty * mark as i128 * rate as i128 * elapsed as i128
            / (10_000_000_000i128 * 1_000_000_000 * interval as i128);
        prop_assert!((paid - exact).abs() <= 1 + qty / 1_000_000_000, "paid {} exact {}", paid, exact);
    }

    /// Invariant 2, the tick-splitting half: a whole interval accrued in `k`
    /// ticks of arbitrary sizes reaches the one-shot value less at most `k-1`
    /// index units of truncation, and always on the side of zero.
    #[test]
    fn accrual_split_into_ticks_only_loses_truncation_dust(
        rate in -5_000_000i64..=5_000_000,
        cuts in prop::collection::vec(1u64..400, 1..12),
        interval in 1u64..3_600,
    ) {
        let total: u64 = cuts.iter().sum();
        let mut idx = Signed::ZERO;
        for c in &cuts {
            idx = accrue_index_delta(idx, rate, PAR, *c, interval);
        }
        let stepped = idx.to_i128();
        let oneshot = accrue_index_delta(Signed::ZERO, rate, PAR, total, interval).to_i128();
        let dust = (oneshot - stepped).abs();
        prop_assert!(dust < cuts.len() as i128, "dust {} over {} ticks", dust, cuts.len());
        prop_assert!(stepped.abs() <= oneshot.abs(), "truncation must never add funding");
        prop_assert!(stepped == 0 || stepped.signum() == (rate as i128).signum());
    }

    /// Invariant 3.
    #[test]
    fn the_worst_case_gate_is_monotone(
        collateral in 0u128..20_000,
        bump in 1u128..5_000,
        held_side in prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        held_qty in 0u128..3 * Q8,
        entry in 40_000u128..60_000,
        mark in 40_000u128..60_000,
        side in prop_oneof![Just(Side::Buy), Just(Side::Sell)],
        qty in 1u128..2 * Q8,
        limit in 40_000u128..60_000,
        worse in 0u128..5_000,
    ) {
        let owner = addr(0);
        let account = |c: u128| MarginAccount { owner, collateral: c * MICRO, unrealized_pnl: Signed::ZERO, bad_debt: 0 };
        let held: Vec<Position> = if held_qty == 0 { vec![] } else {
            vec![apply_fill_to_position(None, owner, MARKET, held_side, held_qty, entry * Q8, 1, Signed::ZERO).0]
        };
        let marks = [(MARKET, mark * Q8)];
        // Filled world at a given limit, and the PnL that fill realises.
        let filled_at = |px: u128| -> (Vec<Position>, i128) {
            let (p, pnl) = apply_fill_to_position(held.first(), owner, MARKET, side, qty, px * Q8, 2, Signed::ZERO);
            (vec![p], pnl.to_i128() / 10i128.pow(10))
        };
        let exposure_for = |extra: u128| -> Vec<Position> {
            let signed_held: i128 = held.first().map(|p| signed(p.side, p.qty)).unwrap_or(0);
            let pot = (signed_held + signed(side, qty + extra)).unsigned_abs();
            vec![Position { qty: pot.max(held_qty), ..apply_fill_to_position(None, owner, MARKET, side, 1, 0, 0, Signed::ZERO).0 }]
        };
        let gate = |c: u128, unfilled: Option<&[Position]>, px: u128, exposure: &[Position]| {
            let (f, pnl) = filled_at(px);
            passes_initial_margin_worst_case(&account(c), unfilled, &f, pnl, exposure, &marks, &marks, DEFAULT_TIERS)
        };
        let exp = exposure_for(0);
        let base = gate(collateral, Some(&held), limit, &exp);

        // More collateral never hurts.
        if base {
            prop_assert!(gate(collateral + bump, Some(&held), limit, &exp));
        }
        // More exposure never helps.
        if !base {
            prop_assert!(!gate(collateral, Some(&held), limit, &exposure_for(bump * 1_000)));
        }
        // A worse fill price never helps: higher for a buy, lower for a sell.
        let worse_px = match side { Side::Buy => limit + worse, Side::Sell => limit.saturating_sub(worse).max(1) };
        if !base {
            prop_assert!(!gate(collateral, Some(&held), worse_px, &exp));
        }
        // An order that cannot rest (no unfilled world) is never judged more
        // harshly than the same order resting.
        if base {
            prop_assert!(gate(collateral, None, limit, &exp));
        }
        // Negative equity passes exactly when nothing is required: an account
        // one micro-dollar in debt may close, and may do nothing else.
        let debtor = MarginAccount { owner, collateral: 0, unrealized_pnl: Signed::ZERO, bad_debt: 1 };
        let required = initial_margin_required(&exp, &marks, DEFAULT_TIERS);
        prop_assert_eq!(
            passes_initial_margin_worst_case(&debtor, Some(&[]), &[], 0, &exp, &marks, &marks, DEFAULT_TIERS),
            required == 0
        );
        prop_assert!(passes_initial_margin_worst_case(&debtor, Some(&[]), &[], 0, &[], &marks, &marks, DEFAULT_TIERS));
    }

    /// Invariant 4, and the liquidation ladder's price bounds.
    #[test]
    fn liquidatable_is_exactly_equity_below_maintenance(
        collateral in 0u128..50_000,
        bad_debt in prop_oneof![3 => Just(0u128), 1 => 0u128..5_000],
        legs in prop::collection::vec((prop_oneof![Just(Side::Buy), Just(Side::Sell)], 1u128..5 * Q8, 30_000u128..70_000, 0u16..2), 0..4),
        marks_px in (30_000u128..70_000, 30_000u128..70_000),
    ) {
        let owner = addr(0);
        let acct = MarginAccount { owner, collateral: collateral * MICRO, unrealized_pnl: Signed::ZERO, bad_debt: bad_debt * MICRO };
        let positions: Vec<Position> = legs.iter().map(|(s, q, e, m)| Position {
            market: DerivId::perp(*m),
            ..apply_fill_to_position(None, owner, DerivId::perp(*m), *s, *q, e * Q8, 1, Signed::ZERO).0
        }).collect();
        let marks = [(DerivId::perp(0), marks_px.0 * Q8), (DerivId::perp(1), marks_px.1 * Q8)];
        let mm = maintenance_margin_required(&positions, &marks, DEFAULT_TIERS);
        let equity = compute_equity(&acct, &positions, &marks).to_i128();
        let expected = mm > 0 && equity < mm as i128;
        prop_assert_eq!(is_liquidatable(&acct, &positions, &marks, DEFAULT_TIERS), expected);

        // The ladder closes the whole position, on the closing side, at
        // prices that only ever move against the victim and never to zero.
        for p in &positions {
            let mark = marks.iter().find(|(m, _)| *m == p.market).unwrap().1;
            let ladder = compute_liquidation_orders(p, mark, p.market);
            prop_assert!(!ladder.is_empty());
            let mut last = mark;
            for t in &ladder {
                prop_assert_eq!(t.side, opposite(p.side));
                prop_assert_eq!(t.qty, p.qty);
                prop_assert!(t.price_q8 > 0);
                match t.side {
                    Side::Sell => { prop_assert!(t.price_q8 <= last); }
                    Side::Buy => { prop_assert!(t.price_q8 >= last); }
                }
                last = t.price_q8;
            }
        }
    }
}

/// The counterexample the reduce-only property found, pinned. An owner long 1
/// rests a plain ask of 4 and, behind it at the same price, a reduce-only ask
/// of 20. A buy of 5 takes the plain ask first (long 1 → short 3) and then
/// reached the reduce-only ask with the *pre-match* capacity of 1 still
/// credited to it, filling it and taking the owner to short 4: a reduce-only
/// order that opened size. The matcher now charges the owner's earlier fills
/// in the same match against the reduce-only budget, so the reduce-only ask
/// fills nothing and — since all it could ever do from here is open — is
/// taken off the book.
#[test]
fn a_plain_fill_ahead_of_a_reduce_only_maker_uses_up_its_budget() {
    let owner = addr(1);
    let mut book = InMemoryBook::default();
    book.set_position(owner, MARKET, Side::Buy, 1);
    let mk = |id: u8, qty: u128, ro: bool, seq: u64| PerpOrder {
        order_id: H256([id; 32]),
        owner,
        market: MARKET,
        side: Side::Sell,
        price_q8: 101,
        qty,
        remaining_qty: qty,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: ro,
        seq,
        placed_at_height: 1,
        client_order_id: H256::default(),
    };
    book.insert(mk(1, 4, false, 1));
    book.insert(mk(2, 20, true, 2));
    let res = match_order(
        &book,
        IncomingOrder {
            order_id: H256([9; 32]),
            owner: addr(0),
            market: MARKET,
            side: Side::Buy,
            price_q8: 101,
            qty: 5,
            tif: Tif::Ioc,
            post_only: false,
            reduce_only: false,
            seq: 3,
            placed_at_height: 2,
            client_order_id: H256::default(),
            stp: SelfTradeAction::CancelTaker,
        },
    )
    .unwrap();
    let fills: Vec<(u8, u128)> = res
        .fills
        .iter()
        .map(|f| (f.maker_order_id.0[0], f.qty))
        .collect();
    assert_eq!(fills, vec![(1, 4)], "only the plain ask fills");
    book.apply_all(&res.mutations);
    assert!(
        !book.orders.contains_key(&H256([2; 32])),
        "a reduce-only ask facing a short can only open; it leaves the book"
    );
}
