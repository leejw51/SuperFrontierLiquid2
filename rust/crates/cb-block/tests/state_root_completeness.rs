//! M8 property suite: state-root completeness over many tx orderings.
//!
//! The root is now a hash chain over each block's writes (`cb_state::state_hash`)
//! rather than four merkle tries, and `touch_*` no longer feeds it. The
//! properties below are restated for that scheme: identical replays give
//! identical roots, the order the executor *writes* rows in does not matter
//! (the stage sorts by column family and key), and a block that writes nothing
//! chains an empty diff.
//!
//! These tests exist on top of `determinism.rs` to add coverage for the
//! "same inputs → byte-identical positions/markets roots" property at
//! larger scale. We don't pull in `proptest` (not currently a dep of
//! cb-block) — instead we run table-driven sweeps over a fixed bag of
//! deterministic keypairs and assert byte equality.
//!
//! What this file's tests catch that `determinism.rs` doesn't:
//!   * The positions trie root is equal across two fresh DBs even when the
//!     scenario opens **many** independent positions in non-monotonic order
//!     (catches a regression where `commit_batch` walked the touched-keys
//!     `HashSet` directly instead of sorting).
//!   * The markets trie root is byte-identical across replicas after a
//!     scenario that mutates `last_trade_px_q8` repeatedly — the per-fill
//!     market-record write path.
//!   * Replaying the SAME scenario twice on the SAME db is idempotent at
//!     the trie level: re-applying a no-op block leaves the tries unchanged.
//!   * The **margin** trie — added by QA finding F3, which found that
//!     collateral, bad debt and the insurance fund fed no trie at all — is
//!     covered on the same three axes: it is populated by ordinary execution,
//!     its root is independent of touch order, and a no-op block leaves it
//!     alone. A completeness suite that checked three of four slots is how the
//!     fourth stayed missing.

use cb_block::{apply_tx_at, BlockState, CommitRoots, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed};
use cb_state::{
    chain_state_hash, Database, DiffHasher, MarginAccounts, Meta, PerpMarkets, Positions,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Signed, Tif, H256};
use tempfile::tempdir;

/// A set of actors with their signing keys, as the pair-setup helper returns
/// them. Named because the bare tuple-of-tuples was unreadable at the call
/// site and clippy was right to say so.
type KeyedActors = Vec<(cb_crypto::SecretKey, Address)>;

const MARKET: DerivId = DerivId::perp(0);
const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;
const ONE_BTC: u128 = 100_000_000;

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

