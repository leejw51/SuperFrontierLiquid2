//! Lot and tick admission at the boundaries, for spot, perpetuals and dated
//! futures.
//!
//! `perp_negative.rs::place_perp_off_lot_or_off_tick_rejects` covers the
//! ordinary off-lot and off-tick perp order; `spot_executor.rs` the ordinary
//! spot ones. This file is about the edges, and about one asymmetry that is
//! deliberate and easy to "fix" by accident, so it is pinned:
//!
//! * On a **spot** market a zero `min_qty` or a zero `tick_size` refuses
//!   *every* order (a zero divisor is a broken market row, and the executor
//!   must refuse rather than divide by it — the working tree's fix for
//!   `min_qty`, mirroring the long-standing one for `tick_size`).
//! * On a **derivative** market a zero `lot_size` or `tick_size` means *no
//!   increment*: every quantity and price is on it.
//!
//! Beyond that: exactly one lot and one tick are accepted and one unit either
//! side refused; multiples are accepted; `u128::MAX` increments and
//! quantities go through admission and fail cleanly on money, never by
//! panicking in the arithmetic behind them; and the order of the checks is
//! pinned (zero shape, then lot, then tick, then the margin account), because
//! it decides which error a client sees and must be the same on every node.

mod support;

use cb_block::ExecError;
use cb_tx::TxBody;
use cb_types::{AssetId, DerivId, Side, Tif};
use support::*;

const FUT: DerivId = DerivId::future(0);

fn spot_chain(tick: u128, min_qty: u128) -> (Chain, Actor) {
    let chain = Chain::new();
    chain.list_spot(spot_market(tick, min_qty, 10));
    let t = actor("lt-spot");
    chain.fund_spot(t.addr, AssetId::Usd, 10u128.pow(30));
    chain.fund_spot(t.addr, AssetId::Btc, 10u128.pow(30));
    (chain, t)
}

fn deriv_chain(market: DerivId, tick: u128, lot: u128) -> (Chain, Actor) {
    let chain = Chain::new();
    match market.kind().unwrap() {
        cb_types::MarketKind::Perp => {
            let mut m = perp_market(market, chain.oracle.addr);
            m.tick_size = tick;
            m.lot_size = lot;
            chain.list_perp(m);
        }
        cb_types::MarketKind::Future => {
            let mut m = future_market(market, chain.oracle.addr, 1_000_000);
            m.tick_size = tick;
            m.lot_size = lot;
            chain.list_future(m);
        }
    }
    chain.set_mark(market, 50_000 * Q8);
    let t = actor("lt-deriv");
    chain.fund_margin(t.addr, 1_000_000_000 * MICRO);
    (chain, t)
}

/// A resting, non-crossing order: bids far below anything, so admission is
/// the only thing under test.
fn bid(market: DerivId, price: u128, qty: u128) -> TxBody {
    perp_order(market, Side::Buy, price, qty, Tif::Gtc, false, false)
}

#[test]
fn spot_zero_increments_refuse_everything() {
    let (mut chain, t) = spot_chain(1, 0);
    let r = chain.block(&[
        (t.clone(), spot_order(Side::Buy, Q8, 1, Tif::Gtc, false)),
        (
            t.clone(),
            spot_order(Side::Sell, Q8, 1_000, Tif::Gtc, false),
        ),
    ]);
    assert_eq!(
        r,
        vec![Err(ExecError::BadLot), Err(ExecError::BadLot)],
        "min_qty 0"
    );
    let (mut chain, t) = spot_chain(0, 1);
    let r = chain.block(&[
        (t.clone(), spot_order(Side::Buy, Q8, 1, Tif::Gtc, false)),
        (t.clone(), spot_order(Side::Sell, 7, 1, Tif::Gtc, false)),
    ]);
    assert_eq!(
        r,
        vec![Err(ExecError::BadTick), Err(ExecError::BadTick)],
        "tick 0"
    );
    // Zero price is reported as such, ahead of the tick.
    let r = chain.block(&[(t.clone(), spot_order(Side::Buy, 0, 1, Tif::Gtc, false))]);
    assert_eq!(r, vec![Err(ExecError::ZeroPrice)]);
}

