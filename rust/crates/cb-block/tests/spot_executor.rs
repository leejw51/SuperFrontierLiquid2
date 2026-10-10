//! S3 integration tests for the spot CLOB executor handlers.
//!
//! Mirrors the structure of `perp_lifecycle.rs` / `perp_negative.rs` but
//! against `TxBody::PlaceOrder` / `CancelOrder` / `BatchCancel`. Each test
//! constructs a fresh tempdir RocksDB, seeds genesis-shaped market config
//! and account balances, then drives the executor through the scenario
//! and asserts on the final CF state.
//!
//! Coverage:
//!
//!   1. `place_buy_on_empty_book_rests_and_locks_quote`
//!   2. `place_sell_on_empty_book_rests_and_locks_base`
//!   3. `crossing_buy_against_resting_sell_settles_with_taker_fee`
//!   4. `cancel_releases_locked_quote_and_clears_index`
//!   5. `cancel_someone_elses_order_returns_not_authorized`
//!   6. `place_with_qty_not_multiple_of_min_qty_is_bad_lot`
//!   7. `place_with_price_not_on_tick_is_bad_tick`
//!   8. `place_buy_with_insufficient_quote_returns_insufficient`
//!   9. `post_only_that_crosses_returns_post_only_crossed_and_unlocks`
//!  10. `self_trade_is_rejected_and_unlocks`
//!  11. `batch_cancel_clears_only_senders_orders`
//!  12. `cancel_unknown_order_returns_order_not_found`
//!  13. `place_against_unknown_market_returns_market_not_found`

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_state::{Accounts, BookIndex, Database, MarketConfig, Markets, Meta, Orders};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use tempfile::tempdir;

const Q8: u128 = 100_000_000;
// Genesis values from `genesis-perps.jsonl`: BTC/USD spot is tick=1, min=1,
// fee=10 bps. Tests pin these explicitly so a future genesis tweak doesn't
// silently change expected balances.
const TICK: u128 = 1;
const MIN_QTY: u128 = 1;
const FEE_BPS: u16 = 10;
const MARKET: MarketId = MarketId::BtcUsd;

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn seed_btc_usd_market(db: &Database) {
    Markets::new(db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: TICK,
        min_qty: MIN_QTY,
        fee_bps: FEE_BPS,
    });
}

fn fund(db: &Database, addr: Address, asset: AssetId, amount: u128) {
    let accts = Accounts::new(db);
    let mut s = accts.get(&addr).unwrap_or_default();
    s.add(asset, amount).expect("seed balance");
    accts.put(&addr, &s);
}

#[allow(clippy::too_many_arguments)]
fn signed_place(
    sk: &SecretKey,
    nonce: u64,
    side: Side,
    price: u128,
    qty: u128,
    tif: Tif,
    post_only: bool,
    cid: u8,
) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::PlaceOrder {
                market: MARKET,
                side,
                price,
                qty,
                tif,
                post_only,
                client_order_id: H256([cid; 32]),
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn signed_cancel(sk: &SecretKey, nonce: u64, order_id: H256) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::CancelOrder { order_id },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn signed_batch_cancel(sk: &SecretKey, nonce: u64) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::BatchCancel { market: MARKET },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn ctx_at(height: u64) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: Address::ZERO,
    }
}

/// Compute the buy-side lock formula the executor uses, mirrored in tests so
/// a math drift in either side surfaces immediately.
fn buy_lock(price: u128, qty: u128) -> u128 {
    let notional = price * qty / Q8;
    let fee_reserve = (notional * FEE_BPS as u128).div_ceil(10_000);
    notional + fee_reserve
}

// ---------------------------------------------------------------------------
// 1. Place buy on empty book → rests; quote locked; nonce advances.
// ---------------------------------------------------------------------------

