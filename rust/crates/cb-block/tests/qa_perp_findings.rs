//! Regression tests for the perpetual-side defects `cb-simgame` found and
//! `docs/qa-report.md` wrote up: F1 (reduce-only bypass), F2 (uninitialised
//! funding index), F4 (bad debt ignored), F5 (self-blocked liquidation),
//! F8 (uncharged fees) and F10 (zero-quantity abort).
//!
//! QA's originals asserted the broken behaviour and were written to fail once
//! it was fixed. These are their replacements: the same minimal scenarios,
//! asserting what the executor is supposed to do instead. Each test names the
//! finding it descends from, so a future regression is legible as a return of a
//! known defect rather than as a fresh mystery.
//!
//! Scope: the perpetual side of the executor. Spot findings live in
//! `qa_spot_findings.rs`; the state-root gap lives in
//! `qa_state_root_margin_gap.rs`.

use cb_block::{apply_tx_at, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_engine::{is_liquidatable, DEFAULT_TIERS};
use cb_state::{
    Database, FundingIndices, MarginAccounts, Meta, OracleState, PerpMarkets, Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, MarginAccount, PerpMarket, Position, Side, Signed, Tif, H256,
};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;
const FUNDING_INTERVAL: u32 = 5;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn setup() -> (tempfile::TempDir, Database, SecretKey, Address) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions in this fixture are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it. `Meta` is
    // outside the state-root preimage, so this does not affect any root asserted here.
    Meta::new(&db).set_chain_id(1);
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"qa-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    PerpMarkets::new(&db).put(&PerpMarket {
        id: MARKET,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: FUNDING_INTERVAL,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    fund(&db, Address::INSURANCE, 1_000_000 * MICRO);
    Meta::new(&db).set_proposer_addr(Address::ZERO);
    (dir, db, oracle_sk, oracle_authority)
}

fn fund(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

fn collateral_of(db: &Database, owner: Address) -> u128 {
    MarginAccounts::new(db)
        .get(&owner)
        .map(|m| m.collateral)
        .unwrap_or(0)
}

fn actor(seed: &str) -> (SecretKey, Address) {
    let (sk, pk) = keypair_from_seed(seed.as_bytes());
    (sk, address_from_pubkey(&pk))
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

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

#[allow(clippy::too_many_arguments)]
fn place(
    sk: &SecretKey,
    nonce: u64,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: Tif,
    reduce_only: bool,
    cid: u8,
) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::PlacePerpOrder {
            market: MARKET,
            side,
            price_q8,
            qty,
            tif,
            post_only: false,
            reduce_only,
            client_order_id: H256([cid; 32]),
        },
    )
}

/// Apply `txs` in order at `height`, returning each result, then commit.
fn apply_block(
    db: &Database,
    height: u64,
    proposer: Address,
    txs: &[VerifiedTx],
) -> Vec<Result<(), ExecError>> {
    let mut state = BlockState::new(db);
    let mut out = Vec::new();
    for (i, v) in txs.iter().enumerate() {
        out.push(apply_tx_at(
            &mut state,
            &ctx_at(height, proposer),
            v,
            i as u32,
        ));
    }
    state.commit().unwrap();
    out
}

fn oracle_at(sk: &SecretKey, nonce: u64, px_q8: u128, height: u64) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8,
            submitted_height: height,
        },
    )
}

/// Net value of a margin account: collateral less what it owes. Bad debt is
/// negative money, so the two fields are two halves of one number and any
/// conservation statement has to be made about the difference.
fn net_of(db: &Database, owner: Address) -> i128 {
    MarginAccounts::new(db)
        .get(&owner)
        .map(|m| m.collateral as i128 - m.bad_debt as i128)
        .unwrap_or(0)
}

fn bad_debt_of(db: &Database, owner: Address) -> u128 {
    MarginAccounts::new(db)
        .get(&owner)
        .map(|m| m.bad_debt)
        .unwrap_or(0)
}

// ===========================================================================
// F1 — a reduce-only order may only reduce.
// ===========================================================================
//
// The defect: `apply_place_perp_order` admitted a reduce-only order on the sole
// condition that an *opposing position existed*, never comparing the order's
// quantity against the position's, and then skipped the initial-margin gate
// because a reduce-only order "cannot grow the IM requirement". The matcher did
// not revalidate either, despite its module documentation claiming it did. One
// base unit of long was therefore a licence to open an unbounded short.
//
// Two layers now enforce the rule and both are tested here: admission rejects
// an oversized order outright, and the matcher caps the fill plan against live
// position state so a reduce-only order that was valid when it was placed
// cannot open a position when it is hit as a maker later.

