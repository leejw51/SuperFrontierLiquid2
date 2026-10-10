//! Invariant 9 — settlement.
//!
//! A dated future at expiry must close every position at the settlement price
//! and leave zero open interest. The test drives a real future through a run
//! that builds genuine open interest first, because settling an empty market
//! is trivially correct and proves nothing.

mod common;

use cb_simgame::invariants::Invariant;
use cb_simgame::universe::FUT_BTC;
use cb_simgame::world::WorldConfig;

#[test]
fn a_dated_future_at_expiry_settles_every_position_and_leaves_no_open_interest() {
    // Deliberately not a multiple of the funding interval: a block that both
    // settles and ticks funding is a legitimate coincidence, but keeping them
    // apart makes this test about settlement alone.
    let expiry = 205;
    let report = common::run(WorldConfig {
        inject_premium: false,
        future_expiry_height: expiry,
        ..common::config(0x5E77_1E00, "calm:80,trend_down:60,calm", 300)
    });

    common::assert_all_invariants_held(&report);

    // Open interest really existed before expiry.
    let peak = report
        .peak_open_interest
        .get(&FUT_BTC)
        .copied()
        .unwrap_or(0);
    assert!(
        peak > 0,
        "the future never carried any open interest, so settling it proves \
         nothing\n{}",
        report.render()
    );
    assert!(
        report.volume_base.get(&FUT_BTC).copied().unwrap_or(0) > 0,
        "the future was never traded"
    );

    // It settled, at a real price.
    let (_, settle_px) = report
        .settled_futures
        .iter()
        .find(|(id, _)| *id == FUT_BTC)
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "the future did not settle at its expiry height of {expiry}\n{}",
                report.render()
            )
        });
    assert!(settle_px > 0, "settlement price must not be zero");

    // And nothing is left open.
    let (long, short) = report
        .open_interest_end
        .get(&FUT_BTC)
        .copied()
        .unwrap_or((0, 0));
    assert_eq!(
        (long, short),
        (0, 0),
        "the settled future still carries {long} long and {short} short base \
         units of open interest\n{}",
        report.render()
    );
    assert!(report.exercised.contains(&Invariant::Settlement));
}

#[test]
fn the_perpetual_markets_keep_trading_after_the_future_settles() {
    // Settlement cancels every resting order on its own market. This guards
    // against it reaching further than it should.
    let report = common::run(WorldConfig {
        inject_premium: false,
        future_expiry_height: 120,
        ..common::config(0x5E77_2E00, "calm", 260)
    });
    common::assert_all_invariants_held(&report);
    assert!(!report.settled_futures.is_empty(), "the future settled");

    let perp_volume: u128 = report
        .volume_base
        .iter()
        .filter(|(id, _)| **id != FUT_BTC)
        .map(|(_, v)| *v)
        .sum();
    assert!(
        perp_volume > 0,
        "the perpetual markets stopped trading after settlement\n{}",
        report.render()
    );
    let still_open: u128 = report
        .open_interest_end
        .iter()
        .filter(|(id, _)| **id != FUT_BTC)
        .map(|(_, (long, _))| *long)
        .sum();
    assert!(
        still_open > 0,
        "settling the future closed positions on the perpetual markets too\n{}",
        report.render()
    );
}
