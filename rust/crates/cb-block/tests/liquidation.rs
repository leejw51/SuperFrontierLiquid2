//! M4 integration tests: margin gate, liquidation flow, insurance fund,
//! reduce-only halt.
//!
//! Where the spot trader/perp tests use tiny synthetic prices (qty=10,
//! price=100), this file works in **realistic units** so the IM/MM math
//! actually triggers — 1 BTC at 50k USD/BTC etc. Numbers are quoted in:
//!
//!   * `qty` — base units, 8 decimals (BTC convention). 1 BTC = 1e8.
//!   * `price_q8` — quote-per-base, scaled 1e8. 50_000 USD/BTC = 5e12.
//!   * `collateral` — micro-USD (1e6). $1_000 = 1e9.
//!
//! These match the conversions baked into `cb_engine::margin` and the
//! executor's `credit_realized_pnl` helper.

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
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

fn setup_market(db: &Database, oracle_authority: Address) {
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
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
}

fn fund_margin(db: &Database, owner: Address, collateral_micro: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral: collateral_micro,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

fn set_mark(db: &Database, market: DerivId, mark_q8: u128) {
    OracleState::new(db).put_mark(&MarkPrice {
        market,
        px_q8: mark_q8,
        computed_at_height: 0,
        premium_ema_q9: 0,
    });
    OracleState::new(db).put_oracle(&OraclePrice {
        market,
        px_q8: mark_q8,
        source_height: 0,
        authority: Address::ZERO,
    });
}

#[allow(clippy::too_many_arguments)]
fn place_tx(
    sk: &SecretKey,
    nonce: u64,
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: Tif,
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
            post_only: false,
            reduce_only,
            client_order_id: H256([cid; 32]),
        },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

fn liq_trigger_tx(sk: &SecretKey, nonce: u64, owner: Address, market: DerivId) -> VerifiedTx {
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::LiquidationTrigger { owner, market },
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
// Margin pre-trade gate: undercollateralized order is rejected without
// mutating book / positions / margin state.
// ---------------------------------------------------------------------------

#[test]
fn place_perp_rejected_when_initial_margin_insufficient() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // $100 collateral against a 1 BTC @ 50k position = $50k notional.
    // IM tier1 = 50k * 2% = $1k > $100 → reject.
    fund_margin(&db, alice, 100 * MICRO);

    // Snapshot state for the no-mutation assertion.
    let pre_collateral = MarginAccounts::new(&db).get(&alice).unwrap().collateral;
    let pre_positions = Positions::new(&db).list_for_owner(&alice).len();

    let mut state = BlockState::new(&db);
    let v = place_tx(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::InsufficientMargin);
    state.commit().unwrap();

    // No state mutation: collateral, positions all unchanged.
    let post_collateral = MarginAccounts::new(&db).get(&alice).unwrap().collateral;
    assert_eq!(pre_collateral, post_collateral);
    assert_eq!(
        Positions::new(&db).list_for_owner(&alice).len(),
        pre_positions
    );
}

#[test]
fn place_perp_accepted_when_initial_margin_sufficient() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    // $5_000 collateral; IM is $1_000 → OK.
    fund_margin(&db, alice, 5_000 * MICRO);

    let mut state = BlockState::new(&db);
    let v = place_tx(
        &sk,
        0,
        MARKET,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap();
    state.commit().unwrap();
    // The order rests since there's no opposing book; collateral untouched.
    assert_eq!(
        MarginAccounts::new(&db).get(&alice).unwrap().collateral,
        5_000 * MICRO
    );
}

// ---------------------------------------------------------------------------
// Equity conservation across a fill: buyer + seller + insurance + keeper
// changes equal zero.
// ---------------------------------------------------------------------------

#[test]
fn equity_conserved_across_simple_fill() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let bob = address_from_pubkey(&pk_b);
    fund_margin(&db, alice, 5_000 * MICRO);
    fund_margin(&db, bob, 5_000 * MICRO);

    // Alice sells 1 BTC @ 50k (rests), Bob buys 1 BTC @ 50k (taker).
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_a,
            0,
            MARKET,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_b,
            0,
            MARKET,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // After the fill: each holds an opposite-side position at entry 50_000.
    // No mark change → realised PnL is zero. The only collateral movement is
    // the 3 bps taker fee Bob pays on $50_000 of notional ($15), which is a
    // transfer into the insurance fund (QA F8). Alice rested her quote in the
    // same block the fill happened, so the wash-trade guard withholds the
    // maker rebate.
    let alice_m = MarginAccounts::new(&db).get(&alice).unwrap();
    let bob_m = MarginAccounts::new(&db).get(&bob).unwrap();
    assert_eq!(alice_m.collateral, 5_000 * MICRO);
    assert_eq!(bob_m.collateral, 5_000 * MICRO - 15 * MICRO);
    assert_eq!(
        MarginAccounts::new(&db)
            .get(&cb_types::Address::INSURANCE)
            .map(|m| m.collateral)
            .unwrap_or(0),
        15 * MICRO,
        "the fee is a transfer, not a burn: every micro-USD reaches the sink"
    );
    // Both have positions of opposite side, same qty.
    let a_pos = Positions::new(&db).list_for_owner(&alice);
    let b_pos = Positions::new(&db).list_for_owner(&bob);
    assert_eq!(a_pos.len(), 1);
    assert_eq!(b_pos.len(), 1);
    assert_eq!(a_pos[0].qty, ONE_BTC);
    assert_eq!(b_pos[0].qty, ONE_BTC);
    assert_ne!(a_pos[0].side, b_pos[0].side);
    // Conservation: (alice_collateral_delta) + (bob_collateral_delta) = 0.
    // Both deltas are 0, so trivially true.
}

