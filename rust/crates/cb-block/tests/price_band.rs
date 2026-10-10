//! The derivative price band: every derivative order is priced, and every
//! derivative fill executes, within `DERIVATIVE_PRICE_BAND_BPS` (10%) of the
//! mark.
//!
//! The rule, in two halves:
//!
//! * **Admission.** `PlacePerpOrder` — perpetual or dated future, plain,
//!   reduce-only or closing — is refused with `PriceOutOfBand` unless
//!   `lo ≤ price ≤ hi`, where `lo = ceil(mark · 0.9)` and
//!   `hi = floor(mark · 1.1)`. The edges are accepted. A market with no mark is
//!   refused with `MissingMarkPrice`. `ClosePosition` clamps its slippage
//!   bound into the band rather than being refused for a wide one.
//! * **Fills.** A resting order whose price has left the band because the mark
//!   moved is cancelled, without a fill, the moment an incoming order reaches
//!   it; the incoming order carries on to the next maker. A fill is always at
//!   the maker's price, so no derivative fill ever executes outside the band
//!   around the mark at the time of the fill. The liquidation ladder
//!   (≤ 200 bps off the mark) is inside the band and needs no exemption; it is
//!   subject to the same fill rule.
//!
//! Why: closing orders skip the margin gate, so without a band a solvent
//! account could close into a colluder at $1 and leave the loss to the
//! insurance fund (`margin_gate_worst_case.rs`).

mod support;

use cb_block::ExecError;
use cb_state::PerpOrders;
use cb_tx::TxBody;
use cb_types::{Address, DerivId, Side, Tif};
use support::*;

const MARK: u128 = 50_000 * Q8;
const LO: u128 = 45_000 * Q8;
const HI: u128 = 55_000 * Q8;

fn banded(tick: u128) -> (Chain, Actor, Actor) {
    let chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    m.tick_size = tick;
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    chain.set_mark(PERP, MARK);
    chain.fund_margin(Address::INSURANCE, 1_000_000 * MICRO);
    let (a, b) = (actor("band-a"), actor("band-b"));
    chain.fund_margin(a.addr, 1_000_000 * MICRO);
    chain.fund_margin(b.addr, 1_000_000 * MICRO);
    (chain, a, b)
}

fn gtc(market: DerivId, side: Side, px: u128) -> TxBody {
    perp_order(market, side, px, ONE_BTC / 100, Tif::Gtc, false, false)
}

fn oob(price_q8: u128, lo_q8: u128, hi_q8: u128) -> Result<(), ExecError> {
    Err(ExecError::PriceOutOfBand {
        price_q8,
        lo_q8,
        hi_q8,
    })
}

#[test]
fn the_band_edges_are_accepted_and_one_tick_past_is_refused_on_both_sides() {
    assert_eq!(cb_engine::derivative_price_band(MARK), (LO, HI));
    // Tick 1 (one q8 unit) and a half-dollar tick: "one tick past" is the
    // smallest step the market allows.
    for tick in [1, Q8 / 2] {
        for side in [Side::Buy, Side::Sell] {
            let (mut chain, a, _) = banded(tick);
            let r = chain.block(&[
                (a.clone(), gtc(PERP, side, LO)),
                (a.clone(), gtc(PERP, side, HI)),
                (a.clone(), gtc(PERP, side, LO - tick)),
                (a.clone(), gtc(PERP, side, HI + tick)),
            ]);
            assert_eq!(
                r,
                vec![
                    Ok(()),
                    Ok(()),
                    oob(LO - tick, LO, HI),
                    oob(HI + tick, LO, HI)
                ],
                "tick {tick} {side:?}"
            );
            assert_eq!(
                PerpOrders::new(&chain.db).list_for_owner(&a.addr).len(),
                2,
                "tick {tick} {side:?}: both edge orders rest"
            );
        }
    }
}

#[test]
fn the_band_applies_to_dated_futures_too() {
    let fut = DerivId::future(0);
    let chain = Chain::new();
    chain.list_future(future_market(fut, chain.oracle.addr, 1_000_000));
    let a = actor("band-fut");
    chain.fund_margin(a.addr, 1_000_000 * MICRO);
    let mut chain = chain;
    // No mark yet: refused before the band is even computed.
    assert_eq!(
        chain.block(&[(a.clone(), gtc(fut, Side::Buy, MARK))]),
        vec![Err(ExecError::MissingMarkPrice)]
    );
    chain.set_mark(fut, MARK);
    let r = chain.block(&[
        (a.clone(), gtc(fut, Side::Buy, LO)),
        (a.clone(), gtc(fut, Side::Buy, LO - 1)),
        (a.clone(), gtc(fut, Side::Sell, HI + 1)),
    ]);
    assert_eq!(r, vec![Ok(()), oob(LO - 1, LO, HI), oob(HI + 1, LO, HI)]);
}

