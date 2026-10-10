//! Regression test for QA finding F3: margin accounts are inside the state
//! root.
//!
//! `compute_state_root` used to hash exactly three inputs — the accounts trie,
//! the positions trie and the markets trie. `CF_MARGIN_ACCOUNTS` fed none of
//! them and no other trie covered it, so two replicas could disagree about
//! every trader's collateral, about every unpaid loss, and about the insurance
//! fund, while reporting byte-identical state roots. Consensus could not detect
//! the divergence; the first symptom in production would have been a
//! liquidation one node performed and another refused, discovered after it had
//! been finalised. Genesis-seeded collateral was unauthenticated for the same
//! reason.
//!
//! `cb-state`'s documentation lists what the root deliberately leaves out —
//! funding indices, mark and oracle prices, the liquidation queue, the perp
//! orderbook, the mark-history ring buffer — all justified as "auxiliary, they
//! derive from the txs and block height". Margin accounts were never on that
//! list and are not auxiliary.
//!
//! A fourth trie slot closed it then. The root is now a hash chain over each
//! block's writes (`cb_state::state_hash`) that covers every consensus column
//! family, margin included, and whose genesis link hashes every seeded row —
//! so the property these tests pin holds by construction, and they stay to
//! prove it.
//!
//! These tests assert the property the slot buys, in both directions: a margin
//! divergence changes the root, and two replicas that agree about margin still
//! agree about the root.

use cb_block::{apply_tx_at, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{genesis_state_hash, Database, MarginAccounts, Meta, PerpMarkets, Positions};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Signed, Tif, H256};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;

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

fn ctx_at(height: u64) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: Address::ZERO,
    }
}

fn fund(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

/// Seal the seeded rows into the chain's genesis root, the way genesis does:
/// `genesis_state_hash` hashes every committed row, margin included, so the
/// very first root already commits to every starting balance.
fn seal_genesis_margin(db: &Database) -> H256 {
    let root = genesis_state_hash(db).unwrap();
    Meta::new(db).set_head(0, H256::ZERO, root);
    root
}

/// A replica: fresh database, same market, same participants, same blocks.
fn build_replica(oracle_authority: Address) -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions in this fixture are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it. `Meta` is
    // outside the state-root preimage, so this does not affect any root asserted here.
    Meta::new(&db).set_chain_id(1);
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
        funding_interval_blocks: 1_000_000,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    Meta::new(&db).set_proposer_addr(Address::ZERO);
    (dir, db)
}

/// Apply one block and return the composed state root.
fn apply_block(db: &Database, height: u64, txs: &[VerifiedTx]) -> H256 {
    let mut state = BlockState::new(db);
    for (i, v) in txs.iter().enumerate() {
        apply_tx_at(&mut state, &ctx_at(height), v, i as u32).unwrap();
    }
    let roots = state.commit().unwrap();
    // Advance the head the way a committed block does, so the next block
    // chains onto this one.
    Meta::new(db).set_head(height, H256::ZERO, roots.state_root);
    roots.state_root
}

#[test]
fn two_replicas_disagreeing_on_every_margin_account_report_different_state_roots() {
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"qa-root-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (sk_alice, pk_alice) = keypair_from_seed(b"qa-root-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = keypair_from_seed(b"qa-root-bob");
    let bob = address_from_pubkey(&pk_bob);

    let (_dir_a, honest) = build_replica(oracle_authority);
    let (_dir_b, divergent) = build_replica(oracle_authority);

    // The honest replica seeds the collateral genesis says it should.
    fund(&honest, alice, 100_000 * MICRO);
    fund(&honest, bob, 100_000 * MICRO);
    fund(&honest, Address::INSURANCE, 1_000_000 * MICRO);

    // The divergent replica seeds wildly different collateral for the same
    // accounts, and empties the insurance fund. Nothing else differs. The
    // amounts stay large enough to clear the initial-margin gate, so the two
    // replicas execute the identical transaction sequence with the identical
    // outcome and differ only in the numbers nobody hashes.
    fund(&divergent, alice, 250_000 * MICRO);
    fund(&divergent, bob, 3_000_000 * MICRO);
    fund(&divergent, Address::INSURANCE, 0);

    // Both replicas seal their seeded balances into the margin trie, exactly as
    // genesis does. Without this the divergence would be invisible until
    // something happened to write one of the rows.
    let honest_genesis = seal_genesis_margin(&honest);
    let divergent_genesis = seal_genesis_margin(&divergent);
    assert_ne!(
        honest_genesis, divergent_genesis,
        "genesis-seeded collateral is committed: the two replicas' roots \
         differ before a single block is applied"
    );

    let blocks: Vec<Vec<VerifiedTx>> = vec![
        vec![sign(
            &oracle_sk,
            0,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 50_000 * Q8,
                submitted_height: 1,
            },
        )],
        vec![
            sign(
                &sk_bob,
                0,
                TxBody::PlacePerpOrder {
                    market: MARKET,
                    side: Side::Sell,
                    price_q8: 50_000 * Q8,
                    qty: ONE_BTC,
                    tif: Tif::Gtc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([1; 32]),
                },
            ),
            sign(
                &sk_alice,
                0,
                TxBody::PlacePerpOrder {
                    market: MARKET,
                    side: Side::Buy,
                    price_q8: 50_000 * Q8,
                    qty: ONE_BTC,
                    tif: Tif::Ioc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([2; 32]),
                },
            ),
        ],
    ];

    let mut honest_root = H256::ZERO;
    let mut divergent_root = H256::ZERO;
    for (i, txs) in blocks.iter().enumerate() {
        let height = i as u64 + 1;
        honest_root = apply_block(&honest, height, txs);
        divergent_root = apply_block(&divergent, height, txs);
    }

    // The two chains genuinely disagree about solvency.
    assert_ne!(
        MarginAccounts::new(&honest).get(&alice).unwrap().collateral,
        MarginAccounts::new(&divergent)
            .get(&alice)
            .unwrap()
            .collateral,
        "the replicas must actually differ for this test to mean anything"
    );
    assert_ne!(
        MarginAccounts::new(&honest)
            .get(&Address::INSURANCE)
            .unwrap()
            .collateral,
        MarginAccounts::new(&divergent)
            .get(&Address::INSURANCE)
            .unwrap()
            .collateral,
        "including the insurance fund"
    );
    // ...and they agree about everything that IS committed.
    assert_eq!(
        Positions::new(&honest).get(&alice, &MARKET).unwrap(),
        Positions::new(&divergent).get(&alice, &MARKET).unwrap(),
        "positions are identical, so any root difference would come from margin"
    );

    assert_ne!(
        honest_root, divergent_root,
        "two replicas that disagree about every trader's collateral and about \
         the insurance fund must not be able to report the same state root: \
         detecting exactly this is what the root is for"
    );
}

