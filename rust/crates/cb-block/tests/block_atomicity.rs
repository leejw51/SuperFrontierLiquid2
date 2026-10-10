//! A block's mutation is transactional, or it is not a block.
//!
//! These are regressions for the defect the flow review filed as findings 1, 4
//! and 12 — one defect, three consequences. The executor wrote order rows,
//! book-index entries, positions and margin straight to RocksDB as it went,
//! while balances were buffered and flushed by `BlockState::commit`. So when a
//! transaction later in the block failed and the builder aborted, the balances
//! were discarded and the direct writes were not.
//!
//! What that cost, in the reviewer's words and reproduced here:
//!
//! * An order left resting on the book whose collateral lock was rolled back.
//!   The next fill against it called `unlock` on a lock that was never taken,
//!   which was a `checked_sub(..).expect(..)` — the sequencer process died and
//!   the chain stopped. Three ordinary REST calls from a plain faucet account.
//! * One failing transaction destroyed the whole block, and every honest
//!   transaction drained alongside it.
//! * On a replica, the state-root check ran *after* the state was written, so
//!   a block that failed the check had already mutated the database the check
//!   exists to protect.
//!
//! Every test here fails on the code as it was.

use cb_block::{
    apply_tx_at, build_and_apply, verify_and_commit, BlockState, ExecCtx, ExecError, VerifyError,
};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{
    Accounts, BalanceError, BookIndex, Database, MarketConfig, Markets, Meta, Orders, SpotOrder,
    ALL_CFS,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: MarketId = MarketId::BtcUsd;
const TICK: u128 = 1;
const MIN_QTY: u128 = 1;
const FEE_BPS: u16 = 10;
const Q8: u128 = 100_000_000;
/// Enough quote for any order in this file. Notional is `price_q8 * qty / 1e8`,
/// so a quarter of a coin at 70 000 reserves ~1.75e12 of it.
const FUNDED_USD: u128 = 1_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn setup() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
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
fn order(
    sk: &SecretKey,
    nonce: u64,
    side: Side,
    price: u128,
    qty: u128,
    tif: Tif,
    post_only: bool,
    cid: u8,
) -> VerifiedTx {
    sign(
        sk,
        nonce,
        TxBody::PlaceOrder {
            market: MARKET,
            side,
            price,
            qty,
            tif,
            post_only,
            client_order_id: H256([cid; 32]),
        },
    )
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn build(db: &Database, txs: Vec<VerifiedTx>) -> Result<cb_block::BuildResult, ExecError> {
    build_and_apply(db, &ctx(), txs, 1_700_000_000, 0, Vec::new(), |_| {
        vec![0u8; 65]
    })
}

/// Every key/value in every column family. Two databases with equal snapshots
/// are equal in every way the chain can observe.
fn snapshot(db: &Database) -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for cf in ALL_CFS {
        let empty: [u8; 0] = [];
        let it = match db.iter_prefix(cf, &empty) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for (k, v) in it.flatten() {
            out.push((cf.to_string(), k.to_vec(), v.to_vec()));
        }
    }
    out.sort();
    out
}

/// The rows a spot order leaves behind, so a test can say "none of them".
fn spot_rows(db: &Database) -> (usize, usize) {
    let empty: [u8; 0] = [];
    let orders = db
        .iter_prefix(cb_state::CF_ORDERS, &empty)
        .map(|it| it.filter(|kv| kv.is_ok()).count())
        .unwrap_or(0);
    let index = db
        .iter_prefix(cb_state::CF_BOOK_INDEX, &empty)
        .map(|it| it.filter(|kv| kv.is_ok()).count())
        .unwrap_or(0);
    (orders, index)
}

fn buy_lock(price: u128, qty: u128) -> u128 {
    let notional = price * qty / Q8;
    let fee_reserve = (notional * FEE_BPS as u128).div_ceil(10_000);
    notional + fee_reserve
}

// ===========================================================================
// 1. A failed transaction leaves no trace — not one row, not one bit of root.
// ===========================================================================
//
// Finding 1's core claim, stated as an equality: the state root of a block
// containing a transaction that failed equals the state root of the same block
// without that transaction. Anything the failure left behind — an order row, a
// book-index entry, a lock, an account row created by a mere read — would
// break it.

#[test]
fn a_failed_transaction_leaves_no_trace_and_does_not_move_the_root() {
    let (_da, db_a) = setup();
    let (_db, db_b) = setup();

    let (alice_sk, alice) = actor("alice");
    let (bob_sk, bob) = actor("bob");

    for db in [&db_a, &db_b] {
        fund(db, alice, AssetId::Btc, 10 * Q8);
        fund(db, bob, AssetId::Usd, FUNDED_USD);
    }

    // Alice rests a post-only ask (intra-block rank 1, so it executes first).
    let ask = order(
        &alice_sk,
        0,
        Side::Sell,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        true,
        1,
    );
    // Bob sends a fill-or-kill bid far below it. It cannot fill, so the
    // matcher kills it — an ordinary transaction, no privileges, and the one
    // the reviewer used to take a node down.
    let fok = order(
        &bob_sk,
        0,
        Side::Buy,
        40_000 * Q8,
        Q8 / 4,
        Tif::Fok,
        false,
        2,
    );

    let with_failure = build(&db_a, vec![ask.clone(), fok]).expect("block must still be produced");
    let without = build(&db_b, vec![ask]).expect("control block");

    // The block carries only the transaction that executed, and says which one
    // it refused and why.
    assert_eq!(with_failure.block.txs.len(), 1);
    assert_eq!(with_failure.rejected.len(), 1);
    assert_eq!(with_failure.rejected[0].error, ExecError::FokUnfilled);
    assert_eq!(with_failure.rejected[0].sender, bob);

    // The equality the whole fix is for.
    assert_eq!(
        with_failure.state_root, without.state_root,
        "a transaction that failed must not be able to move the state root"
    );

    // And spelled out row by row, because a root is a hash and a hash is not a
    // diagnosis.
    let a = Accounts::new(&db_a);
    let b = Accounts::new(&db_b);
    assert_eq!(a.get(&bob), b.get(&bob), "bob must be untouched");
    assert_eq!(a.get(&alice), b.get(&alice));
    assert_eq!(
        a.get(&bob).map(|s| s.locked_of(AssetId::Usd)).unwrap_or(0),
        0,
        "the killed order's lock must have been released"
    );
    assert_eq!(spot_rows(&db_a), spot_rows(&db_b));
    assert_eq!(spot_rows(&db_a).0, 1, "only alice's ask should be resting");
}

// ===========================================================================
// 2. The reviewer's reproduction, end to end.
// ===========================================================================

/// Step one and two of the transcript. Before the fix, the block aborted and
/// the ask survived *without* its lock. Now the block commits, and the ask and
/// its lock are consistent — which is the invariant the third step used to
/// break.
#[test]
fn the_surviving_order_is_backed_by_the_lock_it_claims() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    let (bob_sk, bob) = actor("bob");
    fund(&db, alice, AssetId::Btc, 10 * Q8);
    fund(&db, bob, AssetId::Usd, FUNDED_USD);

    let ask = order(
        &alice_sk,
        0,
        Side::Sell,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        true,
        1,
    );
    let fok = order(
        &bob_sk,
        0,
        Side::Buy,
        40_000 * Q8,
        Q8 / 4,
        Tif::Fok,
        false,
        2,
    );
    let res = build(&db, vec![ask, fok]).expect("block produced");
    assert_eq!(res.rejected.len(), 1);

    // PLAN §7 invariant 3: locked balance equals what the resting orders
    // reserve. This is what a rolled-back block used to violate silently.
    let resting: Vec<SpotOrder> = Orders::new(&db).list_for_market(MARKET);
    assert_eq!(resting.len(), 1);
    let reserved: u128 = resting.iter().map(|o| o.qty_remaining).sum();
    assert_eq!(
        Accounts::new(&db)
            .get(&alice)
            .unwrap()
            .locked_of(AssetId::Btc),
        reserved,
        "a resting sell must be backed by a lock of exactly its remaining size"
    );
}

