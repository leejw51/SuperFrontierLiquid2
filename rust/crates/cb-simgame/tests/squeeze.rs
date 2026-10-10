//! Invariant 2 — a squeeze does not let any account go negative.
//!
//! A squeeze is a crash run upside down: the price gaps up and keeps
//! climbing, so it is the leveraged **shorts** that cannot pay. Balances are
//! unsigned, so "negative" cannot appear as a minus sign — it appears either
//! as a panic inside `AccountState::sub`, or as a locked balance that exceeds
//! what the account holds, which `AccountState::available` hides behind a
//! saturating subtraction. Both are what this test looks for.

mod common;

use cb_simgame::invariants::Invariant;
use cb_simgame::ledger::LedgerSnapshot;
use cb_simgame::world::{World, WorldConfig};

#[test]
fn a_squeeze_never_drives_an_account_negative() {
    let config = WorldConfig {
        inject_premium: false,
        // A decline first, so the momentum cohort is leveraged short when the
        // price gaps up. A squeeze out of a calm market catches almost nobody.
        ..common::config(0x59_0EE2E, "calm:50,trend_down:110,squeeze:90,calm", 360)
    };
    let mut world = World::new(config).expect("world builds");
    let report = world.run().expect("run completes");
    if let Some(fatal) = &report.fatal {
        panic!(
            "the squeeze aborted the executor, which is itself a way for an \
             account to go negative: {fatal}\n{}",
            report.render()
        );
    }

    assert!(
        report.peak_liquidatable_at_block_start >= 2,
        "only {} account(s) were ever liquidatable — the squeeze did not put \
         enough shorts under water to approach the negative-balance paths\n{}",
        report.peak_liquidatable_at_block_start,
        report.render()
    );
    assert!(
        report
            .violations_of(Invariant::NoNegativeBalances)
            .is_empty(),
        "an account's free balance went negative during the squeeze\n{}",
        report.render()
    );
    common::assert_all_invariants_held(&report);

    // Re-derive the claim from raw state rather than trusting the checker.
    let snapshot = LedgerSnapshot::read(world.db());
    for (addr, state) in cb_simgame::ledger::scan_accounts(world.db()) {
        for (asset, locked) in &state.locked {
            let balance = state.balances.get(asset).copied().unwrap_or(0);
            assert!(
                *locked <= balance,
                "{addr} has {locked} of asset {} locked against a balance of \
                 {balance}",
                cb_simgame::ledger::asset_label(*asset)
            );
        }
    }
    // Collateral is unsigned, so an account that cannot pay records the
    // shortfall as `bad_debt` instead of underflowing, and the debit is taken
    // in full at the moment it happens. It is NOT invariant that such an
    // account still has zero collateral at the end of the run: finding F4
    // means nothing deducts the debt from money credited afterwards, so an
    // account can end up holding collateral while still owing. What must hold
    // is that the debt is still recorded rather than quietly forgotten.
    let indebted: Vec<_> = cb_simgame::ledger::scan_margin_accounts(world.db())
        .into_iter()
        .filter(|m| m.bad_debt > 0)
        .collect();
    assert_eq!(
        snapshot.bad_debt_total,
        indebted.iter().map(|m| m.bad_debt).sum::<u128>(),
        "the ledger's bad-debt total must equal the sum of what accounts owe"
    );
    assert!(
        snapshot.collateral_total > 0,
        "the whole system cannot be at zero collateral"
    );
}