#[test]
fn spot_one_increment_either_side() {
    let lot = 1_000u128;
    let tick = 250u128;
    let (mut chain, t) = spot_chain(tick, lot);
    let px = 4 * Q8 / tick * tick; // on tick
    let r = chain.block(&[
        (t.clone(), spot_order(Side::Sell, px, lot, Tif::Gtc, false)),
        (
            t.clone(),
            spot_order(Side::Sell, px, lot - 1, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, px, lot + 1, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, px, 7 * lot, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, px + 1, lot, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, px - 1, lot, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, px + tick, lot, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Sell, tick, lot, Tif::Gtc, false),
        ),
        // Off lot and off tick: the lot is reported.
        (
            t.clone(),
            spot_order(Side::Sell, px + 1, lot + 1, Tif::Gtc, false),
        ),
    ]);
    assert_eq!(
        r,
        vec![
            Ok(()),
            Err(ExecError::BadLot),
            Err(ExecError::BadLot),
            Ok(()),
            Err(ExecError::BadTick),
            Err(ExecError::BadTick),
            Ok(()),
            Ok(()),
            Err(ExecError::BadLot),
        ]
    );
}

#[test]
fn spot_huge_increments_fail_on_money_not_arithmetic() {
    let (mut chain, t) = spot_chain(u128::MAX, u128::MAX);
    let r = chain.block(&[
        // On lot and on tick; the base lock is u128::MAX, which nobody has.
        (
            t.clone(),
            spot_order(Side::Sell, u128::MAX, u128::MAX, Tif::Gtc, false),
        ),
        // The buy-side notional saturates rather than wrapping.
        (
            t.clone(),
            spot_order(Side::Buy, u128::MAX, u128::MAX, Tif::Gtc, false),
        ),
        (
            t.clone(),
            spot_order(Side::Buy, u128::MAX, 1, Tif::Gtc, false),
        ),
    ]);
    assert_eq!(
        r,
        vec![
            Err(ExecError::Insufficient),
            Err(ExecError::Insufficient),
            Err(ExecError::BadLot)
        ]
    );
}

#[test]
fn derivative_zero_increments_constrain_nothing() {
    for market in [PERP, FUT] {
        let (mut chain, t) = deriv_chain(market, 0, 0);
        let r = chain.block(&[
            // Any price inside the band around the 50,000 mark, to the q8
            // unit; the band (`price_band.rs`) is the only price constraint.
            (t.clone(), bid(market, 45_000 * Q8 + 1, 1)),
            (t.clone(), bid(market, 50_000 * Q8 + 12_345, 7)),
            (t.clone(), bid(market, 54_999 * Q8 + 3, 999_999_999)),
        ]);
        assert_eq!(r, vec![Ok(()), Ok(()), Ok(())], "{market}");
    }
}

#[test]
fn derivative_one_increment_either_side() {
    for market in [PERP, FUT] {
        let lot = ONE_BTC / 1_000;
        let tick = 50 * Q8 / 100;
        let (mut chain, t) = deriv_chain(market, tick, lot);
        let px = 46_000 * Q8;
        assert_eq!(px % tick, 0);
        let r = chain.block(&[
            (t.clone(), bid(market, px, lot)),
            (t.clone(), bid(market, px, lot - 1)),
            (t.clone(), bid(market, px, lot + 1)),
            (t.clone(), bid(market, px, 13 * lot)),
            (t.clone(), bid(market, px + 1, lot)),
            (t.clone(), bid(market, px - 1, lot)),
            (t.clone(), bid(market, px - tick, lot)),
            (t.clone(), bid(market, 45_000 * Q8, lot)),
            (t.clone(), bid(market, px + 1, lot + 1)),
            (t.clone(), bid(market, 0, lot + 1)),
            (t.clone(), bid(market, px + 1, 0)),
        ]);
        assert_eq!(
            r,
            vec![
                Ok(()),
                Err(ExecError::BadLot),
                Err(ExecError::BadLot),
                Ok(()),
                Err(ExecError::BadTick),
                Err(ExecError::BadTick),
                Ok(()),
                Ok(()),
                Err(ExecError::BadLot),
                Err(ExecError::BadPerpOrder),
                Err(ExecError::BadPerpOrder),
            ],
            "{market}"
        );
    }
}