/// Step three. A third, unrelated account hits the ask. Before the fix this
/// was `thread 'main' panicked: unlock underflow` and the process was gone.
#[test]
fn matching_the_surviving_order_settles_instead_of_killing_the_node() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    let (bob_sk, bob) = actor("bob");
    let (carol_sk, carol) = actor("carol");
    fund(&db, alice, AssetId::Btc, 10 * Q8);
    fund(&db, bob, AssetId::Usd, FUNDED_USD);
    fund(&db, carol, AssetId::Usd, FUNDED_USD);

    let ask = order(
        &alice_sk,
        0,
        Side::Sell,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        true,
        1,
    );
    let fok = order(
        &bob_sk,
        0,
        Side::Buy,
        40_000 * Q8,
        Q8 / 4,
        Tif::Fok,
        false,
        2,
    );
    build(&db, vec![ask, fok]).expect("block 1");

    let hit = order(
        &carol_sk,
        0,
        Side::Buy,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        false,
        3,
    );
    let res = build(&db, vec![hit]).expect("block 2 — this is where the node used to die");
    assert!(res.rejected.is_empty(), "the fill must settle cleanly");

    assert_eq!(
        Accounts::new(&db)
            .get(&carol)
            .unwrap()
            .balance(AssetId::Btc),
        Q8 / 4
    );
    assert_eq!(
        Accounts::new(&db)
            .get(&alice)
            .unwrap()
            .locked_of(AssetId::Btc),
        0
    );
    assert_eq!(spot_rows(&db).0, 0, "the ask is fully filled and gone");
}

