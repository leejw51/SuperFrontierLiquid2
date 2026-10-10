//! M7 negative-path test bundle.
//!
//! Every tx variant has a small, named set of rejection reasons. This file
//! exhaustively asserts that the executor surfaces the right `ExecError` for
//! each one. Catching a regression here is high-value: a silent fall-through
//! to `Ok(())` on a malformed input is the most insidious class of bug
//! (state mutates without the user noticing).
//!
//! Negative-path coverage by tx variant:
//!
//!   * `PlacePerpOrder`: insufficient margin, paused market, paused-reduce-only
//!     (without reduce_only flag), reduce-only without opposing position,
//!     unknown market, missing margin account.
//!   * `OracleUpdate`: wrong authority, stale height, unknown market.
//!   * `FundingTick`: unauthorized signer, unknown market, missing-required
//!     at boundary.
//!   * `LiquidationTrigger`: healthy account, missing mark, no position.
//!   * `CancelPerpOrder`: cancel of unknown id (should succeed as no-op),
//!     cancel by non-owner (should fail).
//!   * `FuturesSettle`: returns `Unsupported` until M5 lands.
//!   * `OpenPosition`, `ClosePosition`, `AdjustMargin`: all return
//!     `Unsupported` until M5/later.
//!
//! Some of these have lighter coverage in `oracle_update.rs`, `funding_tick.rs`,
//! `funding_tick_required.rs`, `liquidation.rs`, `perp_lifecycle.rs`. This file
//! collects the **comprehensive** matrix and asserts the exact `ExecError`
//! variant — not just "errors", which would let a regression to a different
//! error code slip through.

use cb_block::{apply_tx_at, enforce_required_funding_ticks, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_state::{Database, MarginAccounts, Meta, OracleState, PerpMarkets, Positions};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, MarginAccount, MarkPrice, OraclePrice, PerpMarket, Position, Side,
    Signed, Tif, H256,
};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn setup_market(db: &Database, oracle_authority: Address) -> PerpMarket {
    let pm = PerpMarket {
        id: MARKET,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 5,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    };
    PerpMarkets::new(db).put(&pm);
    pm
}

fn fund_margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

fn set_mark(db: &Database, mark_q8: u128) {
    OracleState::new(db).put_mark(&MarkPrice {
        market: MARKET,
        px_q8: mark_q8,
        computed_at_height: 0,
        premium_ema_q9: 0,
    });
    OracleState::new(db).put_oracle(&OraclePrice {
        market: MARKET,
        px_q8: mark_q8,
        source_height: 0,
        authority: Address::ZERO,
    });
}

fn sign(sk: &SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body,
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn place(
    sk: &SecretKey,
    nonce: u64,
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: Tif,
    post_only: bool,
    reduce_only: bool,
    cid: u8,
) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif,
            post_only,
            reduce_only,
            client_order_id: H256([cid; 32]),
        },
    )
}

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

// ---------------------------------------------------------------------------
// PlacePerpOrder negatives
// ---------------------------------------------------------------------------

#[test]
fn margin_failing_place_perp_rejects_insufficient_margin() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, 50_000 * Q8);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // 1 BTC at 50k = $50k notional. IM tier1 = 2% = $1k. Funded $100 → fail.
    fund_margin(&db, alice, 100 * MICRO);

    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::InsufficientMargin);
}

#[test]
fn place_perp_on_paused_market_rejects_market_paused() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let mut pm = setup_market(&db, Address([3; 20]));
    pm.paused = true;
    PerpMarkets::new(&db).put(&pm);

    let (sk, pk) = generate_keypair();
    fund_margin(&db, address_from_pubkey(&pk), 1_000_000);

    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MarketPaused);
}

#[test]
fn place_perp_on_reduce_only_halt_without_reduce_flag_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let mut pm = setup_market(&db, Address([3; 20]));
    pm.paused_reduce_only_only = true;
    PerpMarkets::new(&db).put(&pm);

    let (sk, pk) = generate_keypair();
    fund_margin(&db, address_from_pubkey(&pk), 1_000_000);

    // Non-reduce-only placement on a halted market → reject.
    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyOnly);
}

#[test]
fn reduce_only_without_opposing_position_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // Marked, so the order at 100 is inside the price band.
    set_mark(&db, 100);

    let (sk, pk) = generate_keypair();
    fund_margin(&db, address_from_pubkey(&pk), 1_000_000);

    // No position at all + reduce_only=true → ReduceOnlyNoPos.
    let mut state = BlockState::new(&db);
    let v = place(&sk, 0, MARKET, Side::Buy, 100, 10, Tif::Gtc, false, true, 1);
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyNoPos);
}