#[test]
fn an_oversized_reduce_only_order_is_rejected_and_the_position_is_untouched() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-ro-alice");
    let (sk_bob, bob) = actor("qa-ro-bob");
    // Alice is funded with ten dollars. Bob is the deep counterparty.
    fund(&db, alice, 10 * MICRO);
    fund(&db, bob, 100_000_000 * MICRO);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    // Alice acquires the smallest possible long: one base unit.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(&sk_bob, 0, Side::Sell, 50_000 * Q8, 1, Tif::Gtc, false, 1),
            place(&sk_alice, 0, Side::Buy, 50_000 * Q8, 1, Tif::Ioc, false, 2),
        ],
    );
    let pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(pos.side, Side::Buy);
    assert_eq!(pos.qty, 1, "alice starts with a one-unit long");

    // Bob rests a ten-bitcoin bid, the liquidity the flip would have used.
    let huge = 10 * ONE_BTC;
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[place(
            &sk_bob,
            1,
            Side::Buy,
            50_000 * Q8,
            huge,
            Tif::Gtc,
            false,
            3,
        )],
    );

    // Control: without the flag the order fails the initial-margin gate.
    let control = apply_block(
        &db,
        4,
        Address::ZERO,
        &[place(
            &sk_alice,
            1,
            Side::Sell,
            50_000 * Q8,
            huge,
            Tif::Ioc,
            false,
            4,
        )],
    );
    assert_eq!(
        control[0],
        Err(ExecError::InsufficientMargin),
        "a plain order of this size must fail the initial-margin gate"
    );

    // The fix: the reduce-only flag no longer buys a way past that gate. The
    // order is rejected for what it actually is — bigger than the position it
    // claims to close — rather than clamped, so the fill the trader gets can
    // never differ from the order they signed.
    let result = apply_block(
        &db,
        5,
        Address::ZERO,
        &[place(
            &sk_alice,
            1,
            Side::Sell,
            50_000 * Q8,
            huge,
            Tif::Ioc,
            true,
            5,
        )],
    );
    assert_eq!(
        result[0],
        Err(ExecError::ReduceOnlyTooLarge),
        "a reduce-only order larger than the position it would close is rejected"
    );

    let after = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(after.side, Side::Buy, "the position did not flip");
    assert_eq!(after.qty, 1, "and it did not change size");

    // And the account is nowhere near liquidation, which is what the
    // initial-margin gate exists to guarantee.
    let marks = vec![(MARKET, 50_000 * Q8)];
    let margin = MarginAccounts::new(&db).get(&alice).unwrap();
    let positions = Positions::new(&db).list_for_owner(&alice);
    assert!(
        !is_liquidatable(&margin, &positions, &marks, DEFAULT_TIERS),
        "a ten-dollar account must not be holding half a million of notional"
    );

    // The rule is a bound, not a ban: an order sized to the position still
    // closes it.
    let ok = apply_block(
        &db,
        6,
        Address::ZERO,
        &[place(
            &sk_alice,
            1,
            Side::Sell,
            50_000 * Q8,
            1,
            Tif::Ioc,
            true,
            6,
        )],
    );
    assert_eq!(ok[0], Ok(()), "a correctly sized reduce-only order is fine");
    assert_eq!(Positions::new(&db).get(&alice, &MARKET).unwrap().qty, 0);
}

#[test]
fn a_resting_reduce_only_order_is_cancelled_rather_than_opening_a_position() {
    // Admission alone cannot close the hole. A reduce-only GTC order is checked
    // against the position that exists when it is placed; it can be hit as a
    // maker blocks later, by which time that position may be gone. The matcher
    // therefore re-derives the owner's capacity at fill time and drops the
    // order rather than letting it open the other side.
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-ro-stale-alice");
    let (sk_bob, bob) = actor("qa-ro-stale-bob");
    fund(&db, alice, 1_000_000 * MICRO);
    fund(&db, bob, 1_000_000 * MICRO);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    // Alice goes long 1 BTC.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );

    // Alice rests a take-profit: reduce-only, sized exactly to her position,
    // at the top of the price band (mark + 10%) so it does not cross yet.
    // Valid when placed.
    let rest = apply_block(
        &db,
        3,
        Address::ZERO,
        &[place(
            &sk_alice,
            1,
            Side::Sell,
            55_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            true,
            3,
        )],
    );
    assert_eq!(rest[0], Ok(()), "a correctly sized reduce-only order rests");
    assert_eq!(
        cb_state::PerpOrders::new(&db)
            .list_for_market(&MARKET)
            .iter()
            .filter(|o| o.owner == alice)
            .count(),
        1
    );

    // She then closes the position by other means, leaving the take-profit
    // resting with nothing behind it.
    apply_block(
        &db,
        4,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                1,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                4,
            ),
            place(
                &sk_alice,
                2,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                true,
                5,
            ),
        ],
    );
    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        0,
        "alice is flat"
    );

    // Bob now crosses the stale take-profit. It must be cancelled, not filled.
    apply_block(
        &db,
        5,
        Address::ZERO,
        &[place(
            &sk_bob,
            2,
            Side::Buy,
            55_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            6,
        )],
    );

    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        0,
        "the stale reduce-only order must not have opened a short for alice"
    );
    assert!(
        cb_state::PerpOrders::new(&db)
            .list_for_market(&MARKET)
            .iter()
            .all(|o| o.owner != alice),
        "and it must have left the book rather than waiting to try again"
    );
}

// ===========================================================================
// F2 — funding is zero-sum because a new position is stamped with the live
// cumulative index.
// ===========================================================================
//
// The defect: `apply_fill_to_position`'s flat-to-open case copied
// `entry_funding_index` from the previous row — `Signed::ZERO` for an account
// that had never held a position on the market. `settle_market_funding` charges
// `qty * (cumulative - entry)`, so a position opened long after the index
// started accruing was charged for the whole history before it existed. Longs
// and shorts carried different snapshots, the flows stopped cancelling, and
// total collateral moved with no counterparty on the other side.

