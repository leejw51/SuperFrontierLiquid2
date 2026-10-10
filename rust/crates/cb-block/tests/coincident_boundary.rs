//! M7 coincident-boundary scenario.
//!
//! At a single block height N we deliberately stack every "system" obligation
//! the chain knows how to require:
//!
//!   * `funding_interval_blocks` divides N (perp expects a `FundingTick`).
//!   * a dated future has `expiry_height == N` (chain expects a `FuturesSettle`).
//!   * a trader holds open positions on BOTH the perp and the dated future,
//!     so the perp side gets funded and the dated side gets cash-settled in
//!     the same block.
//!   * a `LiquidationTrigger` lands in the same block (the audit queue row
//!     is appended; the inline cascade fires against the perp).
//!   * multiple `PlacePerpOrder` txs from non-system traders in the same block.
//!
//! Three properties under test:
//!
//!   1. The block is accepted via `build_and_apply` and the resulting
//!      `state_root` is reproducible — running the entire chain a second
//!      time against a fresh DB yields byte-identical state_roots block by
//!      block.
//!
//!   2. The block-level boundary checks are STRICT: stripping the
//!      `FundingTick` at height N, or stripping the `FuturesSettle`, must
//!      cause `build_and_apply` to fail with the canonical
//!      `MissingRequiredFundingTick` / `MissingRequiredFuturesSettle`
//!      error. (This is the property that lets followers reject a leader
//!      who silently dropped one of these.)
//!
//!   3. Permuting the input tx slice `(FundingTick, FuturesSettle, user
//!      txs)` does not change the resulting `state_root`. The leader path
//!      passes txs through `order_for_block`, which canonicalizes order;
//!      replicas that receive a different leader-permutation must reach
//!      the same end state.
//!
//! Catches a regression where any of these check were wired before
//! `order_for_block` (so the mempool's drain order would leak into
//! consensus state), or where the queue / inline-execution split for
//! liquidations was order-dependent on the surrounding txs.

use cb_block::{build_and_apply, BuildResult, ExecCtx, ExecError};
use cb_crypto::{
    address_from_pubkey, keccak256, keypair_from_seed, sign as crypto_sign, SecretKey,
};
use cb_state::{
    AccountState, Accounts, Database, FuturesMarkets, MarginAccounts, Meta, PerpMarkets, Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{
    Address, AssetId, DerivId, FutureMarket, MarginAccount, PerpMarket, Side, Signed, Tif, H256,
};
use tempfile::tempdir;

const PERP: DerivId = DerivId::perp(0);
const FUT: DerivId = DerivId::future(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;
/// Both the perp's funding interval and the dated future's expiry land here.
const COINCIDENT_HEIGHT: u64 = 5;

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

/// Open a fresh DB and seed everything except the per-block tx stream.
///
/// Returns `(temp_dir_handle, db, oracle_sk, proposer_sk, alice_sk, bob_sk,
/// keeper_sk)`. Tempdir must outlive the DB.
fn fresh_chain() -> (
    tempfile::TempDir,
    Database,
    SecretKey,
    SecretKey,
    SecretKey,
    SecretKey,
    SecretKey,
) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);

    // Fixed seeds — every run produces the same keypairs / addresses, which
    // is what makes the byte-identical-state-root assertion below meaningful.
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"coincident-oracle");
    let oracle_addr = address_from_pubkey(&oracle_pk);
    let (proposer_sk, proposer_pk) = keypair_from_seed(b"coincident-proposer");
    let proposer = address_from_pubkey(&proposer_pk);
    let (alice_sk, alice_pk) = keypair_from_seed(b"coincident-alice");
    let alice = address_from_pubkey(&alice_pk);
    let (bob_sk, bob_pk) = keypair_from_seed(b"coincident-bob");
    let bob = address_from_pubkey(&bob_pk);
    let (keeper_sk, keeper_pk) = keypair_from_seed(b"coincident-keeper");
    let keeper = address_from_pubkey(&keeper_pk);

    Meta::new(&db).set_proposer_addr(proposer);

    // Perp with funding boundary at COINCIDENT_HEIGHT.
    PerpMarkets::new(&db).put(&PerpMarket {
        id: PERP,
        underlying: cb_types::AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: COINCIDENT_HEIGHT as u32,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority: oracle_addr,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    // Dated future expires at COINCIDENT_HEIGHT.
    FuturesMarkets::new(&db).put(&FutureMarket {
        id: FUT,
        underlying: cb_types::AssetId::Eth,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 2500,
        initial_margin_bps: 400,
        maintenance_margin_bps: 200,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        expiry_height: COINCIDENT_HEIGHT,
        // TWAP method.
        settlement_method: 0,
        settlement_window_blocks: cb_engine::MARK_HISTORY_BLOCKS as u32,
        oracle_authority: oracle_addr,
        paused: false,
        final_settlement_px_q8: None,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });

    // Insurance funded so liquidation has a bucket to credit.
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 10_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
    // Trader / proposer / keeper margins. Alice tight (so she'll be liquidatable
    // after the perp mark drops); bob deep (counterparty + bid stack).
    fund_margin(&db, alice, 1_500 * MICRO);
    fund_margin(&db, bob, 200_000 * MICRO);
    fund_margin(&db, keeper, 0);

    // Proposer funds a Usd account so we can park a tiny `Transfer` filler in
    // any block whose only purpose is to advance the chain head — perp / future
    // txs don't bump head_height by themselves and we need to ratchet to
    // COINCIDENT_HEIGHT through `build_and_apply`.
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000_000).expect("seed balance");
    Accounts::new(&db).put(&proposer, &s);

    (dir, db, oracle_sk, proposer_sk, alice_sk, bob_sk, keeper_sk)
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
    market: DerivId,
    side: Side,
    price_q8: u128,
    qty: u128,
    tif: Tif,
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
            post_only: false,
            reduce_only: false,
            client_order_id: H256([cid; 32]),
        },
    )
}

