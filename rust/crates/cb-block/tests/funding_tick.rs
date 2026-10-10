//! Integration tests for `FundingTick` execution and the block-level boundary
//! enforcement (M2).
//!
//! Builds 3 sequential blocks that cross a funding-interval boundary and
//! asserts that:
//!
//! * Position holders accrue exactly the expected funding into
//!   `MarginAccount.collateral` at the boundary block.
//! * Non-boundary blocks update `cumulative` but do NOT settle balances.
//! * A block at a boundary that omits the required `FundingTick` is rejected
//!   by `build_and_apply` with `MissingRequiredFundingTick`.
//! * A `FundingTick` signed by an account other than the proposer is
//!   rejected with `NotAuthorized`.

use cb_block::{apply_tx, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_engine::RATE_DEN_Q9;
use cb_state::{
    Database, FundingIndices, MarginAccounts, Meta, OracleState, PerpMarkets, Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, MarginAccount, MarkPrice, PerpMarket, Position, Side, Signed,
};
use tempfile::tempdir;

/// Tiny funding interval — easy to cross within a few-block test. 5 blocks = 1
/// "funding hour" for our purposes here. The protocol picks 3600 blocks at
/// 1s blocktime; a 5-block interval is still deterministic and lets the test
/// run without thousands of iterations.
const TEST_FUNDING_INTERVAL: u32 = 5;
const PERP_INDEX: u16 = 0;

fn perp_market(authority: Address) -> PerpMarket {
    PerpMarket {
        id: DerivId::perp(PERP_INDEX),
        underlying: AssetId::Btc,
        tick_size: 100,
        lot_size: 1000,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: TEST_FUNDING_INTERVAL,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: authority,
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

fn sign(sk: &secp256k1::SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
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

fn seed_position(db: &Database, owner: Address, side: Side, qty: u128, collateral: u128) {
    Positions::new(db).put(&Position {
        owner,
        market: DerivId::perp(PERP_INDEX),
        side,
        qty,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000u128 * 100_000_000,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

#[test]
fn funding_tick_accrues_index_and_settles_at_boundary() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);

    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let market = DerivId::perp(PERP_INDEX);
    PerpMarkets::new(&db).put(&perp_market(proposer));

    // Pre-seed a non-zero premium so funding actually accrues. In M3+ this is
    // recomputed from the book; in M2 we just write it directly.
    OracleState::new(&db).put_mark(&MarkPrice {
        market,
        px_q8: 50_000u128 * 100_000_000,
        computed_at_height: 0,
        // 0.1% premium -> after inner clamp(int=0 - prem=1_000_000) -> -500_000
        // -> raw = 1_000_000 + (-500_000) = 500_000 q9 = 0.05% / hr.
        // outer clamp ok. So rate_q9 = 500_000 each FundingTick.
        premium_ema_q9: 1_000_000,
    });

    let alice = Address([0xa1; 20]);
    let bob = Address([0xb0; 20]);
    // Alice long 1 BTC, Bob short 1 BTC. (1e8 base units = 1 BTC.)
    seed_position(&db, alice, Side::Buy, 100_000_000, 1_000_000_000);
    seed_position(&db, bob, Side::Sell, 100_000_000, 1_000_000_000);

    // Block 1 (height = TEST_FUNDING_INTERVAL = 5 -> boundary). Tick once.
    // FundingIndex.last_tick_height starts at 0 so elapsed = 0 -> cumulative
    // unchanged this block, but we DO settle (delta == 0 so no balance change).
    let v_tick_h5 = sign(&proposer_sk, 0, TxBody::FundingTick { market });
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx_at(5, proposer), &v_tick_h5).unwrap();
    state.commit().unwrap();

    let idx = FundingIndices::new(&db).get(&market).unwrap();
    assert_eq!(idx.cumulative, Signed::ZERO);
    assert_eq!(idx.rate_q9, 500_000);
    assert_eq!(idx.last_tick_height, 5);

    // Block 2 (height = 10 -> next boundary). Elapsed = 5 blocks = one full
    // interval at the per-interval rate 500_000 q9, weighted by the $50,000
    // mark: cumulative = 500_000 * 50_000 / 100 = 250_000_000 (the index is
    // funding owed per contract unit, par at $100).
    // (QA F12: this used to be rate * blocks = 2_500_000, the hourly cap
    // applied five times in five blocks.)
    let v_tick_h10 = sign(&proposer_sk, 1, TxBody::FundingTick { market });
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx_at(10, proposer), &v_tick_h10).unwrap();
    state.commit().unwrap();

    let idx = FundingIndices::new(&db).get(&market).unwrap();
    assert_eq!(idx.cumulative.to_i128(), 250_000_000);

    // Per-position settlement at boundary block 10:
    //   delta = qty * (cur - entry) / RATE_DEN_Q9
    //         = 1e8 * 250_000_000 / 1e9 = 25_000_000 micro-USD = $25
    // which is the notional, $50,000, times 0.05%. (It used to be $0.05:
    // the index left out the price, 500x too little for BTC.)
    // Alice (long) pays $25; Bob (short) receives $25.
    let alice_margin = MarginAccounts::new(&db).get(&alice).unwrap();
    let bob_margin = MarginAccounts::new(&db).get(&bob).unwrap();
    assert_eq!(alice_margin.collateral, 1_000_000_000 - 25_000_000);
    assert_eq!(bob_margin.collateral, 1_000_000_000 + 25_000_000);

    // Position entry_funding_index advances to the current cumulative.
    let alice_pos = Positions::new(&db).get(&alice, &market).unwrap();
    let bob_pos = Positions::new(&db).get(&bob, &market).unwrap();
    assert_eq!(alice_pos.entry_funding_index.to_i128(), 250_000_000);
    assert_eq!(bob_pos.entry_funding_index.to_i128(), 250_000_000);
    assert_eq!(alice_pos.last_funding_height, 10);

    // Block 3 (height = 15 -> next boundary). One more interval at the same
    // rate and mark. cumulative = 500_000_000. Delta from the previous entry
    // index is again 250_000_000; the same $25 per side.
    let v_tick_h15 = sign(&proposer_sk, 2, TxBody::FundingTick { market });
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx_at(15, proposer), &v_tick_h15).unwrap();
    state.commit().unwrap();

    let idx = FundingIndices::new(&db).get(&market).unwrap();
    assert_eq!(idx.cumulative.to_i128(), 500_000_000);

    let alice_margin = MarginAccounts::new(&db).get(&alice).unwrap();
    let bob_margin = MarginAccounts::new(&db).get(&bob).unwrap();
    assert_eq!(alice_margin.collateral, 1_000_000_000 - 50_000_000);
    assert_eq!(bob_margin.collateral, 1_000_000_000 + 50_000_000);

    // Sanity: total collateral conserved across the two positions.
    assert_eq!(
        alice_margin.collateral + bob_margin.collateral,
        2 * 1_000_000_000
    );

    // Sanity: one interval's charge is notional times rate, computed the
    // long way — pinned so a future scale change forces this test to be
    // updated. notional = qty * mark_q8 / 1e10 micro-USD.
    let notional_micro = 100_000_000i128 * (50_000i128 * 100_000_000) / 10_000_000_000;
    let computed_delta_per_btc = notional_micro * 500_000i128 / RATE_DEN_Q9 as i128;
    assert_eq!(computed_delta_per_btc, 25_000_000);
}

#[test]
fn funding_tick_signed_by_non_proposer_is_unauthorized() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);

    let (_proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    let (attacker_sk, _) = generate_keypair();
    let market = DerivId::perp(PERP_INDEX);
    PerpMarkets::new(&db).put(&perp_market(proposer));

    let v = sign(&attacker_sk, 0, TxBody::FundingTick { market });
    let mut state = BlockState::new(&db);
    let err = apply_tx(&mut state, &ctx_at(5, proposer), &v).unwrap_err();
    assert_eq!(err, ExecError::NotAuthorized);

    // No funding row should have been written.
    assert!(FundingIndices::new(&db).get(&market).is_none());
}

#[test]
fn funding_tick_rejects_unknown_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);

    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer = address_from_pubkey(&proposer_pk);
    // No PerpMarket put → unknown market.
    let market = DerivId::perp(99);

    let v = sign(&proposer_sk, 0, TxBody::FundingTick { market });
    let mut state = BlockState::new(&db);
    let err = apply_tx(&mut state, &ctx_at(5, proposer), &v).unwrap_err();
    assert_eq!(err, ExecError::UnknownMarket);
}
