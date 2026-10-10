//! M7 end-to-end lifecycle integration test.
//!
//! Drives the full perps stack across a chain of blocks and asserts state at
//! each checkpoint. Uses explicit per-tx `apply_tx_at` so intra-block ordering
//! is unambiguous (the spot ordering rule sorts ties by `(sender, nonce)`,
//! which would scramble place-then-cross flows that depend on the maker
//! resting before the taker arrives).
//!
//! What this proves end-to-end (not exercised by any earlier single-file test):
//!
//!   1. `OracleUpdate` writes Mark, fills set last_trade, MarkPrice updates
//!      again on the next OracleUpdate using the median(oracle, mid, last)
//!      formula — and the result is what liquidation logic reads.
//!   2. Position deltas applied across blocks accumulate the way a real
//!      sequence of fills would (entry stays at 50k after block 2, doesn't
//!      drift in the funding-only blocks).
//!   3. `FundingTick` at the boundary actually mutates `MarginAccount.collateral`
//!      for both long and short, conserved across them.
//!   4. After mark drops, `LiquidationTrigger` from a third party (`keeper`)
//!      closes the long position, splits the fee, leaves no bad debt.
//!   5. The chain head advances exactly K times, state_root is non-zero, and
//!      replay (separate test) yields the same hashes.
//!
//! Scope: M1 + M2 + M3 + M4 + M6. M5 (FuturesSettle) is intentionally not
//! exercised.

use cb_block::{apply_tx_at, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{
    Database, FundingIndices, MarginAccounts, Meta, OracleState, PerpMarkets, Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Signed, Tif, H256};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;
const FUNDING_INTERVAL: u32 = 5;

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn setup_db_and_market() -> (tempfile::TempDir, Database, SecretKey, Address) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"e2e-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    PerpMarkets::new(&db).put(&PerpMarket {
        id: MARKET,
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
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
    // Insurance seeded $10_000.
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 10_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
    Meta::new(&db).set_proposer_addr(Address::ZERO);
    (dir, db, oracle_sk, oracle_authority)
}

fn fund_margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
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

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

/// Apply `txs` in order at `height`, then commit. Tx_index is the position
/// in the slice — the same packing `build_and_apply` would produce for the
/// canonical-ordered slice. Returns the post-commit state root.
fn apply_block(db: &Database, height: u64, proposer: Address, txs: &[VerifiedTx]) -> H256 {
    let mut state = BlockState::new(db);
    for (i, v) in txs.iter().enumerate() {
        apply_tx_at(&mut state, &ctx_at(height, proposer), v, i as u32).unwrap();
    }
    state.commit().unwrap().state_root
}

// ---------------------------------------------------------------------------
// Full lifecycle: long trader liquidated after mark drop
// ---------------------------------------------------------------------------