/// The unlock path on its own, with the corrupted state constructed by hand so
/// the test does not depend on any particular way of reaching it.
///
/// A resting ask whose owner holds no lock is exactly what a rolled-back block
/// used to leave behind. Matching it must return an error the block builder
/// can reject, not a panic the operating system reports.
#[test]
fn unlocking_a_lock_that_was_never_taken_is_an_error_not_a_panic() {
    let (_d, db) = setup();
    let (_ghost_sk, ghost) = actor("ghost");
    let (carol_sk, carol) = actor("carol");
    // The ghost has the coin but no lock over it.
    fund(&db, ghost, AssetId::Btc, 10 * Q8);
    fund(&db, carol, AssetId::Usd, FUNDED_USD);

    let stale = SpotOrder {
        order_id: H256([9; 32]),
        market: MARKET,
        owner: ghost,
        side: Side::Sell,
        price: 70_000 * Q8,
        qty_remaining: Q8 / 4,
        qty_total: Q8 / 4,
        seq: 1,
        tif: Tif::Gtc,
        post_only: true,
        client_order_id: H256([9; 32]),
        created_height: 1,
    };
    Orders::new(&db).put(&stale);
    BookIndex::new(&db).put(MARKET, Side::Sell, stale.price, stale.seq, &stale.order_id);

    let hit = order(
        &carol_sk,
        0,
        Side::Buy,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        false,
        3,
    );
    let mut state = BlockState::new(&db);
    let err = apply_tx_at(&mut state, &ctx(), &hit, 0).expect_err("must not succeed");
    assert_eq!(err, ExecError::Balance(BalanceError::UnlockUnderflow));
}

// ===========================================================================
// 3. One bad transaction no longer destroys the block.
// ===========================================================================