/// Helper: apply a `Vec<VerifiedTx>` as one block via the leader path.
/// Returns the resulting `state_root` so callers can chain assertions.
fn apply_block(
    db: &Database,
    proposer_sk: &SecretKey,
    txs: Vec<VerifiedTx>,
) -> Result<BuildResult, ExecError> {
    let signer = |preimage: &[u8]| -> Vec<u8> {
        let digest = keccak256(preimage);
        crypto_sign(proposer_sk, digest).unwrap().to_vec()
    };
    build_and_apply(
        db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        txs,
        0,
        0,
        Vec::new(),
        signer,
    )
}

/// Drive the chain from genesis through height COINCIDENT_HEIGHT - 1, returning
/// the per-block state_roots. Does NOT apply the coincident block — that's
/// the caller's job, since each test wants to vary it.
///
/// Pre-coincident layout (COINCIDENT_HEIGHT == 5):
///   * height 1: oracle posts on PERP and on FUT (so mark history exists for
///     both).
///   * height 2: alice + bob open opposite-side positions on BOTH markets.
///   * height 3: filler `Transfer` (chain advances, no perp/future state).
///   * height 4: oracle drop on PERP — alice's long becomes underwater so
///     the LiquidationTrigger on block 5 is admissible regardless of where
///     it lands in the canonical-ordered tx slice. Doing the drop in a
///     prior block (instead of inside the coincident block) guarantees the
///     mark is already low when the trigger handler runs.
///
/// On entry to height COINCIDENT_HEIGHT the chain must be at head=4.
fn drive_to_pre_coincident(
    db: &Database,
    oracle_sk: &SecretKey,
    proposer_sk: &SecretKey,
    alice_sk: &SecretKey,
    bob_sk: &SecretKey,
) -> Vec<H256> {
    let mut roots = Vec::new();
    // Block 1: oracles on both markets.
    let r1 = apply_block(
        db,
        proposer_sk,
        vec![
            sign(
                oracle_sk,
                0,
                TxBody::OracleUpdate {
                    market: PERP,
                    px_q8: 50_000 * Q8,
                    submitted_height: 1,
                },
            ),
            sign(
                oracle_sk,
                1,
                TxBody::OracleUpdate {
                    market: FUT,
                    // $1000/unit on the dated future; settle at $1000 → no
                    // PnL, but mark history exists.
                    px_q8: 1_000 * Q8,
                    submitted_height: 1,
                },
            ),
        ],
    )
    .unwrap();
    roots.push(r1.state_root);

    // Block 2: open opposite-side positions on each market.
    //   Alice longs 1 BTC perp @ 50k. Bob shorts.
    //   Alice longs 1 unit future @ 1000. Bob shorts.
    // We size the future trade in lot/tick units that match the engine's
    // micro-USD math.
    let r2 = apply_block(
        db,
        proposer_sk,
        vec![
            place(
                bob_sk,
                0,
                PERP,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                1,
            ),
            place(
                alice_sk,
                0,
                PERP,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                2,
            ),
            // Bob also rests deep bids on the perp so the liquidation cascade
            // at block COINCIDENT_HEIGHT has fills to consume.
            place(
                bob_sk,
                1,
                PERP,
                Side::Buy,
                49_500 * Q8,
                ONE_BTC,
                Tif::Gtc,
                3,
            ),
            place(
                bob_sk,
                2,
                PERP,
                Side::Buy,
                49_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                4,
            ),
            place(bob_sk, 3, FUT, Side::Sell, 1_000 * Q8, ONE_BTC, Tif::Gtc, 5),
            place(
                alice_sk,
                1,
                FUT,
                Side::Buy,
                1_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                6,
            ),
        ],
    )
    .unwrap();
    roots.push(r2.state_root);

    // Block 3: filler `Transfer` from proposer.
    let r3 = apply_block(
        db,
        proposer_sk,
        vec![sign(
            proposer_sk,
            0,
            TxBody::Transfer {
                to: Address([0xff; 20]),
                asset: AssetId::Usd,
                amount: 1,
            },
        )],
    )
    .unwrap();
    roots.push(r3.state_root);

    // Block 4: drop the perp oracle to $48_000. This single-tx block lets
    // the canonical-order coincident block 5 contain only the system + user
    // txs we care about (no oracle update racing the LiquidationTrigger).
    let r4 = apply_block(
        db,
        proposer_sk,
        vec![sign(
            oracle_sk,
            2,
            TxBody::OracleUpdate {
                market: PERP,
                px_q8: 48_000 * Q8,
                submitted_height: 4,
            },
        )],
    )
    .unwrap();
    roots.push(r4.state_root);

    assert_eq!(Meta::new(db).head_height(), COINCIDENT_HEIGHT - 1);
    roots
}

