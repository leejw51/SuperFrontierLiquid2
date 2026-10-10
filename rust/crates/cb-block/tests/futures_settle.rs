//! M5 integration tests: dated-future expiry + settlement.
//!
//! Covers:
//! - PlacePerpOrder accepted on a dated future before expiry, rejected after.
//! - Block missing the required `FuturesSettle` at expiry is rejected.
//! - At expiry, the `FuturesSettle` handler closes every open position,
//!   credits/debits collateral by the settlement PnL, cancels resting
//!   orders, marks the market settled, and clears mark history.
//! - Settlement PnL conservation: long collateral delta + short delta = 0.
//! - Duplicate / pre-expiry `FuturesSettle` rejected.

use cb_block::{
    apply_tx_at, build_and_apply, enforce_required_futures_settle, BlockState, ExecCtx, ExecError,
};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign as crypto_sign, SecretKey};
use cb_engine::MARK_HISTORY_BLOCKS;
use cb_state::{
    Database, FuturesMarkets, MarginAccounts, Meta, OracleState, PerpBookIndex, PerpOrders,
    Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, FutureMarket, MarginAccount, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::future(0);
const EXPIRY: u64 = 100;

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn future_market(authority: Address) -> FutureMarket {
    FutureMarket {
        id: MARKET,
        underlying: AssetId::Eth,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 2500,
        initial_margin_bps: 400,
        maintenance_margin_bps: 200,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        expiry_height: EXPIRY,
        // TWAP method.
        settlement_method: 0,
        settlement_window_blocks: MARK_HISTORY_BLOCKS as u32,
        oracle_authority: authority,
        paused: false,
        final_settlement_px_q8: None,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn setup_with_market(db: &Database, authority: Address) {
    FuturesMarkets::new(db).put(&future_market(authority));
}

/// Mark the market at `px_q8`. Every derivative order is priced against the
/// band around the mark, so a test places orders only once the market has one.
fn mark_at(db: &Database, px_q8: u128) {
    OracleState::new(db).put_mark(&cb_types::MarkPrice {
        market: MARKET,
        px_q8,
        computed_at_height: 0,
        premium_ema_q9: 0,
    });
}

fn fund_margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: cb_types::Signed::ZERO,
        bad_debt: 0,
    });
}

fn ctx_for(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

fn signed_place(
    sk: &SecretKey,
    nonce: u64,
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    cid: u8,
) -> VerifiedTx {
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256([cid; 32]),
        },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

fn sign_tx_simple(sk: &SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
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

// ---------------------------------------------------------------------------
// 1. Place + match on a dated future before expiry
// ---------------------------------------------------------------------------

#[test]
fn dated_future_accepts_orders_before_expiry_and_fills() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let (_authority_sk, authority_pk) = generate_keypair();
    let authority = address_from_pubkey(&authority_pk);
    setup_with_market(&db, authority);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin(&db, alice, 1_000_000_000);
    fund_margin(&db, bob, 1_000_000_000);

    let mut state = BlockState::new(&db);
    let ctx = ctx_for(50, Address::ZERO);

    // alice rests a sell at 1000.
    mark_at(&db, 1_000);
    let v0 = signed_place(&sk_alice, 0, MARKET, Side::Sell, 1_000, 5, 0);
    apply_tx_at(&mut state, &ctx, &v0, 0).unwrap();
    // bob takes against it with a buy at 1000 — fills.
    let v1 = signed_place(&sk_bob, 0, MARKET, Side::Buy, 1_000, 5, 1);
    apply_tx_at(&mut state, &ctx, &v1, 1).unwrap();
    state.commit().unwrap();

    let positions = Positions::new(&db);
    let alice_pos = positions
        .get(&alice, &MARKET)
        .expect("alice has a position");
    let bob_pos = positions.get(&bob, &MARKET).expect("bob has a position");
    assert_eq!(alice_pos.side, Side::Sell);
    assert_eq!(alice_pos.qty, 5);
    assert_eq!(bob_pos.side, Side::Buy);
    assert_eq!(bob_pos.qty, 5);
    // Last-trade plumbed through to the FutureMarket record.
    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(fm.last_trade_px_q8, 1_000);
}

// ---------------------------------------------------------------------------
// 2. Block missing FuturesSettle at expiry is rejected
// ---------------------------------------------------------------------------

#[test]
fn enforce_required_settle_rejects_block_missing_settle_at_expiry() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);
    // A settle is required once it can succeed, which needs a price.
    OracleState::new(&db).put_mark_history(&MARKET, EXPIRY - 1, 1_000);

    let err = enforce_required_futures_settle(&db, EXPIRY, proposer, &[]).unwrap_err();
    assert!(
        matches!(err, ExecError::MissingRequiredFuturesSettle { height, market } if height == EXPIRY && market == MARKET),
        "got {err:?}"
    );
}