#[test]
fn derivative_huge_values_fail_on_margin_not_arithmetic() {
    for market in [PERP, FUT] {
        let (mut chain, t) = deriv_chain(market, u128::MAX, u128::MAX);
        let r = chain.block(&[
            (t.clone(), bid(market, u128::MAX, u128::MAX)),
            (t.clone(), bid(market, u128::MAX, 1)),
            (t.clone(), bid(market, 1, u128::MAX)),
        ]);
        // On-lot and on-tick at u128::MAX, and a price that far from the
        // mark is outside the band — refused there, before any arithmetic.
        assert!(
            matches!(r[0], Err(ExecError::PriceOutOfBand { price_q8, .. }) if price_q8 == u128::MAX),
            "{market}: {r:?}"
        );
        assert_eq!(
            r[1..],
            [Err(ExecError::BadLot), Err(ExecError::BadTick)],
            "{market}"
        );
        // No increments at all, and the largest quantity there is at the
        // band's top edge: the notional saturates and the gate refuses the
        // bid. The mirror-image IOC sell is judged in its filled world only,
        // where selling 10% above the mark is a (saturated) gain — correct
        // for an order that can only fill at its limit or better — so it is
        // admitted, finds no bid that high, and does nothing. A price of
        // u128::MAX no longer reaches the gate at all.
        let (mut chain, t) = deriv_chain(market, 0, 0);
        let r = chain.block(&[
            (t.clone(), bid(market, 55_000 * Q8, u128::MAX)),
            (
                t.clone(),
                perp_order(
                    market,
                    Side::Sell,
                    55_000 * Q8,
                    u128::MAX,
                    Tif::Ioc,
                    false,
                    false,
                ),
            ),
            (t.clone(), bid(market, u128::MAX, u128::MAX)),
        ]);
        assert_eq!(r[0], Err(ExecError::InsufficientMargin), "{market}");
        assert_eq!(r[1], Ok(()), "{market}");
        assert!(
            matches!(r[2], Err(ExecError::PriceOutOfBand { .. })),
            "{market}: {r:?}"
        );
        assert!(chain.position(t.addr, market).is_none());
        assert!(cb_state::PerpOrders::new(&chain.db)
            .list_for_owner(&t.addr)
            .is_empty());
    }
}

/// Lot and tick come before the margin account and before the market's own
/// admission state is consulted any further: an account with no margin row
/// sending an off-lot order is told about the lot. The price band comes after
/// lot and tick and before the margin account. A paused market is reported
/// ahead of all of them.
#[test]
fn the_order_of_the_admission_checks_is_fixed() {
    let (mut chain, _) = deriv_chain(PERP, 100, 1_000);
    let nobody = actor("lt-no-margin");
    let px = 50_000 * Q8;
    let r = chain.block(&[
        (nobody.clone(), bid(PERP, 100, 1_001)),
        (nobody.clone(), bid(PERP, 101, 1_000)),
        (nobody.clone(), bid(PERP, 100, 1_000)),
        (nobody.clone(), bid(PERP, px, 1_000)),
    ]);
    assert_eq!(r[..2], [Err(ExecError::BadLot), Err(ExecError::BadTick)]);
    assert!(
        matches!(r[2], Err(ExecError::PriceOutOfBand { .. })),
        "{r:?}"
    );
    assert_eq!(r[3], Err(ExecError::NoMarginAccount));
    let mut m = chain.perp(PERP);
    m.paused = true;
    chain.list_perp(m);
    let r = chain.block(&[(nobody, bid(PERP, 101, 1_001))]);
    assert_eq!(r, vec![Err(ExecError::MarketPaused)]);
}

/// Every position a market's fills produce stays on the lot: each fill is the
/// smaller of two quantities that each passed the lot check. Checked over a
/// random crossing workload on a 0.001 BTC lot.
#[test]
fn positions_stay_on_the_lot_through_random_fills() {
    let lot = ONE_BTC / 1_000;
    let (mut chain, _) = deriv_chain(PERP, Q8, lot);
    let traders: Vec<Actor> = (0..4).map(|i| actor(&format!("lt-rand-{i}"))).collect();
    for t in &traders {
        chain.fund_margin(t.addr, 10_000_000 * MICRO);
    }
    let mut rng = Rng::new(99);
    for _ in 0..30 {
        let mut txs = Vec::new();
        for _ in 0..10 {
            let t = traders[rng.below(4) as usize].clone();
            let px = (49_900 + rng.range(0, 200)) * Q8;
            let qty = rng.range(1, 500) * lot;
            let tif = if rng.chance(30) { Tif::Ioc } else { Tif::Gtc };
            txs.push((
                t,
                perp_order(PERP, rng.side(), px, qty, tif, false, rng.chance(15)),
            ));
        }
        chain.block(&txs);
        for p in chain.all_positions() {
            assert_eq!(p.qty % lot, 0, "{p:?} off lot");
        }
        for o in cb_state::PerpOrders::new(&chain.db).list_for_market(&PERP) {
            assert_eq!(o.remaining_qty % lot, 0, "{o:?} off lot");
            assert_eq!(o.price_q8 % Q8, 0, "{o:?} off tick");
        }
    }
    assert!(!chain.all_positions().is_empty(), "the workload must trade");
}
