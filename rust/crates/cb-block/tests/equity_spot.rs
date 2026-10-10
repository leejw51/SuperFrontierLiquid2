//! Spot-executor coverage for equity markets, and for the decimal-scale
//! mistakes they invite.
//!
//! # Why equities need their own test file
//!
//! BTC has 8 decimals, and so does the fixed derivative contract scale, and so
//! (near enough) does everything in the existing spot tests. That coincidence
//! means a quantity conversion written against BTC can be wrong in a way no
//! BTC test can detect. Equities have 6 decimals; SOL has 9. The moment a market
//! exists whose base unit is not 1e-8, every hard-coded `1e8` becomes a
//! hundred-fold error waiting for a caller.
//!
//! # What the executor can and cannot catch
//!
//! Be precise about this, because it decides where the real defence lives.
//!
//! The executor sees integers. It has no way to know whether `150000000` was
//! meant as 1.5 shares scaled by 1e8 (wrong) or 150 shares scaled by 1e6
//! (right) — both are the same integer. What it *can* do is reject an integer
//! that is inconsistent with the market's declared lot and tick, or that the
//! sender cannot afford. Those catch two of the three realistic decimal
//! mistakes, and the tests below pin both:
//!
//! - **Too few decimals** (treating an equity as 2-dp, cents-like) produces a
//!   quantity that is not a multiple of `min_qty` → `BadLot`.
//! - **Too many decimals** (reusing BTC's 8-dp scaling) produces a quantity
//!   100× too large → `Insufficient`, because nobody's balance covers 100× the
//!   notional they intended.
//!
//! Neither is a *precision* check, and neither would fire on an account rich
//! enough to afford the mistake. The actual boundary defence is
//! `cb_types::scales::parse_spot_qty`, which refuses to build the wrong integer
//! in the first place — see `cb-types`'s
//! `spot_qty_rejects_precision_the_asset_cannot_hold`. The executor is the
//! backstop, not the guard.

use tempfile::tempdir;

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_state::{Accounts, BookIndex, Database, MarketConfig, Markets, Meta, Orders};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::scales::{parse_spot_price, parse_spot_qty, spot_qty_from_whole};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};

/// Equity spot parameters, matching the `aapl_usd` line in `genesis-full.jsonl`.
/// Pinned as literals here so a genesis tweak that changes them shows up as a
/// failure rather than as a quietly different test.
const MARKET: MarketId = MarketId::AaplUsd;
/// $0.01 at the equity wire-price scale of 1e8 per dollar.
const TICK: u128 = 1_000_000;
/// 0.01 share at AAPL's 6 decimals.
const MIN_QTY: u128 = 10_000;
const FEE_BPS: u16 = 10;

/// One micro-USD; the quote asset's base unit.
const MICRO: u128 = 1_000_000;

fn seed_equity_market(db: &Database) {
    Markets::new(db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Aapl,
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

fn ctx_at(height: u64) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: Address::ZERO,
    }
}

fn signed_place(
    sk: &SecretKey,
    nonce: u64,
    side: Side,
    price: u128,
    qty: u128,
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
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: H256([cid; 32]),
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

/// The executor's buy-side lock: notional plus a rounded-up fee reserve.
fn buy_lock(price: u128, qty: u128) -> u128 {
    let notional = price * qty / 100_000_000u128;
    let fee_reserve = (notional * FEE_BPS as u128).div_ceil(10_000);
    notional + fee_reserve
}

// ---------------------------------------------------------------------------
// The happy path, with every number derived from the scale helpers
// ---------------------------------------------------------------------------

#[test]
fn correctly_scaled_equity_buy_rests_and_locks_the_right_notional() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions in this fixture are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it. `Meta` is
    // outside the state-root preimage, so this does not affect any root asserted here.
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000 * MICRO); // $1,000

    // $190.25 for 1.5 shares. Both integers come from the scale helpers rather
    // than from a literal, so the test exercises the conversion it depends on.
    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let qty = parse_spot_qty(AssetId::Aapl, "1.5").unwrap();
    assert_eq!(price, 19_025_000_000, "equity wire price is 1e8 per dollar");
    assert_eq!(qty, 1_500_000, "1.5 shares at 6 decimals");
    assert_eq!(price % TICK, 0, "price must sit on a penny tick");
    assert_eq!(qty % MIN_QTY, 0, "qty must sit on the 0.01-share lot");

    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, qty, 1),
        0,
    )
    .expect("a correctly scaled equity order is accepted");
    state.commit().unwrap();

    // $190.25 x 1.5 = $285.375, and the executor must land on exactly that in
    // micro-USD — not 100x it, which is what a BTC-shaped conversion gives.
    let notional = price * qty / 100_000_000u128;
    assert_eq!(notional, 285 * MICRO + 375_000, "$285.375 in micro-USD");

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(
        after.balance(AssetId::Usd),
        1_000 * MICRO,
        "balance unchanged"
    );
    assert_eq!(after.locked_of(AssetId::Usd), buy_lock(price, qty));
    assert_eq!(after.nonce, 1);
    assert_eq!(Orders::new(&db).list_for_market(MARKET).len(), 1);
    assert_eq!(BookIndex::new(&db).list_side(MARKET, Side::Buy).len(), 1);
}

