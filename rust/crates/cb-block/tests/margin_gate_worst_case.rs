//! The pre-trade margin gate, judged in the worst world an account's resting
//! orders can leave it in.
//!
//! The working tree's gate (`passes_initial_margin_worst_case`) judges an
//! incoming order in two worlds — it fills in full at its limit, or it rests
//! and never fills — and charges the requirement against everything the
//! account could come to hold. What it did not do was put the *other* resting
//! orders in their worst world too: each had been admitted with only its own
//! fill-at-limit loss in the equity, so N bids above the mark each passed
//! against the whole collateral, and one sell into all of them at once took
//! the account far below zero. That is a collusive drain on the insurance
//! fund — the counterparty keeps the gain, the fund eats the loss — and it is
//! fixed in `apply_place_perp_order` by debiting the resting orders' adverse
//! fill-at-limit loss from the equity both worlds are judged on
//! (`resting_adverse_loss_micro`).
//!
//! What is tested here:
//!
//! 1. The regression, as found — ten 0.02 BTC bids above a 50,000 mark, one
//!    sell into all of them — re-expressed inside the price band: the bids
//!    now sit at 55,000, the band's top edge, against $600 of collateral.
//! 2. **Exactness, as a property.** For random sequences of resting orders
//!    from a flat account at a fixed mark, an order is accepted *if and only
//!    if* the account's joint worst case — equity, less the adverse
//!    fill-at-limit loss of every resting order including the new one — covers
//!    the initial margin of its potential position (the larger side). Up to
//!    two micro-dollars of rounding either way. So the gate is neither leaky
//!    nor stricter than it says.
//! 3. **Consequence, as a property.** A counterparty then fills every order
//!    the account has resting, at the order's own limit, in one sweep: the
//!    account ends with equity covering the initial margin of the position it
//!    now holds, and no bad debt — the filled world the gate promised.
//! 4. The closing-order gap, now closed by the price band: a *closing* order
//!    skips the gate, and a solvent account could close at any price into a
//!    colluder. The dump at $1 is refused; what the band still allows is
//!    bounded and pinned alongside it.

mod support;

use cb_block::ExecError;
use cb_engine::{compute_equity, initial_margin_required, DEFAULT_TIERS};
use cb_state::{PerpOrders, Positions};
use cb_types::{Address, Position, Side, Signed, Tif};
use support::*;

const MARK: u128 = 50_000 * Q8;

fn gated_chain(collateral_usd: u128) -> (Chain, Actor, Actor) {
    let chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    chain.set_mark(PERP, MARK);
    chain.fund_margin(Address::INSURANCE, 1_000_000 * MICRO);
    let a = actor("gate-a");
    let sweeper = actor("gate-sweeper");
    chain.fund_margin(a.addr, collateral_usd * MICRO);
    chain.fund_margin(sweeper.addr, 1_000_000_000 * MICRO);
    (chain, a, sweeper)
}

/// The fill-at-limit loss of one resting order against `MARK`, rounded up; a
/// favourable limit counts zero. Written independently of the executor's.
fn adverse(side: Side, price: u128, qty: u128) -> u128 {
    let gap = match side {
        Side::Buy => price.saturating_sub(MARK),
        Side::Sell => MARK.saturating_sub(price),
    };
    (qty * gap).div_ceil(10_000_000_000)
}

/// (joint worst-case equity, initial margin of the potential position) for a
/// flat account holding `orders` resting, all non-reduce-only.
fn joint_worst(collateral: u128, orders: &[(Side, u128, u128)]) -> (i128, u128) {
    let loss: u128 = orders.iter().map(|(s, p, q)| adverse(*s, *p, *q)).sum();
    let bids: u128 = orders
        .iter()
        .filter(|o| o.0 == Side::Buy)
        .map(|o| o.2)
        .sum();
    let asks: u128 = orders
        .iter()
        .filter(|o| o.0 == Side::Sell)
        .map(|o| o.2)
        .sum();
    let pot = Position {
        owner: Address::ZERO,
        market: PERP,
        side: Side::Buy,
        qty: bids.max(asks),
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 0,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    };
    let im = initial_margin_required(&[pot], &[(PERP, MARK)], DEFAULT_TIERS);
    (collateral as i128 - loss as i128, im)
}