#[test]
fn a_future_with_no_price_at_expiry_waits_instead_of_halting_the_chain() {
    // Core review H2. The old rule demanded a settle at exactly `expiry_height`
    // and the handler refused to settle at a zero price, so a future that
    // expired before its first oracle post could never be built past: the
    // settle was present, it failed, the post-loop check failed, the block
    // failed, and the height never moved again. The design says settlement is
    // *delayed* in that case. So: no price, no requirement — and the
    // requirement appears, at any later height, the moment a price does.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    // At expiry, and well past it, with nothing to settle at: not required.
    enforce_required_futures_settle(&db, EXPIRY, proposer, &[]).unwrap();
    enforce_required_futures_settle(&db, EXPIRY + 40, proposer, &[]).unwrap();
    assert!(cb_block::required_system_txs(&db, EXPIRY + 40).is_empty());

    // An oracle price arrives (the TWAP window is empty, so it is the
    // fallback). Now the settle is required at the next height, which is
    // not the expiry height.
    OracleState::new(&db).put_oracle(&cb_types::OraclePrice {
        market: MARKET,
        px_q8: 1_000,
        source_height: EXPIRY + 40,
        authority: proposer,
    });
    let err = enforce_required_futures_settle(&db, EXPIRY + 41, proposer, &[]).unwrap_err();
    assert!(
        matches!(err, ExecError::MissingRequiredFuturesSettle { height, market } if height == EXPIRY + 41 && market == MARKET),
        "got {err:?}"
    );
    assert_eq!(
        cb_block::required_system_txs(&db, EXPIRY + 41),
        vec![TxBody::FuturesSettle { market: MARKET }]
    );
}

#[test]
fn a_future_is_frozen_from_its_expiry_height_even_before_it_settles() {
    // Core review M6. `expiry_height` is documented as the height after which
    // the market is frozen except for settlement, but the placement handler
    // only checked whether a settlement price had been written. With a
    // deferred settlement that gap can be many blocks, and the book was open
    // for all of them.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);
    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_000_000_000);

    // One block before expiry: accepted.
    mark_at(&db, 1_000);
    let mut state = BlockState::new(&db);
    let v = signed_place(&sk_alice, 0, MARKET, Side::Buy, 1_000, 5, 0);
    apply_tx_at(&mut state, &ctx_for(EXPIRY - 1, proposer), &v, 0).unwrap();
    state.commit().unwrap();

    // At expiry, unsettled: frozen.
    let mut state = BlockState::new(&db);
    let v = signed_place(&sk_alice, 1, MARKET, Side::Buy, 1_000, 5, 1);
    let err = apply_tx_at(&mut state, &ctx_for(EXPIRY, proposer), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MarketExpired);
    assert!(
        FuturesMarkets::new(&db)
            .get(&MARKET)
            .unwrap()
            .final_settlement_px_q8
            .is_none(),
        "the market has not settled; the freeze comes from the height alone"
    );
}

#[test]
fn enforce_required_settle_passes_when_settle_present() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    let v = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    enforce_required_futures_settle(&db, EXPIRY, proposer, &[v]).unwrap();
}

#[test]
fn enforce_required_settle_passes_at_non_expiry_heights() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    // Mid-life: no expiring market, no required settle.
    enforce_required_futures_settle(&db, 50, proposer, &[]).unwrap();
}