#[test]
fn a_fill_or_kill_against_an_empty_book_is_rejected_and_the_block_keeps_the_rest() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    let (bob_sk, bob) = actor("bob");
    let (dave_sk, dave) = actor("dave");
    fund(&db, alice, AssetId::Usd, FUNDED_USD);
    fund(&db, bob, AssetId::Usd, FUNDED_USD);
    fund(&db, dave, AssetId::Usd, FUNDED_USD);

    // Nothing is resting: the book is empty, so the FOK cannot fill. The
    // gateway cannot pre-check this, which is exactly why it has to be
    // survivable at block build.
    let doomed = order(
        &bob_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        Q8 / 10,
        Tif::Fok,
        false,
        7,
    );
    let honest_a = order(
        &alice_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        Q8 / 10,
        Tif::Gtc,
        false,
        8,
    );
    let honest_b = sign(
        &dave_sk,
        0,
        TxBody::Transfer {
            to: alice,
            asset: AssetId::Usd,
            amount: 25,
        },
    );

    let res = build(&db, vec![honest_a, doomed, honest_b]).expect("block must be produced");
    assert_eq!(res.block.txs.len(), 2, "both honest txs survive");
    assert_eq!(res.rejected.len(), 1);
    assert_eq!(res.rejected[0].error, ExecError::FokUnfilled);
    assert_eq!(Meta::new(&db).head_height(), 1);

    // Alice's resting bid is on the book with its lock; Dave's transfer landed.
    assert_eq!(spot_rows(&db).0, 1);
    let accounts = Accounts::new(&db);
    assert_eq!(
        accounts.get(&alice).unwrap().locked_of(AssetId::Usd),
        buy_lock(50_000 * Q8, Q8 / 10)
    );
    assert_eq!(
        accounts.get(&dave).unwrap().balance(AssetId::Usd),
        FUNDED_USD - 25
    );
    // And Bob is exactly where he started.
    assert_eq!(accounts.get(&bob).unwrap().nonce, 0);
    assert_eq!(accounts.get(&bob).unwrap().locked_of(AssetId::Usd), 0);
}

#[test]
fn a_block_whose_every_transaction_fails_produces_no_block_at_all() {
    let (_d, db) = setup();
    let (bob_sk, bob) = actor("bob");
    fund(&db, bob, AssetId::Usd, FUNDED_USD);

    let doomed = order(
        &bob_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        Q8 / 10,
        Tif::Fok,
        false,
        7,
    );
    let before = snapshot(&db);
    match build(&db, vec![doomed]) {
        Err(ExecError::NoApplicableTxs { rejected }) => assert_eq!(rejected.len(), 1),
        Err(other) => panic!("expected NoApplicableTxs, got {other:?}"),
        Ok(_) => panic!("a block of nothing but failures must not be produced"),
    }
    assert_eq!(Meta::new(&db).head_height(), 0);
    assert_eq!(
        snapshot(&db),
        before,
        "an unproduced block must not have written anything"
    );
}

// ===========================================================================
// 4. The state-root check precedes the write.
// ===========================================================================

#[test]
fn a_block_that_fails_the_state_root_check_leaves_the_database_untouched() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    let (carol_sk, carol) = actor("carol");
    fund(&db, alice, AssetId::Btc, 10 * Q8);
    fund(&db, carol, AssetId::Usd, FUNDED_USD);

    // A block that does real work: a resting ask, and a bid that takes it.
    // Positions, orders, book index, balances and locks all move.
    let ask = order(
        &alice_sk,
        0,
        Side::Sell,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        true,
        1,
    );
    let hit = order(
        &carol_sk,
        0,
        Side::Buy,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        false,
        2,
    );
    let txs = vec![ask, hit];

    let before = snapshot(&db);
    let err = verify_and_commit(
        &db,
        &ctx(),
        &txs,
        1,
        H256([7; 32]),
        b"block bytes",
        // A root this block cannot possibly produce — the case the
        // re-execution check exists for.
        H256([0xab; 32]),
        0,
        &[],
    )
    .expect_err("a divergent root must be rejected");
    assert!(matches!(err, VerifyError::StateRootMismatch { .. }));

    assert_eq!(
        snapshot(&db),
        before,
        "a rejected block must leave the database byte-for-byte as it was"
    );
    assert_eq!(Meta::new(&db).head_height(), 0);
    assert_eq!(spot_rows(&db), (0, 0));
}