#[test]
fn ten_bids_above_the_mark_are_judged_together_not_one_at_a_time() {
    // As found the bids were at 75,000; the price band now refuses anything
    // above 55,000 against this mark, so they sit at the band's top edge.
    let (mut chain, a, sweeper) = gated_chain(600);
    let bid = || {
        perp_order(
            PERP,
            Side::Buy,
            55_000 * Q8,
            ONE_BTC / 50,
            Tif::Gtc,
            false,
            false,
        )
    };
    let placed = chain.block(&vec![(a.clone(), bid()); 10]);
    // Each bid costs $100 if it fills at its limit; $1,000 of notional each
    // at the mark needs $20 of IM per bid. Any one of them alone passes
    // ($600 against $120). $600 covers five together: after the fifth, $100
    // is left against $100 of IM, and a sixth would leave $0 against $120.
    let accepted = placed.iter().filter(|r| r.is_ok()).count();
    assert_eq!(accepted, 5, "{placed:?}");
    assert_eq!(placed[5], Err(ExecError::InsufficientMargin));

    // The sweep the gate has to survive: one sell into every bid.
    let r = chain.block(&[(
        sweeper,
        perp_order(
            PERP,
            Side::Sell,
            55_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            false,
        ),
    )]);
    assert_eq!(r, vec![Ok(())]);
    let pos = chain.position(a.addr, PERP).unwrap();
    assert_eq!(pos.qty, 5 * ONE_BTC / 50);
    let eq = compute_equity(
        &chain.margin(a.addr),
        std::slice::from_ref(&pos),
        &[(PERP, MARK)],
    )
    .to_i128();
    let im = initial_margin_required(&[pos], &[(PERP, MARK)], DEFAULT_TIERS);
    assert_eq!(eq, 100 * MICRO as i128, "600 less five $100 losses");
    assert!(eq as u128 >= im);
    assert_eq!(chain.margin(a.addr).bad_debt, 0);
    assert_eq!(
        chain.margin(Address::INSURANCE).collateral,
        1_000_000 * MICRO,
        "the fund paid nothing"
    );
}

#[test]
fn a_resting_order_is_accepted_exactly_when_the_joint_worst_case_covers_margin() {
    let mut accepted_total = 0;
    let mut refused_total = 0;
    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        let collateral = rng.range(200, 5_000);
        let (mut chain, a, sweeper) = gated_chain(collateral);
        let mut resting: Vec<(Side, u128, u128)> = Vec::new();
        for step in 0..30 {
            let side = rng.side();
            // Up to ten per cent either side of the mark — the whole price
            // band, edges included; the adverse half is the half that
            // matters.
            let off = rng.range(0, 1_000);
            let price = if rng.chance(50) {
                MARK * (10_000 + off) / 10_000
            } else {
                MARK * (10_000 - off) / 10_000
            };
            let qty = rng.range(ONE_BTC / 200, ONE_BTC / 5);
            let r = chain.block(&[(
                a.clone(),
                perp_order(PERP, side, price, qty, Tif::Gtc, false, false),
            )]);
            let mut with = resting.clone();
            with.push((side, price, qty));
            let (worst, im) = joint_worst(collateral * MICRO, &with);
            match &r[0] {
                Ok(()) => {
                    assert!(
                        worst + 2 >= im as i128,
                        "seed {seed} step {step}: accepted with joint worst {worst} against IM {im}"
                    );
                    resting = with;
                    accepted_total += 1;
                }
                Err(ExecError::InsufficientMargin) => {
                    assert!(
                        worst < im as i128 + 2,
                        "seed {seed} step {step}: refused although joint worst {worst} covers IM {im}"
                    );
                    refused_total += 1;
                }
                // Crossing one's own resting order is self-trade prevention.
                Err(ExecError::OrderRejected) => {}
                other => panic!("seed {seed} step {step}: {other:?}"),
            }
        }
        assert_eq!(
            PerpOrders::new(&chain.db).list_for_owner(&a.addr).len(),
            resting.len(),
            "seed {seed}: nothing filled, everything accepted rests"
        );

        // Property 3: sweep every resting order at its own limit.
        let bids: u128 = resting
            .iter()
            .filter(|o| o.0 == Side::Buy)
            .map(|o| o.2)
            .sum();
        let asks: u128 = resting
            .iter()
            .filter(|o| o.0 == Side::Sell)
            .map(|o| o.2)
            .sum();
        let mut sweep = Vec::new();
        if bids > 0 {
            sweep.push((
                sweeper.clone(),
                // The band's bottom edge: every bid is at or above it.
                perp_order(PERP, Side::Sell, 45_000 * Q8, bids, Tif::Ioc, false, false),
            ));
        }
        if asks > 0 {
            sweep.push((
                sweeper.clone(),
                // The band's top edge: every ask is at or below it.
                perp_order(PERP, Side::Buy, 55_000 * Q8, asks, Tif::Ioc, false, false),
            ));
        }
        let r = chain.block(&sweep);
        assert!(r.iter().all(|x| x.is_ok()), "seed {seed}: {r:?}");
        assert!(
            PerpOrders::new(&chain.db)
                .list_for_owner(&a.addr)
                .is_empty(),
            "seed {seed}: all filled"
        );
        let held: Vec<Position> = Positions::new(&chain.db).list_for_owner(&a.addr);
        let eq = compute_equity(&chain.margin(a.addr), &held, &[(PERP, MARK)]).to_i128();
        let im = initial_margin_required(&held, &[(PERP, MARK)], DEFAULT_TIERS);
        assert_eq!(chain.margin(a.addr).bad_debt, 0, "seed {seed}");
        assert!(
            eq + 2 * resting.len() as i128 >= im as i128,
            "seed {seed}: after every order filled at its limit, equity {eq} < IM {im}"
        );
    }
    assert!(
        accepted_total > 100 && refused_total > 20,
        "{accepted_total} accepted, {refused_total} refused"
    );
}