#[test]
fn enforce_required_settle_skips_already_settled_markets() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let mut m = future_market(proposer);
    m.final_settlement_px_q8 = Some(1_000);
    FuturesMarkets::new(&db).put(&m);

    // Already settled: a re-emission of FuturesSettle is NOT required.
    enforce_required_futures_settle(&db, EXPIRY, proposer, &[]).unwrap();
}

#[test]
fn enforce_required_settle_rejects_settle_signed_by_non_proposer() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (attacker_sk, _) = generate_keypair();
    setup_with_market(&db, proposer);
    OracleState::new(&db).put_mark_history(&MARKET, EXPIRY - 1, 1_000);

    let v = sign_tx_simple(&attacker_sk, 0, TxBody::FuturesSettle { market: MARKET });
    let err = enforce_required_futures_settle(&db, EXPIRY, proposer, &[v]).unwrap_err();
    assert!(matches!(
        err,
        ExecError::MissingRequiredFuturesSettle { .. }
    ));
}

// ---------------------------------------------------------------------------
// 3. End-to-end settlement at expiry
// ---------------------------------------------------------------------------

/// Helper: drive a chain across the expiry boundary using a minimal trader,
/// producing a build-and-apply path that exercises the required-tx check.
#[test]
fn settle_at_expiry_closes_positions_and_credits_collateral() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);
    Meta::new(&db).set_proposer_addr(proposer);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin(&db, alice, 1_000_000_000);
    fund_margin(&db, bob, 1_000_000_000);

    // Pre-expiry: alice longs 5 @ 1_000_000_000 (1e9 q8 = $10/unit) against bob's sell.
    // We use prices around 1e10 so settlement PnL divides cleanly by 1e10 to
    // micro-USD (`pnl_micro = qty * delta_q8 / 1e10`).
    let entry_q8 = 10u128 * 100_000_000; // $10 in q8 units = 1_000_000_000
    let mut state = BlockState::new(&db);
    let ctx_pre = ctx_for(50, proposer);
    mark_at(&db, entry_q8);
    let v0 = signed_place(&sk_alice, 0, MARKET, Side::Sell, entry_q8, 5, 0);
    apply_tx_at(&mut state, &ctx_pre, &v0, 0).unwrap();
    let v1 = signed_place(&sk_bob, 0, MARKET, Side::Buy, entry_q8, 5, 1);
    apply_tx_at(&mut state, &ctx_pre, &v1, 1).unwrap();
    state.commit().unwrap();

    // Push a single oracle update so mark history exists. Per-block oracle
    // posts are the production cadence; one is enough to seed TWAP for this
    // test.
    let store = OracleState::new(&db);
    let settle_target = 11u128 * 100_000_000; // $11 settlement
    store.put_mark_history(&MARKET, EXPIRY - 1, settle_target);

    // Apply FuturesSettle at expiry directly.
    let mut state = BlockState::new(&db);
    let ctx_settle = ctx_for(EXPIRY, proposer);
    let v_settle = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    apply_tx_at(&mut state, &ctx_settle, &v_settle, 0).unwrap();
    state.commit().unwrap();

    // Assertions:
    // - Final settlement price recorded.
    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(fm.final_settlement_px_q8, Some(settle_target));
    // - Both positions deleted.
    let positions = Positions::new(&db);
    assert!(positions.get(&alice, &MARKET).is_none());
    assert!(positions.get(&bob, &MARKET).is_none());
    // - Mark history cleared.
    assert!(store.list_mark_history(&MARKET).is_empty());
    // - Collateral updated.
    //   alice short 5 @ 10, settle @ 11 → loss = 5 * (10 - 11) = -5 (in q8 units, x1e8 = -5e8 → -0 micro after /1e10)
    //   Settle delta in q8: 5 * (11e8 - 10e8) = 5e9, divided by 1e10 = 0 micro-USD
    //   Better example: scale prices up. Let's recompute with our values:
    //   delta_q8 = 1e8 (= $1). pnl_raw = qty * delta_q8 = 5 * 1e8 = 5e8.
    //   pnl_micro = 5e8 / 1e10 = 0 (integer div).
    //   That's degenerate at this scale — the settlement PnL math rounds to 0
    //   for a $1-per-unit move on 5 units, which is too small for micro-USD.
    //   We assert the conservation property below with a larger scale.
    let alice_margin = MarginAccounts::new(&db).get(&alice).unwrap();
    let bob_margin = MarginAccounts::new(&db).get(&bob).unwrap();
    // At this small scale both should be unchanged from the 1e9 starting balance.
    assert_eq!(alice_margin.collateral, 1_000_000_000);
    assert_eq!(bob_margin.collateral, 1_000_000_000);
}