#[test]
fn place_buy_on_empty_book_rests_and_locks_quote() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 10_000);

    // Buy 100 base @ price 1 (1e8 scale: 1*100/1e8 = 0 — too small). Use
    // bigger numbers: price 5_000_000_000 (= $50) per base unit, qty 100.
    let price = 5_000_000_000u128; // $50 in q8 notional per unit base.
    let qty = 100u128;
    let lock = buy_lock(price, qty);
    let verified = signed_place(&sk, 0, Side::Buy, price, qty, Tif::Gtc, false, 1);

    let mut state = BlockState::new(&db);
    apply_tx_at(&mut state, &ctx_at(1), &verified, 0).unwrap();
    state.commit().unwrap();

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.balance(AssetId::Usd), 10_000, "balance unchanged");
    assert_eq!(after.locked_of(AssetId::Usd), lock);
    assert_eq!(after.nonce, 1);

    // One resting order in CF_ORDERS + one row in CF_BOOK_INDEX.
    let resting = Orders::new(&db).list_for_market(MARKET);
    assert_eq!(resting.len(), 1);
    assert_eq!(resting[0].owner, alice);
    assert_eq!(resting[0].qty_remaining, qty);
    let bids = BookIndex::new(&db).list_side(MARKET, Side::Buy);
    assert_eq!(bids.len(), 1);
    assert_eq!(bids[0].0, price);
}

#[test]
fn place_sell_on_empty_book_rests_and_locks_base() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Btc, 1_000);

    let verified = signed_place(&sk, 0, Side::Sell, 5_000_000_000, 200, Tif::Gtc, false, 1);
    let mut state = BlockState::new(&db);
    apply_tx_at(&mut state, &ctx_at(1), &verified, 0).unwrap();
    state.commit().unwrap();

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.balance(AssetId::Btc), 1_000);
    assert_eq!(after.locked_of(AssetId::Btc), 200);
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 1);
}

// ---------------------------------------------------------------------------
// 2. Crossing trade: buy hits a resting sell; balances swap minus taker fee.
// ---------------------------------------------------------------------------

#[test]
fn crossing_buy_against_resting_sell_settles_with_taker_fee() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);

    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let bob = address_from_pubkey(&pk_b);

    // Alice starts with 1_000 BTC. Bob starts with 1_000_000_000 USD.
    fund(&db, alice, AssetId::Btc, 1_000);
    fund(&db, bob, AssetId::Usd, 1_000_000_000);

    let price = 5_000_000_000u128;
    let qty = 100u128;
    let notional = price * qty / Q8; // = 5_000
    let fee = notional * FEE_BPS as u128 / 10_000; // = 5

    // Block 1: alice rests a sell @ price.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 0, Side::Sell, price, qty, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Block 2: bob crosses the ask with an IOC buy.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(&sk_b, 0, Side::Buy, price, qty, Tif::Ioc, false, 2),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Alice gave up 100 BTC (lock fully consumed) and received `notional` USD.
    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_after.balance(AssetId::Btc), 1_000 - qty);
    assert_eq!(alice_after.locked_of(AssetId::Btc), 0);
    assert_eq!(alice_after.balance(AssetId::Usd), notional);
    assert_eq!(alice_after.nonce, 1);

    // Bob paid `notional + fee` USD and received 100 BTC.
    let bob_after = Accounts::new(&db).get(&bob).unwrap();
    assert_eq!(
        bob_after.balance(AssetId::Usd),
        1_000_000_000 - notional - fee
    );
    assert_eq!(
        bob_after.locked_of(AssetId::Usd),
        0,
        "IOC lock fully released"
    );
    assert_eq!(bob_after.balance(AssetId::Btc), qty);
    assert_eq!(bob_after.nonce, 1);

    // Insurance fund accrued `fee` USD.
    let ins = Accounts::new(&db).get(&Address::INSURANCE).unwrap();
    assert_eq!(ins.balance(AssetId::Usd), fee);

    // No resting orders left, no book index entries.
    assert!(Orders::new(&db).list_for_market(MARKET).is_empty());
    assert!(BookIndex::new(&db).list_side(MARKET, Side::Sell).is_empty());
    assert!(BookIndex::new(&db).list_side(MARKET, Side::Buy).is_empty());
}

// ---------------------------------------------------------------------------
// 3. Cancel own order → balance unlocked; row gone from both CFs.
// ---------------------------------------------------------------------------