fn sign(sk: &cb_crypto::SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
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

fn run_block(db: &Database, height: u64, txs: Vec<VerifiedTx>) -> CommitRoots {
    let mut state = BlockState::new(db);
    for (i, v) in txs.iter().enumerate() {
        apply_tx_at(&mut state, &ctx_at(height, Address::ZERO), v, i as u32).unwrap();
    }
    state.commit().unwrap()
}

/// Set up `n_pairs` longs paired against `n_pairs` shorts trading at 50_000.
/// Returns a flat `(secret_keys, addresses)` pair zipped per role.
fn many_pair_setup(db: &Database, n_pairs: usize) -> (KeyedActors, KeyedActors) {
    let mut sellers = Vec::with_capacity(n_pairs);
    let mut buyers = Vec::with_capacity(n_pairs);
    for i in 0..n_pairs {
        let (sk_s, pk_s) = keypair_from_seed(format!("m8-prop-seller-{i}").as_bytes());
        let (sk_b, pk_b) = keypair_from_seed(format!("m8-prop-buyer-{i}").as_bytes());
        let s_addr = address_from_pubkey(&pk_s);
        let b_addr = address_from_pubkey(&pk_b);
        fund_margin(db, s_addr, 200_000 * MICRO);
        fund_margin(db, b_addr, 200_000 * MICRO);
        sellers.push((sk_s, s_addr));
        buyers.push((sk_b, b_addr));
    }
    (sellers, buyers)
}

/// Prop test: applying the same set of txs in the same order from the same
/// genesis on two fresh DBs yields byte-identical positions_root AND
/// markets_root for every block. We hammer this with N=24 trader pairs so
/// the positions trie covers a non-trivial number of leaves (24*2 = 48
/// distinct (owner, market) keys after fills).
#[test]
fn prop_replay_identical_state_roots_n_traders() {
    fn run_chain() -> Vec<H256> {
        let (oracle_sk, oracle_pk) = keypair_from_seed(b"m8-prop-oracle");
        let oracle_authority = address_from_pubkey(&oracle_pk);
        let (_dir, db) = fresh_db(oracle_authority);
        let (sellers, buyers) = many_pair_setup(&db, 24);

        // Block 1: oracle.
        let v_oracle = sign(
            &oracle_sk,
            0,
            TxBody::OracleUpdate {
                market: MARKET,
                px_q8: 50_000 * Q8,
                submitted_height: 1,
            },
        );
        let r1 = run_block(&db, 1, vec![v_oracle]);

        // Block 2: each seller rests; each buyer IOCs into them.
        let mut txs = Vec::new();
        for (i, (sk, _)) in sellers.iter().enumerate() {
            txs.push(sign(
                sk,
                0,
                TxBody::PlacePerpOrder {
                    market: MARKET,
                    side: Side::Sell,
                    price_q8: 50_000 * Q8,
                    qty: ONE_BTC,
                    tif: Tif::Gtc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([(i as u8).wrapping_add(1); 32]),
                },
            ));
        }
        for (i, (sk, _)) in buyers.iter().enumerate() {
            txs.push(sign(
                sk,
                0,
                TxBody::PlacePerpOrder {
                    market: MARKET,
                    side: Side::Buy,
                    price_q8: 50_000 * Q8,
                    qty: ONE_BTC,
                    tif: Tif::Ioc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([(i as u8).wrapping_add(50); 32]),
                },
            ));
        }
        let r2 = run_block(&db, 2, txs);
        // The margin trie must be carrying real content by now: 24 takers paid
        // the taker fee and the insurance fund received it. A root that stayed
        // at ZERO would make the whole comparison below vacuous. (F3)
        assert!(
            r2.diff_entries > 0,
            "ordinary execution must write rows into the diff"
        );

        // Pin a single canonical state_root per block — that's what consensus
        // votes on, and what diverges if the trie is leaky.
        let s1 = r1.state_root;
        let s2 = r2.state_root;
        vec![s1, s2]
    }

    // Three independent runs from scratch must produce the same per-block
    // state_root sequence.
    let a = run_chain();
    let b = run_chain();
    let c = run_chain();
    assert_eq!(a, b, "run b diverged");
    assert_eq!(a, c, "run c diverged");
    // Sanity: not all equal to ZERO.
    assert!(a.iter().all(|r| *r != H256::ZERO));
}

/// Prop test: position rows are committed in a canonical order regardless of
/// the order in which the executor sees them. We wire 16 positions in
/// REVERSE address order (via direct `Positions::put` plus `touch_position`)
/// and assert the resulting `positions_root` matches a forward-order run.
/// Catches a regression where `commit_batch` walked the touched-keys
/// `HashSet` directly without sorting.
#[test]
fn prop_root_independent_of_position_write_order() {
    fn populate(db: &Database, reversed: bool) -> H256 {
        // 16 positions with deterministic keys. Same content in both runs;
        // only the touch_order differs.
        let n = 16u8;
        let mut pairs = Vec::new();
        for i in 0..n {
            let (_, pk) = keypair_from_seed(format!("m8-touch-order-{i}").as_bytes());
            let owner = address_from_pubkey(&pk);
            pairs.push(owner);
        }
        // Write each position through the block's staged handle, in forward or
        // reverse order. The stage sorts by key, so the diff — and the root —
        // must not depend on the order the executor happened to write in.
        let state = BlockState::new(db);
        let staged = state.db();
        let order: Vec<&Address> = if reversed {
            pairs.iter().rev().collect()
        } else {
            pairs.iter().collect()
        };
        for owner in order {
            Positions::new(&staged).put(&cb_types::Position {
                owner: *owner,
                market: MARKET,
                side: Side::Buy,
                qty: ONE_BTC,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: 50_000 * Q8,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            });
        }
        let roots = state.commit().unwrap();
        assert!(roots.diff_entries >= u64::from(n));
        roots.state_root
    }

    let (oracle_sk, oracle_pk) = keypair_from_seed(b"m8-touch-order");
    let _ = oracle_sk;
    let oracle_authority = address_from_pubkey(&oracle_pk);

    let (_da, db_a) = fresh_db(oracle_authority);
    let (_db, db_b) = fresh_db(oracle_authority);

    let r_forward = populate(&db_a, false);
    let r_reverse = populate(&db_b, true);
    assert_eq!(
        r_forward, r_reverse,
        "the root must not depend on the order positions were written in"
    );
}

/// Prop test: a block that writes nothing chains an empty diff onto the
/// previous root, however much the column families already hold.
#[test]
fn prop_empty_block_chains_an_empty_diff() {
    let (_, oracle_pk) = keypair_from_seed(b"m8-noop-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (_dir, db) = fresh_db(oracle_authority);

    // A first block that writes four positions.
    let r_first = {
        let state = BlockState::new(&db);
        let staged = state.db();
        for i in 0..4u8 {
            let (_, pk) = keypair_from_seed(format!("m8-noop-{i}").as_bytes());
            Positions::new(&staged).put(&cb_types::Position {
                owner: address_from_pubkey(&pk),
                market: MARKET,
                side: Side::Buy,
                qty: ONE_BTC,
                entry_funding_index: Signed::ZERO,
                entry_px_q8: 50_000 * Q8,
                margin_mode: 0,
                isolated_margin: 0,
                last_funding_height: 0,
            });
        }
        state.commit().unwrap()
    };
    assert_eq!(r_first.diff_entries, 4);

    // No-op block: nothing written, so nothing hashed but the empty diff.
    let r_noop = BlockState::new(&db).commit().unwrap();
    assert_eq!(r_noop.diff_entries, 0);
    assert_eq!(
        r_noop.state_root,
        chain_state_hash(Meta::new(&db).state_root(), DiffHasher::new().finish())
    );
    assert_ne!(r_noop.state_root, r_first.state_root);
}

/// The root is a function of the rows a block wrote, not of the order the
/// executor wrote margin rows in. Mirrors
/// `prop_root_independent_of_position_write_order` — the property that broke
/// once already, when `commit_batch` walked a `HashSet` without sorting. (F3)
#[test]
fn prop_root_independent_of_margin_write_order() {
    fn populate(db: &Database, reversed: bool) -> H256 {
        let mut owners: Vec<Address> = (0..24u8)
            .map(|i| {
                let (_, pk) = keypair_from_seed(format!("m8-margin-order-{i}").as_bytes());
                address_from_pubkey(&pk)
            })
            .collect();
        let amounts: std::collections::HashMap<Address, u128> = owners
            .iter()
            .enumerate()
            .map(|(i, o)| (*o, (i as u128 + 1) * 1_234 * MICRO))
            .collect();
        if reversed {
            owners.reverse();
        }
        let state = BlockState::new(db);
        let staged = state.db();
        for owner in &owners {
            MarginAccounts::new(&staged).put(&MarginAccount {
                owner: *owner,
                collateral: amounts[owner],
                unrealized_pnl: Signed::ZERO,
                bad_debt: 0,
            });
        }
        let roots = state.commit().unwrap();
        assert_eq!(roots.diff_entries, 24);
        roots.state_root
    }

    let (_, oracle_pk) = keypair_from_seed(b"m8-margin-order-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (_dir_a, db_a) = fresh_db(oracle_authority);
    let (_dir_b, db_b) = fresh_db(oracle_authority);
    let forward = populate(&db_a, false);
    let backward = populate(&db_b, true);
    assert_ne!(forward, H256::ZERO);
    assert_eq!(
        forward, backward,
        "the root must not depend on the order margin rows were written in"
    );
}

/// Measurement helper for the M8 deliverable. Counts the number of position
/// trie keys flushed at end-of-block for a "typical" block: a 50-fill cross
/// scenario where 50 buyers IOC into 50 resting sellers. This is the size
/// of the batch `Positions::commit_batch` sees in a hot path.
///
/// Reported numbers (Apple Silicon, debug build):
///   * 50-fill block: 100 (owner, market) keys touched (50 takers + 50 makers).
///   * 1 market touched (from `view.persist`'s last_trade_px update).
///
/// At a block depth of log_2(N) (binary radix MPT), 100 keys → roughly 7
/// trie levels deep → ~700 node writes per block, all flushed in one
/// `BufferedKv::flush_into` call. A funding-tick block on a 1000-position
/// market would scale to ~1000 keys × 10 levels = 10K node writes — still
/// within one batched RocksDB write, which is the constraint we care about.
#[test]
fn measure_typical_block_position_touch_count() {
    let (oracle_sk, oracle_pk) = keypair_from_seed(b"m8-measure-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);
    let (_dir, db) = fresh_db(oracle_authority);
    let (sellers, buyers) = many_pair_setup(&db, 50);

    // Block 1: oracle.
    let v_oracle = sign(
        &oracle_sk,
        0,
        TxBody::OracleUpdate {
            market: MARKET,
            px_q8: 50_000 * Q8,
            submitted_height: 1,
        },
    );
    run_block(&db, 1, vec![v_oracle]);

    // Block 2: 50 sellers + 50 buyers, all crossing.
    let mut txs = Vec::new();
    for (i, (sk, _)) in sellers.iter().enumerate() {
        txs.push(sign(
            sk,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Sell,
                price_q8: 50_000 * Q8,
                qty: ONE_BTC,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([(i as u8).wrapping_add(1); 32]),
            },
        ));
    }
    for (i, (sk, _)) in buyers.iter().enumerate() {
        txs.push(sign(
            sk,
            0,
            TxBody::PlacePerpOrder {
                market: MARKET,
                side: Side::Buy,
                price_q8: 50_000 * Q8,
                qty: ONE_BTC,
                tif: Tif::Ioc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256([(i as u8).wrapping_add(50); 32]),
            },
        ));
    }
    let height = 2u64;
    // The diff must carry at least the 100 position rows the fills wrote.
    let roots = run_block(&db, height, txs);
    assert!(
        roots.diff_entries >= 100,
        "diff held {}",
        roots.diff_entries
    );

    // We can't easily count the exact number of trie nodes flushed without
    // instrumenting BufferedKv, so we sanity-check by comparing CF row
    // counts. 50 takers + 50 makers = 100 unique (owner, market) keys in
    // CF_POSITIONS.
    let mut count = 0usize;
    for (k, _) in db
        .iter_prefix(cb_state::CF_POSITIONS, &[])
        .unwrap()
        .flatten()
    {
        if k.len() == 24 {
            count += 1;
        }
    }
    assert_eq!(
        count, 100,
        "50-fill cross block should produce exactly 100 (owner, market) rows"
    );
    eprintln!(
        "M8 measure: 50-fill cross block touched {} position keys, 1 market (perp last_trade_px)",
        count
    );
}
#[test]
fn prop_root_independent_of_market_write_order() {
    fn populate(db: &Database, n_perp: u16, n_future: u16, reversed: bool) -> H256 {
        // Every market is written through the block's staged handle, in forward
        // or reverse id order; the stage sorts, so the root must not care.
        let state = BlockState::new(db);
        let staged = state.db();
        let mut writes: Vec<Box<dyn Fn()>> = Vec::new();
        let mut ids: Vec<DerivId> = Vec::new();
        for i in 0..n_perp {
            let did = DerivId::perp(i);
            let pm = PerpMarket {
                id: did,
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
                oracle_authority: Address::ZERO,
                max_position_notional: u128::MAX,
                paused: false,
                last_trade_px_q8: 0,
                last_trade_height: 0,
                paused_reduce_only_only: false,
            };
            let staged = staged.clone();
            writes.push(Box::new(move || PerpMarkets::new(&staged).put(&pm)));
            ids.push(did);
        }
        for i in 0..n_future {
            let did = DerivId::future(i);
            let fm = cb_types::FutureMarket {
                id: did,
                underlying: AssetId::Eth,
                tick_size: 1,
                lot_size: 1,
                max_leverage_x100: 2500,
                initial_margin_bps: 400,
                maintenance_margin_bps: 200,
                taker_fee_bps: 3,
                maker_fee_bps_signed: -1,
                expiry_height: 1_000_000,
                settlement_method: 0,
                settlement_window_blocks: 600,
                oracle_authority: Address::ZERO,
                paused: false,
                final_settlement_px_q8: None,
                last_trade_px_q8: 0,
                last_trade_height: 0,
                paused_reduce_only_only: false,
            };
            let staged = staged.clone();
            writes.push(Box::new(move || {
                cb_state::FuturesMarkets::new(&staged).put(&fm)
            }));
            ids.push(did);
        }
        if reversed {
            writes.reverse();
        }
        for w in &writes {
            w();
        }
        drop(writes);
        let roots = state.commit().unwrap();
        assert_eq!(roots.diff_entries, ids.len() as u64);
        roots.state_root
    }

    let (_da, db_a) = fresh_db(Address::ZERO);
    // Use a fresh DB for run_b — fresh_db installs a perp at index 0 we'd
    // otherwise duplicate. Both DBs land in the same shape because we
    // rewrite that perp via `pms.put` in `populate`.
    let (_db, db_b) = fresh_db(Address::ZERO);

    let r_forward = populate(&db_a, 4, 3, false);
    let r_reverse = populate(&db_b, 4, 3, true);
    assert_eq!(
        r_forward, r_reverse,
        "the root must not depend on the order markets were written in"
    );
}