#[test]
fn reduce_only_with_same_side_position_rejects() {
    // Long position + reduce_only buy (would grow the long) → reject.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, 50_000 * Q8);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin(&db, alice, 1_000_000);
    Positions::new(&db).put(&Position {
        owner: alice,
        market: MARKET,
        side: Side::Buy,
        qty: ONE_BTC,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
    let mut state = BlockState::new(&db);
    // Reduce-only BUY on a LONG position → would grow → rejected.
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        true,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyNoPos);
}

#[test]
fn place_perp_off_lot_or_off_tick_rejects() {
    // `lot_size` and `tick_size` sat on every market row from genesis and the
    // executor read neither, so the gateway's refusal was the only one and
    // anything that bypassed it could rest a one-unit order at any price.
    // The genesis values: a lot of 1,000 base units, a tick of 100 q8.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let mut pm = setup_market(&db, Address([3; 20]));
    pm.lot_size = 1_000;
    pm.tick_size = 100;
    PerpMarkets::new(&db).put(&pm);
    set_mark(&db, 50_000 * Q8);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin(&db, alice, 1_000_000 * MICRO);

    let off_lot = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC + 1,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let mut state = BlockState::new(&db);
    assert_eq!(
        apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &off_lot, 0),
        Err(ExecError::BadLot)
    );
    let off_tick = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8 + 1,
        ONE_BTC,
        Tif::Gtc,
        false,
        false,
        2,
    );
    assert_eq!(
        apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &off_tick, 0),
        Err(ExecError::BadTick)
    );
    // On both, the same order rests; the rejections consumed no nonce.
    let on_both = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        false,
        3,
    );
    assert_eq!(
        apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &on_both, 0),
        Ok(())
    );
    state.commit().unwrap();
    assert_eq!(
        cb_state::PerpOrders::new(&db).list_for_owner(&alice).len(),
        1
    );
}

#[test]
fn place_perp_unknown_market_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    // No market registered.
    let (sk, pk) = generate_keypair();
    fund_margin(&db, address_from_pubkey(&pk), 1_000_000);

    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}

#[test]
fn place_perp_missing_margin_account_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // Marked, so the order at 100 is inside the price band.
    set_mark(&db, 100);

    let (sk, _pk) = generate_keypair();
    // No margin account funded.
    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NoMarginAccount);
}

// ---------------------------------------------------------------------------
// OracleUpdate negatives
// ---------------------------------------------------------------------------

#[test]
fn oracle_update_wrong_authority_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_auth_sk, auth_pk) = generate_keypair();
    setup_market(&db, address_from_pubkey(&auth_pk));

    let (attacker_sk, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &attacker_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 50_000 * Q8,
            submitted_height: 1,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);
    assert!(OracleState::new(&db).get_oracle(&MARKET).is_none());
}

#[test]
fn oracle_update_stale_height_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, auth_pk) = generate_keypair();
    let auth = address_from_pubkey(&auth_pk);
    setup_market(&db, auth);

    // First update at height 100.
    let mut state = BlockState::new(&db);
    let v1 = sign(
        &auth_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 50_000 * Q8,
            submitted_height: 100,
        },
    );
    apply_tx_at(&mut state, &ctx_at(100, Address::ZERO), &v1, 0).unwrap();
    state.commit().unwrap();

    // Second update at height 99 → stale.
    let mut state = BlockState::new(&db);
    let v2 = sign(
        &auth_sk,
        1,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 51_000 * Q8,
            submitted_height: 99,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(101, Address::ZERO), &v2, 0).unwrap_err();
    assert_eq!(err, ExecError::OracleStale);
}

#[test]
fn oracle_update_unknown_market_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (auth_sk, _) = generate_keypair();

    let mut state = BlockState::new(&db);
    let v = sign(
        &auth_sk,
        0,
        TxBody::OracleUpdate {
            market: DerivId::perp(99),
            px_q8: 50_000 * Q8,
            submitted_height: 1,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}

// ---------------------------------------------------------------------------
// FundingTick negatives
// ---------------------------------------------------------------------------

#[test]
fn funding_tick_signed_by_non_proposer_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_market(&db, proposer);

    let (attacker_sk, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(&attacker_sk, 0, TxBody::FundingTick { market: MARKET });
    let err = apply_tx_at(&mut state, &ctx_at(5, proposer), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);
}

#[test]
fn funding_tick_unknown_market_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);

    let mut state = BlockState::new(&db);
    let v = sign(
        &proposer_sk,
        0,
        TxBody::FundingTick {
            market: DerivId::perp(99),
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(5, proposer), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}

#[test]
fn block_at_funding_boundary_missing_required_tick_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_market(&db, proposer);

    // height = 5 = boundary. No tick in the slice → reject.
    let err = enforce_required_funding_ticks(&db, 5, proposer, &[]).unwrap_err();
    assert_eq!(
        err,
        ExecError::MissingRequiredFundingTick {
            height: 5,
            market: MARKET,
        }
    );
}

#[test]
fn block_at_non_boundary_does_not_require_tick() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_market(&db, proposer);

    // height 4, not a boundary. Empty slice is fine.
    assert!(enforce_required_funding_ticks(&db, 4, proposer, &[]).is_ok());
}

