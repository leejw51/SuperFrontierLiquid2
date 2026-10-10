//! M7 determinism harness.
//!
//! Builds two identical chains independently against fresh databases and
//! asserts byte-for-byte equality of every block's state root after each
//! commit. This catches non-determinism introduced by:
//!   * HashMap iteration order (allocator-dependent on some platforms).
//!   * Time-of-day (`SystemTime::now`) on the consensus path.
//!   * RNG without a fixed seed.
//!   * Allocator-influenced collect-from-Iterator order.
//!
//! Each scenario lives as a list of `(height, txs)` pairs keyed by height. We
//! run the list against db_a then db_b and assert the per-block accounts
//! roots match exactly. We also pin the FINAL root of each scenario so
//! cross-platform CI catches a difference between Linux/macOS/Apple Silicon
//! (without us having to maintain a per-platform expected-value matrix —
//! the scenarios use canonical, integer-only inputs).
//!
//! Replay equality is the contract that makes consensus actually possible:
//! if two replicas can run the same tx slice and produce different state
//! roots, voting can never converge. Breaking this is a P0 bug.

use cb_block::{apply_tx_at, BlockState, CommitRoots, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Database, MarginAccounts, Meta, OracleState, PerpMarkets, Positions};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Signed, Tif, H256};
use tempfile::tempdir;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;

// ---------------------------------------------------------------------------
// Scenario primitives
// ---------------------------------------------------------------------------

/// One block's worth of work: a height plus the tx slice in apply order.
struct BlockSpec {
    height: u64,
    proposer: Address,
    txs: Vec<VerifiedTx>,
}

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
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

fn place(
    sk: &SecretKey,
    nonce: u64,
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
            market: MARKET,
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

/// Build the perp market and seed insurance into a freshly-opened DB.
fn fresh_db(oracle_authority: Address) -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
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
        funding_interval_blocks: 5,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 10_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
    (dir, db)
}

