//! Regression tests for the five findings audited at commit 9c2ab02.
//! Run: cargo test -p cb-block --test audit_20260929 -- --test-threads=1

mod support;

use cb_block::{ExecError, MAX_MARK_AGE_BLOCKS};
use cb_engine::compute_equity;
use cb_state::PerpOrders;
use cb_types::{Address, DerivId, Side, Tif};
use support::*;

fn setup(collateral: u128) -> (Chain, Actor, Actor) {
    let c = Chain::new();
    let mut m = perp_market(PERP, c.oracle.addr);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    m.funding_interval_blocks = 1_000_000;
    c.list_perp(m);
    c.set_mark(PERP, 50_000 * Q8);
    let a = actor("audit-a");
    let b = actor("audit-b");
    c.fund_margin(a.addr, collateral * MICRO);
    c.fund_margin(b.addr, 100_000 * MICRO);
    c.fund_margin(Address::INSURANCE, 1_000_000 * MICRO);
    (c, a, b)
}

fn order(m: DerivId, side: Side, dollars: u128, tif: Tif, ro: bool) -> cb_tx::TxBody {
    perp_order(m, side, dollars * Q8, ONE_BTC, tif, false, ro)
}

#[test]
fn solvent_hedged_account_repays_its_losing_leg_before_receiving_gains() {
    hedge_unwind(false);
    hedge_unwind(true);
}