#[test]
fn settle_pnl_conservation_long_short_sum_to_zero_micro() {
    // PnL conservation property: at a scale where settlement deltas land in
    // measurable micro-USD, alice's loss equals bob's gain (modulo rounding).
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin(&db, alice, 1_000_000_000_000);
    fund_margin(&db, bob, 1_000_000_000_000);

    // Use a large entry/exit so PnL is large enough to register in micro-USD.
    let entry_q8 = 50_000u128 * 100_000_000; // $50,000
    let qty = 100_000_000u128; // 1.0 units (e.g. 1 BTC)
    let mut state = BlockState::new(&db);
    let ctx_pre = ctx_for(50, proposer);
    mark_at(&db, entry_q8);
    let v0 = signed_place(&sk_alice, 0, MARKET, Side::Sell, entry_q8, qty, 0);
    apply_tx_at(&mut state, &ctx_pre, &v0, 0).unwrap();
    let v1 = signed_place(&sk_bob, 0, MARKET, Side::Buy, entry_q8, qty, 1);
    apply_tx_at(&mut state, &ctx_pre, &v1, 1).unwrap();
    state.commit().unwrap();

    // Settle at $51,000 → bob (long) gains, alice (short) loses.
    let settle_target = 51_000u128 * 100_000_000;
    let store = OracleState::new(&db);
    store.put_mark_history(&MARKET, EXPIRY - 1, settle_target);

    let alice_pre = MarginAccounts::new(&db).get(&alice).unwrap().collateral;
    let bob_pre = MarginAccounts::new(&db).get(&bob).unwrap().collateral;

    let mut state = BlockState::new(&db);
    let ctx_settle = ctx_for(EXPIRY, proposer);
    let v_settle = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    apply_tx_at(&mut state, &ctx_settle, &v_settle, 0).unwrap();
    state.commit().unwrap();

    let alice_post = MarginAccounts::new(&db).get(&alice).unwrap().collateral as i128;
    let bob_post = MarginAccounts::new(&db).get(&bob).unwrap().collateral as i128;
    let alice_delta = alice_post - alice_pre as i128;
    let bob_delta = bob_post - bob_pre as i128;

    // alice short 1 BTC @ $50k, settle $51k → -$1000 → -1_000_000_000 micro-USD
    assert_eq!(alice_delta, -1_000_000_000);
    // bob long 1 BTC @ $50k, settle $51k → +$1000 → +1_000_000_000 micro-USD
    assert_eq!(bob_delta, 1_000_000_000);
    // Conservation: net collateral movement is zero.
    assert_eq!(alice_delta + bob_delta, 0);
}

// ---------------------------------------------------------------------------
// 4. Post-expiry orders rejected
// ---------------------------------------------------------------------------

#[test]
fn post_expiry_place_perp_order_is_rejected() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_000_000_000);

    // Settle the market by hand (set final_settlement_px_q8) — exercising the
    // post-settle rejection path without driving a full chain to expiry.
    let mut fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    fm.final_settlement_px_q8 = Some(50_000 * 100_000_000);
    FuturesMarkets::new(&db).put(&fm);

    let mut state = BlockState::new(&db);
    let ctx = ctx_for(EXPIRY + 5, proposer);
    let v = signed_place(&sk_alice, 0, MARKET, Side::Buy, 1_000, 5, 0);
    let err = apply_tx_at(&mut state, &ctx, &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MarketExpired);

    // Re-emitting FuturesSettle is rejected as AlreadySettled.
    let mut state = BlockState::new(&db);
    let v = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    let err = apply_tx_at(&mut state, &ctx, &v, 0).unwrap_err();
    assert_eq!(err, ExecError::AlreadySettled);
}