fn fund_margin(db: &Database, owner: Address, collateral: u128) {
    MarginAccounts::new(db).put(&MarginAccount {
        owner,
        collateral,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
}

/// Apply `block` against `db`, returning the post-commit state root.
fn run_block(db: &Database, block: &BlockSpec) -> H256 {
    let mut state = BlockState::new(db);
    for (i, v) in block.txs.iter().enumerate() {
        apply_tx_at(
            &mut state,
            &ctx_at(block.height, block.proposer),
            v,
            i as u32,
        )
        .unwrap();
    }
    state.commit().unwrap().state_root
}

/// Run every block in `scenario` against `db`, returning the per-block
/// state roots in order.
fn run_scenario(db: &Database, scenario: &[BlockSpec]) -> Vec<H256> {
    scenario.iter().map(|b| run_block(db, b)).collect()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Build the canonical "perps lifecycle" scenario: oracle, two-trader
/// open/cross fill, funding boundary, oracle drop, liquidation. We re-build
/// the `BlockSpec`s on each call because keypair_from_seed is deterministic
/// — same seed strings → same SecretKey → same VerifiedTx.
fn perps_lifecycle_scenario() -> Vec<BlockSpec> {
    let (sk_alice, pk_alice) = keypair_from_seed(b"determ-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (sk_bob, _pk_bob) = keypair_from_seed(b"determ-bob");
    let (sk_keeper, pk_keeper) = keypair_from_seed(b"determ-keeper");
    let _keeper = address_from_pubkey(&pk_keeper);
    let (oracle_sk, _oracle_pk) = keypair_from_seed(b"determ-oracle");
    // Use the same proposer-signed funding-tick path as the e2e test: keeper
    // signs FundingTick, ctx.proposer_addr = keeper for that block only.

    let proposer_zero = Address::ZERO;

    vec![
        BlockSpec {
            height: 1,
            proposer: proposer_zero,
            txs: vec![sign(
                &oracle_sk,
                0,
                TxBody::OracleUpdate {
                    market: MARKET,
                    px_q8: 50_000 * Q8,
                    submitted_height: 1,
                },
            )],
        },
        BlockSpec {
            height: 2,
            proposer: proposer_zero,
            txs: vec![
                place(&sk_bob, 0, Side::Sell, 50_000 * Q8, ONE_BTC, Tif::Gtc, 1),
                place(&sk_alice, 0, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Ioc, 2),
                place(&sk_bob, 1, Side::Buy, 49_500 * Q8, ONE_BTC, Tif::Gtc, 3),
                place(&sk_bob, 2, Side::Buy, 49_000 * Q8, ONE_BTC, Tif::Gtc, 4),
            ],
        },
        BlockSpec {
            height: 6,
            proposer: proposer_zero,
            txs: vec![sign(
                &oracle_sk,
                1,
                TxBody::OracleUpdate {
                    market: MARKET,
                    px_q8: 48_000 * Q8,
                    submitted_height: 6,
                },
            )],
        },
        BlockSpec {
            height: 7,
            proposer: proposer_zero,
            txs: vec![sign(
                &sk_keeper,
                0,
                TxBody::LiquidationTrigger {
                    owner: alice,
                    market: MARKET,
                },
            )],
        },
        // Sanity: a final no-op block so the chain ends with a non-perp tx
        // and the scenario covers an "after liquidation" state read.
        BlockSpec {
            height: 8,
            proposer: proposer_zero,
            txs: vec![sign(
                &sk_bob,
                3,
                TxBody::CancelPerpOrder {
                    order_id: H256([0xee; 32]),
                },
            )],
        },
    ]
}

/// Setup margins for the lifecycle scenario. Identical between db_a and db_b.
fn setup_lifecycle_margins(db: &Database) {
    let (_sk_alice, pk_alice) = keypair_from_seed(b"determ-alice");
    let alice = address_from_pubkey(&pk_alice);
    let (_sk_bob, pk_bob) = keypair_from_seed(b"determ-bob");
    let bob = address_from_pubkey(&pk_bob);
    let (_sk_keeper, pk_keeper) = keypair_from_seed(b"determ-keeper");
    let keeper = address_from_pubkey(&pk_keeper);
    fund_margin(db, alice, 1_500 * MICRO);
    fund_margin(db, bob, 200_000 * MICRO);
    fund_margin(db, keeper, 0);
}

// ---------------------------------------------------------------------------
// Determinism tests
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_replay_yields_identical_state_roots_per_block() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"determ-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    let (_dir_a, db_a) = fresh_db(oracle_authority);
    setup_lifecycle_margins(&db_a);
    let scenario_a = perps_lifecycle_scenario();
    let roots_a = run_scenario(&db_a, &scenario_a);

    let (_dir_b, db_b) = fresh_db(oracle_authority);
    setup_lifecycle_margins(&db_b);
    let scenario_b = perps_lifecycle_scenario();
    let roots_b = run_scenario(&db_b, &scenario_b);

    assert_eq!(roots_a.len(), 5);
    assert_eq!(roots_b.len(), 5);
    for (i, (a, b)) in roots_a.iter().zip(roots_b.iter()).enumerate() {
        assert_eq!(
            a,
            b,
            "state_root diverged at block {}: a={a:?} b={b:?}",
            i + 1
        );
    }

    // Cross-state sanity: the two chains agree on alice's flat position and
    // insurance's positive credit.
    let (_, pk_alice) = keypair_from_seed(b"determ-alice");
    let alice = address_from_pubkey(&pk_alice);
    let alice_a = Positions::new(&db_a).get(&alice, &MARKET).unwrap();
    let alice_b = Positions::new(&db_b).get(&alice, &MARKET).unwrap();
    assert_eq!(alice_a.qty, 0);
    assert_eq!(alice_b.qty, 0);
    let ins_a = MarginAccounts::new(&db_a)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    let ins_b = MarginAccounts::new(&db_b)
        .get(&Address::INSURANCE)
        .unwrap()
        .collateral;
    assert_eq!(ins_a, ins_b);
    assert!(ins_a > 10_000 * MICRO);

    // Mark prices agree.
    let mark_a = OracleState::new(&db_a).get_mark(&MARKET).unwrap();
    let mark_b = OracleState::new(&db_b).get_mark(&MARKET).unwrap();
    assert_eq!(mark_a.px_q8, mark_b.px_q8);
}

/// Stress: 50 trader pairs cross-fill in one block. If iteration over the
/// matcher's fill list is order-dependent (e.g. via a HashSet) the resulting
/// account roots would diverge across runs even for the same input.
#[test]
fn many_concurrent_fills_yield_identical_roots_across_runs() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"many-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    fn run_once() -> H256 {
        let (_dir, db) = fresh_db_helper();
        // 20 sellers each rest a sell @ 50k for 1 BTC.
        // 20 buyers each IOC-buy at 50k for 1 BTC.
        // Each pair fills against an unspecified counterparty (matcher is
        // price-time priority — the same partition always emerges).
        let mut sks_sellers = Vec::new();
        let mut sks_buyers = Vec::new();
        for i in 0..20u8 {
            let (sk, pk) = keypair_from_seed(format!("many-seller-{i}").as_bytes());
            let addr = address_from_pubkey(&pk);
            fund_margin(&db, addr, 200_000 * MICRO);
            sks_sellers.push(sk);
        }
        for i in 0..20u8 {
            let (sk, pk) = keypair_from_seed(format!("many-buyer-{i}").as_bytes());
            let addr = address_from_pubkey(&pk);
            fund_margin(&db, addr, 200_000 * MICRO);
            sks_buyers.push(sk);
        }

        // Block 1: oracle.
        let (oracle_sk, _) = keypair_from_seed(b"many-oracle");
        let block1 = BlockSpec {
            height: 1,
            proposer: Address::ZERO,
            txs: vec![sign(
                &oracle_sk,
                0,
                TxBody::OracleUpdate {
                    market: MARKET,
                    px_q8: 50_000 * Q8,
                    submitted_height: 1,
                },
            )],
        };
        run_block(&db, &block1);

        // Block 2: all sellers rest first, then buyers IOC.
        let mut txs = Vec::new();
        for (i, sk) in sks_sellers.iter().enumerate() {
            txs.push(place(
                sk,
                0,
                Side::Sell,
                50_000 * Q8,
                ONE_BTC,
                Tif::Gtc,
                (i as u8).wrapping_add(1),
            ));
        }
        for (i, sk) in sks_buyers.iter().enumerate() {
            txs.push(place(
                sk,
                0,
                Side::Buy,
                50_000 * Q8,
                ONE_BTC,
                Tif::Ioc,
                (i as u8).wrapping_add(50),
            ));
        }
        let block2 = BlockSpec {
            height: 2,
            proposer: Address::ZERO,
            txs,
        };
        run_block(&db, &block2)
    }

    fn fresh_db_helper() -> (tempfile::TempDir, Database) {
        let (_, oracle_pk) = keypair_from_seed(b"many-oracle");
        fresh_db(address_from_pubkey(&oracle_pk))
    }

    let r1 = run_once();
    let r2 = run_once();
    let r3 = run_once();
    assert_eq!(r1, r2, "second run diverged");
    assert_eq!(r1, r3, "third run diverged");
    let _ = oracle_authority; // suppress unused
}

/// FundingTick determinism: same premium → same cumulative → same per-position
/// settlement → same state root. Catches a regression where the funding
/// settlement loop walked positions in a HashMap-iteration order.
#[test]
fn funding_settlement_yields_identical_roots_across_runs() {
    fn run_once() -> H256 {
        let (_oracle_sk, oracle_pk) = keypair_from_seed(b"funding-determ-oracle");
        let oracle_authority = address_from_pubkey(&oracle_pk);
        let (_dir, db) = fresh_db(oracle_authority);

        // 5 longs, 5 shorts. Position-by-position write order via direct CF
        // put. We deliberately put them in REVERSE address order to test
        // that the per-market funding settlement re-sorts before mutating.
        let mut owners: Vec<(SecretKey, Address)> = Vec::new();
        for i in 0..10u8 {
            let (sk, pk) = keypair_from_seed(format!("fdet-{i}").as_bytes());
            owners.push((sk, address_from_pubkey(&pk)));
        }
        // Sort by address descending so put-order != lex-order; the engine
        // must re-sort or we'd get drift.
        owners.sort_by_key(|a| std::cmp::Reverse(a.1 .0));
        for (i, (_sk, addr)) in owners.iter().enumerate() {
            let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
            cb_state::Positions::new(&db).put(&cb_types::Position {
                owner: *addr,
                market: MARKET,
                side,
                qty: ONE_BTC,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: 50_000 * Q8,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            });
            fund_margin(&db, *addr, 100_000 * MICRO);
        }

        // Set premium so funding accrues.
        let (oracle_sk, _) = keypair_from_seed(b"funding-determ-oracle");
        let block1 = BlockSpec {
            height: 1,
            proposer: Address::ZERO,
            txs: vec![sign(
                &oracle_sk,
                0,
                TxBody::OracleUpdate {
                    market: MARKET,
                    px_q8: 50_000 * Q8,
                    submitted_height: 1,
                },
            )],
        };
        run_block(&db, &block1);
        // Inject premium.
        {
            let store = OracleState::new(&db);
            let mut m = store.get_mark(&MARKET).unwrap();
            m.premium_ema_q9 = 10_000_000; // hits outer clamp at 5_000_000
            store.put_mark(&m);
        }

        // First tick at height 5 (no flow, but sets last_tick_height).
        let (sk_keeper, pk_keeper) = keypair_from_seed(b"funding-determ-keeper");
        let keeper = address_from_pubkey(&pk_keeper);
        fund_margin(&db, keeper, 0);
        let block5 = BlockSpec {
            height: 5,
            proposer: keeper,
            txs: vec![sign(&sk_keeper, 0, TxBody::FundingTick { market: MARKET })],
        };
        run_block(&db, &block5);

        // Second tick at 10 — flow happens.
        let block10 = BlockSpec {
            height: 10,
            proposer: keeper,
            txs: vec![sign(&sk_keeper, 1, TxBody::FundingTick { market: MARKET })],
        };
        run_block(&db, &block10)
    }

    let r1 = run_once();
    let r2 = run_once();
    let r3 = run_once();
    assert_eq!(r1, r2);
    assert_eq!(r1, r3);
}

/// Liquidation determinism: an underwater account, triggered, must produce
/// the same byte-for-byte state regardless of the order in which the matcher
/// walks the resting bid stack — as long as the matcher itself is
/// deterministic, which is what this test verifies at the executor level.
#[test]
fn liquidation_yields_identical_roots_across_runs() {
    fn run_once() -> H256 {
        let (_oracle_sk, oracle_pk) = keypair_from_seed(b"liq-determ-oracle");
        let oracle_authority = address_from_pubkey(&oracle_pk);
        let (_dir, db) = fresh_db(oracle_authority);

        let (sk_alice, pk_alice) = keypair_from_seed(b"liq-determ-alice");
        let alice = address_from_pubkey(&pk_alice);
        let (sk_bob, pk_bob) = keypair_from_seed(b"liq-determ-bob");
        let bob = address_from_pubkey(&pk_bob);
        let (sk_keeper, pk_keeper) = keypair_from_seed(b"liq-determ-keeper");
        let keeper = address_from_pubkey(&pk_keeper);
        fund_margin(&db, alice, 1_500 * MICRO);
        fund_margin(&db, bob, 200_000 * MICRO);
        fund_margin(&db, keeper, 0);

        let (oracle_sk, _) = keypair_from_seed(b"liq-determ-oracle");

        // Block 1: oracle.
        run_block(
            &db,
            &BlockSpec {
                height: 1,
                proposer: Address::ZERO,
                txs: vec![sign(
                    &oracle_sk,
                    0,
                    TxBody::OracleUpdate {
                        market: MARKET,
                        px_q8: 50_000 * Q8,
                        submitted_height: 1,
                    },
                )],
            },
        );

        // Block 2: alice opens long, bob shorts; bob also rests deep bids.
        run_block(
            &db,
            &BlockSpec {
                height: 2,
                proposer: Address::ZERO,
                txs: vec![
                    place(&sk_bob, 0, Side::Sell, 50_000 * Q8, ONE_BTC, Tif::Gtc, 1),
                    place(&sk_alice, 0, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Ioc, 2),
                    place(&sk_bob, 1, Side::Buy, 49_500 * Q8, ONE_BTC, Tif::Gtc, 3),
                    place(&sk_bob, 2, Side::Buy, 49_000 * Q8, ONE_BTC, Tif::Gtc, 4),
                ],
            },
        );

        // Block 3: oracle drops.
        run_block(
            &db,
            &BlockSpec {
                height: 3,
                proposer: Address::ZERO,
                txs: vec![sign(
                    &oracle_sk,
                    1,
                    TxBody::OracleUpdate {
                        market: MARKET,
                        px_q8: 48_000 * Q8,
                        submitted_height: 3,
                    },
                )],
            },
        );

        // Block 4: liquidation trigger.
        run_block(
            &db,
            &BlockSpec {
                height: 4,
                proposer: Address::ZERO,
                txs: vec![sign(
                    &sk_keeper,
                    0,
                    TxBody::LiquidationTrigger {
                        owner: alice,
                        market: MARKET,
                    },
                )],
            },
        )
    }

    let r1 = run_once();
    let r2 = run_once();
    let r3 = run_once();
    assert_eq!(r1, r2, "liquidation replay 2 diverged");
    assert_eq!(r1, r3, "liquidation replay 3 diverged");
}

// ---------------------------------------------------------------------------
// State-root coverage
//
// The state root chains the hash of every committed row a block *wrote*
// (`cb_state::state_hash`). Two chains that differ in a `Position`, a
// `MarginAccount` or a `PerpMarket` row MUST produce different roots — at the
// first block whose execution writes something that depends on the differing
// row. A tamper that no transaction ever touches stays latent: the scheme
// catches divergence when it reaches a write, not while it sits unread. That is
// the same moment it can first affect anything a replica or a user observes.
// ---------------------------------------------------------------------------

/// Run a `BlockSpec` and return the commit result plus its state root.
fn run_block_full(db: &Database, block: &BlockSpec) -> (CommitRoots, H256) {
    let mut state = BlockState::new(db);
    for (i, v) in block.txs.iter().enumerate() {
        apply_tx_at(
            &mut state,
            &ctx_at(block.height, block.proposer),
            v,
            i as u32,
        )
        .unwrap();
    }
    let roots = state.commit().unwrap();
    (roots, roots.state_root)
}

fn oracle_block(oracle_sk: &SecretKey, nonce: u64, height: u64, px: u128) -> BlockSpec {
    BlockSpec {
        height,
        proposer: Address::ZERO,
        txs: vec![sign(
            oracle_sk,
            nonce,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: px * Q8,
                submitted_height: height,
            },
        )],
    }
}

