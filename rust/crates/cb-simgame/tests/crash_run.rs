//! A crash produces liquidations and still holds the invariants.
//!
//! This is the case conservation bugs actually surface in. A liquidation
//! moves value through four places at once — the victim's collateral, the
//! counterparty's realised profit, the insurance fund and the keeper's bounty
//! — and if any of them is computed against a different rounding or a
//! different price than the others, the ledger stops balancing.
//!
//! The test asserts on the cascade as well as on the invariants, because a
//! crash scenario that liquidates nobody proves nothing. If the liquidation
//! assertions start failing, the scenario has become too gentle and
//! `Scenario::drift_bps` / `entry_gap_bps` need to be harsher — not the
//! assertions loosened.

mod common;

use cb_simgame::ledger::LedgerSnapshot;
use cb_simgame::world::{World, WorldConfig};

#[test]
fn a_crash_cascades_liquidations_and_every_invariant_still_holds() {
    let report = common::run(WorldConfig {
        inject_premium: false,
        // The ramp is not decoration. Momentum bots chase trends, so a rally
        // is what leaves the whole cohort leveraged long at the same time —
        // and a crash into a one-sided book is the only way several accounts
        // go under at once. A crash out of a calm market liquidates them one
        // at a time as the price grinds down, which exercises the path
        // serially but never tests it under contention.
        ..common::config(0xC7A5_0004, "calm:60,trend_up:110,crash:90,calm", 400)
    });

    // The cascade actually happened.
    // "Cascade", not "one unlucky account". A single liquidation exercises
    // the path; several at once exercise the path while the book is thin,
    // which is where the loss-absorption arithmetic actually gets hard.
    assert!(
        report.peak_liquidatable_at_block_start >= 2,
        "only {} account(s) were ever liquidatable at once — the scenario is \
         too gentle to be testing a cascade. Harshen `Scenario::drift_bps` / \
         `entry_gap_bps` rather than lowering this number.\n{}",
        report.peak_liquidatable_at_block_start,
        report.render()
    );
    assert!(
        report.peak_liquidations_in_block >= 2,
        "no block ever liquidated more than {} account(s); that is an incident, \
         not a cascade\n{}",
        report.peak_liquidations_in_block,
        report.render()
    );
    assert!(
        report.liquidations >= 5,
        "only {} liquidation(s) triggered across the whole run\n{}",
        report.liquidations,
        report.render()
    );
    // Value really did move through the insurance fund and into bad debt,
    // which is what makes the conservation check meaningful here.
    assert!(
        report.insurance_min < report.insurance_start || report.bad_debt_end > 0,
        "the cascade never touched the insurance fund or produced bad debt, so \
         the loss-absorption path was not exercised\n{}",
        report.render()
    );

    common::assert_all_invariants_held(&report);
}

/// Insurance covers the residual deficit of closed accounts. Cash debt on
/// live portfolios remains a liability until close-out, including when a
/// funding payment exceeds cash but is backed by unrealized gains.
/// The minimal fill and zero-PnL-close cases live in cb-block; this checks
/// account eligibility and conservation across a complete crash simulation.
#[test]
fn insurance_covers_closed_accounts_and_retains_live_portfolio_liabilities() {
    let mut world = World::new(WorldConfig {
        inject_premium: false,
        ..common::config(0xC7A5_0007, "calm:120,crash:120,calm", 380)
    })
    .expect("world builds");
    let report = world.run().expect("run completes");
    assert!(report.fatal.is_none(), "{}", report.render());
    common::assert_all_invariants_held(&report);

    // The cascade must have put real losses through accounts that could not
    // cover them, or the test proves nothing.
    assert!(
        report.insurance_end < report.insurance_start,
        "the crash never drew on the insurance fund, so the absorption path was \
         not exercised\n{}",
        report.render()
    );
    assert!(
        report.insurance_end > 0,
        "insurance can cover flat deficits"
    );
    let ledger = LedgerSnapshot::read(world.db());
    assert_eq!(ledger.bad_debt_total, report.bad_debt_end);
    for (owner, debt) in &ledger.bad_debt_by_owner {
        if *debt > 0 {
            assert!(
                ledger
                    .signed_positions
                    .iter()
                    .any(|((address, _), qty)| address == owner && *qty != 0),
                "flat account {owner} has uncovered debt {debt} with funded insurance"
            );
        }
    }
}