#[test]
fn funding_is_zero_sum_because_new_positions_inherit_the_live_funding_index() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-fund-alice"); // long from the start
    let (sk_bob, bob) = actor("qa-fund-bob"); // short throughout
    let (sk_eve, eve) = actor("qa-fund-eve"); // long, opened late
    let (sk_prop, proposer) = actor("qa-fund-proposer");
    for a in [alice, bob, eve] {
        fund(&db, a, 1_000_000 * MICRO);
    }
    fund(&db, proposer, 0);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    // Alice long 1 BTC, Bob short 1 BTC.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );

    // Give the market a premium so the funding rate is non-zero. The executor
    // derives one from the book midpoint now (F9), but this test is about the
    // index stamp, so write the premium directly and keep the rate independent
    // of whatever depth the fixture happens to leave resting.
    {
        let store = OracleState::new(&db);
        let mut m = store.get_mark(&MARKET).unwrap();
        m.premium_ema_q9 = 1_000_000; // 0.1%
        store.put_mark(&m);
    }

    let tick = |db: &Database, height: u64, nonce: u64| {
        let mut state = BlockState::new(db);
        apply_tx_at(
            &mut state,
            &ctx_at(height, proposer),
            &sign(&sk_prop, nonce, TxBody::FundingTick { market: MARKET }),
            0,
        )
        .unwrap();
        state.commit().unwrap();
    };
    // First tick establishes `last_tick_height` and accrues nothing; the second
    // moves the cumulative index for the first time.
    tick(&db, 5, 0);
    tick(&db, 10, 1);

    let cumulative_after_two_ticks = FundingIndices::new(&db)
        .get(&MARKET)
        .unwrap()
        .cumulative
        .to_i128();
    assert!(
        cumulative_after_two_ticks > 0,
        "the funding index must have accrued for this test to mean anything"
    );

    // Eve opens a fresh long against Bob, who adds to his short. Eve has never
    // held a position on this market.
    apply_block(
        &db,
        11,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                1,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &sk_eve,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                4,
            ),
        ],
    );
    let eve_pos = Positions::new(&db).get(&eve, &MARKET).unwrap();
    assert_eq!(
        eve_pos.entry_funding_index.to_i128(),
        cumulative_after_two_ticks,
        "a position opened now is liable for funding from now, not from the \
         beginning of the market"
    );

    // Bob doubled his short across the index move. The tick at height 10
    // already settled his first BTC and re-stamped it at the live index, so the
    // volume-weighted average of "1 BTC at the live index" and "1 BTC opened
    // now at the live index" is that same index — every lot he holds is square
    // with the market up to this moment.
    let bob_pos = Positions::new(&db).get(&bob, &MARKET).unwrap();
    assert_eq!(bob_pos.qty, 2 * ONE_BTC);
    assert_eq!(
        bob_pos.entry_funding_index.to_i128(),
        cumulative_after_two_ticks,
        "adding to a position volume-weights the stamp, so the added lots owe \
         nothing for the period before they were added"
    );

    let participants = [alice, bob, eve, Address::INSURANCE];
    let before: i128 = participants.iter().map(|a| net_of(&db, *a)).sum();

    // Third tick. Funding now flows for real.
    tick(&db, 15, 2);

    let after: i128 = participants.iter().map(|a| net_of(&db, *a)).sum();
    assert_eq!(
        after - before,
        0,
        "funding is a transfer between longs and shorts: the total across every \
         participant must not move by a single micro-USD"
    );

    // And it really did flow — a tick that moved nothing would satisfy the
    // conservation check above for the wrong reason.
    let alice_delta = net_of(&db, alice);
    assert!(
        alice_delta < 1_000_000 * MICRO as i128,
        "the long must actually have paid something"
    );
}

// ===========================================================================
// F5 — a resting order cannot block its owner's liquidation.
// ===========================================================================
//
// The defect: `apply_liquidation_trigger` submitted its reduce-only tranches
// through the ordinary matcher with `SelfTradeAction::CancelTaker`. An account
// with a resting order on the closing side at a crossing price had it hit first
// in price-time order, so the matcher returned `RejectedSelfTrade` with no
// fills — discarding fills it had already planned against other makers — and
// the liquidation loop, which never inspected the outcome, reported success
// having closed nothing and paid the keeper nothing. All three tranches walked
// the same book, so one small order defeated the whole ladder.
//
// The fix cancels the owner's resting orders on the market before any tranche
// is generated. Filling against the victim's own quote (the `CancelMaker`
// alternative) would have let them choose their own liquidation price.