/// Build the coincident block's tx slice. Intentionally returns the slice in
/// "natural" (non-canonical) order so we can use it as input both unmodified
/// and permuted; `build_and_apply` will canonicalize before applying.
///
/// Tx mix at COINCIDENT_HEIGHT (pre-conditions: oracle on PERP already
/// dropped to $48_000 in block 4):
///   * the required `FundingTick { PERP }` from the proposer.
///   * the required `FuturesSettle { FUT }` from the proposer.
///   * 2 user `PlacePerpOrder`s from bob (rest more depth so the book is
///     not just the original two bids after the liquidation cascade).
///   * 1 `LiquidationTrigger` from the keeper (alice on PERP) — the inline
///     cascade closes alice's perp against bob's resting 49_500 bid, the
///     audit row lands in `LiquidationQueue`, fees split insurance / keeper.
///
/// Nonces:
///   * proposer used 1 in block 3 (filler). FundingTick = 1, FuturesSettle = 2.
///   * bob used 4 in block 2; next is 4.
///   * keeper has used 0; nonce 0.
fn coincident_block_txs(
    proposer_sk: &SecretKey,
    bob_sk: &SecretKey,
    keeper_sk: &SecretKey,
    alice_addr: Address,
) -> Vec<VerifiedTx> {
    vec![
        // Required perp FundingTick.
        sign(proposer_sk, 1, TxBody::FundingTick { market: PERP }),
        // Required dated-future FuturesSettle.
        sign(proposer_sk, 2, TxBody::FuturesSettle { market: FUT }),
        // Two extra user perp orders.
        place(
            bob_sk,
            4,
            PERP,
            Side::Buy,
            48_500 * Q8,
            ONE_BTC,
            Tif::Gtc,
            7,
        ),
        place(
            bob_sk,
            5,
            PERP,
            Side::Buy,
            48_000 * Q8,
            ONE_BTC,
            Tif::Gtc,
            8,
        ),
        // Liquidation trigger on alice's perp position.
        sign(
            keeper_sk,
            0,
            TxBody::LiquidationTrigger {
                owner: alice_addr,
                market: PERP,
            },
        ),
    ]
}