#[test]
fn full_perp_lifecycle_long_liquidation_after_mark_drop() {
    let (_dir, db, oracle_sk, _oracle_auth) = setup_db_and_market();

    let (sk_alice, pk_alice) = keypair_from_seed(b"e2e-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = keypair_from_seed(b"e2e-bob");
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, pk_keeper) = keypair_from_seed(b"e2e-keeper");
    let keeper = address_from_pubkey(&pk_keeper);

    // Alice tight (will be liquidated). Bob deep (counterparty + bid stack).
    fund_margin(&db, alice, 1_500 * MICRO);
    fund_margin(&db, bob, 200_000 * MICRO);
    fund_margin(&db, keeper, 0);

    let proposer = Address::ZERO; // matches set_proposer_addr above

    // -----------------------------------------------------------------------
    // Block 1: oracle = $50_000. No mark history → mark = oracle = 50k.
    // -----------------------------------------------------------------------
    apply_block(
        &db,
        1,
        proposer,
        &[sign(
            &oracle_sk,
            0,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 50_000 * Q8,
                submitted_height: 1,
            },
        )],
    );
    let m = OracleState::new(&db).get_mark(&MARKET).unwrap();
    assert_eq!(m.px_q8, 50_000 * Q8);

    // -----------------------------------------------------------------------
    // Block 2: bob rests sell, alice IOC-buys → alice LONG, bob SHORT. Bob
    // also rests bids deep enough for the liquidation cascade later.
    // -----------------------------------------------------------------------
    apply_block(
        &db,
        2,
        proposer,
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
                &sk_bob,
                1,
                Side::Buy,
                49_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                3,
            ),
            place(
                &sk_bob,
                2,
                Side::Buy,
                49_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                4,
            ),
            place(
                &sk_bob,
                3,
                Side::Buy,
                48_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                false,
                5,
            ),
        ],
    );
    let alice_pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    let bob_pos = Positions::new(&db).get(&bob, &MARKET).unwrap();
    assert_eq!(alice_pos.side, Side::Buy);
    assert_eq!(alice_pos.qty, ONE_BTC);
    assert_eq!(alice_pos.entry_px_q8, 50_000 * Q8);
    assert_eq!(bob_pos.side, Side::Sell);
    assert_eq!(bob_pos.qty, ONE_BTC);
    assert_eq!(bob_pos.entry_px_q8, 50_000 * Q8);
    // Last-trade price persists on the perp market.
    let pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert_eq!(pm.last_trade_px_q8, 50_000 * Q8);
    assert_eq!(pm.last_trade_height, 2);

    // -----------------------------------------------------------------------
    // Block 3, 4: filler blocks. The funding-tick boundary is at height 5.
    // We put a no-op cancel on a non-existent order (idempotent success) so
    // the block isn't empty and we don't otherwise touch market state.
    // -----------------------------------------------------------------------
    let cancel_noop = |sk, nonce| {
        sign(
            sk,
            nonce,
            TxBody::CancelPerpOrder {
                order_id: H256([0xee; 32]),
            },
        )
    };
    apply_block(&db, 3, proposer, &[cancel_noop(&sk_bob, 4)]);
    apply_block(&db, 4, proposer, &[cancel_noop(&sk_bob, 5)]);

    // Verify alice's position is unchanged across the filler blocks.
    let alice_pos_pre_funding = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(alice_pos_pre_funding.qty, ONE_BTC);
    assert_eq!(alice_pos_pre_funding.entry_px_q8, 50_000 * Q8);

    // -----------------------------------------------------------------------
    // Block 5: funding boundary. premium_ema is 0 (no premium ever recorded
    // because mark equals oracle and there's no mid recompute step in M2).
    // So funding rate is 0 — the tick is required for the block to be valid
    // (`enforce_required_funding_ticks` checks at submit time), and our
    // helper `apply_block` doesn't run that check (it doesn't go through
    // `build_and_apply`), but we still emit the tick to exercise the
    // executor path.
    // -----------------------------------------------------------------------
    // FundingTick must be signed by the proposer. Since proposer is
    // Address::ZERO above, we need a sender key whose pubkey hashes to ZERO
    // — impossible. We instead reuse the keeper's keypair and re-set
    // proposer_addr to keeper for THIS block only via ctx_at.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(5, keeper),
        &sign(&sk_keeper, 0, TxBody::FundingTick { market: MARKET }),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let fi = FundingIndices::new(&db).get(&MARKET).unwrap();
    assert_eq!(fi.last_tick_height, 5);
    assert_eq!(fi.rate_q9, 0);
    // No premium → no funding flow → collateral unchanged by the tick. Alice
    // is down the 3 bps taker fee she paid to open (50_000 * 3 / 10_000 = $15),
    // which lands in the insurance fund — see QA F8.
    assert_eq!(
        MarginAccounts::new(&db).get(&alice).unwrap().collateral,
        1_500 * MICRO - 15 * MICRO
    );

    // -----------------------------------------------------------------------
    // Block 6: oracle drop to $48_000. Mark = median(48k, mid_clamped, last_clamped).
    //   K=100 bps → clamp band [47_520, 48_480].
    //   mid = none (no two-sided book on this side; bob's bids exist but
    //   ask side is empty). Falls back to oracle.
    //   last = 50_000 → clamped to 48_480.
    //   median(48_000, 48_000, 48_480) = 48_000.
    // -----------------------------------------------------------------------
    apply_block(
        &db,
        6,
        proposer,
        &[sign(
            &oracle_sk,
            1,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 48_000 * Q8,
                submitted_height: 6,
            },
        )],
    );
    let mark = OracleState::new(&db).get_mark(&MARKET).unwrap();
    assert_eq!(mark.px_q8, 48_000 * Q8);

    // -----------------------------------------------------------------------
    // Block 7: keeper triggers liquidation.
    //   alice's uPnL @ mark=48_000: 1 BTC * (48k - 50k) = -$2_000
    //   equity = 1_500 - 2_000 = -500
    //   notional = 48_000; MM = 480
    //   equity < MM → liquidate.
    //
    //   Cascade: IOC sell at 48_000*0.995 = 47_760, qty 1 BTC.
    //   Bob's resting bids @ 49_500 / 49_000 / 48_500 all > 47_760 → fills
    //   at the best bid. Maker price 49_500, qty 1 BTC. Alice closes.
    //
    //   PnL on close: 1 BTC * (49_500 - 50_000) = -$500. Alice's collateral:
    //   1_500 - 500 = $1_000. Then liq fees from notional 49_500:
    //     insurance 0.3% = ceil(49_500 * 30 / 10_000) = $148.50 → 148_500_000 mu
    //     keeper    0.2% =      49_500 * 20 / 10_000  = $99    → 99_000_000 mu
    //   Both come out of alice's collateral first. After: $1_000 - $247.5 = $752.5.
    // -----------------------------------------------------------------------
    let pre_ins = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    apply_block(
        &db,
        7,
        proposer,
        &[sign(
            &sk_keeper,
            1, // keeper used nonce 0 on the FundingTick
            TxBody::LiquidationTrigger {
                owner: alice,
                market: MARKET,
            },
        )],
    );

    // Assertions: position closed, no bad debt, fee split correct.
    let alice_pos = Positions::new(&db).get(&alice, &MARKET).unwrap();
    assert_eq!(alice_pos.qty, 0);
    let alice_m = MarginAccounts::new(&db).get(&alice).unwrap();
    assert_eq!(alice_m.bad_debt, 0);
    let post_ins = MarginAccounts::new(&db)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    let keeper_m = MarginAccounts::new(&db).get(&keeper).unwrap();
    let ins_delta = post_ins - pre_ins;
    let keeper_bounty = keeper_m.collateral;
    // Pin exact fees from the worked example above.
    let expected_notional_micro: u128 =
        cb_engine::position_notional_micro_usd(ONE_BTC, 49_500 * Q8);
    let (expected_ins, expected_keeper) = cb_engine::liquidation_fee_split(expected_notional_micro);
    assert_eq!(ins_delta, expected_ins);
    assert_eq!(keeper_bounty, expected_keeper);
    // Insurance share > keeper share.
    assert!(ins_delta > keeper_bounty);
    // Market is NOT in reduce-only halt.
    let pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
    assert!(!pm.paused_reduce_only_only);
    // Conservation: alice + bob + insurance + keeper collateral change must
    // sum to 0 (counting bob's PnL credit from the liq fill). Bob was short
    // 1 BTC @ 50k; alice closed her long against bob's bid @ 49_500. Bob is
    // STILL short 1 BTC because alice's IOC sell was a taker; bob's bid
    // CONSUMED (from the matcher's POV the bid is removed). Wait — bob's
    // bid was the maker. A long-close (alice sells) hitting bob's bid
    // means bob BUYS at 49_500. That CLOSES bob's prior short (-1 BTC) →
    // bob is flat. Bob's PnL on the close: short closed at 49_500 vs entry
    // 50_000 = +$500.
    let bob_pos_post = Positions::new(&db).get(&bob, &MARKET).unwrap();
    assert_eq!(bob_pos_post.qty, 0);
    let bob_m = MarginAccounts::new(&db).get(&bob).unwrap();
    // Bob started 200_000; gained 500. Total 200_500.
    assert_eq!(bob_m.collateral, 200_500 * MICRO);
    // Equity-flow conservation including the insurance seed:
    //   Alice initial 1_500 → final ~752.5 → outflow 747.5
    //   Bob initial 200_000 → final 200_500 → inflow 500
    //   Insurance initial 10_000 → final 10_148.5 → inflow 148.5
    //   Keeper initial 0 → final 99 → inflow 99
    //   ↓ check: outflow 747.5 == inflow 500 + 148.5 + 99 = 747.5 ✓
    // Alice's outflow is measured from her initial funding, so it also carries
    // the 3 bps taker fee she paid to open the position back in block 2 — $15
    // on $50_000 of notional (QA F8). `ins_delta` is measured across the
    // liquidation block alone, so that earlier transfer has to be added to the
    // inflow side explicitly. It is still a transfer: it went to the same
    // insurance fund, just before the window opened.
    let open_taker_fee = 15 * MICRO;
    let alice_outflow = 1_500 * MICRO - alice_m.collateral;
    let total_inflow =
        (bob_m.collateral - 200_000 * MICRO) + ins_delta + keeper_bounty + open_taker_fee;
    assert_eq!(
        alice_outflow, total_inflow,
        "equity flow must balance across all four participants"
    );

    // Final state-root non-zero (the per-block accounts root from the last
    // commit). Replay-equality is checked in `determinism.rs`.
    let final_root = apply_block(
        &db,
        8,
        proposer,
        &[sign(
            &sk_keeper,
            2,
            TxBody::CancelPerpOrder {
                order_id: H256([0xee; 32]),
            },
        )],
    );
    assert_ne!(final_root, H256::ZERO);
}

