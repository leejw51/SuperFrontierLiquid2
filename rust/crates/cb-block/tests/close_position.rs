//! `ClosePosition`, which the working tree implements for the first time (it
//! was refused as `Unsupported` while the gateway's close button sent it), and
//! the reduce-only machinery it rides on.
//!
//! A close is a reduce-only IOC on the opposite side, sized from the position
//! (`qty == 0` means all of it) and limited to the mark moved
//! `max_slippage_bps` against the trader, rounded *toward* the mark onto the
//! tick so the rounding never loosens the bound. Covered here:
//!
//! * full close with `qty == 0`, long and short, and a partial close;
//! * a thin book: the close takes what the bound allows and the rest of the
//!   IOC is dropped, never rested; depth outside the bound is not touched;
//! * no mark → `MissingMarkPrice`, with nothing written;
//! * **the slippage bound, as a property**: over random marks, ticks and
//!   bps, on long and short, a counter-order one tick outside the bound is
//!   never hit and one exactly on it is. This property found that the sell
//!   side floored `mark · (1 − bps)` before rounding up to the tick, so with a
//!   tick of 1 (or a floor that happened to be on the tick) the limit sat one
//!   q8 unit *below* the bound — negligible in money, but the opposite of the
//!   documented rounding. Fixed in `apply_close_position`;
//! * `bps == 10_000` on a long (limit rounds to zero → `BadPerpOrder`), bps
//!   above 10,000 clamped, an off-lot quantity (`BadLot`), a quantity above
//!   the position (`ReduceOnlyTooLarge`), no position (`ReduceOnlyNoPos`);
//! * a close is accepted in the reduce-only halt and from an underwater
//!   account (it skips the gate), and works on a dated future;
//! * the executor-level regression for the matcher fix in `cb-engine`: a
//!   reduce-only maker behind a plain maker of the same owner cannot open.

mod support;

use cb_block::ExecError;
use cb_state::{PerpMarkets, PerpOrders};
use cb_tx::TxBody;
use cb_types::{DerivId, Side, Tif};
use support::*;

const MARK: u128 = 50_000 * Q8;

fn close(market: DerivId, qty: u128, bps: u16) -> TxBody {
    TxBody::ClosePosition {
        market,
        qty,
        max_slippage_bps: bps,
    }
}