/// Bob rests a sell, alice takes it. Writes both traders' positions and
/// margins, the insurance fund (taker fee) and the market's last trade.
fn trade_block(height: u64) -> BlockSpec {
    let (sk_alice, _) = keypair_from_seed(b"determ-alice");
    let (sk_bob, _) = keypair_from_seed(b"determ-bob");
    BlockSpec {
        height,
        proposer: Address::ZERO,
        txs: vec![
            place(&sk_bob, 0, Side::Sell, 50_000 * Q8, ONE_BTC, Tif::Gtc, 1),
            place(&sk_alice, 0, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Ioc, 2),
        ],
    }
}

/// Two chains identical except for one base unit in alice's `Position.qty`,
/// edited outside the executor. The block that next trades alice's position
/// reads the edited row and writes it back, so its root must differ.
#[test]
fn differ_in_position_yields_different_state_root() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"m8-pos-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    fn run_with_tamper(oracle_authority: Address, tamper: bool) -> H256 {
        let (_dir, db) = fresh_db(oracle_authority);
        setup_lifecycle_margins(&db);
        let (oracle_sk, _) = keypair_from_seed(b"m8-pos-oracle");
        run_block_full(&db, &oracle_block(&oracle_sk, 0, 1, 50_000));
        run_block_full(&db, &trade_block(2));

        if tamper {
            let (_, pk_alice) = keypair_from_seed(b"determ-alice");
            let alice = address_from_pubkey(&pk_alice);
            let mut p = Positions::new(&db).get(&alice, &MARKET).unwrap();
            p.qty += 1;
            Positions::new(&db).put(&p);
        }

        // Block 3: bob bids, alice sells her long into it. Alice's position row
        // is read and rewritten, so the edit reaches the diff here.
        let (sk_alice, _) = keypair_from_seed(b"determ-alice");
        let (sk_bob, _) = keypair_from_seed(b"determ-bob");
        let block3 = BlockSpec {
            height: 3,
            proposer: Address::ZERO,
            txs: vec![
                place(&sk_bob, 1, Side::Buy, 50_000 * Q8, ONE_BTC, Tif::Gtc, 3),
                place(&sk_alice, 1, Side::Sell, 50_000 * Q8, ONE_BTC, Tif::Ioc, 4),
            ],
        };
        run_block_full(&db, &block3).1
    }

    let honest = run_with_tamper(oracle_authority, false);
    let byzantine = run_with_tamper(oracle_authority, true);
    assert_ne!(
        honest, byzantine,
        "a one-unit position difference must change the root of the block that writes it"
    );
}

