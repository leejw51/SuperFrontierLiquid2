//! Negative and positive controls for the simulation's invariant checker.
//!
//! Every other test in this crate ends in "all invariants held", and that
//! statement is only as strong as the checker behind it: a checker that
//! never fires passes every run. These tests plant one defect at a time
//! directly in committed state — value minted from nowhere, a lock with no
//! order behind it, a crossed book, an unmatched position, a one-sided
//! funding flow — and assert the checker names the invariant it breaks. The
//! positive control moves value *legitimately* (a balanced spot transfer, a
//! balanced collateral transfer) and asserts the checker stays quiet, so the
//! negatives are not passing merely because the checker fires on anything.

use cb_simgame::invariants::{BlockContext, CheckerConfig, Invariant, InvariantChecker};
use cb_simgame::scenario::Scenario;
use cb_state::{
    Accounts, Database, MarginAccounts, MarketConfig, Markets, PerpBookIndex, PerpMarkets,
    Positions,
};
use cb_types::{
    Address, AssetId, DerivId, MarginAccount, MarketId, PerpMarket, Position, Side, Signed, H256,
};
use std::collections::BTreeMap;

const PERP: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const A: Address = Address([0xA1; 20]);
const B: Address = Address([0xB2; 20]);

fn base() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Markets::new(&db).put(&MarketConfig {
        id: MarketId::BtcUsd,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 1,
        min_qty: 1,
        fee_bps: 10,
    });
    PerpMarkets::new(&db).put(&PerpMarket {
        id: PERP,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 0,
        maker_fee_bps_signed: 0,
        funding_interval_blocks: 25,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: Address::ZERO,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    for who in [A, B] {
        let mut s = Accounts::new(&db).get(&who).unwrap_or_default();
        s.add(AssetId::Btc, 10 * Q8).unwrap();
        s.add(AssetId::Usd, 1_000_000 * MICRO).unwrap();
        Accounts::new(&db).put(&who, &s);
        margin(&db, who, 100_000 * MICRO);
    }
    position(&db, A, Side::Buy, Q8);
    position(&db, B, Side::Sell, Q8);
    (dir, db)
}

fn margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

fn position(db: &Database, owner: Address, side: Side, qty: u128) {
    Positions::new(db).put(&Position {
        owner,
        market: PERP,
        side,
        qty,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
}

/// Prime a checker on the base state, apply `plant`, check one block, and
/// return the set of invariants it reported.
fn check_after(plant: impl FnOnce(&Database), funding: bool) -> Vec<Invariant> {
    let (_dir, db) = base();
    let mut checker = InvariantChecker::new(CheckerConfig::default());
    checker.prime(&db);
    plant(&db);
    let labels = BTreeMap::new();
    let funding_markets = if funding { vec![PERP] } else { vec![] };
    let ctx = BlockContext {
        height: 1,
        scenario: Scenario::Calm,
        txs: &[],
        state_root: H256::default(),
        funding_markets: &funding_markets,
        settled_markets: &[],
        labels: &labels,
    };
    checker.check_block(&db, &ctx).expect("not fail-fast");
    let mut found: Vec<Invariant> = checker.violations().iter().map(|v| v.invariant).collect();
    found.sort();
    found.dedup();
    found
}

fn edit_account(db: &Database, who: Address, f: impl FnOnce(&mut cb_state::AccountState)) {
    let mut s = Accounts::new(db).get(&who).unwrap();
    f(&mut s);
    Accounts::new(db).put(&who, &s);
}

#[test]
fn legitimate_transfers_raise_nothing() {
    let found = check_after(
        |db| {
            edit_account(db, A, |s| s.sub(AssetId::Usd, 7).unwrap());
            edit_account(db, B, |s| s.add(AssetId::Usd, 7).unwrap());
            margin(db, A, 100_000 * MICRO - 5 * MICRO);
            margin(db, B, 100_000 * MICRO + 5 * MICRO);
        },
        false,
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_spot_unit_from_nowhere_breaks_conservation() {
    let found = check_after(
        |db| edit_account(db, A, |s| s.add(AssetId::Btc, 1).unwrap()),
        false,
    );
    assert!(found.contains(&Invariant::Conservation), "{found:?}");
}

#[test]
fn a_dollar_of_collateral_from_nowhere_breaks_conservation() {
    let found = check_after(|db| margin(db, A, 100_001 * MICRO), false);
    assert!(found.contains(&Invariant::Conservation), "{found:?}");
}

#[test]
fn a_lock_beyond_the_balance_is_a_negative_free_balance() {
    let found = check_after(
        |db| edit_account(db, A, |s| s.lock(AssetId::Btc, 11 * Q8).unwrap()),
        false,
    );
    assert!(found.contains(&Invariant::NoNegativeBalances), "{found:?}");
}

#[test]
fn a_lock_with_no_order_behind_it_breaks_lock_consistency() {
    let found = check_after(
        |db| edit_account(db, A, |s| s.lock(AssetId::Usd, 1).unwrap()),
        false,
    );
    assert_eq!(found, vec![Invariant::LockConsistency]);
}

#[test]
fn a_crossed_derivative_book_breaks_book_integrity() {
    let found = check_after(
        |db| {
            let idx = PerpBookIndex::new(db);
            idx.put(&PERP, Side::Buy, 50_001 * Q8, 1, &H256([1; 32]));
            idx.put(&PERP, Side::Sell, 50_000 * Q8, 2, &H256([2; 32]));
        },
        false,
    );
    assert!(found.contains(&Invariant::BookIntegrity), "{found:?}");
}

#[test]
fn an_unmatched_position_breaks_netting() {
    let found = check_after(|db| position(db, A, Side::Buy, Q8 + 1), false);
    assert!(found.contains(&Invariant::PositionNetting), "{found:?}");
}

#[test]
fn a_one_sided_funding_flow_breaks_neutrality() {
    let found = check_after(
        |db| {
            margin(db, A, 100_000 * MICRO - 100);
            margin(db, B, 100_000 * MICRO + 300);
        },
        true,
    );
    assert!(found.contains(&Invariant::FundingNeutrality), "{found:?}");
    // The same balanced flow on a funding block is not a violation.
    let found = check_after(
        |db| {
            margin(db, A, 100_000 * MICRO - 300);
            margin(db, B, 100_000 * MICRO + 300);
        },
        true,
    );
    assert!(found.is_empty(), "{found:?}");
}
