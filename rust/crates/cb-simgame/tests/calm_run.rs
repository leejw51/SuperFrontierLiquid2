//! A long calm run holds every invariant.
//!
//! The control case. If a few thousand blocks of ordinary two-sided trading
//! break something, nothing that follows is worth reading. It is also the run
//! that proves the rejection paths stay clean: the noise bot submits
//! deliberately invalid orders throughout, and a rejection that half-applied
//! would show up here as a lock or conservation failure rather than as a
//! passing unit test.

mod common;

use cb_simgame::invariants::Invariant;
use cb_simgame::world::WorldConfig;

#[test]
fn several_thousand_calm_blocks_hold_every_invariant() {
    // `inject_premium: false` leaves the chain to derive its own premium from
    // the book, which is what a real node does. See `tests/common/mod.rs`.
    let report = common::run(WorldConfig {
        inject_premium: false,
        ..common::config(0xCA1_3105, "calm", 2_000)
    });

    common::assert_all_invariants_held(&report);
    assert_eq!(report.blocks_checked, 2_000, "every block boundary checked");

    // The run has to have actually done something.
    let volume: u128 = report.volume_notional.values().sum();
    assert!(
        volume > 1_000_000 * 1_000_000,
        "a calm run must still trade real size; got ${volume} micro-USD of notional"
    );
    assert!(
        report.txs_accepted > 5_000,
        "too few accepted transactions to be exercising anything: {}",
        report.txs_accepted
    );
    for inv in Invariant::CHECKED_PER_BLOCK {
        assert!(report.exercised.contains(&inv), "{inv} was never evaluated");
    }
}

#[test]
fn rejection_paths_are_exercised_continuously_and_leave_no_residue() {
    // The noise bot's whole purpose. A rejected transaction must not advance
    // a nonce, mutate a book, or leave a lock behind — and the only way to
    // know is to reject thousands of them while the invariants watch.
    let report = common::run(WorldConfig {
        inject_premium: false,
        ..common::config(0xBAD_0DE, "chop", 600)
    });

    common::assert_all_invariants_held(&report);
    let rejected: u64 = report.rejections.values().sum();
    assert!(
        rejected > 200,
        "only {rejected} rejections; the noise bot is not exercising the \
         rejection paths"
    );
    assert!(
        report.rejections.len() >= 3,
        "only {} distinct rejection reasons were reached: {:?}",
        report.rejections.len(),
        report.rejections.keys().collect::<Vec<_>>()
    );
}

/// The same control case with the spot market listed — QA findings F6 and F7,
/// at simulation scale.
///
/// Spot is off by default in the harness because it used to panic the executor
/// inside a few hundred blocks: a resting buy filled in pieces released more
/// than it had locked, and `AccountState::unlock` is a `checked_sub`, so the
/// overshoot aborted a block mid-apply. `cb-simgame` hit it unprompted at block
/// 113 of the very first spot run. The companion defect stranded a taker's
/// price improvement in its locked balance forever, which invariant 3 sees
/// directly.
///
/// An order's lock is now defined as a function of its remaining quantity and
/// moves only by differences between two evaluations of that definition — the
/// same definition `check_lock_consistency` re-derives independently, which is
/// what makes invariant 3 a real check here rather than a restatement.
///
/// This run is what "let spot run" means given that the `--spot` default lives
/// in the harness, which QA owns. Flipping that default is a one-line change
/// for them; this test is the part that belongs in CI either way.
#[test]
fn a_long_run_with_spot_listed_holds_every_invariant() {
    let report = common::run(WorldConfig {
        inject_premium: false,
        enable_spot: true,
        ..common::config(0x5907_0001, "calm:200,trend_up:150,crash:150,calm", 800)
    });

    common::assert_all_invariants_held(&report);
    assert_eq!(report.blocks_checked, 800, "every block boundary checked");
    assert!(
        report.spot_enabled,
        "the spot market must actually be listed"
    );

    // Spot has to have traded, or the lock accounting was never exercised.
    // `spot_pnl` is per-bot and nets to zero across the population, so take
    // the gross: any non-zero entry means spot balances actually moved.
    let spot_moved = report.bots.iter().any(|b| b.spot_pnl != 0);
    assert!(
        spot_moved,
        "no spot P&L anywhere, so the lock path was not exercised\n{}",
        report.render()
    );
}
