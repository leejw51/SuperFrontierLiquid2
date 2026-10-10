//! Invariant 8 — determinism.
//!
//! This is the most valuable test in the crate. Every other failure is only
//! actionable if it reproduces, and everything that makes a run reproducible
//! — the bot decisions, the scenario's price path, the order in which bots
//! act, the transaction index each signature is bound to — funnels through
//! the one seed. If that chain breaks anywhere, a conservation bug found on
//! Tuesday is gone by Wednesday.
//!
//! So the assertion is not "the final state root matches". It is "the state
//! root after **every** block matches", which localises a divergence to the
//! exact height it started at instead of leaving a whole run to bisect.
//!
//! ## Why the state root alone is not enough
//!
//! `compute_state_root` covers the accounts, positions and markets tries and
//! nothing else — margin accounts are outside it (finding F3 in
//! `docs/qa-report.md`). Two runs could therefore disagree about every
//! trader's collateral from block 50 onward and compare equal on roots alone.
//! Each block's ledger digest is compared alongside the root for exactly that
//! reason, which makes this test a second demonstration of F3 as well as the
//! determinism check.

mod common;

use cb_simgame::world::WorldConfig;

#[test]
fn the_same_seed_produces_an_identical_run_block_by_block() {
    let make = || common::config(0xD37E_1111, "calm:60,crash:40,calm", 220);
    let a = common::run(make());
    let b = common::run(make());

    assert_eq!(
        a.state_roots.len(),
        b.state_roots.len(),
        "the two runs produced different numbers of blocks"
    );
    for ((ha, ra), (hb, rb)) in a.state_roots.iter().zip(b.state_roots.iter()) {
        assert_eq!(ha, hb, "block heights diverged");
        assert_eq!(
            ra, rb,
            "state roots diverged at height {ha}: {ra:?} vs {rb:?}. \
             Everything downstream of this height is unreproducible."
        );
    }

    // The ledger the state root does not cover.
    assert_eq!(
        a.ledger_digests.len(),
        b.ledger_digests.len(),
        "different numbers of ledger digests"
    );
    for ((ha, da), (hb, db)) in a.ledger_digests.iter().zip(b.ledger_digests.iter()) {
        assert_eq!(ha, hb);
        assert_eq!(
            da, db,
            "collateral, bad debt or the insurance fund diverged at height {ha}: \
             {da:?} vs {db:?}. The state root would not have caught this — see \
             finding F3."
        );
    }

    // Transaction flow and outcomes must match too: identical roots with a
    // different transaction mix would mean the roots are not covering
    // something they should.
    assert_eq!(a.txs_submitted, b.txs_submitted);
    assert_eq!(a.txs_accepted, b.txs_accepted);
    assert_eq!(a.rejections, b.rejections);
    assert_eq!(a.volume_base, b.volume_base);
    assert_eq!(a.liquidations, b.liquidations);

    // Per-bot outcomes, which depend on every fill in the run.
    assert_eq!(a.bots.len(), b.bots.len());
    for (x, y) in a.bots.iter().zip(b.bots.iter()) {
        assert_eq!(x.label, y.label);
        assert_eq!(
            x.end_collateral, y.end_collateral,
            "{} ended with different collateral across two runs of the same seed",
            x.label
        );
        assert_eq!(x.end_equity, y.end_equity);
    }
}

#[test]
fn a_different_seed_produces_a_different_run() {
    // Guards the test above against passing for the wrong reason: if the
    // seed were ignored entirely, every run would match.
    let a = common::run(common::config(1, "chop", 60));
    let b = common::run(common::config(2, "chop", 60));
    assert_ne!(
        a.state_roots.last().map(|(_, r)| *r),
        b.state_roots.last().map(|(_, r)| *r),
        "two different seeds produced an identical chain, so the seed is not \
         reaching the simulation"
    );
}

#[test]
fn determinism_survives_a_settlement_and_a_squeeze() {
    // The settlement path mutates every position on a market at once and the
    // squeeze path drives liquidations; both are the kind of bulk state
    // change where an unsorted iteration would leak nondeterminism.
    let make = || WorldConfig {
        future_expiry_height: 90,
        ..common::config(0x5EED_5EED, "calm:40,squeeze:40,calm", 140)
    };
    let a = common::run(make());
    let b = common::run(make());
    assert!(
        !a.settled_futures.is_empty(),
        "the dated future must actually settle for this test to mean anything"
    );
    assert_eq!(a.settled_futures, b.settled_futures);
    assert_eq!(a.state_roots, b.state_roots);
    assert_eq!(a.ledger_digests, b.ledger_digests);
}