#[test]
fn a_resting_order_cannot_block_its_own_liquidation() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-stp-alice");
    let (sk_bob, bob) = actor("qa-stp-bob");
    let (sk_keeper, keeper) = actor("qa-stp-keeper");
    // Enough that closing her out still leaves a residue to pay the keeper
    // from, so "the keeper was paid" isolates the liquidation rather than an
    // empty account.
    fund(&db, alice, 2_300 * MICRO);
    fund(&db, bob, 10_000_000 * MICRO);
    fund(&db, keeper, 0);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    // Alice goes long 1 BTC at 50k and — while still healthy — rests a small
    // bid of her own at 49_000. Bob stacks real bids underneath it.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
            place(
                &sk_alice,
                1,
                Side::Buy,
                49_000 * Q8,
                ONE_BTC / 100,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &sk_bob,
                1,
                Side::Buy,
                48_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                4,
            ),
            place(
                &sk_bob,
                2,
                Side::Buy,
                48_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                5,
            ),
        ],
    );
    assert!(
        cb_state::PerpOrders::new(&db)
            .list_for_market(&MARKET)
            .iter()
            .any(|o| o.owner == alice && o.side == Side::Buy),
        "alice has a resting bid in front of bob's"
    );

    // The market falls. Alice is underwater.
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 1, 48_000 * Q8, 3)],
    );
    let marks = vec![(MARKET, 48_000 * Q8)];
    assert!(
        is_liquidatable(
            &MarginAccounts::new(&db).get(&alice).unwrap(),
            &Positions::new(&db).list_for_owner(&alice),
            &marks,
            DEFAULT_TIERS
        ),
        "alice must be liquidatable for this test to mean anything"
    );

    // The keeper triggers, with alice's bid still in the book.
    let result = apply_block(
        &db,
        4,
        Address::ZERO,
        &[sign(
            &sk_keeper,
            0,
            TxBody::LiquidationTrigger {
                owner: alice,
                market: MARKET,
            },
        )],
    );
    assert_eq!(result[0], Ok(()));

    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        0,
        "the position is closed: a self-owned quote no longer blocks the ladder"
    );
    assert!(
        collateral_of(&db, keeper) > 0,
        "and the keeper collects its bounty"
    );
    assert!(
        cb_state::PerpOrders::new(&db)
            .list_for_market(&MARKET)
            .iter()
            .all(|o| o.owner != alice),
        "alice's own resting orders on the market were cancelled by the \
         liquidation, so nothing is left that could re-open exposure"
    );
}

// ===========================================================================
// F8 — perpetual trading fees are charged.
// ===========================================================================
//
// The defect: `PerpMarket` carried `taker_fee_bps` and `maker_fee_bps_signed`,
// the Cap'n Proto surface published both and the dashboard showed them, but no
// path in `cb-block` read either field. On the perpetual side only the
// liquidation fee was ever charged.

#[test]
fn perpetual_fills_charge_the_taker_fee_and_pay_the_maker_rebate() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-fee-alice");
    let (sk_bob, bob) = actor("qa-fee-bob");
    fund(&db, alice, 100_000 * MICRO);
    fund(&db, bob, 100_000 * MICRO);

    let market = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(market.taker_fee_bps, 3, "the market declares a taker fee");
    assert_eq!(market.maker_fee_bps_signed, -1, "and a maker rebate");

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    // Bob rests in block 2 and alice crosses in block 3, so bob's quote has
    // survived a block boundary and earns the rebate.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[place(
            &sk_bob,
            0,
            Side::Sell,
            50_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            false,
            1,
        )],
    );
    let insurance_before = collateral_of(&db, Address::INSURANCE);
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[place(
            &sk_alice,
            0,
            Side::Buy,
            50_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            false,
            2,
        )],
    );

    // A 1 BTC fill at 50 000 is $50 000 of notional. Three basis points of it
    // is $15 from the taker; one basis point is $5 back to the maker.
    let taker_fee = 15 * MICRO;
    let maker_rebate = 5 * MICRO;
    assert_eq!(
        collateral_of(&db, alice),
        100_000 * MICRO - taker_fee,
        "the taker pays `taker_fee_bps` on the notional"
    );
    assert_eq!(
        collateral_of(&db, bob),
        100_000 * MICRO + maker_rebate,
        "and the maker is paid `maker_fee_bps_signed`"
    );
    assert_eq!(
        collateral_of(&db, Address::INSURANCE),
        insurance_before + taker_fee - maker_rebate,
        "the fee sink nets the difference: nothing is minted and nothing burned"
    );
}

#[test]
fn a_maker_that_has_not_rested_a_block_earns_no_rebate() {
    // `PerpOrder.placed_at_height` exists for this guard and nothing read it.
    // Without it, an account can place a quote and cross it from a second
    // address in the same block, harvesting the rebate on volume it created for
    // exactly that purpose.
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-wash-alice");
    let (sk_bob, bob) = actor("qa-wash-bob");
    fund(&db, alice, 100_000 * MICRO);
    fund(&db, bob, 100_000 * MICRO);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    // Rest and cross within one block.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    assert_eq!(
        collateral_of(&db, bob),
        100_000 * MICRO,
        "a quote that never survived a block boundary earns nothing"
    );
    assert_eq!(
        collateral_of(&db, alice),
        100_000 * MICRO - 15 * MICRO,
        "the taker still pays"
    );
}

