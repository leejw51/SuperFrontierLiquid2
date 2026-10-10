//! Regression tests for the spot executor's lock accounting — QA findings F6
//! and F7, both surfaced by `cb-simgame`'s lock-consistency invariant, which
//! compares each account's locked balance against what its resting orders
//! actually reserve.
//!
//! The shared cause was that a buy-side lock was taken **once**, over the whole
//! order at its limit price, and released **per fill**, at each fill's own
//! price and quantity. Neither the rounding nor the price can be assumed to
//! match, so the two sides did not reconcile — over-releasing into a panic in
//! one direction (F6) and stranding the price improvement in the other (F7).
//!
//! The fix defines an order's lock as a function of its *remaining* quantity
//! (`spot_lock_required` in the executor) and moves it only by differences
//! between two evaluations of that function. These tests assert the property
//! that definition buys: for every path an order can take through the book, the
//! sum of what it releases equals what it reserved, exactly.

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Accounts, Database, MarketConfig, Markets, Meta, Orders};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: MarketId = MarketId::BtcUsd;
const TICK: u128 = 1;
const MIN_QTY: u128 = 1;
const FEE_BPS: u16 = 10;
const Q8: u128 = 100_000_000;

fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions in this fixture are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it. `Meta` is
    // outside the state-root preimage, so this does not affect any root asserted here.
    Meta::new(&db).set_chain_id(1);
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: TICK,
        min_qty: MIN_QTY,
        fee_bps: FEE_BPS,
    });
    (dir, db)
}

fn actor(seed: &str) -> (SecretKey, Address) {
    let (sk, pk) = keypair_from_seed(seed.as_bytes());
    (sk, address_from_pubkey(&pk))
}

fn fund(db: &Database, addr: Address, asset: AssetId, amount: u128) {
    let accounts = Accounts::new(db);
    let mut s = accounts.get(&addr).unwrap_or_default();
    s.add(asset, amount).expect("seed balance");
    accounts.put(&addr, &s);
}

fn balance(db: &Database, addr: Address, asset: AssetId) -> u128 {
    Accounts::new(db)
        .get(&addr)
        .map(|s| s.balance(asset))
        .unwrap_or(0)
}

fn locked(db: &Database, addr: Address, asset: AssetId) -> u128 {
    Accounts::new(db)
        .get(&addr)
        .map(|s| s.locked_of(asset))
        .unwrap_or(0)
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

fn place(sk: &SecretKey, nonce: u64, side: Side, price: u128, qty: u128, cid: u8) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::PlaceOrder {
            market: MARKET,
            side,
            price,
            qty,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([cid; 32]),
        },
    )
}

fn ctx_at(height: u64) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: Address::ZERO,
    }
}

fn apply_block(db: &Database, height: u64, txs: &[VerifiedTx]) -> Vec<Result<(), ExecError>> {
    let mut state = BlockState::new(db);
    let mut out = Vec::new();
    for (i, v) in txs.iter().enumerate() {
        out.push(apply_tx_at(&mut state, &ctx_at(height), v, i as u32));
    }
    state.commit().unwrap();
    out
}

/// The executor's own buy-side lock formula, restated.
fn buy_lock(price: u128, qty: u128) -> u128 {
    let notional = price * qty / Q8;
    let fee_reserve = (notional * FEE_BPS as u128).div_ceil(10_000);
    notional + fee_reserve
}

// ===========================================================================
// F6 — a resting buy filled in several pieces releases exactly what it locked.
// ===========================================================================
//
// The defect: the maker locked `floor(notional) + ceil(fee)` once, for the
// whole order, and each fill released `floor(notional_i) + ceil(fee_i)`.
// Because the fee reserve rounds up, the sum of the per-fill ceilings could
// exceed the single ceiling taken at placement, so the releases totalled more
// than was ever locked. `AccountState::unlock` is a `checked_sub().expect(...)`
// — the overshoot was not a silent inconsistency but a panic inside block
// execution, which stops the chain. `cb-simgame` hit it unprompted at block 113
// of the first spot run.