#[test]
fn crossing_equity_trade_settles_in_micro_usd() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk_a, pk_a) = generate_keypair();
    let seller = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let buyer = address_from_pubkey(&pk_b);

    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let qty = parse_spot_qty(AssetId::Aapl, "2").unwrap();
    let notional = price * qty / 100_000_000u128;
    assert_eq!(notional, 380 * MICRO + 500_000, "$380.50");

    fund(
        &db,
        seller,
        AssetId::Aapl,
        spot_qty_from_whole(AssetId::Aapl, 10).unwrap(),
    );
    fund(&db, buyer, AssetId::Usd, 10_000 * MICRO);

    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk_a, 0, Side::Sell, price, qty, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &signed_place(&sk_b, 0, Side::Buy, price, qty, 2),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let accts = Accounts::new(&db);
    let buyer_state = accts.get(&buyer).unwrap();
    let seller_state = accts.get(&seller).unwrap();

    // The buyer received exactly 2 shares — in AAPL base units, not contract units.
    assert_eq!(
        buyer_state.balance(AssetId::Aapl),
        spot_qty_from_whole(AssetId::Aapl, 2).unwrap()
    );
    assert_eq!(
        seller_state.balance(AssetId::Aapl),
        spot_qty_from_whole(AssetId::Aapl, 8).unwrap(),
        "seller delivered 2 of 10 shares"
    );
    // And paid the notional plus a taker fee, all in micro-USD.
    let fee = notional * FEE_BPS as u128 / 10_000;
    assert_eq!(
        buyer_state.balance(AssetId::Usd),
        10_000 * MICRO - notional - fee
    );
    assert!(
        buyer_state.balance(AssetId::Usd) > 9_000 * MICRO,
        "sanity: ~$380 spent, not ~$38,000"
    );
}

// ---------------------------------------------------------------------------
// Wrong decimals
// ---------------------------------------------------------------------------

#[test]
fn equity_order_scaled_with_too_few_decimals_is_rejected() {
    // The mistake: treating an equity like a cents-denominated 2-decimal
    // quantity, so "1.5 shares" is encoded as 150 rather than 1_500_000.
    //
    // 150 is not a multiple of the 0.01-share lot, so the executor refuses it
    // before touching any balance.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000 * MICRO);

    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let wrong_qty = 150u128; // 1.5 shares, if an equity had 2 decimals
    let right_qty = parse_spot_qty(AssetId::Aapl, "1.5").unwrap();
    assert_eq!(
        right_qty / wrong_qty,
        10_000,
        "the mistake is a 10,000x error"
    );

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, wrong_qty, 1),
        0,
    )
    .expect_err("a wrongly scaled equity quantity must be rejected");
    assert!(
        matches!(err, ExecError::BadLot),
        "expected BadLot, got {err:?}"
    );

    // Rejected before any mutation: no lock, no nonce advance, no order.
    state.commit().unwrap();
    let after = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(after.locked_of(AssetId::Usd), 0, "nothing locked");
    assert_eq!(
        after.nonce, 0,
        "a rejected order must not advance the nonce"
    );
    assert!(Orders::new(&db).list_for_market(MARKET).is_empty());
}

#[test]
fn equity_order_scaled_with_btc_decimals_cannot_be_afforded() {
    // The other mistake, and the one this whole module exists for: reusing a
    // BTC-shaped 1e8 conversion on a 6-decimal asset. "1.5 shares" becomes
    // 150_000_000 base units — which IS a valid lot, so the lot check passes —
    // but it is 150 shares, a notional 100x what the trader intended.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // Plenty for the intended $285 trade; nowhere near the $28,537 it becomes.
    fund(&db, alice, AssetId::Usd, 1_000 * MICRO);

    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let intended = parse_spot_qty(AssetId::Aapl, "1.5").unwrap();
    let btc_scaled = 150_000_000u128;
    assert_eq!(btc_scaled / intended, 100, "the error is exactly 100-fold");
    assert_eq!(
        btc_scaled % MIN_QTY,
        0,
        "the lot check cannot catch this one"
    );

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, btc_scaled, 1),
        0,
    )
    .expect_err("100x the intended notional must not be affordable");
    assert!(
        matches!(err, ExecError::Insufficient),
        "expected Insufficient, got {err:?}"
    );

    state.commit().unwrap();
    let after = Accounts::new(&db).get(&alice).unwrap_or_default();
    assert_eq!(after.locked_of(AssetId::Usd), 0);
    assert_eq!(after.nonce, 0);
}