// ---------------------------------------------------------------------------
// 5. Pre-expiry FuturesSettle is rejected
// ---------------------------------------------------------------------------

#[test]
fn settle_before_expiry_is_rejected_as_not_yet_expiry() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    let mut state = BlockState::new(&db);
    let ctx = ctx_for(EXPIRY - 1, proposer);
    let v = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    let err = apply_tx_at(&mut state, &ctx, &v, 0).unwrap_err();
    assert!(matches!(err, ExecError::NotYetExpiry { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// 6. Settlement cancels resting orders
// ---------------------------------------------------------------------------

#[test]
fn settle_cancels_resting_orders_for_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    fund_margin(&db, alice, 1_000_000_000);

    // Alice posts two resting orders that don't fill (no opposing book).
    let mut state = BlockState::new(&db);
    let ctx_pre = ctx_for(50, proposer);
    mark_at(&db, 100);
    let v0 = signed_place(&sk_alice, 0, MARKET, Side::Buy, 100, 1, 0);
    apply_tx_at(&mut state, &ctx_pre, &v0, 0).unwrap();
    let v1 = signed_place(&sk_alice, 1, MARKET, Side::Buy, 99, 1, 1);
    apply_tx_at(&mut state, &ctx_pre, &v1, 1).unwrap();
    state.commit().unwrap();

    // Both rest in the book.
    let perp_orders = PerpOrders::new(&db);
    let perp_book = PerpBookIndex::new(&db);
    assert_eq!(perp_orders.list_for_market(&MARKET).len(), 2);
    assert_eq!(perp_book.list_side(&MARKET, Side::Buy).len(), 2);

    // Pre-seed mark history so settlement has a price.
    OracleState::new(&db).put_mark_history(&MARKET, EXPIRY - 1, 100);

    // Settle.
    let mut state = BlockState::new(&db);
    let ctx_settle = ctx_for(EXPIRY, proposer);
    let v_settle = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    apply_tx_at(&mut state, &ctx_settle, &v_settle, 0).unwrap();
    state.commit().unwrap();

    // No more resting orders for the market; book index empty.
    assert!(perp_orders.list_for_market(&MARKET).is_empty());
    assert!(perp_book.list_side(&MARKET, Side::Buy).is_empty());
    assert!(perp_book.list_side(&MARKET, Side::Sell).is_empty());
}

// ---------------------------------------------------------------------------
// 7. Build-and-apply: a block crossing expiry without FuturesSettle is rejected
// ---------------------------------------------------------------------------

#[test]
fn build_and_apply_rejects_expiry_block_missing_settle() {
    // We use EXPIRY = 1 so we can test directly at block height 1.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let mut m = future_market(proposer);
    m.expiry_height = 1;
    FuturesMarkets::new(&db).put(&m);
    Meta::new(&db).set_proposer_addr(proposer);

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    {
        let mut s = cb_state::AccountState::default();
        s.add(AssetId::Usd, 1_000_000).expect("seed balance");
        cb_state::Accounts::new(&db).put(&alice, &s);
    }

    let sign_block = |preimage: &[u8]| -> Vec<u8> {
        let digest = keccak256(preimage);
        let sig = crypto_sign(&proposer_sk, digest).unwrap();
        sig.to_vec()
    };

    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };

    // A settle is only required once a settlement price exists; give the
    // TWAP window one sample so it does.
    OracleState::new(&db).put_mark_history(&MARKET, 0, 1_000);

    // Build block 1 (= expiry) with only a transfer (no FuturesSettle).
    let v_xfer = sign_tx_simple(
        &alice_sk,
        0,
        TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let err = match build_and_apply(&db, &ctx, vec![v_xfer], 1, 0, Vec::new(), sign_block) {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    };
    assert!(
        matches!(
            err,
            ExecError::MissingRequiredFuturesSettle { height: 1, .. }
        ),
        "got {err:?}"
    );

    // Add the FuturesSettle and the block lands.
    let v_xfer = sign_tx_simple(
        &alice_sk,
        0,
        TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let v_settle = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    build_and_apply(
        &db,
        &ctx,
        vec![v_xfer, v_settle],
        1,
        0,
        Vec::new(),
        sign_block,
    )
    .unwrap();
    assert_eq!(Meta::new(&db).head_height(), 1);
    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(fm.final_settlement_px_q8, Some(1_000));
}

// ---------------------------------------------------------------------------
// 8. Mark-history persistence via OracleUpdate
// ---------------------------------------------------------------------------

#[test]
fn oracle_update_writes_mark_history_for_dated_futures() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (authority_sk, authority_pk) = generate_keypair();
    let authority = address_from_pubkey(&authority_pk);
    setup_with_market(&db, authority);

    let mut state = BlockState::new(&db);
    let ctx = ctx_for(50, Address::ZERO);
    // submit_height must be >= existing oracle's source_height; first time, any positive value works.
    let v = sign_tx_simple(
        &authority_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 12_345_678,
            submitted_height: 50,
        },
    );
    apply_tx_at(&mut state, &ctx, &v, 0).unwrap();
    state.commit().unwrap();

    let store = OracleState::new(&db);
    let history = store.list_mark_history(&MARKET);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].0, 50);
    // Mark equals oracle (no book mid yet); under TIGHT/WIDE clamp the
    // median of (oracle, oracle, oracle) is oracle.
    assert_eq!(history[0].1, 12_345_678);
}

