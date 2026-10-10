//! M3 integration test: place 5 perp orders across 2 blocks, assert book and
//! position state at each step. Plus negative tests for the perp executor's
//! admission checks.

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, keypair_from_seed, SecretKey};
use cb_state::{Database, MarginAccounts, Meta, PerpBookIndex, PerpMarkets, PerpOrders, Positions};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn setup_db_with_perp_market(db: &Database) {
    PerpMarkets::new(db).put(&PerpMarket {
        id: MARKET,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 3600,
        funding_rate_cap_q9: 1_000_000,
        oracle_authority: Address([3; 20]),
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    // Every derivative order is priced against the band around the mark
    // (±10%); these tests trade between 95 and 110.
    cb_state::OracleState::new(db).put_mark(&cb_types::MarkPrice {
        market: MARKET,
        px_q8: 100,
        computed_at_height: 0,
        premium_ema_q9: 0,
    });
}

fn fund_margin_account(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: cb_types::Signed::ZERO,
        bad_debt: 0,
    });
}

#[allow(clippy::too_many_arguments)]
fn signed_place(
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
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::PlacePerpOrder {
            market,
            side,
            price_q8,
            qty,
            tif,
            post_only,
            reduce_only,
            client_order_id: H256([cid; 32]),
        },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

fn signed_cancel(sk: &SecretKey, nonce: u64, order_id: H256) -> VerifiedTx {
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::CancelPerpOrder { order_id },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

fn ctx_at(height: u64) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: Address::ZERO,
    }
}

// ---------------------------------------------------------------------------
// 5-order, 2-block lifecycle
// ---------------------------------------------------------------------------

#[test]
fn five_orders_across_two_blocks_book_and_positions_evolve() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    // Two traders, each with a margin account.
    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin_account(&db, alice, 1_000_000);
    fund_margin_account(&db, bob, 1_000_000);

    // -----------------------------------------------------------------------
    // BLOCK 1 (height = 1):
    //   tx 0: alice GTC sell 10 @ 100 -> rests.
    //   tx 1: alice GTC sell 5  @ 110 -> rests.
    //   tx 2: bob   IOC buy  10 @ 100 -> fills against alice's sell @ 100 (full).
    // After block 1:
    //   - 1 resting ask remaining (alice sell 5 @ 110)
    //   - alice short 10 @ 100, bob long 10 @ 100
    //   - last_trade = 100
    // -----------------------------------------------------------------------
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk_alice,
            0,
            MARKET,
            Side::Sell,
            100,
            10,
            Tif::Gtc,
            false,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk_alice,
            1,
            MARKET,
            Side::Sell,
            110,
            5,
            Tif::Gtc,
            false,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk_bob,
            0,
            MARKET,
            Side::Buy,
            100,
            10,
            Tif::Ioc,
            false,
            false,
            3,
        ),
        2,
    )
    .unwrap();
    state.commit().unwrap();

    // Verify book after block 1.
    let book = PerpBookIndex::new(&db);
    let asks = book.list_side(&MARKET, Side::Sell);
    assert_eq!(asks.len(), 1, "only the 110-priced ask should remain");
    assert_eq!(asks[0].0, 110);

    // Verify positions after block 1.
    let positions = Positions::new(&db);
    let alice_pos = positions
        .get(&alice, &MARKET)
        .expect("alice has a position");
    let bob_pos = positions.get(&bob, &MARKET).expect("bob has a position");
    assert_eq!(alice_pos.side, Side::Sell);
    assert_eq!(alice_pos.qty, 10);
    assert_eq!(alice_pos.entry_px_q8, 100);
    assert_eq!(bob_pos.side, Side::Buy);
    assert_eq!(bob_pos.qty, 10);
    assert_eq!(bob_pos.entry_px_q8, 100);

    // Verify last-trade price persisted on the perp market record.
    let pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(pm.last_trade_px_q8, 100);
    assert_eq!(pm.last_trade_height, 1);

    // -----------------------------------------------------------------------
    // BLOCK 2 (height = 2):
    //   tx 0: bob   GTC buy  3 @ 95 -> rests (won't cross alice's 110 ask).
    //   tx 1: alice IOC sell 3 @ 95 -> fills against bob's bid @ 95.
    //
    //   Bob is the maker on this trade — he was already long; the new buy
    //   ADDS to his long position, taking him from long 10 @ 100 to long
    //   13 @ VWAP((10*100 + 3*95)/13) = 1285/13 = 98 (round toward zero).
    //   Alice is the taker; the new sell ADDS to her short, taking her from
    //   short 10 @ 100 to short 13 @ VWAP((10*100 + 3*95)/13) = 98.
    //
    // After block 2:
    //   - 1 resting ask at 110 qty 5 (alice's old GTC, untouched)
    //   - alice short 13 @ 98
    //   - bob   long  13 @ 98
    //   - last_trade = 95
    // -----------------------------------------------------------------------
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(
            &sk_bob,
            1,
            MARKET,
            Side::Buy,
            95,
            3,
            Tif::Gtc,
            false,
            false,
            4,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(
            &sk_alice,
            2,
            MARKET,
            Side::Sell,
            95,
            3,
            Tif::Ioc,
            false,
            false,
            5,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // Verify after block 2.
    let asks = book.list_side(&MARKET, Side::Sell);
    let bids = book.list_side(&MARKET, Side::Buy);
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].0, 110);
    assert_eq!(bids.len(), 0, "bob's bid was fully consumed");

    let alice_pos = positions.get(&alice, &MARKET).unwrap();
    let bob_pos = positions.get(&bob, &MARKET).unwrap();
    assert_eq!(alice_pos.side, Side::Sell);
    assert_eq!(alice_pos.qty, 13);
    // VWAP: (10*100 + 3*95) / 13 = 1285 / 13 = 98 (round toward zero)
    assert_eq!(alice_pos.entry_px_q8, 98);
    assert_eq!(bob_pos.side, Side::Buy);
    assert_eq!(bob_pos.qty, 13);
    assert_eq!(bob_pos.entry_px_q8, 98);

    let pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(pm.last_trade_px_q8, 95);
    assert_eq!(pm.last_trade_height, 2);
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