#[test]
fn a_resting_buy_filled_in_two_pieces_releases_exactly_what_it_locked() {
    let (_dir, db) = setup();
    let (sk_maker, maker) = actor("qa-spot-maker");
    let (sk_taker, taker) = actor("qa-spot-taker");

    // Numbers chosen so the arithmetic is checkable by hand, and so that the
    // old per-fill formula over-releases:
    //   price 1e8 makes notional equal to quantity.
    //   lock at placement    = 2000 + ceil(2000 * 10 / 10_000) = 2000 + 2 = 2002
    //   naive fill-1001 release = 1001 + ceil(1001 * 10 / 10_000) = 1003
    //   naive fill- 999 release =  999 + ceil( 999 * 10 / 10_000) = 1000
    //   naive total 2003 > 2002 locked → `unlock underflow`.
    let price = Q8;
    let qty = 2000u128;
    assert_eq!(buy_lock(price, qty), 2002);
    assert_eq!(
        buy_lock(price, 1001) + buy_lock(price, 999),
        2003,
        "the sum of the per-fill ceilings still exceeds the placement lock — \
         that is why the lock is reconciled against the remainder instead"
    );

    fund(&db, maker, AssetId::Usd, 1_000_000);
    fund(&db, taker, AssetId::Btc, 1_000_000);
    let maker_start = balance(&db, maker, AssetId::Usd);

    apply_block(&db, 1, &[place(&sk_maker, 0, Side::Buy, price, qty, 1)]);
    assert_eq!(
        locked(&db, maker, AssetId::Usd),
        2002,
        "the maker locks notional plus the rounded-up fee reserve, once"
    );

    // First taker sell consumes 1001 of the 2000. What is released is the drop
    // in what the *remainder* reserves, not a fresh ceiling at the fill size.
    apply_block(&db, 2, &[place(&sk_taker, 0, Side::Sell, price, 1001, 2)]);
    assert_eq!(
        locked(&db, maker, AssetId::Usd),
        buy_lock(price, 999),
        "after the first fill the maker holds exactly what 999 remaining needs"
    );

    // The second taker sell consumes the remaining 999. Under the old rule this
    // tried to release 1000 from the 999 that was left and aborted the block.
    apply_block(&db, 3, &[place(&sk_taker, 1, Side::Sell, price, 999, 3)]);
    assert_eq!(
        locked(&db, maker, AssetId::Usd),
        0,
        "a fully-filled order releases its whole lock and not one unit more"
    );
    assert!(
        Orders::new(&db)
            .list_for_market(MARKET)
            .iter()
            .all(|o| o.owner != maker),
        "and it is gone from the book"
    );

    // The maker bought 2 000 base for 2 000 quote and paid no fee (makers do
    // not), so the fee reserve comes back as spendable balance.
    assert_eq!(balance(&db, maker, AssetId::Usd), maker_start - 2000);
    assert_eq!(balance(&db, maker, AssetId::Btc), 2000);
}

#[test]
fn a_resting_buy_cancelled_after_a_partial_fill_ends_with_nothing_locked() {
    // The other exit from a partially-filled resting order. Placement,
    // fill and cancel must add up the same way.
    let (_dir, db) = setup();
    let (sk_maker, maker) = actor("qa-spot-cancel-maker");
    let (sk_taker, taker) = actor("qa-spot-cancel-taker");
    fund(&db, maker, AssetId::Usd, 1_000_000);
    fund(&db, taker, AssetId::Btc, 1_000_000);

    let price = Q8;
    apply_block(&db, 1, &[place(&sk_maker, 0, Side::Buy, price, 2000, 1)]);
    apply_block(&db, 2, &[place(&sk_taker, 0, Side::Sell, price, 1001, 2)]);

    let order_id = Orders::new(&db)
        .list_for_market(MARKET)
        .into_iter()
        .find(|o| o.owner == maker)
        .expect("the maker's remainder is resting")
        .order_id;
    apply_block(
        &db,
        3,
        &[sign(&sk_maker, 1, TxBody::CancelOrder { order_id })],
    );
    assert_eq!(
        locked(&db, maker, AssetId::Usd),
        0,
        "cancelling the remainder returns precisely the rest of the lock"
    );
}