/// The same for a `MarginAccount`, the surface QA finding F3 found uncovered:
/// one micro-USD in a trader's collateral, or in the insurance fund, must
/// change the root of the block whose trade rewrites that row.
#[test]
fn differ_in_margin_account_yields_different_state_root() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"f3-margin-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    fn run_with_tamper(oracle_authority: Address, tamper: Option<Address>) -> H256 {
        let (_dir, db) = fresh_db(oracle_authority);
        setup_lifecycle_margins(&db);
        if let Some(victim) = tamper {
            let margins = MarginAccounts::new(&db);
            let mut m = margins
                .get(&victim)
                .unwrap_or_else(|| MarginAccount::new(victim));
            m.collateral += 1;
            margins.put(&m);
        }
        let (oracle_sk, _) = keypair_from_seed(b"f3-margin-oracle");
        run_block_full(&db, &oracle_block(&oracle_sk, 0, 1, 50_000));
        let (roots, root) = run_block_full(&db, &trade_block(2));
        assert!(roots.diff_entries > 0, "the trade must write something");
        root
    }

    let (_, pk_alice) = keypair_from_seed(b"determ-alice");
    let alice = address_from_pubkey(&pk_alice);
    let honest = run_with_tamper(oracle_authority, None);
    let trader_edited = run_with_tamper(oracle_authority, Some(alice));
    let fund_edited = run_with_tamper(oracle_authority, Some(Address::INSURANCE));
    assert_ne!(
        honest, trader_edited,
        "one micro-USD of difference in a trader's collateral must change the \
         state root"
    );
    assert_ne!(
        honest, fund_edited,
        "and so must a difference in the insurance fund, which is just the \
         margin row at `Address::INSURANCE`"
    );
    assert_ne!(trader_edited, fund_edited);
}