// ---------------------------------------------------------------------------
// LiquidationTrigger negatives
// ---------------------------------------------------------------------------

#[test]
fn liquidation_on_healthy_account_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, 50_000 * Q8);

    let (_sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 50_000 * MICRO);
    Positions::new(&db).put(&Position {
        owner: alice,
        market: MARKET,
        side: Side::Buy,
        qty: ONE_BTC,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });

    let (sk_keeper, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk_keeper,
        0,
        TxBody::LiquidationTrigger {
            owner: alice,
            market: MARKET,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotLiquidatable);
}

#[test]
fn liquidation_no_position_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, 50_000 * Q8);

    let (_sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_500 * MICRO);
    // No position.

    let (sk_keeper, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk_keeper,
        0,
        TxBody::LiquidationTrigger {
            owner: alice,
            market: MARKET,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotLiquidatable);
}

#[test]
fn liquidation_without_mark_rejects() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // No mark set.

    let (_sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_000 * MICRO);
    Positions::new(&db).put(&Position {
        owner: alice,
        market: MARKET,
        side: Side::Buy,
        qty: ONE_BTC,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });

    let (sk_keeper, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk_keeper,
        0,
        TxBody::LiquidationTrigger {
            owner: alice,
            market: MARKET,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MissingMarkPrice);
}

// ---------------------------------------------------------------------------
// CancelPerpOrder negatives
// ---------------------------------------------------------------------------

#[test]
fn cancel_unknown_order_is_idempotent_success() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin(&db, alice, 1_000_000);

    let mut state = BlockState::new(&db);
    let v = sign(
        &sk,
        0,
        TxBody::CancelPerpOrder {
            order_id: H256([0xff; 32]),
        },
    );
    apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0)
        .expect("cancel of unknown id is a no-op success");
    state.commit().unwrap();

    let acct = cb_state::Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(acct.nonce, 1);
}

#[test]
fn cancel_by_non_owner_rejects_not_authorized() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // Marked, so the order at 100 is inside the price band.
    set_mark(&db, 100);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_000_000);

    // Alice rests an order.
    let mut state = BlockState::new(&db);
    let v = place(
        &sk_alice,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap();
    state.commit().unwrap();
    let resting_oid = cb_state::PerpBookIndex::new(&db).list_side(&MARKET, Side::Buy)[0].2;

    // Bob tries to cancel.
    let (sk_bob, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk_bob,
        0,
        TxBody::CancelPerpOrder {
            order_id: resting_oid,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(2, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);
}

// ---------------------------------------------------------------------------
// FuturesSettle / OpenPosition / ClosePosition / AdjustMargin: still
// `Unsupported` until M5+. The match in `apply_tx_at` is exhaustive so adding
// a new variant without a handler is a *compile* error rather than a silent
// fallthrough — we just confirm the current `Unsupported` semantics here.
// ---------------------------------------------------------------------------

#[test]
fn open_position_returns_unsupported_until_m5() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk,
        0,
        TxBody::OpenPosition {
            market: MARKET,
            side: Side::Buy,
            qty: 1,
            max_slippage_bps: 800,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::Unsupported);
}

#[test]
fn close_position_without_a_position_rejects() {
    // `ClosePosition` is implemented now (a reduce-only IOC at the mark,
    // bounded by the slippage); with nothing to close it is refused as any
    // reduce-only order with no opposing position is.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, 50_000 * Q8);
    let (sk, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk,
        0,
        TxBody::ClosePosition {
            market: MARKET,
            qty: 0,
            max_slippage_bps: 800,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyNoPos);
}

#[test]
fn adjust_margin_returns_unsupported_until_m5() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, _) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = sign(
        &sk,
        0,
        TxBody::AdjustMargin {
            market: MARKET,
            is_topup: true,
            amount: 1,
        },
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::Unsupported);
}

// ---------------------------------------------------------------------------
// Bad nonce: a tx with the wrong nonce errors with BadNonce regardless of the
// body. Pinning the perp variants explicitly because they touch CFs the spot
// path doesn't, and "nonce check before market check" is the contract that
// keeps account state coherent across rejection cases.
// ---------------------------------------------------------------------------

#[test]
fn place_perp_with_wrong_nonce_rejects_before_touching_market_state() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin(&db, alice, 1_000_000);
    // Bump alice's nonce to 5 directly via Accounts CF — simulate prior txs.
    let acc_cf = cb_state::Accounts::new(&db);
    let mut a = acc_cf.get(&alice).unwrap_or_default();
    a.nonce = 5;
    acc_cf.put(&alice, &a);

    // Submit at nonce 0 — wrong.
    let mut state = BlockState::new(&db);
    let v = place(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap_err();
    assert_eq!(
        err,
        ExecError::BadNonce {
            expected: 5,
            got: 0
        }
    );
}