// ===========================================================================
// F7 — price improvement comes back to the trader, not the lock.
// ===========================================================================
//
// The defect: a taker buy locked against its **limit** price and each fill
// released against the **fill** price. When the taker got price improvement the
// releases fell short, and the reconciliation step deliberately left the whole
// remainder locked when the order rested. Cancelling later released only what
// the remainder reserved *at the limit price*, so the improvement was stranded:
// the locked balance never returned to zero and that quote was unspendable for
// the life of the account. The old comment bounded the residue by "the
// fee_reserve ceiling"; it was in fact bounded by the price improvement, which
// is unbounded.

#[test]
fn price_improvement_on_a_partially_filled_buy_is_returned_to_the_trader() {
    let (_dir, db) = setup();
    let (sk_maker, maker) = actor("qa-spot-imp-maker");
    let (sk_taker, taker) = actor("qa-spot-imp-taker");

    fund(&db, maker, AssetId::Btc, 1_000_000);
    fund(&db, taker, AssetId::Usd, 1_000_000);
    let taker_start = balance(&db, taker, AssetId::Usd);

    let ask = Q8; // maker offers at 1.0
    let limit = 2 * Q8; // taker is willing to pay 2.0
    let maker_qty = 400u128;
    let taker_qty = 1000u128;

    apply_block(
        &db,
        1,
        &[place(&sk_maker, 0, Side::Sell, ask, maker_qty, 1)],
    );
    // The taker crosses: 400 fills at 1.0, 600 rests at 2.0.
    apply_block(
        &db,
        2,
        &[place(&sk_taker, 0, Side::Buy, limit, taker_qty, 2)],
    );

    let expected_for_resting = buy_lock(limit, taker_qty - maker_qty);
    assert_eq!(
        locked(&db, taker, AssetId::Usd),
        expected_for_resting,
        "the taker holds exactly what its 600 resting units reserve at its own \
         limit price — the improvement on the filled 400 is spendable again"
    );
    // The improvement is real money: 400 units bought at 1.0 against a limit of
    // 2.0 is 400 quote less spent than the lock reserved.
    assert_eq!(
        balance(&db, taker, AssetId::Usd),
        taker_start - 400,
        "and only the 400 actually spent has left the balance"
    );

    // Cancelling the remainder returns the rest, leaving nothing behind.
    let order_id = Orders::new(&db)
        .list_for_market(MARKET)
        .into_iter()
        .find(|o| o.owner == taker)
        .expect("the taker's remainder is resting")
        .order_id;
    apply_block(
        &db,
        3,
        &[sign(&sk_taker, 1, TxBody::CancelOrder { order_id })],
    );

    assert_eq!(
        locked(&db, taker, AssetId::Usd),
        0,
        "with no orders left the account has nothing locked"
    );
    assert!(
        Orders::new(&db)
            .list_for_market(MARKET)
            .iter()
            .all(|o| o.owner != taker),
        "and it has no resting order that could justify a lock"
    );
}

#[test]
fn an_ioc_buy_with_price_improvement_leaves_nothing_locked() {
    // The same arithmetic on the path that does not rest: an IOC that fills
    // part way and drops the rest must release the whole lock, improvement
    // included.
    let (_dir, db) = setup();
    let (sk_maker, maker) = actor("qa-spot-ioc-maker");
    let (sk_taker, taker) = actor("qa-spot-ioc-taker");
    fund(&db, maker, AssetId::Btc, 1_000_000);
    fund(&db, taker, AssetId::Usd, 1_000_000);
    let taker_start = balance(&db, taker, AssetId::Usd);

    apply_block(&db, 1, &[place(&sk_maker, 0, Side::Sell, Q8, 400, 1)]);
    apply_block(
        &db,
        2,
        &[sign(
            &sk_taker,
            0,
            TxBody::PlaceOrder {
                market: MARKET,
                side: Side::Buy,
                price: 2 * Q8,
                qty: 1000,
                tif: Tif::Ioc,
                post_only: false,
                client_order_id: H256([2; 32]),
            },
        )],
    );

    assert_eq!(locked(&db, taker, AssetId::Usd), 0);
    assert_eq!(balance(&db, taker, AssetId::Usd), taker_start - 400);
    assert_eq!(balance(&db, taker, AssetId::Btc), 400);
}