#[test]
fn two_replicas_that_agree_about_margin_still_agree_about_the_root() {
    // The negative half. A fourth slot that made every root differ would
    // "pass" the test above and break consensus outright, so pin that
    // identical margin state still produces an identical root — including
    // after fills, fees and absorption have written the rows.
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"qa-root3-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (sk_alice, pk_alice) = keypair_from_seed(b"qa-root3-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = keypair_from_seed(b"qa-root3-bob");
    let bob = address_from_pubkey(&pk_bob);

    let (_dir_a, a) = build_replica(oracle_authority);
    let (_dir_b, b) = build_replica(oracle_authority);
    for db in [&a, &b] {
        fund(db, alice, 100_000 * MICRO);
        fund(db, bob, 100_000 * MICRO);
        fund(db, Address::INSURANCE, 1_000_000 * MICRO);
        seal_genesis_margin(db);
    }

    let oracle = sign(
        &oracle_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 50_000 * Q8,
            submitted_height: 1,
        },
    );
    let fill = vec![
        sign(
            &sk_bob,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Sell,
                price_q8: 50_000 * Q8,
                qty: ONE_BTC,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([1; 32]),
            },
        ),
        sign(
            &sk_alice,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Buy,
                price_q8: 50_000 * Q8,
                qty: ONE_BTC,
                tif: Tif::Ioc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([2; 32]),
            },
        ),
    ];

    assert_eq!(
        apply_block(&a, 1, std::slice::from_ref(&oracle)),
        apply_block(&b, 1, std::slice::from_ref(&oracle))
    );
    let root_a = apply_block(&a, 2, &fill);
    let root_b = apply_block(&b, 2, &fill);
    assert_eq!(
        root_a, root_b,
        "identical margin state must still produce an identical root"
    );
    // And the fill really did move margin, so the agreement is not vacuous.
    assert!(
        MarginAccounts::new(&a)
            .get(&Address::INSURANCE)
            .unwrap()
            .collateral
            > 1_000_000 * MICRO,
        "the taker fee reached the insurance fund, so margin rows were written"
    );
}

#[test]
fn positive_control_a_position_difference_does_change_the_state_root() {
    // The control QA wrote to prove the methodology, kept because it still
    // earns its place: it pins that the comparison these tests make is capable
    // of distinguishing two chains at all.
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"qa-root2-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (sk_alice, pk_alice) = keypair_from_seed(b"qa-root2-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, pk_bob) = keypair_from_seed(b"qa-root2-bob");
    let bob = address_from_pubkey(&pk_bob);

    let (_dir_a, a) = build_replica(oracle_authority);
    let (_dir_b, b) = build_replica(oracle_authority);
    for db in [&a, &b] {
        fund(db, alice, 100_000 * MICRO);
        fund(db, bob, 100_000 * MICRO);
    }

    let oracle = sign(
        &oracle_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 50_000 * Q8,
            submitted_height: 1,
        },
    );
    apply_block(&a, 1, std::slice::from_ref(&oracle));
    apply_block(&b, 1, std::slice::from_ref(&oracle));

    let maker = |qty: u128| {
        sign(
            &sk_bob,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Sell,
                price_q8: 50_000 * Q8,
                qty,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([1; 32]),
            },
        )
    };
    let taker = |qty: u128| {
        sign(
            &sk_alice,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Buy,
                price_q8: 50_000 * Q8,
                qty,
                tif: Tif::Ioc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([2; 32]),
            },
        )
    };
    let root_a = apply_block(&a, 2, &[maker(ONE_BTC), taker(ONE_BTC)]);
    let root_b = apply_block(&b, 2, &[maker(ONE_BTC / 2), taker(ONE_BTC / 2)]);
    assert_ne!(
        root_a, root_b,
        "a difference in position size must change the state root"
    );
}