// ===========================================================================
// F10 — a zero-quantity perpetual order is rejected, not aborted.
// ===========================================================================
//
// The defect: the pre-trade margin gate simulated a worst-case fill *before*
// the matcher's `BookError::ZeroQty` guard ran, so `apply_fill_to_position` was
// reached with `fill_qty == 0` and its `debug_assert!` fired.
// `cb_tx::validate_static` rejects the transaction at mempool admission, so the
// ordinary path was shielded — but the executor is the consensus-critical
// layer, and a replica re-executing a block a Byzantine proposer built aborted
// instead of rejecting the block.

#[test]
fn a_zero_quantity_perp_order_is_rejected_rather_than_aborting_the_executor() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-zero-alice");
    fund(&db, alice, 1_000 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    let tx = place(&sk_alice, 0, Side::Buy, 50_000 * Q8, 0, Tif::Gtc, false, 1);
    assert!(
        cb_tx::validate::validate_static(&tx.signed.tx).is_err(),
        "mempool admission rejects a zero-quantity order"
    );

    // The executor is the layer under test: it must answer "invalid tx", under
    // debug assertions as well as without them.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut state = BlockState::new(&db);
        let r = apply_tx_at(&mut state, &ctx_at(2, Address::ZERO), &tx, 0);
        state.commit().unwrap();
        r
    }));
    assert_eq!(
        outcome.expect("the executor must not abort on input it can receive"),
        Err(ExecError::BadPerpOrder)
    );

    // Zero price is the same class of input and takes the same path.
    let zero_price = place(&sk_alice, 0, Side::Buy, 0, ONE_BTC, Tif::Gtc, false, 2);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut state = BlockState::new(&db);
        let r = apply_tx_at(&mut state, &ctx_at(3, Address::ZERO), &zero_price, 0);
        state.commit().unwrap();
        r
    }));
    assert_eq!(
        outcome.expect("nor on a zero price"),
        Err(ExecError::BadPerpOrder)
    );
}

// ===========================================================================
// F4 — bad debt is absorbed, restricts the account, and is repaid.
// ===========================================================================
//
// The defect: three paths wrote `MarginAccount.bad_debt` — realised PnL on a
// fill, a funding tick, and futures settlement — and exactly one read it:
// `apply_bad_debt_absorption`, reachable only from the tail of a
// `LiquidationTrigger` and only for the account that trigger named. Nothing in
// `cb-engine` read it at all, so an account that owed the protocol money was
// treated as one with *no* money rather than *negative* money.

#[test]
fn bad_debt_from_an_ordinary_fill_is_absorbed_by_the_insurance_fund() {
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-debt-alice");
    let (sk_bob, bob) = actor("qa-debt-bob");
    fund(&db, alice, 1_500 * MICRO);
    fund(&db, bob, 10_000_000 * MICRO);
    let insurance_start = collateral_of(&db, Address::INSURANCE);
    assert!(insurance_start > 0, "the fund starts with money in it");

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    // Alice goes long 1 BTC at 50k on $1_500 of collateral.
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    // The market falls to 45k. Alice closes into Bob's bid, realising a $5_000
    // loss against collateral that cannot cover it.
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 1, 45_000 * Q8, 3)],
    );

    let participants = [alice, bob, Address::INSURANCE];
    let value_before: i128 = participants.iter().map(|a| net_of(&db, *a)).sum();
    let alice_before = collateral_of(&db, alice);
    let insurance_before = collateral_of(&db, Address::INSURANCE);

    apply_block(
        &db,
        4,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                1,
                Side::Buy,
                45_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &sk_alice,
                1,
                Side::Sell,
                45_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                4,
            ),
        ],
    );

    let alice_margin = MarginAccounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_margin.collateral, 0, "the collateral is wiped out");
    assert_eq!(
        alice_margin.bad_debt, 0,
        "and the uncovered loss is absorbed rather than left outstanding: no \
         liquidation was triggered, because the position closed itself"
    );
    assert_eq!(
        Positions::new(&db).get(&alice, &MARKET).unwrap().qty,
        0,
        "the position is flat, so no liquidation would ever have run on it"
    );

    // The fund paid at least the uncovered part of the $5_000 loss.
    let insurance_after = collateral_of(&db, Address::INSURANCE);
    let uncovered = 5_000 * MICRO - alice_before;
    assert!(
        insurance_before - insurance_after >= uncovered,
        "the insurance fund covered the shortfall the account could not"
    );

    // Absorption is a transfer, not a write-off: the total across every
    // participant is unchanged, counting debt as negative money.
    let value_after: i128 = participants.iter().map(|a| net_of(&db, *a)).sum();
    assert_eq!(value_after, value_before);
}