// ---------------------------------------------------------------------------
// 9. FundingTick + FuturesSettle on the same block (expiry_height happens to
//    land on a funding-interval boundary). Both system txs are required and
//    must coexist deterministically.
// ---------------------------------------------------------------------------

#[test]
fn block_at_funding_boundary_and_futures_expiry_accepts_both_system_txs() {
    use cb_types::PerpMarket;

    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    Meta::new(&db).set_proposer_addr(proposer);

    // Future expires at block 5; perp ticks every 5 blocks. Block 5 needs
    // both a FundingTick (perp) and a FuturesSettle (future).
    let mut fm = future_market(proposer);
    fm.expiry_height = 5;
    FuturesMarkets::new(&db).put(&fm);
    cb_state::PerpMarkets::new(&db).put(&PerpMarket {
        id: DerivId::perp(0),
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 5,
        funding_rate_cap_q9: 1_000_000,
        oracle_authority: proposer,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    // Mark history so the dated future has a settlement price.
    OracleState::new(&db).put_mark_history(&MARKET, 4, 1_234);

    // Walk head_height from 0 to 4 with empty-ish blocks (no boundaries hit).
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    {
        let mut s = cb_state::AccountState::default();
        s.add(AssetId::Usd, 1_000_000).expect("seed balance");
        cb_state::Accounts::new(&db).put(&alice, &s);
    }
    let sign_block = |preimage: &[u8]| -> Vec<u8> {
        let digest = keccak256(preimage);
        let sig = crypto_sign(&proposer_sk, digest).unwrap();
        sig.to_vec()
    };
    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };
    for n in 0..4u64 {
        let v = sign_tx_simple(
            &alice_sk,
            n,
            TxBody::Transfer {
                to: Address([2; 20]),
                asset: AssetId::Usd,
                amount: 1,
            },
        );
        build_and_apply(&db, &ctx, vec![v], 1, 0, Vec::new(), sign_block).unwrap();
    }
    assert_eq!(Meta::new(&db).head_height(), 4);

    // Block 5 needs BOTH FundingTick and FuturesSettle from the proposer.
    // Their nonces ratchet up since they share the proposer address (same
    // mempool-style nonce sequence).
    let v_xfer = sign_tx_simple(
        &alice_sk,
        4,
        TxBody::Transfer {
            to: Address([2; 20]),
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let v_tick = sign_tx_simple(
        &proposer_sk,
        0,
        TxBody::FundingTick {
            market: DerivId::perp(0),
        },
    );
    let v_settle = sign_tx_simple(&proposer_sk, 1, TxBody::FuturesSettle { market: MARKET });
    build_and_apply(
        &db,
        &ctx,
        vec![v_xfer, v_tick, v_settle],
        1,
        0,
        Vec::new(),
        sign_block,
    )
    .unwrap();
    assert_eq!(Meta::new(&db).head_height(), 5);
    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(fm.final_settlement_px_q8, Some(1_234));
}

// ---------------------------------------------------------------------------
// Liquidation works on a dated future.
//
// Found by running the simulation with a future listed into a crash. The
// liquidation handler resolved its market through `PerpMarkets` alone, so a
// `LiquidationTrigger` naming a dated future was rejected with
// `UnknownMarket` — 1 536 rejected triggers in a 400-block run — and a
// leveraged futures position could never be liquidated at all. The account
// simply sat below maintenance margin until expiry. Margin is cross-market, so
// there is no version of this where the liquidation engine gets to know about
// only one of the two contract kinds.
//
//   cargo run -p cb-simgame -- run --blocks 400 \
//     --scenario "calm:100,crash:150,calm" --future-expiry 300 --seed 11
// ---------------------------------------------------------------------------

#[test]
fn a_leveraged_position_on_a_dated_future_can_be_liquidated() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (oracle_sk, oracle_pk) = generate_keypair();
    let authority = address_from_pubkey(&oracle_pk);
    setup_with_market(&db, authority);

    let q8: u128 = 100_000_000;
    let micro: u128 = 1_000_000;
    let one_unit: u128 = 100_000_000;

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, pk_keeper) = generate_keypair();
    let keeper = address_from_pubkey(&pk_keeper);
    // Alice is thinly margined; bob is the deep counterparty and the bid the
    // liquidation will close into.
    fund_margin(&db, alice, 2_300 * micro);
    fund_margin(&db, bob, 10_000_000 * micro);
    fund_margin(&db, keeper, 0);

    // Block 1: an oracle print, so the market has a mark.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_for(1, Address::ZERO),
        &sign_tx_simple(
            &oracle_sk,
            0,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 50_000 * q8,
                submitted_height: 1,
            },
        ),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Block 2: alice goes long one unit at 50k; bob stacks bids underneath.
    let mut state = BlockState::new(&db);
    let txs = [
        signed_place(&sk_bob, 0, MARKET, Side::Sell, 50_000 * q8, one_unit, 1),
        sign_tx_simple(
            &sk_alice,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Buy,
                price_q8: 50_000 * q8,
                qty: one_unit,
                tif: Tif::Ioc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([2; 32]),
            },
        ),
        signed_place(&sk_bob, 1, MARKET, Side::Buy, 48_500 * q8, one_unit, 3),
        signed_place(&sk_bob, 2, MARKET, Side::Buy, 48_000 * q8, one_unit, 4),
    ];
    for (i, v) in txs.iter().enumerate() {
        apply_tx_at(&mut state, &ctx_for(2, Address::ZERO), v, i as u32).unwrap();
    }
    state.commit().unwrap();
    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        one_unit
    );

    // Block 3: the market falls and alice goes under maintenance.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_for(3, Address::ZERO),
        &sign_tx_simple(
            &oracle_sk,
            1,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 48_000 * q8,
                submitted_height: 3,
            },
        ),
        0,
    )
    .unwrap();
    state.commit().unwrap();
    assert!(
        cb_engine::is_liquidatable(
            &MarginAccounts::new(&db).get(&alice).unwrap(),
            &Positions::new(&db).list_for_owner(&alice),
            &[(MARKET, 48_000 * q8)],
            cb_engine::DEFAULT_TIERS,
        ),
        "alice must be liquidatable for this test to mean anything"
    );

    // Block 4: the keeper triggers. This used to return `UnknownMarket`.
    let mut state = BlockState::new(&db);
    let result = apply_tx_at(
        &mut state,
        &ctx_for(4, Address::ZERO),
        &sign_tx_simple(
            &sk_keeper,
            0,
            TxBody::LiquidationTrigger {
                owner: alice,
                market: MARKET,
            },
        ),
        0,
    );
    state.commit().unwrap();
    assert_eq!(
        result,
        Ok(()),
        "a LiquidationTrigger naming a dated future must be accepted"
    );

    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        0,
        "and it must actually close the position"
    );
    assert!(
        MarginAccounts::new(&db)
            .get(&keeper)
            .map(|m| m.collateral)
            .unwrap_or(0)
            > 0,
        "and pay the keeper its bounty"
    );
    // The close publishes a last trade on the FutureMarket record, through the
    // same `MarketView` the place-order path uses.
    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert!(fm.last_trade_px_q8 > 0);
    assert_eq!(fm.last_trade_height, 4);
}