#[test]
fn realized_pnl_credited_on_close() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk_a, pk_a) = generate_keypair();
    let alice = address_from_pubkey(&pk_a);
    let (sk_b, pk_b) = generate_keypair();
    let bob = address_from_pubkey(&pk_b);
    fund_margin(&db, alice, 5_000 * MICRO);
    fund_margin(&db, bob, 5_000 * MICRO);

    // Alice longs 1 BTC @ 50k.
    // Bob makes the matching short.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_b,
            0,
            MARKET,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_a,
            0,
            MARKET,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // Now both close at 51k. Bob bids 51k as a maker, Alice sells (taker reduce-only).
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &place_tx(
            &sk_b,
            1,
            MARKET,
            Side::Buy,
            51_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            3,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &place_tx(
            &sk_a,
            1,
            MARKET,
            Side::Sell,
            51_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            true,
            4,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // Alice closed long at 51k vs entry 50k → +1_000 USD profit.
    // Bob closed short at 51k vs entry 50k → -1_000 USD loss.
    //
    // Alice was the taker on both legs, so she also paid 3 bps twice: $15 on
    // the $50_000 open and $15.30 on the $51_000 close (QA F8). Bob was the
    // maker on both and rested each quote in the block it filled, so the
    // wash-trade guard withheld the rebate.
    let alice_fees = 15 * MICRO + 15_300_000;
    let alice_m = MarginAccounts::new(&db).get(&alice).unwrap();
    let bob_m = MarginAccounts::new(&db).get(&bob).unwrap();
    assert_eq!(alice_m.collateral, 6_000 * MICRO - alice_fees);
    assert_eq!(bob_m.collateral, 4_000 * MICRO);
    // Symmetry: the PnL is still zero-sum; the fees left the pair for the
    // insurance fund rather than vanishing.
    let insurance = MarginAccounts::new(&db)
        .get(&cb_types::Address::INSURANCE)
        .map(|m| m.collateral)
        .unwrap_or(0);
    assert_eq!(insurance, alice_fees);
    assert_eq!(
        alice_m.collateral + bob_m.collateral + insurance,
        10_000 * MICRO
    );
}

// ---------------------------------------------------------------------------
// Liquidation flow: open large position → mark drops → trigger → fees split,
// position closed, insurance / keeper credited.
// ---------------------------------------------------------------------------

#[test]
fn full_liquidation_flow_with_fee_split() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // Seed insurance with $10_000.
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 10_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, pk_keeper) = generate_keypair();
    let keeper = address_from_pubkey(&pk_keeper);
    fund_margin(&db, alice, 1_500 * MICRO); // 1500 collateral
    fund_margin(&db, bob, 200_000 * MICRO);
    fund_margin(&db, keeper, 0);

    // Alice longs 1 BTC @ 50k. Bob is the seller (resting).
    // After fill, alice has position 1 BTC @ 50k entry. IM was 1_000 USD;
    // collateral 1_500 covers it.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_bob,
            0,
            MARKET,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_alice,
            0,
            MARKET,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    // Bob also rests a fat bid stack so the liquidation IOC has something
    // to fill against. 1 BTC each at 49_750, 49_500, 49_000 (covers all
    // three liq tranches).
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_bob,
            1,
            MARKET,
            Side::Buy,
            49_750 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            3,
        ),
        2,
    )
    .unwrap();
    state.commit().unwrap();

    // Mark drops to $48k. Alice's position now has uPnL = -2_000 USD;
    // equity = 1_500 - 2_000 = -500 USD < MM ($480). Liquidatable.
    set_mark(&db, MARKET, 48_000 * Q8);

    // Snapshot pre-trigger insurance + keeper balances.
    let pre_ins = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    let pre_keeper = MarginAccounts::new(&db).get(&keeper).unwrap().collateral;

    // Keeper triggers liquidation.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &liq_trigger_tx(&sk_keeper, 0, alice, MARKET),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Alice's position must be flat (or smaller) after the cascade.
    let a_pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(a_pos.qty, 0, "alice should be flat after liquidation");

    // Insurance + keeper got SOME credit (we filled at least one tranche
    // against bob's resting buy at 49_750). Notional ≈ $49_750 →
    // insurance fee ≈ 0.3% = ~$149.25 → at micro-USD = ~149_250_000.
    // Keeper ≈ 0.2% = ~$99.5 → ~99_500_000 micro-USD.
    let post_ins = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    let post_keeper = MarginAccounts::new(&db).get(&keeper).unwrap().collateral;
    let ins_delta = post_ins as i128 - pre_ins as i128;
    let keeper_delta = post_keeper as i128 - pre_keeper as i128;
    // Insurance must NOT have decreased on this run — alice still had some
    // collateral, so fees came from her. (Bad-debt absorption is a separate
    // path tested below.)
    assert!(
        ins_delta > 0,
        "insurance should be credited liq fee; got delta {ins_delta}"
    );
    // Keeper bounty also credited.
    assert!(
        keeper_delta > 0,
        "keeper should be credited liq bounty; got delta {keeper_delta}"
    );
    // Insurance > keeper because 0.3% > 0.2%.
    assert!(
        ins_delta > keeper_delta,
        "insurance share must exceed keeper share"
    );
}