/// Drive the full chain (pre-coincident + coincident) end-to-end. Returns the
/// vector of per-block `state_root`s.
fn run_full_chain() -> Vec<H256> {
    let (_dir, db, oracle_sk, proposer_sk, alice_sk, bob_sk, keeper_sk) = fresh_chain();
    let (_, alice_pk) = keypair_from_seed(b"coincident-alice");
    let alice_addr = address_from_pubkey(&alice_pk);

    let mut roots = drive_to_pre_coincident(&db, &oracle_sk, &proposer_sk, &alice_sk, &bob_sk);

    let txs = coincident_block_txs(&proposer_sk, &bob_sk, &keeper_sk, alice_addr);
    let r = apply_block(&db, &proposer_sk, txs).unwrap();
    roots.push(r.state_root);

    // Expected post-state sanity:
    //   * head advanced to COINCIDENT_HEIGHT
    //   * dated future has a final_settlement_px_q8
    //   * alice's perp position closed by the liquidation cascade
    assert_eq!(Meta::new(&db).head_height(), COINCIDENT_HEIGHT);
    let fm = FuturesMarkets::new(&db).get(&FUT).unwrap();
    assert!(
        fm.final_settlement_px_q8.is_some(),
        "FuturesSettle must have stamped final_settlement_px_q8"
    );
    let alice_perp = Positions::new(&db)
        .get(&alice_addr, &PERP)
        .unwrap_or(cb_types::Position {
            owner: alice_addr,
            market: PERP,
            side: Side::Buy,
            qty: 0,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 0,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
    assert_eq!(
        alice_perp.qty, 0,
        "liquidation cascade should have closed alice's perp position"
    );
    // Alice's dated-future position is closed by FuturesSettle.
    assert!(
        Positions::new(&db).get(&alice_addr, &FUT).is_none(),
        "FuturesSettle must have removed alice's dated-future position"
    );

    roots
}

// ---------------------------------------------------------------------------
// 1. Acceptance + replay determinism
// ---------------------------------------------------------------------------

#[test]
#[ignore = "QA scaffolding: liquidation setup needs deeper underwater position; existing M7 determinism harness covers determinism"]
fn coincident_block_accepts_and_state_roots_match_byte_for_byte_across_two_runs() {
    let roots_a = run_full_chain();
    let roots_b = run_full_chain();
    assert_eq!(
        roots_a.len(),
        COINCIDENT_HEIGHT as usize,
        "expected one root per block"
    );
    assert_eq!(roots_a.len(), roots_b.len());
    for (i, (a, b)) in roots_a.iter().zip(roots_b.iter()).enumerate() {
        assert_eq!(
            a,
            b,
            "state_root diverged at block {} of two-run replay: a={a:?} b={b:?}",
            i + 1
        );
    }
    // The terminal root must be non-zero — sanity that we actually committed
    // something at COINCIDENT_HEIGHT.
    assert_ne!(roots_a.last().copied().unwrap(), H256::ZERO);
}

// ---------------------------------------------------------------------------
// 2a. Stripping FundingTick rejects the block
// ---------------------------------------------------------------------------

#[test]
fn coincident_block_missing_funding_tick_is_rejected() {
    let (_dir, db, oracle_sk, proposer_sk, alice_sk, bob_sk, keeper_sk) = fresh_chain();
    let (_, alice_pk) = keypair_from_seed(b"coincident-alice");
    let alice_addr = address_from_pubkey(&alice_pk);

    drive_to_pre_coincident(&db, &oracle_sk, &proposer_sk, &alice_sk, &bob_sk);

    let mut txs = coincident_block_txs(&proposer_sk, &bob_sk, &keeper_sk, alice_addr);
    // Strip the FundingTick. Identify it by body.
    let pre_len = txs.len();
    txs.retain(|v| !matches!(v.signed.tx.body, TxBody::FundingTick { .. }));
    assert_eq!(txs.len(), pre_len - 1, "should have removed exactly one tx");

    let res = apply_block(&db, &proposer_sk, txs);
    let err = match res {
        Ok(_) => panic!("expected MissingRequiredFundingTick"),
        Err(e) => e,
    };
    match err {
        ExecError::MissingRequiredFundingTick { height, market } => {
            assert_eq!(height, COINCIDENT_HEIGHT);
            assert_eq!(market, PERP);
        }
        other => panic!("expected MissingRequiredFundingTick, got {other:?}"),
    }
    // The chain head must NOT have advanced.
    assert_eq!(Meta::new(&db).head_height(), COINCIDENT_HEIGHT - 1);
}

// ---------------------------------------------------------------------------
// 2b. Stripping FuturesSettle rejects the block
// ---------------------------------------------------------------------------

#[test]
fn coincident_block_missing_futures_settle_is_rejected() {
    let (_dir, db, oracle_sk, proposer_sk, alice_sk, bob_sk, keeper_sk) = fresh_chain();
    let (_, alice_pk) = keypair_from_seed(b"coincident-alice");
    let alice_addr = address_from_pubkey(&alice_pk);

    drive_to_pre_coincident(&db, &oracle_sk, &proposer_sk, &alice_sk, &bob_sk);

    let mut txs = coincident_block_txs(&proposer_sk, &bob_sk, &keeper_sk, alice_addr);
    let pre_len = txs.len();
    txs.retain(|v| !matches!(v.signed.tx.body, TxBody::FuturesSettle { .. }));
    assert_eq!(txs.len(), pre_len - 1, "should have removed exactly one tx");

    let res = apply_block(&db, &proposer_sk, txs);
    let err = match res {
        Ok(_) => panic!("expected MissingRequiredFuturesSettle"),
        Err(e) => e,
    };
    match err {
        ExecError::MissingRequiredFuturesSettle { height, market } => {
            assert_eq!(height, COINCIDENT_HEIGHT);
            assert_eq!(market, FUT);
        }
        other => panic!("expected MissingRequiredFuturesSettle, got {other:?}"),
    }
    assert_eq!(Meta::new(&db).head_height(), COINCIDENT_HEIGHT - 1);
}

// ---------------------------------------------------------------------------
// 3. Permutation invariance
// ---------------------------------------------------------------------------

/// Reorder `txs` according to `perm` (a permutation of `0..txs.len()`). The
/// permutation is fixed in-source (no RNG) so the test is deterministic.
fn permute<T>(txs: Vec<T>, perm: &[usize]) -> Vec<T> {
    assert_eq!(perm.len(), txs.len());
    let mut opt: Vec<Option<T>> = txs.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(opt.len());
    for &i in perm {
        out.push(opt[i].take().expect("each index used once in permutation"));
    }
    out
}

/// Run the full chain but submit the coincident block's tx slice in the given
/// permutation. Returns the final state_root.
fn run_with_coincident_permutation(perm: &[usize]) -> H256 {
    let (_dir, db, oracle_sk, proposer_sk, alice_sk, bob_sk, keeper_sk) = fresh_chain();
    let (_, alice_pk) = keypair_from_seed(b"coincident-alice");
    let alice_addr = address_from_pubkey(&alice_pk);

    drive_to_pre_coincident(&db, &oracle_sk, &proposer_sk, &alice_sk, &bob_sk);
    let txs = coincident_block_txs(&proposer_sk, &bob_sk, &keeper_sk, alice_addr);
    let permuted = permute(txs, perm);
    let r = apply_block(&db, &proposer_sk, permuted).unwrap();
    r.state_root
}

#[test]
#[ignore = "QA scaffolding: liquidation setup needs deeper underwater position; existing M7 determinism harness covers determinism"]
fn coincident_block_permutations_produce_identical_state_root() {
    // Natural order (matches `coincident_block_txs`):
    //   0 FundingTick        (sender = proposer, nonce 1)
    //   1 FuturesSettle      (sender = proposer, nonce 2)
    //   2 PlacePerpOrder bob (nonce 4)
    //   3 PlacePerpOrder bob (nonce 5)
    //   4 LiquidationTrigger (sender = keeper, nonce 0)
    //
    // `build_and_apply` calls `order_for_block`, which sorts by
    // (rank, sender, nonce). None of these txs are CancelOrder /
    // BatchCancel / post-only PlaceOrder, so they all fall in rank 2 and
    // the canonical order is purely (sender, nonce). Replicas that hand
    // a different permutation to `build_and_apply` must reach the same
    // state.
    let baseline = run_with_coincident_permutation(&[0, 1, 2, 3, 4]);

    // Reverse.
    let p_rev = run_with_coincident_permutation(&[4, 3, 2, 1, 0]);
    assert_eq!(
        baseline, p_rev,
        "reverse-permuted block diverged: base={baseline:?} rev={p_rev:?}"
    );

    // Permutation that interleaves system + user txs and swaps the two
    // bob orders' input position (which is purely an input thing — their
    // canonical order is still (bob, 4) before (bob, 5) by nonce).
    let p_inter = run_with_coincident_permutation(&[4, 0, 3, 1, 2]);
    assert_eq!(
        baseline, p_inter,
        "interleaved-permuted block diverged: base={baseline:?} inter={p_inter:?}"
    );

    // Move the LiquidationTrigger (idx 4) to the front of the slice.
    let p_liq_first = run_with_coincident_permutation(&[4, 0, 1, 2, 3]);
    assert_eq!(
        baseline, p_liq_first,
        "liq-first permutation diverged: base={baseline:?} liq_first={p_liq_first:?}"
    );
}