#[test]
fn cancel_of_nonexistent_order_is_idempotent() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin_account(&db, alice, 1_000_000);

    let mut state = BlockState::new(&db);
    let v = signed_cancel(&sk, 0, H256([0xff; 32]));
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).expect("cancel of unknown id is a no-op success");
    state.commit().unwrap();

    // Nonce advanced.
    use cb_state::Accounts;
    let acct = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(acct.nonce, 1);
}

#[test]
fn cancel_removes_resting_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin_account(&db, alice, 1_000_000);

    // Place a resting GTC.
    let mut state = BlockState::new(&db);
    let v = signed_place(
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
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
    state.commit().unwrap();

    // Find the resting order's id.
    let book = PerpBookIndex::new(&db);
    let bids = book.list_side(&MARKET, Side::Buy);
    assert_eq!(bids.len(), 1);
    let resting_oid = bids[0].2;
    assert!(PerpOrders::new(&db).get(&resting_oid).is_some());

    // Cancel it.
    let mut state = BlockState::new(&db);
    let v = signed_cancel(&sk, 1, resting_oid);
    apply_tx_at(&mut state, &ctx_at(2), &v, 0).unwrap();
    state.commit().unwrap();

    let bids = book.list_side(&MARKET, Side::Buy);
    assert_eq!(bids.len(), 0);
    assert!(PerpOrders::new(&db).get(&resting_oid).is_none());
}

#[test]
fn cancel_by_non_owner_rejected() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, _pk_bob) = generate_keypair();
    fund_margin_account(&db, alice, 1_000_000);

    // Alice places.
    let mut state = BlockState::new(&db);
    let v = signed_place(
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
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
    state.commit().unwrap();

    let book = PerpBookIndex::new(&db);
    let bids = book.list_side(&MARKET, Side::Buy);
    let resting_oid = bids[0].2;

    // Bob tries to cancel.
    let mut state = BlockState::new(&db);
    let v = signed_cancel(&sk_bob, 0, resting_oid);
    let err = apply_tx_at(&mut state, &ctx_at(2), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);
}

// ---------------------------------------------------------------------------
// Admission checks
// ---------------------------------------------------------------------------

#[test]
fn place_perp_rejects_unknown_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    // No perp market registered.
    let (sk, pk) = generate_keypair();
    fund_margin_account(&db, address_from_pubkey(&pk), 1_000_000);

    let mut state = BlockState::new(&db);
    let v = signed_place(
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
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}