#[test]
fn equity_sell_scaled_with_btc_decimals_cannot_be_afforded() {
    // Same mistake on the sell side: the lock asset is the base, so a 100x
    // quantity fails against the share balance instead of the USD balance.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // Alice really holds 2 shares.
    fund(
        &db,
        alice,
        AssetId::Aapl,
        spot_qty_from_whole(AssetId::Aapl, 2).unwrap(),
    );

    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let btc_scaled = 150_000_000u128; // "1.5 shares" at BTC's scale

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Sell, price, btc_scaled, 1),
        0,
    )
    .expect_err("cannot sell 150 shares while holding 2");
    assert!(
        matches!(err, ExecError::Insufficient),
        "expected Insufficient, got {err:?}"
    );

    // The correctly scaled version of the same intent is accepted.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk,
            0,
            Side::Sell,
            price,
            parse_spot_qty(AssetId::Aapl, "1.5").unwrap(),
            2,
        ),
        0,
    )
    .expect("1.5 shares out of 2 held is fine");
    state.commit().unwrap();
    assert_eq!(
        Accounts::new(&db)
            .get(&alice)
            .unwrap()
            .locked_of(AssetId::Aapl),
        1_500_000
    );
}

#[test]
fn a_quantity_finer_than_the_asset_can_hold_never_reaches_the_chain() {
    // The real guard, shown here next to the executor backstops so the division
    // of responsibility is visible in one place: an 8-decimal quantity string
    // offered to a 6-decimal asset is refused at the boundary, so no integer is
    // ever built for the executor to judge.
    let err = parse_spot_qty(AssetId::Aapl, "1.12345678")
        .expect_err("excess precision must be refused, not rounded");
    assert!(
        err.to_string().contains("decimal places"),
        "error should explain the precision mismatch, got `{err}`"
    );

    // The identical string is valid on BTC, which is precisely why the check has
    // to be per-asset rather than a single global scale.
    assert_eq!(
        parse_spot_qty(AssetId::Btc, "1.12345678").unwrap(),
        112_345_678
    );
}

#[test]
fn an_off_tick_equity_price_is_rejected() {
    // Sub-penny pricing on an equity: valid at the wire scale, not on this
    // market's tick.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 1_000 * MICRO);

    let sub_penny = parse_spot_price(MARKET, "190.255").unwrap();
    assert_ne!(sub_penny % TICK, 0);

    let mut state = BlockState::new(&db);
    let err = apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(
            &sk,
            0,
            Side::Buy,
            sub_penny,
            parse_spot_qty(AssetId::Aapl, "1").unwrap(),
            1,
        ),
        0,
    )
    .expect_err("sub-penny equity price must be rejected");
    assert!(
        matches!(err, ExecError::BadTick),
        "expected BadTick, got {err:?}"
    );
}

#[test]
fn equity_markets_are_isolated_from_one_another() {
    // Four equities share decimals, tick and lot, so a market-id mix-up would
    // produce numbers that all look plausible. Assert the book keyspace keeps
    // them apart.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    seed_equity_market(&db);
    Markets::new(&db).put(&MarketConfig {
        id: MarketId::TslaUsd,
        base: AssetId::Tsla,
        quote: AssetId::Usd,
        tick_size: TICK,
        min_qty: MIN_QTY,
        fee_bps: FEE_BPS,
    });

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, alice, AssetId::Usd, 10_000 * MICRO);

    let price = parse_spot_price(MARKET, "190.25").unwrap();
    let qty = parse_spot_qty(AssetId::Aapl, "1").unwrap();
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &signed_place(&sk, 0, Side::Buy, price, qty, 1),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    assert_eq!(Orders::new(&db).list_for_market(MarketId::AaplUsd).len(), 1);
    assert!(
        Orders::new(&db)
            .list_for_market(MarketId::TslaUsd)
            .is_empty(),
        "an AAPL order must not appear on the TSLA book"
    );
    assert!(BookIndex::new(&db)
        .list_side(MarketId::TslaUsd, Side::Buy)
        .is_empty());
}