// ---------------------------------------------------------------------------
// Negative test: keeper triggers on healthy account → rejected, no state
// mutation.
// ---------------------------------------------------------------------------

#[test]
fn liquidation_trigger_on_healthy_account_rejected() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, _pk_keeper) = generate_keypair();
    fund_margin(&db, alice, 50_000 * MICRO); // big collateral, definitely healthy
    fund_margin(&db, bob, 50_000 * MICRO);

    // Build alice's long 1 BTC @ 50k.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_bob,
            0,
            MARKET,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_alice,
            0,
            MARKET,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // Trigger: account is solvent. Should error.
    let mut state = BlockState::new(&db);
    let v = liq_trigger_tx(&sk_keeper, 0, alice, MARKET);
    let err = apply_tx_at(&mut state, &ctx_at(2), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::NotLiquidatable);
}

// ---------------------------------------------------------------------------
// Insurance exhaustion → reduce-only halt → subsequent non-reduce-only
// PlacePerpOrder rejected.
// ---------------------------------------------------------------------------

#[test]
fn insurance_exhaustion_triggers_reduce_only_halt() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    set_mark(&db, MARKET, 50_000 * Q8);

    // SEED INSURANCE WITH ZERO. Any bad debt will not be absorbed.
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 0,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    let (sk_alice, pk_alice) = generate_keypair();
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = generate_keypair();
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, _pk_keeper) = generate_keypair();
    fund_margin(&db, alice, 1_500 * MICRO); // tight collateral
    fund_margin(&db, bob, 200_000 * MICRO);

    // Alice goes long 1 BTC @ 50k. Bob is seller.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_bob,
            0,
            MARKET,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        ),
        0,
    )
    .unwrap();
    apply_tx_at(
        &mut state,
        &ctx_at(1),
        &place_tx(
            &sk_alice,
            0,
            MARKET,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        ),
        1,
    )
    .unwrap();
    state.commit().unwrap();

    // Mark CRASHES to 30_000 — a $20_000 loss vs $1_500 collateral.
    // Equity = 1_500 - 20_000 = -18_500. Bad debt will be ≈ $18_500 after
    // closing the position.
    set_mark(&db, MARKET, 30_000 * Q8);

    // The book has no bid at our liquidation prices in this test, so the
    // liquidation cascade can't fully close the position. We seed a thin
    // bid: bob places a buy at 30_000 q8 (the mark), worse than 50/100/200
    // bps of the new mark. Wait — adverse offset for selling = mark*9_950/10_000
    // = 29_850 q8. So bob's bid at 30_000 will cross the first tranche.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(2),
        &place_tx(
            &sk_bob,
            1,
            MARKET,
            Side::Buy,
            30_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            3,
        ),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // Trigger.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(3),
        &liq_trigger_tx(&sk_keeper, 0, alice, MARKET),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    // After liquidation: alice has bad_debt (insurance can't cover), and the
    // market is in reduce-only halt.
    let alice_m = MarginAccounts::new(&db).get(&alice).unwrap();
    assert!(alice_m.bad_debt > 0, "alice should have residual bad debt");
    let pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert!(
        pm.paused_reduce_only_only,
        "market should enter reduce-only halt after insurance exhaustion"
    );

    // Now a NON-reduce-only PlacePerpOrder by bob (anybody, really) should
    // be rejected with `ReduceOnlyOnly`.
    let mut state = BlockState::new(&db);
    let v = place_tx(
        &sk_bob,
        2,
        MARKET,
        Side::Buy,
        30_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        5,
    );
    let err = apply_tx_at(&mut state, &ctx_at(4), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::ReduceOnlyOnly);

    // Verify a reduce-only order would also be accepted in the halted
    // market. Set up a fresh trader with a long position to close.
    let (sk_carol, pk_carol) = generate_keypair();
    let carol = address_from_pubkey(&pk_carol);
    fund_margin(&db, carol, 200_000 * MICRO);
    // Carol opens 0.5 BTC short via a maker against bob's still-resting
    // bid at 30_000. Wait — the simpler path: put a fresh position on
    // carol by hand and let her close it reduce-only. (Direct CF write,
    // outside the executor — tests do this elsewhere too.)
    Positions::new(&db).put(&Position {
        owner: carol,
        market: MARKET,
        side: Side::Buy,
        qty: ONE_BTC / 100, // 0.01 BTC
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
    // Carol's reduce-only sell at 30_000 q8: matches against bob's resting bid.
    // The market is in reduce-only halt; we expect this to be ACCEPTED.
    let mut state = BlockState::new(&db);
    let v = place_tx(
        &sk_carol,
        0,
        MARKET,
        Side::Sell,
        30_000 * Q8,
        ONE_BTC / 100,
        Tif::Ioc,
        true,
        6,
    );
    apply_tx_at(&mut state, &ctx_at(4), &v, 0).unwrap();
}

// ---------------------------------------------------------------------------
// Negative: liquidation tx with no mark price errors with `MissingMarkPrice`.
// ---------------------------------------------------------------------------

#[test]
fn liquidation_trigger_without_mark_errors() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));
    // Intentionally do NOT set a mark.

    let (_sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund_margin(&db, alice, 1_000 * MICRO);
    // Stuff a position by hand (no order flow) so list_for_owner finds it.
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

    let (sk_keeper, _pk_keeper) = generate_keypair();
    let mut state = BlockState::new(&db);
    let v = liq_trigger_tx(&sk_keeper, 0, alice, MARKET);
    let err = apply_tx_at(&mut state, &ctx_at(1), &v, 0).unwrap_err();
    assert_eq!(err, ExecError::MissingMarkPrice);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// (No reusable helpers below; tests embed assertion logic inline because the
// state surface they touch is small and grep-able.)

#[test]
fn an_owner_who_triggers_their_own_liquidation_earns_no_keeper_share() {
    // Core review L3. The keeper share (0.2% of the liquidated notional) is
    // paid to the trigger's sender. When the sender is the account being
    // liquidated the share goes to the insurance fund instead, so
    // self-triggering cannot discount the fee. The close itself still runs.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    setup_market(&db, Address([3; 20]));

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (bob_sk, bob_pk) = generate_keypair();
    let bob = address_from_pubkey(&bob_pk);
    // Alice long 1 BTC from 50k with thin collateral; Bob short.
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
    Positions::new(&db).put(&Position {
        owner: bob,
        market: MARKET,
        side: Side::Sell,
        qty: ONE_BTC,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * Q8,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
    fund_margin(&db, alice, 1_280 * MICRO);
    fund_margin(&db, bob, 1_000_000 * MICRO);
    fund_margin(&db, Address::INSURANCE, 1_000_000 * MICRO);
    // Mark drops to 49,200: Alice's equity is 480, MM on 49.2k is 492 — she
    // is liquidatable, and after the 1,000 loss of closing at 49,000 she has
    // 280 left, enough to pay the whole 245 fee.
    set_mark(&db, MARKET, 49_200 * Q8);
    // Bob bids at 49,000 so the liquidation ladder (49,200 - 50 bps = 48,954
    // and lower) crosses it.
    let mut state = BlockState::new(&db);
    let bid = place_tx(
        &bob_sk,
        0,
        MARKET,
        Side::Buy,
        49_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    apply_tx_at(&mut state, &ctx_at(1), &bid, 0).unwrap();
    state.commit().unwrap();

    let insurance_before = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    let mut state = BlockState::new(&db);
    let trigger = liq_trigger_tx(&alice_sk, 0, alice, MARKET);
    apply_tx_at(&mut state, &ctx_at(2), &trigger, 0).unwrap();
    state.commit().unwrap();

    assert!(
        Positions::new(&db)
            .get(&alice, &MARKET)
            .is_none_or(|p| p.qty == 0),
        "the position was closed"
    );
    // 1 BTC at 49,000 = 49,000 notional; 0.5% = 245 USD total, all of it
    // to insurance because the keeper was the owner.
    let insurance_after = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    assert_eq!(insurance_after - insurance_before, 245 * MICRO);
}