#[test]
fn outstanding_bad_debt_is_negative_equity_and_blocks_new_risk() {
    // The other half of F4: when the insurance fund cannot cover the loss, the
    // residual has to restrict the account that owes it. `compute_equity`,
    // `is_liquidatable` and the initial-margin gate all read it now.
    let (_dir, db, oracle_sk, _) = setup();
    let (sk_alice, alice) = actor("qa-debt2-alice");
    let (sk_bob, bob) = actor("qa-debt2-bob");
    fund(&db, alice, 1_500 * MICRO);
    fund(&db, bob, 10_000_000 * MICRO);
    // An empty fund, so the debt has nowhere to go.
    fund(&db, Address::INSURANCE, 0);

    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &sk_alice,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 1, 45_000 * Q8, 3)],
    );
    apply_block(
        &db,
        4,
        Address::ZERO,
        &[
            place(
                &sk_bob,
                1,
                Side::Buy,
                45_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &sk_alice,
                1,
                Side::Sell,
                45_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                4,
            ),
        ],
    );

    let debt = bad_debt_of(&db, alice);
    assert!(
        debt > 0,
        "with an empty fund the uncovered loss stays on the account"
    );
    // Insurance exhaustion flips the market into reduce-only, which is the
    // halt the design specifies and which this class of debt never reached.
    assert!(
        PerpMarkets::new(&db)
            .get(&MARKET)
            .unwrap()
            .paused_reduce_only_only,
        "an exhausted insurance fund halts the market to reduce-only"
    );

    let alice_margin = MarginAccounts::new(&db).get(&alice).unwrap();
    let equity = cb_engine::compute_equity(&alice_margin, &[], &[]);
    assert_eq!(
        equity.to_i128(),
        -(debt as i128),
        "an account that owes the protocol money has negative equity, not zero"
    );

    // Fund it again the way a faucet or a deposit would. The credit pays the
    // debt down instead of sitting alongside it, and while the debt outlasts
    // the credit the account cannot take on new risk.
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: alice,
        collateral: 1_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: debt,
    });
    let still_indebted = MarginAccounts::new(&db).get(&alice).unwrap();
    assert!(
        !cb_engine::passes_initial_margin(
            &still_indebted,
            &[Position {
                owner: alice,
                market: MARKET,
                side: Side::Buy,
                qty: ONE_BTC / 20,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: 45_000 * Q8,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            }],
            &[(MARKET, 45_000 * Q8)],
            DEFAULT_TIERS
        ),
        "an account still owing {debt} micro-USD must not pass the \
         initial-margin gate as though it owed nothing"
    );
}

// ===========================================================================
// F11 — resting perpetual orders reserve margin.
// ===========================================================================
//
// The defect: the initial-margin gate simulated the incoming order filling in
// full and charged the requirement against the resulting position set — and
// nothing else. An account's other resting orders were invisible to it, so N
// orders that each passed the gate on their own could all rest, and all fill.
// The gate now charges against the account's *potential* position: what it
// holds plus what every non-reduce-only resting order could add, per market.