#[test]
fn a_block_that_passes_the_check_commits_state_block_and_head_together() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    fund(&db, alice, AssetId::Btc, 10 * Q8);
    let ask = order(
        &alice_sk,
        0,
        Side::Sell,
        70_000 * Q8,
        Q8 / 4,
        Tif::Gtc,
        true,
        1,
    );

    // Learn the correct root by building the block on a twin database.
    let (_d2, twin) = setup();
    fund(&twin, alice, AssetId::Btc, 10 * Q8);
    let expected = build(&twin, vec![ask.clone()]).unwrap();

    let root = verify_and_commit(
        &db,
        &ctx(),
        &[ask],
        1,
        expected.block.header.hash(),
        &expected.block.encode(),
        expected.state_root,
        expected.block.header.timestamp,
        &[],
    )
    .expect("the honest block must be accepted");
    assert_eq!(root, expected.state_root);
    assert_eq!(Meta::new(&db).head_height(), 1);
    assert_eq!(Meta::new(&db).state_root(), expected.state_root);
    assert!(cb_state::Blocks::new(&db).get(1).is_some());
    assert_eq!(
        cb_state::Blocks::new(&db).height_of(&expected.block.header.hash()),
        Some(1)
    );
    // Byte-identical state to the leader's, not merely an equal root.
    assert_eq!(snapshot(&db), snapshot(&twin));
}

// ===========================================================================
// 5. A failed transaction does not brick the account that sent it.
// ===========================================================================

#[test]
fn an_account_whose_transaction_failed_can_transact_again() {
    let (_d, db) = setup();
    let (alice_sk, alice) = actor("alice");
    let (bob_sk, bob) = actor("bob");
    fund(&db, alice, AssetId::Usd, FUNDED_USD);
    fund(&db, bob, AssetId::Usd, FUNDED_USD);

    // Block 1: bob's FOK dies against an empty book. It is dropped, so it
    // never consumed a nonce — the chain's nonce for bob is still 0.
    let doomed = order(
        &bob_sk,
        0,
        Side::Buy,
        50_000 * Q8,
        Q8 / 10,
        Tif::Fok,
        false,
        7,
    );
    let filler = sign(
        &alice_sk,
        0,
        TxBody::Transfer {
            to: bob,
            asset: AssetId::Usd,
            amount: 1,
        },
    );
    let res = build(&db, vec![doomed, filler]).expect("block 1");
    assert_eq!(res.rejected.len(), 1);
    assert_eq!(Accounts::new(&db).get(&bob).unwrap().nonce, 0);

    // Block 2: bob signs the *same* nonce again, this time with something that
    // works. Before the fix, his nonce was unrecoverable at the gateway and
    // every later submission killed another block.
    let good = sign(
        &bob_sk,
        0,
        TxBody::Transfer {
            to: alice,
            asset: AssetId::Usd,
            amount: 500,
        },
    );
    let res = build(&db, vec![good]).expect("block 2");
    assert!(res.rejected.is_empty(), "the retry must execute");
    assert_eq!(Accounts::new(&db).get(&bob).unwrap().nonce, 1);
    assert_eq!(
        Accounts::new(&db).get(&bob).unwrap().balance(AssetId::Usd),
        FUNDED_USD + 1 - 500
    );
}