#[test]
fn cancel_releases_locked_quote_and_clears_index() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 10_000);

    let price = 5_000_000_000u128;
    let qty = 100u128;
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, qty, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let resting = Orders::new(&db).list_for_market(MARKET);
    assert_eq!(resting.len(), 1);
    let order_id = resting[0].order_id;

    let mut state = BlockState::new(&db);
    apply_tx_at(&mut state, &ctx_at(2), &signed_cancel(&sk, 1, order_id), 0).unwrap();
    state.commit().unwrap();

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.balance(AssetId::Usd), 10_000);
    assert_eq!(after.nonce, 2);
    assert!(Orders::new(&db).list_for_market(MARKET).is_empty());
    assert!(BookIndex::new(&db).list_side(MARKET, Side::Buy).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Cancel someone else's order → NotAuthorized; no state change.
// ---------------------------------------------------------------------------

#[test]
fn cancel_someone_elses_order_returns_not_authorized() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, _pk_b) = generate_keypair();
    fund(&db, alice, AssetId::Btc, 1_000);

    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 0, Side::Sell, 5_000_000_000, 100, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();
    let order_id = Orders::new(&db).list_for_market(MARKET)[0].order_id;
    let alice_before = Accounts::new(&db).get(&alice).unwrap();

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_cancel(&sk_b, 0, order_id),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);

    // Alice's order still resting; balances unchanged.
    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_after, alice_before);
    assert_eq!(Orders::new(&db).list_for_market(MARKET).len(), 1);
}

// ---------------------------------------------------------------------------
// 5. qty not multiple of min_qty → BadLot. No state change, nonce unchanged.
// ---------------------------------------------------------------------------

#[test]
fn place_with_qty_not_multiple_of_min_qty_is_bad_lot() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    // Override genesis: tick=10, min_qty=10 to make non-trivial validations.
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 10,
        min_qty: 10,
        fee_bps: FEE_BPS,
    });
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000_000);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        // qty 7, not a multiple of min_qty 10.
        &signed_place(&sk, 0, Side::Buy, 5_000_000_000, 7, Tif::Gtc, false, 1),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::BadLot);

    let after = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 0);
}

#[test]
fn a_market_with_a_zero_min_qty_rejects_orders_instead_of_dividing_by_zero() {
    // `qty % min_qty` with a zero `min_qty` is a division by zero, and the
    // executor is the layer that has to reject rather than abort. The
    // `tick_size` check on the next line already guarded its own zero; this
    // one did not.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 1,
        min_qty: 0,
        fee_bps: FEE_BPS,
    });
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000_000);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut state = BlockState::new(&db);
        apply_tx_at(
            &mut state,
            &ctx_at(1),
            &signed_place(&sk, 0, Side::Buy, 5_000_000_000, 100, Tif::Gtc, false, 1),
            0,
        )
    }));
    assert_eq!(
        outcome.expect("the executor must not abort on a market row it can hold"),
        Err(ExecError::BadLot)
    );
    let after = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 0);
}

// ---------------------------------------------------------------------------
// 6. price not on tick → BadTick. No state change, nonce unchanged.
// ---------------------------------------------------------------------------

#[test]
fn place_with_price_not_on_tick_is_bad_tick() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 100,
        min_qty: 1,
        fee_bps: FEE_BPS,
    });
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000_000);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, 5_000_000_001, 100, Tif::Gtc, false, 1),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::BadTick);
    let after = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 0);
}

// ---------------------------------------------------------------------------
// 7. Insufficient balance → Insufficient.
// ---------------------------------------------------------------------------

#[test]
fn place_buy_with_insufficient_quote_returns_insufficient() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 100); // way too little

    let price = 5_000_000_000u128;
    let qty = 100u128;
    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, qty, Tif::Gtc, false, 1),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::Insufficient);
    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 0);
}

// ---------------------------------------------------------------------------
// 8. Post-only that crosses → PostOnlyCrossed; lock fully released.
// ---------------------------------------------------------------------------

#[test]
fn post_only_that_crosses_returns_post_only_crossed_and_unlocks() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let bob = address_from_pubkey(&pk_b);
    fund(&db, alice, AssetId::Btc, 1_000);
    fund(&db, bob, AssetId::Usd, 1_000_000);

    // Alice rests an ask @ price.
    let price = 5_000_000_000u128;
    let qty = 100u128;
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 0, Side::Sell, price, qty, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Bob places a post-only buy at the same price → would cross.
    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(&sk_b, 0, Side::Buy, price, qty, Tif::Gtc, true, 2),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::PostOnlyCrossed);
    state.commit().unwrap();

    // Bob's lock is fully released; nonce did NOT advance.
    let bob_after = Accounts::new(&db).get(&bob).unwrap_or_default();
    assert_eq!(bob_after.locked_of(AssetId::Usd), 0);
    assert_eq!(bob_after.balance(AssetId::Usd), 1_000_000);
    assert_eq!(bob_after.nonce, 0);

    // Alice's resting ask is untouched.
    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_after.locked_of(AssetId::Btc), qty);
    assert_eq!(Orders::new(&db).list_for_market(MARKET).len(), 1);
}

