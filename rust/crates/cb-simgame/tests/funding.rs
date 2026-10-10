//! Invariant 7 — funding neutrality.
//!
//! Funding is supposed to be a pure transfer: whatever the longs pay, the
//! shorts receive, net of any insurance-fund share (this implementation takes
//! none). Two findings used to stand between that statement and a test of it,
//! and both are fixed:
//!
//! 1. **F9 — the chain never computed a funding premium.**
//!    `apply_oracle_update` carried `MarkPrice::premium_ema_q9` forward
//!    unchanged and passed `mid = None` to `compute_mark_price`, so the rate
//!    was always zero and every tick moved nothing. Neutrality held trivially,
//!    which is not the same as holding. The executor now reads the book
//!    midpoint and folds it into the premium EMA.
//! 2. **F2 — a new position's funding index was never initialised.** Once a
//!    premium existed, a position opened after the index started accruing was
//!    charged for the whole history before it existed; longs and shorts
//!    carried different snapshots, the flows stopped cancelling, and collateral
//!    was minted or burned with nobody on the other side. Positions are now
//!    stamped with the market's live cumulative index on open and on flip, and
//!    the stamp is volume-weighted when a position grows.
//!
//! So this file now asserts the thing it always wanted to: funding flows
//! without any help from the harness, and it nets to zero.

mod common;

use cb_simgame::invariants::Invariant;
use cb_simgame::world::WorldConfig;

const FUNDING_INTERVAL: u32 = 25;

#[test]
fn the_chain_derives_its_own_premium_so_funding_actually_flows() {
    // No injected premium: this is what a real node does. The executor derives
    // the premium from the book midpoint against the oracle (F9), so funding is
    // a live mechanism rather than a specified one that does nothing.
    let report = common::run(WorldConfig {
        inject_premium: false,
        funding_interval_blocks: FUNDING_INTERVAL,
        ..common::config(0xF00D_0001, "calm:100,trend_up:100,calm", 300)
    });
    common::assert_all_invariants_held(&report);
    assert!(
        report.funding_paid > 0,
        "the chain must derive a premium from its own book and move value on a \
         funding tick\n{}",
        report.render()
    );
    assert!(
        report.blocks_run >= 300,
        "the run must cross several funding boundaries"
    );
}

#[test]
fn funding_transfers_value_and_nets_to_zero() {
    // The harness's injected premium on top of the chain's own, to drive
    // funding harder than a calm book would and to put positions that open
    // mid-interval on both sides of the market. This is the run that used to
    // violate invariants 1 and 7 together on every funding boundary.
    let report = common::run(WorldConfig {
        inject_premium: true,
        funding_interval_blocks: FUNDING_INTERVAL,
        ..common::config(0xF00D_0002, "calm:100,trend_up:120,calm", 320)
    });

    // Value really does move from one side of the market to the other.
    assert!(
        report.funding_paid > 0,
        "no funding flowed, so neutrality is untested\n{}",
        report.render()
    );

    assert!(
        report
            .violations_of(Invariant::FundingNeutrality)
            .is_empty(),
        "what the longs pay, the shorts must receive: a position opened after \
         the index started accruing owes funding only from the moment it \
         opened\n{}",
        report.render()
    );
    assert!(
        report.violations_of(Invariant::Conservation).is_empty(),
        "and no collateral appears from nowhere on a funding boundary\n{}",
        report.render()
    );
    common::assert_all_invariants_held(&report);
}

#[test]
fn funding_neutrality_is_checked_on_quiet_blocks_so_the_measurement_is_exact() {
    // The world submits nothing but oracle updates and the funding ticks
    // themselves at a boundary height, so the collateral delta across that
    // block is the funding flow and nothing else. This test guards that
    // property: if bots ever start trading on boundary blocks, the funding
    // measurement silently starts including their fills.
    let report = common::run(WorldConfig {
        inject_premium: false,
        funding_interval_blocks: 10,
        ..common::config(0xF00D_0003, "chop", 120)
    });
    common::assert_all_invariants_held(&report);
    assert!(
        report
            .violations_of(Invariant::FundingNeutrality)
            .is_empty(),
        "the collateral delta across a boundary block is the funding flow and \
         nothing else, and it nets to zero\n{}",
        report.render()
    );
}