#[test]
fn a_second_resting_order_the_account_cannot_also_afford_is_rejected() {
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice");
    // 1 BTC at 50k is 50k notional, IM 2% = 1,000 USD. Alice has 1,500: one
    // such order passes, two together do not. Both bids sit at the mark so
    // the gate's worst-case fill carries no unrealised gain to hide behind.
    fund(&db, alice, 1_500 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );

    let first = place(
        &alice_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    let second = place(
        &alice_sk,
        1,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        2,
    );
    let results = apply_block(&db, 2, Address::ZERO, &[first, second]);
    assert_eq!(results[0], Ok(()), "the first order fits");
    assert_eq!(
        results[1],
        Err(ExecError::InsufficientMargin),
        "the second is charged for the first's potential fill and does not"
    );
    assert_eq!(
        cb_state::PerpOrders::new(&db).list_for_owner(&alice).len(),
        1,
        "one order rests, not two"
    );

    // Cancel the first and the second fits again.
    let resting = cb_state::PerpOrders::new(&db).list_for_owner(&alice)[0].order_id;
    let cancel = sign(&alice_sk, 1, TxBody::CancelPerpOrder { order_id: resting });
    let again = place(
        &alice_sk,
        2,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        3,
    );
    let results = apply_block(&db, 3, Address::ZERO, &[cancel, again]);
    assert_eq!(results, vec![Ok(()), Ok(())]);
    assert_eq!(
        cb_state::PerpOrders::new(&db)
            .list_for_owner(&alice)
            .iter()
            .map(|o| o.price_q8)
            .collect::<Vec<_>>(),
        vec![50_000 * Q8]
    );
}

#[test]
fn opposite_side_resting_orders_are_charged_as_the_larger_side_not_the_sum() {
    // A bid and an ask cannot both add to the same position: whichever side
    // fills, the other reduces it. The exposure is the larger side, so an
    // account that can afford one 1 BTC order can rest a bid and an ask.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-two-sided");
    fund(&db, alice, 1_500 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        49_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    let ask = place(
        &alice_sk,
        1,
        Side::Sell,
        51_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        2,
    );
    let results = apply_block(&db, 2, Address::ZERO, &[bid, ask]);
    assert_eq!(results, vec![Ok(()), Ok(())]);
    assert_eq!(
        cb_state::PerpOrders::new(&db).list_for_owner(&alice).len(),
        2
    );
}

#[test]
fn a_resting_order_against_a_held_position_is_charged_net_of_that_position() {
    // Long 1 BTC with a 2 BTC ask resting is a potential short of 1, not 2:
    // the first BTC of the ask closes the long. The gate charges the net.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-net");
    let (bob_sk, bob) = actor("f11-bob-net");
    fund(&db, alice, 1_100 * MICRO);
    fund(&db, bob, 1_000_000 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    let results = apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &bob_sk,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &alice_sk,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    assert_eq!(results, vec![Ok(()), Ok(())], "alice is long 1 BTC");
    // A 2 BTC ask at the mark: post-fill she would be short 1 BTC, IM 1,000,
    // against ~1,085 of collateral after the taker fee. Charged as a 2 BTC
    // short it would need 2,000 and be refused.
    let ask = place(
        &alice_sk,
        1,
        Side::Sell,
        50_000 * Q8,
        2 * ONE_BTC,
        Tif::Gtc,
        false,
        3,
    );
    let results = apply_block(&db, 3, Address::ZERO, &[ask]);
    assert_eq!(results, vec![Ok(())]);
}

// ===========================================================================
// F11, second pass — three ways the gate still let an order rest with
// nothing behind it, found by the 2026-09-25 core audit.
// ===========================================================================
//
// The gate charged the requirement against a potential position built from
// the incoming order *as a fill*, netted against the held position and, in
// `potential_exposures`, against the resting orders on the other side; it
// took the equity from that filled world alone; and it dropped the PnL the
// simulated fill realised. Each is a test below.

#[test]
fn resting_bids_and_asks_are_not_netted_against_each_other() {
    // One bid and one ask of 1 BTC are charged as 1 BTC (the larger side),
    // and that is correct: whichever fills, the other reduces. But a second
    // bid makes the bid side 2 BTC, and 2 BTC is what the account holds if
    // only the bids fill — the resting ask does not shrink that. The old
    // gate subtracted the ask from the bids and charged 1 BTC for any number
    // of pairs.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-pairs");
    // 1 BTC at 50k is IM 1,000; 1,500 affords one side of 1 BTC, not 2.
    fund(&db, alice, 1_500 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        49_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    let ask = place(
        &alice_sk,
        1,
        Side::Sell,
        51_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        2,
    );
    assert_eq!(
        apply_block(&db, 2, Address::ZERO, &[bid, ask]),
        vec![Ok(()), Ok(())],
        "a bid and an ask of 1 BTC are one side's worth of exposure"
    );

    let second_bid = place(
        &alice_sk,
        2,
        Side::Buy,
        49_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        3,
    );
    assert_eq!(
        apply_block(&db, 3, Address::ZERO, &[second_bid]),
        vec![Err(ExecError::InsufficientMargin)],
        "two bids are 2 BTC of potential long whatever is resting on the ask side"
    );
    let second_ask = place(
        &alice_sk,
        2,
        Side::Sell,
        51_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        4,
    );
    assert_eq!(
        apply_block(&db, 4, Address::ZERO, &[second_ask]),
        vec![Err(ExecError::InsufficientMargin)],
        "and the same on the ask side"
    );
    assert_eq!(
        cb_state::PerpOrders::new(&db).list_for_owner(&alice).len(),
        2,
        "the first pair rests; nothing more"
    );
}

#[test]
fn a_bid_below_the_mark_is_not_credited_with_the_gain_it_would_make_if_it_filled() {
    // $500 of collateral and a 1 BTC bid at 45k against a 50k mark. Filled
    // at its limit that is a $5,000 gain, and the old gate counted it —
    // 5,500 against a 1,000 requirement — so an account with nothing could
    // rest a bid that fills exactly when the market has come down to it and
    // the gain is gone.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-below-mark");
    fund(&db, alice, 500 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        45_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    assert_eq!(
        apply_block(&db, 2, Address::ZERO, &[bid]),
        vec![Err(ExecError::InsufficientMargin)],
        "the requirement is judged on what the account has, not on what the fill would hand it"
    );
    assert!(cb_state::PerpOrders::new(&db)
        .list_for_owner(&alice)
        .is_empty());

    // Funded for the requirement, the same bid rests.
    fund(&db, alice, 1_100 * MICRO);
    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        45_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        2,
    );
    assert_eq!(apply_block(&db, 3, Address::ZERO, &[bid]), vec![Ok(())]);
}

#[test]
fn an_underwater_account_cannot_sell_through_its_long_into_a_short() {
    // Long 1 BTC from 50k on $1,500; the mark falls to 45k. Equity is
    // -3,500 and the account is liquidatable. Selling 2 BTC would realise
    // the $5,000 loss and leave a 1 BTC short whose own PnL at the mark is
    // zero. The old gate saw the short, saw the untouched 1,500 of
    // collateral, and let the account flip into a position it has nothing
    // behind. Reducing is still allowed; the flag is how it is asked for.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-flip");
    let (bob_sk, bob) = actor("f11-bob-flip");
    fund(&db, alice, 1_500 * MICRO);
    fund(&db, bob, 10_000_000 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    let results = apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &bob_sk,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &alice_sk,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    assert_eq!(results, vec![Ok(()), Ok(())], "alice is long 1 BTC");
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 1, 45_000 * Q8, 3)],
    );
    let marks = vec![(MARKET, 45_000 * Q8)];
    assert!(
        is_liquidatable(
            &MarginAccounts::new(&db).get(&alice).unwrap(),
            &Positions::new(&db).list_for_owner(&alice),
            &marks,
            DEFAULT_TIERS
        ),
        "alice must be underwater for this test to mean anything"
    );

    // Bob bids 2 BTC at the mark; alice tries to sell 2 into it.
    let results = apply_block(
        &db,
        4,
        Address::ZERO,
        &[
            place(
                &bob_sk,
                1,
                Side::Buy,
                45_000 * Q8,
                2 * ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &alice_sk,
                1,
                Side::Sell,
                45_000 * Q8,
                2 * ONE_BTC,
                Tif::Ioc,
                false,
                4,
            ),
        ],
    );
    assert_eq!(results[0], Ok(()));
    assert_eq!(
        results[1],
        Err(ExecError::InsufficientMargin),
        "the realised loss is charged before the new side is opened"
    );
    let pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(
        (pos.side, pos.qty),
        (Side::Buy, ONE_BTC),
        "the long is untouched"
    );

    // A reduce-only close of the position is what an underwater account is
    // for, and it still goes through.
    let results = apply_block(
        &db,
        5,
        Address::ZERO,
        &[place(
            &alice_sk,
            1,
            Side::Sell,
            45_000 * Q8,
            ONE_BTC,
            Tif::Ioc,
            true,
            5,
        )],
    );
    assert_eq!(results, vec![Ok(())]);
    assert_eq!(Positions::new(&db).get(&alice, &MARKET).unwrap().qty, 0);
}

#[test]
fn an_unmarked_market_takes_no_derivative_orders() {
    // A market that has never had an oracle post has no mark, and the gate
    // skipped markets without one — requirement zero — so any funded margin
    // account could open any size on a freshly listed market until the
    // first post arrived, and nothing could liquidate it until then either.
    // That was first fixed by pricing the requirement at the order's own
    // limit. Every derivative order is now priced against a band around the
    // mark, and a market with no mark has no band: the order is refused
    // outright, however well funded the account.
    let (_dir, db, _oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("f11-alice-unmarked");
    fund(&db, alice, 100 * MICRO);
    assert!(OracleState::new(&db).get_mark(&MARKET).is_none());

    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        1,
    );
    assert_eq!(
        apply_block(&db, 1, Address::ZERO, &[bid]),
        vec![Err(ExecError::MissingMarkPrice)],
    );

    fund(&db, alice, 5_000 * MICRO);
    let bid = place(
        &alice_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        ONE_BTC,
        Tif::Gtc,
        false,
        2,
    );
    assert_eq!(
        apply_block(&db, 2, Address::ZERO, &[bid]),
        vec![Err(ExecError::MissingMarkPrice)],
        "funding does not help: there is no band to price the order against"
    );
    assert!(cb_state::PerpOrders::new(&db)
        .list_for_market(&MARKET)
        .is_empty());
}

// ===========================================================================
// ClosePosition — the gateway's close button reached an executor that
// refused it.
// ===========================================================================

#[test]
fn close_position_closes_at_the_mark_within_the_slippage_bound() {
    // `POST /positions/close` submits `ClosePosition`, and every one came
    // back `Unsupported`. It is a reduce-only IOC on the other side of the
    // position, limited to the mark moved `max_slippage_bps` against the
    // trader; `qty == 0` closes all of it.
    let (_dir, db, oracle_sk, _) = setup();
    let (alice_sk, alice) = actor("close-alice");
    let (bob_sk, bob) = actor("close-bob");
    fund(&db, alice, 10_000 * MICRO);
    fund(&db, bob, 10_000_000 * MICRO);
    apply_block(
        &db,
        1,
        Address::ZERO,
        &[oracle_at(&oracle_sk, 0, 50_000 * Q8, 1)],
    );
    apply_block(
        &db,
        2,
        Address::ZERO,
        &[
            place(
                &bob_sk,
                0,
                Side::Sell,
                50_000 * Q8,
                2 * ONE_BTC,
                Tif::Gtc,
                false,
                1,
            ),
            place(
                &alice_sk,
                0,
                Side::Buy,
                50_000 * Q8,
                2 * ONE_BTC,
                Tif::Ioc,
                false,
                2,
            ),
        ],
    );
    // Bob bids 1% under the mark and 3% under it.
    apply_block(
        &db,
        3,
        Address::ZERO,
        &[
            place(
                &bob_sk,
                1,
                Side::Buy,
                49_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &bob_sk,
                2,
                Side::Buy,
                48_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                4,
            ),
        ],
    );
    let close = |nonce, qty| {
        sign(
            &alice_sk,
            nonce,
            TxBody::ClosePosition {
                market: MARKET,
                qty,
                max_slippage_bps: 200,
            },
        )
    };
    // Close everything with a 2% bound: the 49.5k bid is inside it, the
    // 48.5k bid is not, so half the position closes and the rest stays.
    assert_eq!(
        apply_block(&db, 4, Address::ZERO, &[close(1, 0)]),
        vec![Ok(())]
    );
    let pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!((pos.side, pos.qty), (Side::Buy, ONE_BTC));
    // Bob is the other side of the fill: his short is down by the same 1 BTC.
    assert_eq!(Positions::new(&db).get(&bob, &MARKET).unwrap().qty, ONE_BTC);

    // A size larger than the position is refused rather than clamped, as
    // any reduce-only order is.
    assert_eq!(
        apply_block(&db, 5, Address::ZERO, &[close(2, 2 * ONE_BTC)]),
        vec![Err(ExecError::ReduceOnlyTooLarge)]
    );
}