/// An IOC is judged in its filled world only — it cannot rest — but that
/// world still carries the account's resting orders at their worst, on every
/// market: a bid resting above the mark on one perpetual makes a marketable
/// order on another more expensive.
#[test]
fn an_immediate_order_still_pays_for_what_is_already_resting_elsewhere() {
    let other = cb_types::DerivId::perp(1);
    let whale = actor("gate-whale");
    let setup = || {
        let (chain, a, _) = gated_chain(1_000);
        let mut m = perp_market(other, chain.oracle.addr);
        m.funding_interval_blocks = 1_000_000;
        chain.list_perp(m);
        chain.set_mark(other, MARK);
        chain.fund_margin(whale.addr, 1_000_000_000 * MICRO);
        (chain, a)
    };
    // $1,000 of collateral: a 0.5 BTC IOC at the mark needs $500 of IM.
    let ioc = || perp_order(PERP, Side::Buy, MARK, ONE_BTC / 2, Tif::Ioc, false, false);
    let ask = || perp_order(PERP, Side::Sell, MARK, ONE_BTC, Tif::Gtc, false, false);
    let (mut chain, a) = setup();
    let r = chain.block(&[(whale.clone(), ask()), (a.clone(), ioc())]);
    assert_eq!(r, vec![Ok(()), Ok(())], "on its own the IOC is affordable");

    // With 0.16 BTC bid at 55,000 (the band's top edge) resting on the other
    // market — $800 lost if it fills — the same IOC no longer is:
    // 1,000 − 800 < 500 + 160.
    let (mut chain, a) = setup();
    let r = chain.block(&[
        (
            a.clone(),
            perp_order(
                other,
                Side::Buy,
                55_000 * Q8,
                ONE_BTC * 16 / 100,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (whale.clone(), ask()),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    let r = chain.block(&[(a.clone(), ioc())]);
    assert_eq!(r, vec![Err(ExecError::InsufficientMargin)]);
    assert!(chain.position(a.addr, PERP).is_none());
}

/// **The closing-order gap, closed by the price band.** Closing orders skip
/// the margin gate: a reduce-only order always (`apply_place_perp_order`
/// step 5, the `!reduce_only` guard), and a plain order that `closes_only`
/// too. And the gate itself passes any full close whatever its price, because
/// it lets negative equity through when the requirement is zero
/// (`passes_initial_margin_worst_case`: `if equity < 0 { return im_required
/// == 0 }`). Before the band, a *solvent* account could sell its whole long to
/// a colluder at $1, realise a loss far beyond its collateral, and the
/// insurance fund paid the difference to the colluder, who then held the long
/// at $1 against a $50,000 mark: $10,000 of collateral opened 5 BTC at 50,000
/// (IM $8,000), dumped it reduce-only at $1, and the fund paid $239,995.
///
/// Every derivative order is now priced within 10% of the mark
/// (`DERIVATIVE_PRICE_BAND_BPS`), reduce-only and closing orders included, so
/// both legs of the dump are refused and nothing moves.
#[test]
fn a_solvent_account_cannot_close_into_a_colluder_at_any_price() {
    let (mut chain, a, _) = gated_chain(10_000);
    let (colluder, whale) = (actor("gap-colluder"), actor("gap-whale"));
    chain.fund_margin(colluder.addr, 20_000 * MICRO);
    chain.fund_margin(whale.addr, 1_000_000_000 * MICRO);
    let r = chain.block(&[
        (
            whale,
            perp_order(PERP, Side::Sell, MARK, 10 * ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Buy, MARK, 5 * ONE_BTC, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    let (lo, hi) = cb_engine::derivative_price_band(MARK);
    let out_of_band = ExecError::PriceOutOfBand {
        price_q8: Q8,
        lo_q8: lo,
        hi_q8: hi,
    };
    let r = chain.block(&[
        (
            colluder.clone(),
            perp_order(PERP, Side::Buy, Q8, 5 * ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Sell, Q8, 5 * ONE_BTC, Tif::Ioc, false, true),
        ),
        // The same dump as a plain (not reduce-only) order, which the gate
        // also skips because it only closes.
        (
            a.clone(),
            perp_order(PERP, Side::Sell, Q8, 5 * ONE_BTC, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(
        r,
        vec![
            Err(out_of_band.clone()),
            Err(out_of_band.clone()),
            Err(out_of_band)
        ],
        "both legs of the dump are refused"
    );
    assert_eq!(chain.margin(a.addr).collateral, 10_000 * MICRO);
    assert_eq!(chain.margin(a.addr).bad_debt, 0);
    assert_eq!(
        chain.margin(Address::INSURANCE).collateral,
        1_000_000 * MICRO,
        "the fund paid nothing"
    );
    let held = chain.position(a.addr, PERP).unwrap();
    assert_eq!((held.side, held.qty), (Side::Buy, 5 * ONE_BTC));
    assert!(chain.position(colluder.addr, PERP).is_none());
}

/// What the band does *not* do, pinned so it is visible: it bounds a
/// collusive close, it does not zero it. The same account closes at the
/// band's bottom edge, 45,000, into the colluder: a $25,000 loss (10% of
/// $250,000 notional) against $10,000 of collateral, and the fund covers
/// $15,000 — against $239,995 at $1. The bound is (band − the account's
/// margin cushion) × notional; closing it entirely needs a rule on closing
/// orders themselves (refuse a close that realises more than the account's
/// equity at a worse-than-mark price), which is a separate policy decision.
#[test]
fn the_band_bounds_what_a_collusive_close_can_cost_the_fund() {
    let (mut chain, a, _) = gated_chain(10_000);
    let (colluder, whale) = (actor("gap-colluder"), actor("gap-whale"));
    chain.fund_margin(colluder.addr, 20_000 * MICRO);
    chain.fund_margin(whale.addr, 1_000_000_000 * MICRO);
    let r = chain.block(&[
        (
            whale,
            perp_order(PERP, Side::Sell, MARK, 10 * ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Buy, MARK, 5 * ONE_BTC, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    let edge = 45_000 * Q8;
    let r = chain.block(&[
        (
            colluder.clone(),
            perp_order(PERP, Side::Buy, edge, 5 * ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Sell, edge, 5 * ONE_BTC, Tif::Ioc, false, true),
        ),
    ]);
    assert_eq!(
        r,
        vec![Ok(()), Ok(())],
        "a close at the band's edge is legal"
    );
    assert_eq!(chain.margin(a.addr).collateral, 0);
    let paid = 1_000_000 * MICRO - chain.margin(Address::INSURANCE).collateral;
    assert_eq!(
        paid,
        15_000 * MICRO,
        "25,000 lost less the 10,000 behind it"
    );
    let notional = 5 * 50_000 * MICRO;
    assert!(
        paid <= notional / 10,
        "never more than the band's width of notional"
    );
}