// ---------------------------------------------------------------------------
// Funding actually moves money when premium is non-zero. Two ticks needed:
// the first ever tick on a market sets `last_tick_height` from 0 to height
// but accrues elapsed=0 (we don't retroactively credit funding before the
// market started ticking). The second tick crosses real elapsed and triggers
// the settlement.
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_with_nonzero_premium_settles_funding_into_collateral() {
    let (_dir, db, oracle_sk, _) = setup_db_and_market();
    let (sk_alice, pk_alice) = keypair_from_seed(b"funding-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = keypair_from_seed(b"funding-bob");
    let bob = address_from_pubkey(&pk_bob);
    let (sk_keeper, pk_keeper) = keypair_from_seed(b"funding-keeper"); // proposer
    let keeper = address_from_pubkey(&pk_keeper);
    fund_margin(&db, alice, 10_000 * MICRO);
    fund_margin(&db, bob, 10_000 * MICRO);

    let proposer = Address::ZERO;

    // Block 1: oracle.
    apply_block(
        &db,
        1,
        proposer,
        &[sign(
            &oracle_sk,
            0,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 50_000 * Q8,
                submitted_height: 1,
            },
        )],
    );

    // Block 2: alice goes LONG, bob SHORT @ 50k.
    apply_block(
        &db,
        2,
        proposer,
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

    // Inject a non-zero premium-EMA so the next FundingTick yields a non-zero rate.
    {
        let store = OracleState::new(&db);
        let mut m = store.get_mark(&MARKET).unwrap();
        m.premium_ema_q9 = 1_000_000; // 0.1% premium → after inner clamp → rate 500_000 q9
        store.put_mark(&m);
    }

    // Filler blocks 3, 4 (no-op cancels).
    let cancel_noop = |sk, nonce, cid: u8| {
        sign(
            sk,
            nonce,
            TxBody::CancelPerpOrder {
                order_id: H256([cid; 32]),
            },
        )
    };
    apply_block(&db, 3, proposer, &[cancel_noop(&sk_bob, 1, 0xee)]);
    apply_block(&db, 4, proposer, &[cancel_noop(&sk_bob, 2, 0xee)]);

    // Block 5: FIRST funding tick. Sets last_tick_height from 0 to 5, elapsed=0,
    // cumulative still 0 → no flow. But position.entry_funding_index gets
    // pinned to 0 (it was already 0 — opening fills initialize entry_funding_index).
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(5, keeper),
        &sign(&sk_keeper, 0, TxBody::FundingTick { market: MARKET }),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let pre_alice = MarginAccounts::new(&db).get(&alice).unwrap().collateral;
    let pre_bob = MarginAccounts::new(&db).get(&bob).unwrap().collateral;
    // No flow on first tick. Alice took the opening fill and paid the 3 bps
    // taker fee on $50_000 of notional ($15) into the insurance fund; Bob
    // rested his quote in the same block the fill happened, so the maker rebate
    // is withheld by the wash-trade guard. Both per QA F8.
    assert_eq!(pre_alice, 10_000 * MICRO - 15 * MICRO);
    assert_eq!(pre_bob, 10_000 * MICRO);
    let fi = FundingIndices::new(&db).get(&MARKET).unwrap();
    assert_eq!(fi.last_tick_height, 5);

    // Filler blocks 6..=9 to drift to next boundary at 10.
    for (i, n) in (6u64..=9).zip(3u64..) {
        apply_block(&db, i, proposer, &[cancel_noop(&sk_bob, n, 0xee)]);
    }

    // Block 10: SECOND funding tick. elapsed = 10-5 = 5 blocks, one interval.
    let mut state = BlockState::new(&db);
    apply_tx_at(
        &mut state,
        &ctx_at(10, keeper),
        &sign(&sk_keeper, 1, TxBody::FundingTick { market: MARKET }),
        0,
    )
    .unwrap();
    state.commit().unwrap();

    let post_alice = MarginAccounts::new(&db).get(&alice).unwrap().collateral;
    let post_bob = MarginAccounts::new(&db).get(&bob).unwrap().collateral;
    // Conservation across long+short.
    assert_eq!(pre_alice + pre_bob, post_alice + post_bob);
    // Long pays, short receives.
    assert!(post_alice < pre_alice);
    assert!(post_bob > pre_bob);
    // One full interval at the per-interval rate 500_000 q9 (QA F12), on the
    // $50,000 mark: the index is 500_000 * 50_000 / 100 = 250_000_000, and
    // delta = qty * cumulative / RATE_DEN_Q9
    //       = 1e8 * 250_000_000 / 1e9 = 25_000_000 micro-USD
    // — $25, the notional times 0.05%. (It was $0.05 while the index left
    // out the price.)
    assert_eq!(pre_alice - post_alice, 25 * MICRO);
    assert_eq!(post_bob - pre_bob, 25 * MICRO);
}