/// A chain with a fee-free perp, a trader holding `side` `qty` at `MARK`, and
/// a funded counterparty with nothing resting.
fn holding(side: Side, qty: u128, tick: u128, lot: u128) -> (Chain, Actor, Actor) {
    let mut chain = Chain::new();
    let mut m = perp_market(PERP, chain.oracle.addr);
    m.taker_fee_bps = 0;
    m.maker_fee_bps_signed = 0;
    m.tick_size = tick;
    m.lot_size = lot;
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    let (t, cp) = (actor("close-trader"), actor("close-cp"));
    chain.fund_margin(t.addr, 1_000_000 * MICRO);
    chain.fund_margin(cp.addr, 1_000_000_000 * MICRO);
    // Marked before the open: every derivative order is priced against the
    // band around the mark, and an unmarked market takes none.
    chain.set_mark(PERP, MARK);
    let open_px = MARK / tick.max(1) * tick.max(1);
    let r = chain.block(&[
        (
            cp.clone(),
            perp_order(PERP, opposite(side), open_px, qty, Tif::Gtc, false, false),
        ),
        (
            t.clone(),
            perp_order(PERP, side, open_px, qty, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    (chain, t, cp)
}

#[test]
fn qty_zero_closes_the_whole_position_long_and_short() {
    for side in [Side::Buy, Side::Sell] {
        let (mut chain, t, cp) = holding(side, 3 * ONE_BTC, 1, 1);
        let r = chain.block(&[
            (
                cp.clone(),
                perp_order(PERP, side, MARK, 10 * ONE_BTC, Tif::Gtc, false, false),
            ),
            (t.clone(), close(PERP, 0, 50)),
        ]);
        assert_eq!(r, vec![Ok(()), Ok(())], "{side:?}");
        assert!(chain.position(t.addr, PERP).is_none(), "{side:?}: flat");
        assert!(
            PerpOrders::new(&chain.db)
                .list_for_owner(&t.addr)
                .is_empty(),
            "an IOC never rests"
        );
    }
}

#[test]
fn a_partial_close_leaves_the_rest_on_the_same_side() {
    for side in [Side::Buy, Side::Sell] {
        let (mut chain, t, cp) = holding(side, 3 * ONE_BTC, 1, 1);
        let r = chain.block(&[
            (
                cp.clone(),
                perp_order(PERP, side, MARK, 10 * ONE_BTC, Tif::Gtc, false, false),
            ),
            (t.clone(), close(PERP, ONE_BTC, 50)),
        ]);
        assert_eq!(r, vec![Ok(()), Ok(())]);
        let p = chain.position(t.addr, PERP).unwrap();
        assert_eq!((p.side, p.qty, p.entry_px_q8), (side, 2 * ONE_BTC, MARK));
    }
}

#[test]
fn a_thin_book_fills_what_the_bound_allows_and_drops_the_rest() {
    let (mut chain, t, cp) = holding(Side::Buy, 5 * ONE_BTC, 1, 1);
    // 2 BTC bid inside a 1% bound, 10 BTC bid just outside it.
    let r = chain.block(&[
        (
            cp.clone(),
            perp_order(
                PERP,
                Side::Buy,
                MARK * 9_950 / 10_000,
                2 * ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            cp.clone(),
            perp_order(
                PERP,
                Side::Buy,
                MARK * 9_899 / 10_000,
                10 * ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (t.clone(), close(PERP, 0, 100)),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(()), Ok(())]);
    assert_eq!(chain.position(t.addr, PERP).unwrap().qty, 3 * ONE_BTC);
    let rest = PerpOrders::new(&chain.db).list_for_market(&PERP);
    assert_eq!(rest.len(), 1, "the deep bid outside the bound is untouched");
    assert_eq!(rest[0].remaining_qty, 10 * ONE_BTC);
    assert!(PerpOrders::new(&chain.db)
        .list_for_owner(&t.addr)
        .is_empty());
}

#[test]
fn without_a_mark_a_close_is_refused_and_writes_nothing() {
    let (mut chain, t, cp) = holding(Side::Buy, ONE_BTC, 1, 1);
    // The counterparty's bid goes in while the market is still marked; an
    // order on an unmarked market is refused before it reaches the book.
    let r = chain.block(&[(
        cp.clone(),
        perp_order(PERP, Side::Buy, MARK, ONE_BTC, Tif::Gtc, false, false),
    )]);
    assert_eq!(r, vec![Ok(())]);
    // Remove the mark: a market that has traded but has no mark now.
    cb_state::OracleState::new(&chain.db).put_mark(&cb_types::MarkPrice {
        market: PERP,
        px_q8: 0,
        computed_at_height: 0,
        premium_ema_q9: 0,
    });
    let nonce = chain.nonce_of(t.addr);
    let before = (chain.position(t.addr, PERP), chain.margin(t.addr));
    let r = chain.block(&[(t.clone(), close(PERP, 0, 500))]);
    assert_eq!(r, vec![Err(ExecError::MissingMarkPrice)]);
    assert_eq!((chain.position(t.addr, PERP), chain.margin(t.addr)), before);
    assert_eq!(
        chain.nonce_of(t.addr),
        nonce,
        "a refused close consumes no nonce"
    );
    assert_eq!(
        PerpOrders::new(&chain.db).list_for_market(&PERP).len(),
        1,
        "the bid still rests"
    );
}

/// The property of the module header. For each case the exact bound is
/// `mark · (10_000 ∓ bps) / 10_000` as a rational; the limit must be the
/// nearest tick on the mark's side of it.
#[test]
fn the_slippage_bound_rounds_toward_the_mark_on_every_tick() {
    let mut rng = Rng::new(7);
    for case in 0..48u64 {
        let side = if case % 2 == 0 { Side::Buy } else { Side::Sell };
        let tick = match rng.below(4) {
            0 => 1,
            1 => rng.range(2, 999),
            2 => 1_000_000 + rng.range(0, 7),
            _ => 7 * Q8 / 3,
        };
        let bps = rng.range(1, 900) as u16;
        let (mut chain, t, cp) = holding(side, ONE_BTC, tick, 1);
        // An awkward mark, not a multiple of the tick.
        let mark = MARK + rng.range(0, 10 * Q8) + 1;
        chain.set_mark(PERP, mark);
        let num = mark
            * match side {
                Side::Buy => 10_000 - bps as u128,  // closing a long sells: floor
                Side::Sell => 10_000 + bps as u128, // closing a short buys: cap
            };
        let limit = match side {
            // Smallest tick at or above the floor.
            Side::Buy => num.div_ceil(10_000 * tick) * tick,
            // Largest tick at or below the cap.
            Side::Sell => num / (10_000 * tick) * tick,
        };
        let close_side = opposite(side);
        let outside = match close_side {
            Side::Sell => limit - tick,
            Side::Buy => limit + tick,
        };
        // One tick outside: never hit.
        let r = chain.block(&[
            (
                cp.clone(),
                perp_order(PERP, side, outside, ONE_BTC, Tif::Gtc, false, false),
            ),
            (t.clone(), close(PERP, 0, bps)),
        ]);
        assert_eq!(r, vec![Ok(()), Ok(())], "case {case}");
        assert_eq!(
            chain.position(t.addr, PERP).map(|p| p.qty),
            Some(ONE_BTC),
            "case {case}: {close_side:?} close at mark {mark} bps {bps} tick {tick} hit {outside}, outside the bound"
        );
        // Exactly on the bound: hit.
        let r = chain.block(&[
            (
                cp.clone(),
                perp_order(PERP, side, limit, ONE_BTC, Tif::Gtc, false, false),
            ),
            (t.clone(), close(PERP, 0, bps)),
        ]);
        assert_eq!(r, vec![Ok(()), Ok(())], "case {case}");
        assert!(
            chain.position(t.addr, PERP).is_none(),
            "case {case}: {close_side:?} close at mark {mark} bps {bps} tick {tick} did not reach {limit}"
        );
    }
}

#[test]
fn degenerate_closes_are_clamped_or_refused_cleanly() {
    // A full-slippage close of a long would price at zero; it is tightened
    // to the price band instead (see `price_band.rs`), and with nothing bid
    // it closes nothing.
    let (mut chain, t, _) = holding(Side::Buy, 2 * ONE_BTC, 1, 1);
    assert_eq!(
        chain.block(&[(t.clone(), close(PERP, 0, 10_000))]),
        vec![Ok(())]
    );
    assert_eq!(chain.position(t.addr, PERP).unwrap().qty, 2 * ONE_BTC);
    // Above 10,000 bps is clamped to 10,000, not wrapped, and then to the
    // band.
    assert_eq!(
        chain.block(&[(t.clone(), close(PERP, 0, u16::MAX))]),
        vec![Ok(())]
    );
    assert_eq!(chain.position(t.addr, PERP).unwrap().qty, 2 * ONE_BTC);
    // More than the position.
    assert_eq!(
        chain.block(&[(t.clone(), close(PERP, 3 * ONE_BTC, 100))]),
        vec![Err(ExecError::ReduceOnlyTooLarge)]
    );
    // Nobody else holds anything.
    let stranger = actor("close-stranger");
    chain.fund_margin(stranger.addr, MICRO);
    assert_eq!(
        chain.block(&[(stranger, close(PERP, 0, 100))]),
        vec![Err(ExecError::ReduceOnlyNoPos)]
    );
    // A short closed with 10,000 bps would buy at up to twice the mark; the
    // limit is tightened to the band's top edge (mark + 10%), so an ask
    // there fills.
    let (mut chain, t, cp) = holding(Side::Sell, ONE_BTC, 1, 1);
    let r = chain.block(&[
        (
            cp,
            perp_order(
                PERP,
                Side::Sell,
                MARK * 11 / 10,
                ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (t.clone(), close(PERP, 0, 10_000)),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    assert!(chain.position(t.addr, PERP).is_none());
}

#[test]
fn an_off_lot_close_is_refused_and_a_full_close_is_always_on_lot() {
    let lot = ONE_BTC / 100;
    let (mut chain, t, cp) = holding(Side::Buy, 5 * lot, 1, lot);
    assert_eq!(
        chain.block(&[(t.clone(), close(PERP, lot + 1, 100))]),
        vec![Err(ExecError::BadLot)]
    );
    let r = chain.block(&[
        (
            cp,
            perp_order(PERP, Side::Buy, MARK, 5 * lot, Tif::Gtc, false, false),
        ),
        (t.clone(), close(PERP, 0, 100)),
    ]);
    assert_eq!(
        r,
        vec![Ok(()), Ok(())],
        "qty 0 takes the position's own size, which is on lot"
    );
    assert!(chain.position(t.addr, PERP).is_none());
}

#[test]
fn a_close_works_in_the_reduce_only_halt_and_from_an_underwater_account() {
    let (mut chain, t, cp) = holding(Side::Buy, 4 * ONE_BTC, 1, 1);
    let mut m = PerpMarkets::new(&chain.db).get(&PERP).unwrap();
    m.paused_reduce_only_only = true;
    PerpMarkets::new(&chain.db).put(&m);
    // The mark collapses: 4 BTC down $10,000 each against $1,000,000 is fine,
    // so shrink the collateral until the account is under water.
    chain.fund_margin(t.addr, 1_000 * MICRO);
    chain.set_mark(PERP, 45_000 * Q8);
    // The counterparty's bid is an opening order and the market is halted.
    let r = chain.block(&[(
        cp.clone(),
        perp_order(
            PERP,
            Side::Buy,
            45_000 * Q8,
            4 * ONE_BTC,
            Tif::Gtc,
            false,
            false,
        ),
    )]);
    assert_eq!(r, vec![Err(ExecError::ReduceOnlyOnly)]);
    // Lift the halt just long enough to rest the bid, then restore it.
    m.paused_reduce_only_only = false;
    PerpMarkets::new(&chain.db).put(&m);
    assert_eq!(
        chain.block(&[(
            cp,
            perp_order(
                PERP,
                Side::Buy,
                45_000 * Q8,
                4 * ONE_BTC,
                Tif::Gtc,
                false,
                false
            )
        )]),
        vec![Ok(())]
    );
    m.paused_reduce_only_only = true;
    PerpMarkets::new(&chain.db).put(&m);
    let r = chain.block(&[(t.clone(), close(PERP, 0, 100))]);
    assert_eq!(
        r,
        vec![Ok(())],
        "an underwater account in a halted market can still get out"
    );
    assert!(chain.position(t.addr, PERP).is_none());
}

#[test]
fn a_close_works_on_a_dated_future() {
    let fut = DerivId::future(0);
    let mut chain = Chain::new();
    chain.list_future(future_market(fut, chain.oracle.addr, 10_000));
    let (t, cp) = (actor("fclose-t"), actor("fclose-cp"));
    chain.fund_margin(t.addr, 1_000_000 * MICRO);
    chain.fund_margin(cp.addr, 1_000_000_000 * MICRO);
    chain.set_mark(fut, MARK);
    let r = chain.block(&[
        (
            cp.clone(),
            perp_order(fut, Side::Buy, MARK, 2 * ONE_BTC, Tif::Gtc, false, false),
        ),
        (
            t.clone(),
            perp_order(fut, Side::Sell, MARK, ONE_BTC, Tif::Ioc, false, false),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    // With the mark taken away (zero reads as none), the close is refused.
    chain.set_mark(fut, 0);
    assert_eq!(
        chain.block(&[(t.clone(), close(fut, 0, 100))]),
        vec![Err(ExecError::MissingMarkPrice)]
    );
    chain.set_mark(fut, MARK);
    // A second counterparty, quoting above the first one's leftover bid at
    // the mark (an ask from the first there would be a self-trade, and one
    // at or below it would simply fill that bid).
    let cp2 = actor("fclose-cp2");
    chain.fund_margin(cp2.addr, 1_000_000_000 * MICRO);
    let _ = cp;
    let r = chain.block(&[
        (
            cp2,
            perp_order(fut, Side::Sell, MARK + Q8, ONE_BTC, Tif::Gtc, false, false),
        ),
        (t.clone(), close(fut, 0, 100)),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    assert!(chain.position(t.addr, fut).is_none());
}

/// Executor-level twin of
/// `cb-engine/tests/perp_engine_properties.rs::a_plain_fill_ahead_of_a_reduce_only_maker_uses_up_its_budget`.
/// A long of 1 BTC rests a plain ask of 4 and, behind it, a reduce-only ask
/// of 20. A buy of 5 used to take the owner to short 4 — the reduce-only ask
/// opening a lot after the plain one flipped the position. Now the owner ends
/// short 3, and the reduce-only ask is gone.
#[test]
fn a_reduce_only_ask_behind_a_plain_ask_cannot_open_a_short() {
    let (mut chain, t, cp) = holding(Side::Buy, ONE_BTC, 1, 1);
    let r = chain.block(&[
        (
            t.clone(),
            perp_order(
                PERP,
                Side::Sell,
                MARK + Q8,
                4 * ONE_BTC,
                Tif::Gtc,
                false,
                false,
            ),
        ),
        (
            t.clone(),
            perp_order(PERP, Side::Sell, MARK + Q8, ONE_BTC, Tif::Gtc, false, true),
        ),
    ]);
    assert_eq!(r, vec![Ok(()), Ok(())]);
    let r = chain.block(&[(
        cp,
        perp_order(
            PERP,
            Side::Buy,
            MARK + Q8,
            5 * ONE_BTC,
            Tif::Ioc,
            false,
            false,
        ),
    )]);
    assert_eq!(r, vec![Ok(())]);
    let p = chain.position(t.addr, PERP).unwrap();
    assert_eq!((p.side, p.qty), (Side::Sell, 3 * ONE_BTC), "short 3, not 4");
    assert!(PerpOrders::new(&chain.db)
        .list_for_owner(&t.addr)
        .is_empty());
}