// ===========================================================================
// 6. The property, over sequences.
// ===========================================================================
//
// For any sequence of transactions, the state root of the block that applies
// it is the same whether or not a transaction that fails is inserted into it,
// wherever it is inserted. That is the general statement of finding 1, and the
// reason to state it generally is that the specific cases above only cover the
// paths somebody thought of.
//
// No proptest dependency in this crate: a seeded xorshift generator over a
// small alphabet of transaction shapes, which is reproducible and reports the
// seed it failed on.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Build a workload from three traders, plus one transaction from a fourth
/// account that is guaranteed to fail. Both databases in a trial are seeded
/// identically, including the doomed account, so the only difference between
/// them is whether the doomed transaction was submitted.
fn workload(rng: &mut Rng, db: &Database, doomed_seed: &str) -> (Vec<VerifiedTx>, VerifiedTx) {
    let traders = [actor("t0"), actor("t1"), actor("t2")];
    for (_, addr) in &traders {
        fund(db, *addr, AssetId::Usd, FUNDED_USD);
        fund(db, *addr, AssetId::Btc, 100 * Q8);
    }
    let (doomed_sk, doomed_addr) = actor(doomed_seed);
    fund(db, doomed_addr, AssetId::Usd, FUNDED_USD);

    let mut nonces = [0u64; 3];
    let mut txs = Vec::new();
    let count = 3 + rng.below(6);
    for i in 0..count {
        let who = rng.below(3) as usize;
        let (sk, _) = &traders[who];
        let nonce = nonces[who];
        nonces[who] += 1;
        let tx = match rng.below(3) {
            0 => order(
                sk,
                nonce,
                Side::Buy,
                (50_000 + rng.below(5) as u128 * 100) * Q8,
                Q8 / 10,
                Tif::Gtc,
                false,
                i as u8,
            ),
            1 => order(
                sk,
                nonce,
                Side::Sell,
                (50_000 + rng.below(5) as u128 * 100) * Q8,
                Q8 / 10,
                Tif::Gtc,
                false,
                (100 + i) as u8,
            ),
            _ => sign(
                sk,
                nonce,
                TxBody::Transfer {
                    to: traders[(who + 1) % 3].1,
                    asset: AssetId::Usd,
                    amount: 1 + rng.below(50) as u128,
                },
            ),
        };
        txs.push(tx);
    }

    // The failing transaction: an affordable fill-or-kill bid priced where
    // nothing can cross it. It fails *after* taking its lock and writing the
    // sender back, and after hydrating the block's cached order book — so a
    // rollback has all three to undo, which is the point.
    let doomed = order(&doomed_sk, 0, Side::Buy, Q8, Q8 / 10, Tif::Fok, false, 250);
    (txs, doomed)
}

#[test]
fn inserting_a_failing_transaction_anywhere_never_changes_the_state_root() {
    for seed in 1..=25u64 {
        let base = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;

        // How many transactions this seed produces (the generator is
        // deterministic, so one probe run is enough to know).
        let probe_len = {
            let (_d, probe_db) = setup();
            workload(&mut Rng(base), &probe_db, "doomed-probe").0.len()
        };

        for at in 0..=probe_len {
            // Each position gets its own doomed account, because
            // `order_for_block` sorts by (rank, sender, nonce) — varying only
            // the input position would not vary where the failure actually
            // lands in the applied order, and where it lands is the thing
            // under test.
            let doomed_seed = format!("doomed-{seed}-{at}");

            let (_dc, control_db) = setup();
            let (control_txs, _) = workload(&mut Rng(base), &control_db, &doomed_seed);
            let control = build(&control_db, control_txs.clone()).expect("control block");

            let (_dt, trial_db) = setup();
            let (trial_txs, doomed) = workload(&mut Rng(base), &trial_db, &doomed_seed);
            let mut with_doomed = trial_txs;
            with_doomed.insert(at, doomed);
            let trial = build(&trial_db, with_doomed).expect("trial block");

            assert_eq!(
                trial.rejected.len(),
                control.rejected.len() + 1,
                "seed {seed}, at {at}: exactly the injected transaction should fail"
            );
            assert_eq!(
                trial.block.txs.len(),
                control.block.txs.len(),
                "seed {seed}, at {at}"
            );
            assert_eq!(
                trial.state_root, control.state_root,
                "seed {seed}: a failing tx inserted at position {at} moved the state root"
            );
            assert_eq!(
                snapshot(&trial_db)
                    .into_iter()
                    .filter(|(cf, _, _)| cf != cb_state::CF_BLOCKS && cf != cb_state::CF_BLOCK_HASH)
                    .collect::<Vec<_>>(),
                snapshot(&control_db)
                    .into_iter()
                    .filter(|(cf, _, _)| cf != cb_state::CF_BLOCKS && cf != cb_state::CF_BLOCK_HASH)
                    .collect::<Vec<_>>(),
                "seed {seed}, at {at}: state diverged row for row"
            );
        }
    }
}