// ---------------------------------------------------------------------------
// 9. Self-trade: same address has resting + incoming → SelfTrade; both
//     untouched; lock released.
// ---------------------------------------------------------------------------

#[test]
fn self_trade_is_rejected_and_unlocks() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Btc, 1_000);
    fund(&db, alice, AssetId::Usd, 1_000_000);

    let price = 5_000_000_000u128;
    let qty = 100u128;
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Sell, price, qty, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();
    let alice_after_sell = Accounts::new(&db).get(&alice).unwrap();
    let usd_before_buy = alice_after_sell.balance(AssetId::Usd);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(&sk, 1, Side::Buy, price, qty, Tif::Gtc, false, 2),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::SelfTrade);

    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    // Sell-side base lock still present from block 1.
    assert_eq!(alice_after.locked_of(AssetId::Btc), qty);
    // Buy-side quote lock fully released.
    assert_eq!(alice_after.locked_of(AssetId::Usd), 0);
    assert_eq!(alice_after.balance(AssetId::Usd), usd_before_buy);
    // Nonce stuck at 1 (block-1 sell advanced it; block-2 self-trade did not).
    assert_eq!(alice_after.nonce, 1);
    // Both makers and takers untouched: still one resting sell, no buy.
    assert_eq!(Orders::new(&db).list_for_market(MARKET).len(), 1);
}

// ---------------------------------------------------------------------------
// 10. BatchCancel removes only sender's orders; foreign orders untouched.
// ---------------------------------------------------------------------------

#[test]
fn batch_cancel_clears_only_senders_orders() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let bob = address_from_pubkey(&pk_b);
    fund(&db, alice, AssetId::Btc, 1_000);
    fund(&db, bob, AssetId::Btc, 1_000);

    let price = 5_000_000_000u128;
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 0, Side::Sell, price, 100, Tif::Gtc, false, 1),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 1, Side::Sell, price + TICK, 50, Tif::Gtc, false, 2),
        1,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk_b,
            0,
            Side::Sell,
            price + TICK * 2,
            70,
            Tif::Gtc,
            false,
            3,
        ),
        2,
    )
    .unwrap();
    state.commit().unwrap();
    assert_eq!(Orders::new(&db).list_for_market(MARKET).len(), 3);

    // Alice batch-cancels.
    let mut state = BlockState::new(&db);
    apply_tx_at(&mut state, &ctx_at(2), &signed_batch_cancel(&sk_a, 2), 0).unwrap();
    state.commit().unwrap();

    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_after.locked_of(AssetId::Btc), 0);
    assert_eq!(alice_after.balance(AssetId::Btc), 1_000);
    assert_eq!(alice_after.nonce, 3);

    let bob_after = Accounts::new(&db).get(&bob).unwrap();
    assert_eq!(bob_after.locked_of(AssetId::Btc), 70);

    let remaining = Orders::new(&db).list_for_market(MARKET);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].owner, bob);
}

// ---------------------------------------------------------------------------
// 11. Cancel of an unknown order id → OrderNotFound (no nonce advance).
// ---------------------------------------------------------------------------

#[test]
fn cancel_unknown_order_returns_order_not_found() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_btc_usd_market(&db);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // Pre-fund alice so she has an account row to load.
    fund(&db, alice, AssetId::Usd, 100);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_cancel(&sk, 0, H256([0xaa; 32])),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::OrderNotFound);
    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.nonce, 0);
}

// ---------------------------------------------------------------------------
// 12. Place against a market that hasn't been registered → MarketNotFound.
// ---------------------------------------------------------------------------

#[test]
fn place_against_unknown_market_returns_market_not_found() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    // No `seed_btc_usd_market` — Markets CF empty.
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000_000);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, 5_000_000_000, 100, Tif::Gtc, false, 1),
        0,
    )
    .unwrap_err();
    assert_eq!(err, ExecError::MarketNotFound);
}