#[test]
fn place_perp_rejects_missing_margin_account() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk, _pk) = generate_keypair();
    // No margin account funded.
    let mut state = BlockState::new(&db);
    let v = signed_place(
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
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NoMarginAccount);
}

#[test]
fn place_perp_rejects_paused_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let mut pm = PerpMarket {
        id: MARKET,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 3600,
        funding_rate_cap_q9: 1_000_000,
        oracle_authority: Address([3; 20]),
        max_position_notional: u128::MAX,
        paused: true,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    };
    pm.paused = true;
    PerpMarkets::new(&db).put(&pm);

    let (sk, pk) = generate_keypair();
    fund_margin_account(&db, address_from_pubkey(&pk), 1_000_000);

    let mut state = BlockState::new(&db);
    let v = signed_place(
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
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MarketPaused);
}

#[test]
fn reduce_only_without_opposing_position_rejected() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk, pk) = generate_keypair();
    fund_margin_account(&db, address_from_pubkey(&pk), 1_000_000);

    // Reduce-only buy with no position at all → reject.
    let mut state = BlockState::new(&db);
    let v = signed_place(&sk, 0, MARKET, Side::Buy, 100, 10, Tif::Gtc, false, true, 1);
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyNoPos);
}

#[test]
fn post_only_cross_rejected_at_apply_time() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin_account(&db, alice, 1_000_000);
    fund_margin_account(&db, bob, 1_000_000);

    // Alice rests an ask at 100.
    let mut state = BlockState::new(&db);
    let v = signed_place(
        &sk_alice,
        0,
        MARKET,
        Side::Sell,
        100,
        10,
        Tif::Gtc,
        false,
        false,
        1,
    );
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
    state.commit().unwrap();

    // Bob tries a post-only buy at 100 → would cross → reject.
    let mut state = BlockState::new(&db);
    let v = signed_place(
        &sk_bob,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Gtc,
        true,
        false,
        2,
    );
    let err = apply_tx_at(&mut state, &ctx_at(2), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::OrderRejected);
}

#[test]
fn fok_kill_when_book_too_thin_rejected() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_db_with_perp_market(&db);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    fund_margin_account(&db, alice, 1_000_000);
    fund_margin_account(&db, bob, 1_000_000);

    // Alice has only 5 @ 100 to sell.
    let mut state = BlockState::new(&db);
    let v = signed_place(
        &sk_alice,
        0,
        MARKET,
        Side::Sell,
        100,
        5,
        Tif::Gtc,
        false,
        false,
        1,
    );
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
    state.commit().unwrap();

    // Bob's FOK buy 10 @ 100 cannot fill → reject. No fills, no mutations.
    let mut state = BlockState::new(&db);
    let v = signed_place(
        &sk_bob,
        0,
        MARKET,
        Side::Buy,
        100,
        10,
        Tif::Fok,
        false,
        false,
        2,
    );
    let err = apply_tx_at(&mut state, &ctx_at(2), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::OrderRejected);

    // Alice's resting ask is still there.
    let book = PerpBookIndex::new(&db);
    let asks = book.list_side(&MARKET, Side::Sell);
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].0, 100);
}

#[test]
fn determinism_check_replay_yields_identical_state_root() {
    // Replay the same sequence of perp txs against two fresh databases and
    // verify the resulting `accounts_root` (which captures nonce changes from
    // every tx that touched the executor) is identical. This is the M3
    // smoke test for the "replicas reproduce state" contract.
    fn run() -> H256 {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Meta::new(&db).set_chain_id(1);
        setup_db_with_perp_market(&db);

        // Use a fixed seed so both runs share the same keypair.
        let (sk_alice, pk_alice) = keypair_from_seed(b"alice-fixed-seed");
        let alice = address_from_pubkey(&pk_alice);
        fund_margin_account(&db, alice, 1_000_000);

        let mut state = BlockState::new(&db);
        let v = signed_place(
            &sk_alice,
            0,
            MARKET,
            Side::Sell,
            100,
            5,
            Tif::Gtc,
            false,
            false,
            1,
        );
        apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
        // The state root is the deterministic surface — same txs + same
        // height ⇒ same root, no matter the OS allocator's whim.
        state.commit().unwrap().state_root
    }
    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "two independent runs must agree on the state root");
}