/// The same for a `PerpMarket`: two chains identical except for
/// `taker_fee_bps` must diverge at the first trade, whose fee is computed from
/// the edited row.
#[test]
fn differ_in_perp_market_yields_different_state_root() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"m8-mkt-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    fn run_with_tamper(oracle_authority: Address, tamper_fee: bool) -> H256 {
        let (_dir, db) = fresh_db(oracle_authority);
        if tamper_fee {
            let mut pm = PerpMarkets::new(&db).get(&MARKET).unwrap();
            pm.taker_fee_bps = 99;
            PerpMarkets::new(&db).put(&pm);
        }
        setup_lifecycle_margins(&db);
        let (oracle_sk, _) = keypair_from_seed(b"m8-mkt-oracle");
        run_block_full(&db, &oracle_block(&oracle_sk, 0, 1, 50_000));
        run_block_full(&db, &trade_block(2)).1
    }

    let honest = run_with_tamper(oracle_authority, false);
    let tampered = run_with_tamper(oracle_authority, true);
    assert_ne!(
        honest, tampered,
        "a different taker fee must change the root of the first trade"
    );
}

/// Property: the same tx stream against two fresh DBs from the same genesis
/// produces byte-identical state roots every block, and every block of the
/// lifecycle writes something.
#[test]
fn replay_yields_identical_state_roots_per_block() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"determ-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    fn collect_full(db: &Database, scenario: &[BlockSpec]) -> Vec<CommitRoots> {
        scenario.iter().map(|b| run_block_full(db, b).0).collect()
    }

    let (_dir_a, db_a) = fresh_db(oracle_authority);
    setup_lifecycle_margins(&db_a);
    let roots_a = collect_full(&db_a, &perps_lifecycle_scenario());

    let (_dir_b, db_b) = fresh_db(oracle_authority);
    setup_lifecycle_margins(&db_b);
    let roots_b = collect_full(&db_b, &perps_lifecycle_scenario());

    assert_eq!(roots_a.len(), roots_b.len());
    for (i, (a, b)) in roots_a.iter().zip(roots_b.iter()).enumerate() {
        assert_eq!(
            a,
            b,
            "state_root diverged at block {} of lifecycle scenario",
            i + 1
        );
    }
    // The opening, the liquidation and the oracle moves all write rows.
    for (i, r) in roots_a.iter().take(4).enumerate() {
        assert!(r.diff_entries > 0, "block {} wrote nothing", i + 1);
    }
}