fn hedge_unwind(winner_first: bool) {
    let (mut c, a, b) = setup(2_200);
    let future = DerivId::future(0);
    let mut m = future_market(future, c.oracle.addr, 1_000_000);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    c.list_future(m);
    c.set_mark(future, 50_000 * Q8);
    assert!(c
        .block(&[
            (b.clone(), order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
            (b.clone(), order(future, Side::Buy, 50_000, Tif::Gtc, false)),
            (
                a.clone(),
                order(future, Side::Sell, 50_000, Tif::Ioc, false)
            ),
        ])
        .iter()
        .all(Result::is_ok));
    // Ordinary market movement: the hedge has zero net PnL.
    c.set_mark(PERP, 47_000 * Q8);
    c.set_mark(future, 47_000 * Q8);
    let positions = vec![
        c.position(a.addr, PERP).unwrap(),
        c.position(a.addr, future).unwrap(),
    ];
    assert_eq!(
        compute_equity(
            &c.margin(a.addr),
            &positions,
            &[(PERP, 47_000 * Q8), (future, 47_000 * Q8)]
        )
        .to_i128(),
        2_200 * MICRO as i128
    );
    let losing_leg = [
        (b.clone(), order(PERP, Side::Buy, 47_000, Tif::Gtc, true)),
        (a.clone(), order(PERP, Side::Sell, 47_000, Tif::Ioc, true)),
    ];
    let winning_leg = [
        (b.clone(), order(future, Side::Sell, 47_000, Tif::Gtc, true)),
        (a.clone(), order(future, Side::Buy, 47_000, Tif::Ioc, true)),
    ];
    let (first, second) = if winner_first {
        (&winning_leg, &losing_leg)
    } else {
        (&losing_leg, &winning_leg)
    };
    assert!(c.block(first).iter().all(Result::is_ok));
    if !winner_first {
        assert_eq!(c.margin(a.addr).bad_debt, 800 * MICRO);
        assert_eq!(c.margin(Address::INSURANCE).collateral, 1_000_000 * MICRO);
    }
    assert!(c.block(second).iter().all(Result::is_ok));
    assert!(c.position(a.addr, PERP).is_none());
    assert!(c.position(a.addr, future).is_none());
    assert_eq!(c.margin(a.addr).collateral, 2_200 * MICRO);
    assert_eq!(c.margin(b.addr).collateral, 100_000 * MICRO);
    assert_eq!(c.margin(Address::INSURANCE).collateral, 1_000_000 * MICRO);
    assert_eq!(c.margin(a.addr).bad_debt, 0);
}

#[test]
fn a_zero_pnl_final_close_settles_retained_cash_debt() {
    let (mut c, a, b) = setup(1_000);
    assert!(c
        .block(&[
            (b.clone(), order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
        ])
        .iter()
        .all(Result::is_ok));
    // Model cash debt retained from an earlier funding payment while open.
    let mut margin = c.margin(a.addr);
    margin.collateral = 0;
    margin.bad_debt = 400 * MICRO;
    cb_state::MarginAccounts::new(&c.db).put(&margin);
    assert!(c
        .block(&[
            (b, order(PERP, Side::Buy, 50_000, Tif::Gtc, true)),
            (a.clone(), order(PERP, Side::Sell, 50_000, Tif::Ioc, true)),
        ])
        .iter()
        .all(Result::is_ok));
    assert!(c.position(a.addr, PERP).is_none());
    assert_eq!(c.margin(a.addr).collateral, 0);
    assert_eq!(c.margin(a.addr).bad_debt, 0);
    assert_eq!(c.margin(Address::INSURANCE).collateral, 999_600 * MICRO);
}

#[test]
fn unfunded_former_closing_maker_is_cancelled_instead_of_reopening() {
    let (mut c, a, b) = setup(1_000);
    assert!(c
        .block(&[
            (b.clone(), order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
        ])
        .iter()
        .all(Result::is_ok));
    c.set_mark(PERP, 49_010 * Q8);
    assert!(c
        .block(&[
            (a.clone(), order(PERP, Side::Sell, 49_011, Tif::Gtc, false)),
            (b.clone(), order(PERP, Side::Buy, 49_010, Tif::Gtc, true)),
            (a.clone(), order(PERP, Side::Sell, 49_010, Tif::Ioc, true)),
        ])
        .iter()
        .all(Result::is_ok));
    assert!(c.position(a.addr, PERP).is_none());
    assert_eq!(c.margin(a.addr).collateral, 10 * MICRO);
    assert_eq!(PerpOrders::new(&c.db).list_for_owner(&a.addr).len(), 1);
    let buyer = actor("audit-buyer");
    c.fund_margin(buyer.addr, 100_000 * MICRO);
    assert_eq!(
        c.block(&[(buyer, order(PERP, Side::Buy, 49_011, Tif::Ioc, false))]),
        vec![Ok(())]
    );
    assert!(c.position(a.addr, PERP).is_none());
    assert_eq!(c.margin(a.addr).collateral, 10 * MICRO);
    assert!(PerpOrders::new(&c.db).list_for_owner(&a.addr).is_empty());
}

#[test]
fn configured_position_cap_rejects_excess_exposure() {
    let (mut c, a, _) = setup(1_000);
    let mut m = perp_market(PERP, c.oracle.addr);
    m.initial_margin_bps = 10_000;
    m.maintenance_margin_bps = 5_000;
    m.max_leverage_x100 = 100;
    m.max_position_notional = 1_000 * MICRO;
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    c.list_perp(m);
    assert_eq!(
        c.block(&[(a.clone(), order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
        vec![Err(ExecError::PositionLimit)]
    );
    assert!(c.position(a.addr, PERP).is_none());
}

#[test]
fn stale_mark_cannot_authorize_new_exposure() {
    let (mut c, a, b) = setup(1_000);
    c.height = 1_000_000;
    assert_eq!(
        c.block(&[
            (b, order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
        ]),
        vec![
            Err(ExecError::StaleMarkPrice),
            Err(ExecError::StaleMarkPrice)
        ]
    );
    assert!(c.position(a.addr, PERP).is_none());
}

#[test]
fn initial_margin_and_leverage_floors_are_enforced_independently() {
    for (im, leverage) in [(10_000, 5_000), (200, 100)] {
        let (mut c, a, _) = setup(1_000);
        let mut m = perp_market(PERP, c.oracle.addr);
        m.initial_margin_bps = im;
        m.max_leverage_x100 = leverage;
        c.list_perp(m);
        assert_eq!(
            c.block(&[(a.clone(), order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
            vec![Err(ExecError::InsufficientMargin)]
        );
        c.fund_margin(a.addr, 50_000 * MICRO);
        assert_eq!(
            c.block(&[(a, order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
            vec![Ok(())]
        );
    }
}

#[test]
fn configured_maintenance_margin_controls_liquidation() {
    let (mut c, a, b) = setup(1_000);
    assert!(c
        .block(&[
            (b.clone(), order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
        ])
        .iter()
        .all(Result::is_ok));
    let trigger = cb_tx::TxBody::LiquidationTrigger {
        owner: a.addr,
        market: PERP,
    };
    assert_eq!(
        c.block(&[(b.clone(), trigger.clone())]),
        vec![Err(ExecError::NotLiquidatable)]
    );
    let mut m = perp_market(PERP, c.oracle.addr);
    m.initial_margin_bps = 1_000;
    m.maintenance_margin_bps = 500;
    c.list_perp(m);
    // No closing liquidity; success here proves eligibility, not a fill.
    assert_eq!(c.block(&[(b, trigger)]), vec![Ok(())]);
}

#[test]
fn fresh_mark_boundary_and_cancellation_during_an_outage() {
    let (mut c, a, _) = setup(2_000);
    c.height = MAX_MARK_AGE_BLOCKS - 1;
    assert_eq!(
        c.block(&[(a.clone(), order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
        vec![Ok(())]
    );
    let oid = PerpOrders::new(&c.db).list_for_owner(&a.addr)[0].order_id;
    assert_eq!(
        c.block(&[(a.clone(), order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
        vec![Err(ExecError::StaleMarkPrice)]
    );
    assert_eq!(
        c.block(&[(a, cb_tx::TxBody::CancelPerpOrder { order_id: oid })]),
        vec![Ok(())]
    );
}

#[test]
fn recomputing_a_mark_does_not_refresh_old_oracle_input() {
    let (mut c, a, _) = setup(2_000);
    assert_eq!(
        c.block(&[(c.oracle.clone(), oracle(PERP, 50_000 * Q8, 1))]),
        vec![Ok(())]
    );
    c.height = MAX_MARK_AGE_BLOCKS + 1;
    c.set_mark(PERP, 50_000 * Q8);
    assert_eq!(
        c.block(&[(a.clone(), order(PERP, Side::Buy, 50_000, Tif::Gtc, false))]),
        vec![Err(ExecError::StaleMarkPrice)]
    );
    // A source height newer than the previous submission is still too old.
    assert_eq!(
        c.block(&[(c.oracle.clone(), oracle(PERP, 50_000 * Q8, 2))]),
        vec![Err(ExecError::OracleStale)]
    );
    let height = c.height + 1;
    assert_eq!(
        c.block(&[
            (c.oracle.clone(), oracle(PERP, 50_000 * Q8, height)),
            (a, order(PERP, Side::Buy, 50_000, Tif::Gtc, false)),
        ]),
        vec![Ok(()), Ok(())]
    );
}

#[test]
fn stale_marks_cannot_liquidate_an_account() {
    let (mut c, a, b) = setup(1_000);
    assert!(c
        .block(&[
            (b.clone(), order(PERP, Side::Sell, 50_000, Tif::Gtc, false)),
            (a.clone(), order(PERP, Side::Buy, 50_000, Tif::Ioc, false)),
        ])
        .iter()
        .all(Result::is_ok));
    c.set_mark(PERP, 45_000 * Q8);
    c.height += MAX_MARK_AGE_BLOCKS;
    assert_eq!(
        c.block(&[(
            b,
            cb_tx::TxBody::LiquidationTrigger {
                owner: a.addr,
                market: PERP
            }
        )]),
        vec![Err(ExecError::StaleMarkPrice)]
    );
    assert_eq!(c.position(a.addr, PERP).unwrap().qty, ONE_BTC);
}