#[test]
fn reduce_only_and_closing_orders_are_banded() {
    let (mut chain, a, b) = banded(1);
    // a goes long 1 BTC at the mark.
    let r = chain.block(&[
        (
            b.clone(),
            perp_order(PERP, Side::Sell, MARK, ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Buy, MARK, ONE_BTC, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    let r = chain.block(&[
        // Reduce-only, one unit under the band.
        (
            a.clone(),
            perp_order(PERP, Side::Sell, LO - 1, ONE_BTC, Tif::Gtc, false, true),
        ),
        // A plain order that only closes (the margin gate skips it).
        (
            a.clone(),
            perp_order(PERP, Side::Sell, LO - 1, ONE_BTC, Tif::Ioc, false, false),
        ),
        // Reduce-only at the edge: accepted, rests.
        (
            a.clone(),
            perp_order(PERP, Side::Sell, LO, ONE_BTC, Tif::Gtc, false, true),
        ),
    ]);
    assert_eq!(r, vec![oob(LO - 1, LO, HI), oob(LO - 1, LO, HI), Ok(())]);
    assert_eq!(chain.position(a.addr, PERP).unwrap().qty, ONE_BTC);
}

#[test]
fn close_position_clamps_its_slippage_bound_into_the_band() {
    let close = |bps| TxBody::ClosePosition {
        market: PERP,
        qty: 0,
        max_slippage_bps: bps,
    };
    // Long 1 BTC; a bid rests at the band's bottom edge.
    let (mut chain, a, b) = banded(1);
    let r = chain.block(&[
        (
            b.clone(),
            perp_order(PERP, Side::Sell, MARK, ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Buy, MARK, ONE_BTC, Tif::Ioc, false, false),
        ),
        (
            b.clone(),
            perp_order(PERP, Side::Buy, LO, ONE_BTC, Tif::Gtc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(()), Ok(())]);
    // 50% slippage would sell down to 25,000; it is clamped to 45,000, which
    // is exactly where the bid is, and the close fills there.
    assert_eq!(chain.block(&[(a.clone(), close(5_000))]), vec![Ok(())]);
    assert!(chain.position(a.addr, PERP).is_none());
    assert_eq!(
        chain.margin(a.addr).collateral,
        1_000_000 * MICRO - 5_000 * MICRO
    );

    // The mark then rises to 52,000, so the band is [46,800, 57,200]. A bid
    // resting at 46,000 (in band when placed) is now below it. A full-
    // slippage close of a long clamps to 46,800 and does not reach it.
    let (mut chain, a, b) = banded(1);
    let r = chain.block(&[
        (
            b.clone(),
            perp_order(PERP, Side::Sell, MARK, ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            a.clone(),
            perp_order(PERP, Side::Buy, MARK, ONE_BTC, Tif::Ioc, false, false),
        ),
        (
            b.clone(),
            perp_order(
                PERP,
                Side::Buy,
                46_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(()), Ok(())]);
    chain.set_mark(PERP, 52_000 * Q8);
    assert_eq!(chain.block(&[(a.clone(), close(10_000))]), vec![Ok(())]);
    assert_eq!(
        chain.position(a.addr, PERP).unwrap().qty,
        ONE_BTC,
        "nothing bid inside the band; the close does nothing"
    );
}

#[test]
fn a_resting_order_the_mark_has_left_behind_is_cancelled_not_filled() {
    let (mut chain, a, b) = banded(1);
    let c = actor("band-c");
    chain.fund_margin(c.addr, 1_000_000 * MICRO);
    // Two bids, both in band against the 50,000 mark.
    let r = chain.block(&[
        (
            a.clone(),
            perp_order(
                PERP,
                Side::Buy,
                54_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            c.clone(),
            perp_order(
                PERP,
                Side::Buy,
                49_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    // The mark falls to 45,000: the band is [40,500, 49,500], and a's bid at
    // 54,000 is above it. A sell at 45,000 reaches a's bid first (best
    // price), cancels it without a fill, and fills c's bid at 49,000.
    chain.set_mark(PERP, 45_000 * Q8);
    let r = chain.block(&[(
        b.clone(),
        perp_order(
            PERP,
            Side::Sell,
            45_000 * Q8,
            2 * ONE_BTC,
            Tif::Ioc,
            false,
            false,
        ),
    )]);
    assert_eq!(r, vec![Ok(())]);
    assert!(
        chain.position(a.addr, PERP).is_none(),
        "a did not buy at 54,000"
    );
    assert!(
        PerpOrders::new(&chain.db)
            .list_for_owner(&a.addr)
            .is_empty(),
        "a's stale bid left the book"
    );
    let cp = chain.position(c.addr, PERP).unwrap();
    assert_eq!(
        (cp.side, cp.qty, cp.entry_px_q8),
        (Side::Buy, ONE_BTC, 49_000 * Q8)
    );
    let bp = chain.position(b.addr, PERP).unwrap();
    assert_eq!(
        (bp.side, bp.qty),
        (Side::Sell, ONE_BTC),
        "only the in-band bid filled"
    );
    assert_eq!(chain.perp(PERP).last_trade_px_q8, 49_000 * Q8);

    // The same on the ask side: an ask at 46,000 placed against a 50,000 mark
    // is below the band once the mark reaches 52,000, and a buy passes it by.
    let (mut chain, a, b) = banded(1);
    assert_eq!(
        chain.block(&[(
            a.clone(),
            perp_order(
                PERP,
                Side::Sell,
                46_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false
            ),
        )]),
        vec![Ok(())]
    );
    chain.set_mark(PERP, 52_000 * Q8);
    let r = chain.block(&[(
        b.clone(),
        perp_order(
            PERP,
            Side::Buy,
            52_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            false,
        ),
    )]);
    assert_eq!(r, vec![Ok(())]);
    assert!(chain.position(a.addr, PERP).is_none());
    assert!(chain.position(b.addr, PERP).is_none());
    assert!(PerpOrders::new(&chain.db)
        .list_for_owner(&a.addr)
        .is_empty());
    assert_eq!(
        PerpOrders::new(&chain.db).list_for_owner(&b.addr).len(),
        1,
        "b's bid rests; the book is not left crossed"
    );
}

/// A liquidation needs no exemption: its tranches are priced at most 200 bps
/// off the mark. It is subject to the same fill rule, so a stale maker is
/// cancelled even when it is on the side that would have helped the account
/// being closed out.
#[test]
fn liquidation_works_inside_the_band_and_skips_a_stale_maker() {
    let (mut chain, victim, whale) = banded(1);
    let (stale, fresh, keeper) = (
        actor("band-stale"),
        actor("band-fresh"),
        actor("band-keeper"),
    );
    chain.fund_margin(victim.addr, 1_500 * MICRO);
    for x in [&stale, &fresh] {
        chain.fund_margin(x.addr, 1_000_000 * MICRO);
    }
    chain.fund_margin(keeper.addr, 0);
    let r = chain.block(&[
        (
            whale.clone(),
            perp_order(PERP, Side::Sell, MARK, ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            victim.clone(),
            perp_order(PERP, Side::Buy, MARK, ONE_BTC, Tif::Ioc, false, false),
        ),
        (
            stale.clone(),
            perp_order(
                PERP,
                Side::Buy,
                54_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            fresh.clone(),
            perp_order(
                PERP,
                Side::Buy,
                48_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(()), Ok(()), Ok(())]);
    // Mark 48,700: equity 1,500 − 1,300 = 200 < MM 487. Band [43,830,
    // 53,570]: the 54,000 bid is stale. The first tranche sells at
    // 48,456.5 and reaches the stale bid first.
    chain.set_mark(PERP, 48_700 * Q8);
    let r = chain.block(&[(
        keeper.clone(),
        TxBody::LiquidationTrigger {
            owner: victim.addr,
            market: PERP,
        },
    )]);
    assert_eq!(r, vec![Ok(())]);
    assert!(chain.position(victim.addr, PERP).is_none(), "closed out");
    assert!(
        chain.position(stale.addr, PERP).is_none(),
        "stale bid not filled"
    );
    assert!(PerpOrders::new(&chain.db)
        .list_for_owner(&stale.addr)
        .is_empty());
    let fp = chain.position(fresh.addr, PERP).unwrap();
    assert_eq!((fp.qty, fp.entry_px_q8), (ONE_BTC, 48_500 * Q8));
}