#[test]
fn a_liquidation_trigger_on_an_unlisted_market_is_still_rejected() {
    // The kind-agnostic lookup must not turn `UnknownMarket` into a silent
    // success for a market that genuinely does not exist.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (_oracle_sk, oracle_pk) = generate_keypair();
    setup_with_market(&db, address_from_pubkey(&oracle_pk));
    let (sk_keeper, _pk_keeper) = generate_keypair();

    let mut state = BlockState::new(&db);
    let result = apply_tx_at(
        &mut state,
        &ctx_for(1, Address::ZERO),
        &sign_tx_simple(
            &sk_keeper,
            0,
            TxBody::LiquidationTrigger {
                owner: Address([9; 20]),
                market: DerivId::future(77),
            },
        ),
        0,
    );
    assert_eq!(result, Err(ExecError::UnknownMarket));
}

#[test]
fn a_future_that_exhausts_the_insurance_fund_enters_the_reduce_only_halt() {
    // Core review L4. A perpetual whose bad debt outran the insurance fund
    // flipped `paused_reduce_only_only`; a future had no such flag, so it
    // kept accepting opening orders with an empty fund behind it.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    setup_with_market(&db, proposer);
    Meta::new(&db).set_proposer_addr(proposer);

    // Alice long 5 at 1_000 with next to no collateral; Bob short.
    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin(&db, alice, 1_000_000_000);
    fund_margin(&db, bob, 1_000_000_000);
    // No insurance fund at all.
    mark_at(&db, 1_000 * 100_000_000);
    let mut state = BlockState::new(&db);
    let ctx = ctx_for(50, proposer);
    let v0 = signed_place(
        &sk_alice,
        0,
        MARKET,
        Side::Buy,
        1_000 * 100_000_000,
        5 * 100_000_000,
        0,
    );
    apply_tx_at(&mut state, &ctx, &v0, 0).unwrap();
    let v1 = signed_place(
        &sk_bob,
        0,
        MARKET,
        Side::Sell,
        1_000 * 100_000_000,
        5 * 100_000_000,
        1,
    );
    apply_tx_at(&mut state, &ctx, &v1, 1).unwrap();
    state.commit().unwrap();
    // Take Alice's collateral away so settlement at a low price is bad debt.
    fund_margin(&db, alice, 1);

    // Settle at 1 (TWAP sample): Alice loses ~5,000 USD she does not have.
    OracleState::new(&db).put_mark_history(&MARKET, EXPIRY - 1, 100_000_000);
    let mut state = BlockState::new(&db);
    let settle = sign_tx_simple(&proposer_sk, 0, TxBody::FuturesSettle { market: MARKET });
    apply_tx_at(&mut state, &ctx_for(EXPIRY, proposer), &settle, 0).unwrap();
    state.commit().unwrap();

    let fm = FuturesMarkets::new(&db).get(&MARKET).unwrap();
    assert!(
        MarginAccounts::new(&db).get(&alice).unwrap().bad_debt > 0,
        "the loss became bad debt"
    );
    assert!(
        fm.paused_reduce_only_only,
        "the future entered the reduce-only halt"
    );
}