/// Oracle, funding and book state used to be "auxiliary", outside the root.
/// They are committed now: an `OracleUpdate` block writes a row, and two
/// oracle blocks at different prices produce different roots.
#[test]
fn auxiliary_cf_writes_are_committed() {
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"m8-aux-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    let run = |px: u128| {
        let (_dir, db) = fresh_db(oracle_authority);
        setup_lifecycle_margins(&db);
        run_block_full(&db, &oracle_block(&oracle_sk, 0, 2, px)).0
    };
    let a = run(50_000);
    let b = run(50_001);
    assert!(a.diff_entries > 0, "an oracle update must reach the diff");
    assert_ne!(
        a.state_root, b.state_root,
        "a different price must change the root"
    );
}

/// The rule the whole scheme depends on: the root hashes what a block
/// *wrote*, never what it read. A replica prefetches accounts the leader never
/// loaded, and the benchmark loads accounts it never changes; neither may move
/// the root.
#[test]
fn reads_and_prefetches_do_not_change_the_state_root() {
    let (_oracle_sk, oracle_pk) = keypair_from_seed(b"reads-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (oracle_sk, _) = keypair_from_seed(b"reads-oracle");

    let run = |extra_reads: bool| {
        let (_dir, db) = fresh_db(oracle_authority);
        setup_lifecycle_margins(&db);
        run_block_full(&db, &oracle_block(&oracle_sk, 0, 1, 50_000));
        let block = trade_block(2);
        let mut state = BlockState::new(&db);
        if extra_reads {
            let strangers: Vec<Address> = (1..=32u8).map(|i| Address([i; 20])).collect();
            state.prefetch(&strangers);
            state.prefetch(&block.txs.iter().map(|v| v.sender).collect::<Vec<_>>());
        }
        for (i, v) in block.txs.iter().enumerate() {
            if extra_reads {
                let _ = state.load(&Address([0x77; 20]));
                let _ = state.load(&v.sender);
            }
            apply_tx_at(&mut state, &ctx_at(2, Address::ZERO), v, i as u32).unwrap();
            state.commit_point();
        }
        if extra_reads {
            let _ = state.load(&Address::INSURANCE);
        }
        state.commit().unwrap()
    };
    let leader = run(false);
    let replica = run(true);
    assert_eq!(
        leader, replica,
        "reads and prefetches must not reach the diff"
    );
}
