//! The order books carried from one block to the next must produce the same
//! chain as books rebuilt from the column families every block.
//!
//! # What this pins
//!
//! `BlockState` used to hydrate each spot book from the database on first use
//! within a block, and to throw it away when the block ended. Profiling a
//! loaded sequencer put half the executor's time in that rebuild, so the books
//! are now handed to the next block through the database handle's
//! derived-state slot, stamped with the height they describe.
//!
//! That turns a derived structure into carried state, and carried state that
//! drifts from the database is a leader and a replica disagreeing on a state
//! root — the chain halting, not a slow node. So the property worth a test is
//! not "the cache is faster" but "the cache is invisible": run the same
//! transactions twice, once letting the books carry and once forcing every
//! block to rebuild, and the two chains must agree block for block.
//!
//! The workload deliberately includes the three things that can put the
//! carried books out of step with the rows: partial fills (which change a
//! quantity without changing an index key), cancels (which delete rows behind
//! the matcher's back), and a transaction that fails after touching the book
//! (which rolls back).

use cb_block::{apply_tx_at, build_and_apply, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Accounts, Database, MarketConfig, Markets, Meta};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use tempfile::tempdir;

const MARKET: MarketId = MarketId::BtcUsd;
const TICK: u128 = 1;
const MIN_QTY: u128 = 1;
const FEE_BPS: u16 = 10;
const FUNDED_USD: u128 = 1_000_000_000_000_000;
const FUNDED_BTC: u128 = 1_000_000_000_000;

fn setup() -> (tempfile::TempDir, Database, Vec<(SecretKey, Address)>) {
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
    let mut actors = Vec::new();
    for i in 0..6u8 {
        let (sk, pk) = keypair_from_seed(format!("book-cache-{i}").as_bytes());
        let addr = address_from_pubkey(&pk);
        let accounts = Accounts::new(&db);
        let mut s = accounts.get(&addr).unwrap_or_default();
        s.add(AssetId::Usd, FUNDED_USD).unwrap();
        s.add(AssetId::Btc, FUNDED_BTC).unwrap();
        accounts.put(&addr, &s);
        actors.push((sk, addr));
    }
    (dir, db, actors)
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
    price: u128,
    qty: u128,
    tif: Tif,
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
            post_only: false,
            client_order_id: H256([cid; 32]),
        },
    )
}

/// The block workload, as a function of block number, so both runs execute
/// byte-identical transactions.
fn block_txs(actors: &[(SecretKey, Address)], nonces: &mut [u64], n: u64) -> Vec<VerifiedTx> {
    let mut txs = Vec::new();
    let mid = 50_000u128;
    // Two makers rest on each side, a tick apart, at sizes that make the
    // taker below fill one of them completely and the next only partly.
    for (i, (sk, _)) in actors.iter().enumerate().take(4) {
        let side = if i % 2 == 0 { Side::Buy } else { Side::Sell };
        let price = match side {
            Side::Buy => mid - 1 - (i as u128),
            Side::Sell => mid + 1 + (i as u128),
        };
        txs.push(place(
            sk,
            nonces[i],
            side,
            price,
            1_000 + (n as u128 % 3) * 100,
            Tif::Gtc,
            i as u8,
        ));
        nonces[i] += 1;
    }
    // A taker crossing everything on one side: full fills and a partial.
    let (sk, _) = &actors[4];
    let side = if n % 2 == 0 { Side::Buy } else { Side::Sell };
    let price = match side {
        Side::Buy => mid + 50,
        Side::Sell => mid - 50,
    };
    txs.push(place(sk, nonces[4], side, price, 1_500, Tif::Ioc, 40));
    nonces[4] += 1;

    // Every block, an order the matcher rejects after consulting the book: a
    // fill-or-kill for more than rests. It changes nothing, and the books
    // must stay cached through its rollback (see
    // `a_rejected_order_keeps_the_cached_books`).
    let (sk, _) = &actors[5];
    txs.push(place(
        sk,
        nonces[5],
        Side::Buy,
        mid + 50,
        1_000_000,
        Tif::Fok,
        55,
    ));
    // (The nonce does not advance: the transaction is refused.)

    // Every third block, a maker cancels the lot: rows disappear without the
    // matcher seeing them go.
    if n % 3 == 2 {
        let (sk, _) = &actors[0];
        txs.push(sign(sk, nonces[0], TxBody::BatchCancel { market: MARKET }));
        nonces[0] += 1;
    }

    // Every fifth block, a transaction that fails after the book was touched:
    // an account with no balance at all tries to buy. The builder rolls it
    // back, and the carried books must roll back with it.
    if n % 5 == 4 {
        let (sk, pk) = keypair_from_seed(b"book-cache-pauper");
        let _ = pk;
        txs.push(place(&sk, 0, Side::Buy, mid + 60, 2_000, Tif::Ioc, 99));
    }
    txs
}

fn run(blocks: u64, force_rebuild: bool) -> Vec<H256> {
    let (_dir, db, actors) = setup();
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        ..Default::default()
    };
    let mut nonces = vec![0u64; actors.len()];
    let mut roots = Vec::new();
    for n in 0..blocks {
        if force_rebuild {
            // Park something under a stamp no block will ever ask for. The
            // next `BlockState` finds the slot holding another height, drops
            // it, and hydrates from the column families — which is exactly
            // the behaviour this test compares against.
            db.derived_put(u64::MAX, Box::new(()));
        }
        let txs = block_txs(&actors, &mut nonces, n);
        let res = build_and_apply(&db, &ctx, txs, 1_000 + n, 0, Vec::new(), |_| vec![0u8; 65])
            .expect("block builds");
        roots.push(res.block.header.state_root);
    }
    roots
}

#[test]
fn carrying_the_books_between_blocks_changes_no_state_root() {
    let carried = run(30, false);
    let rebuilt = run(30, true);
    assert_eq!(carried.len(), 30);
    assert_eq!(
        carried, rebuilt,
        "a chain whose blocks carried their order books diverged from one that \
         rebuilt them every block"
    );
}

/// A block that never lands must not leave its books behind for the next one.
#[test]
fn an_abandoned_block_leaves_no_usable_books() {
    let (_dir, db, actors) = setup();
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        ..Default::default()
    };
    let mut nonces = vec![0u64; actors.len()];

    // One real block, so the slot holds books for height 1.
    let txs = block_txs(&actors, &mut nonces, 0);
    build_and_apply(&db, &ctx, txs, 1_000, 0, Vec::new(), |_| vec![0u8; 65]).expect("block 1");

    // A `BlockState` that takes them and is dropped without committing — the
    // shape of a block abandoned between execution and its write.
    {
        let mut state = BlockState::new(&db);
        let _ = state.spot_book_mut(MARKET);
    }

    // The slot is empty, so the next block rebuilds and still agrees with a
    // chain that never had a cache at all.
    let root_after = {
        let txs = block_txs(&actors, &mut nonces, 1);
        build_and_apply(&db, &ctx, txs, 1_001, 0, Vec::new(), |_| vec![0u8; 65])
            .expect("block 2")
            .block
            .header
            .state_root
    };
    let clean = run(2, true);
    assert_eq!(
        root_after, clean[1],
        "a block that followed an abandoned one diverged"
    );
}

/// A rollback drops only the books the rolled-back transaction changed. A
/// rejected order changed nothing, so the cached book survives it; a cancel
/// that was rolled back had taken an order out of the cached book, so that
/// book goes and the next use rehydrates it from the rows.
///
/// `rollback` used to clear every book. Under load a quarter of a block was
/// refused orders, and the rebuild that followed each one was three quarters
/// of the executor's time.
#[test]
fn a_rejected_order_keeps_the_cached_books() {
    let (_dir, db, actors) = setup();
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        block_height: 1,
        ..Default::default()
    };
    let mut state = BlockState::new(&db);
    let (sk0, maker) = &actors[0];
    apply_tx_at(
        &mut state,
        &ctx,
        &place(sk0, 0, Side::Sell, 50_001, 1_000, Tif::Gtc, 1),
        0,
    )
    .expect("the maker rests");
    state.commit_point();
    assert!(state.has_cached_spot_book(MARKET));

    // Rejected by the matcher after it consulted the book: nothing changed.
    let sp = state.savepoint();
    let err = apply_tx_at(
        &mut state,
        &ctx,
        &place(&actors[5].0, 0, Side::Buy, 60_000, 1_000_000, Tif::Fok, 2),
        1,
    )
    .expect_err("a fill-or-kill for more than rests is refused");
    assert!(matches!(err, ExecError::FokUnfilled), "{err:?}");
    state.rollback(sp);
    assert!(
        state.has_cached_spot_book(MARKET),
        "a rejected order threw the cached book away"
    );
    state.commit_point();

    // A cancel takes the maker out of the cached book; rolled back, the
    // rows have the maker again and the cached book must not disagree.
    let sp = state.savepoint();
    apply_tx_at(
        &mut state,
        &ctx,
        &sign(sk0, 1, TxBody::BatchCancel { market: MARKET }),
        1,
    )
    .expect("the cancel applies");
    assert!(state
        .spot_book_mut(MARKET)
        .resting_orders_of(maker)
        .is_empty());
    state.rollback(sp);
    assert!(
        !state.has_cached_spot_book(MARKET),
        "a rolled-back cancel left a stale book cached"
    );
    assert_eq!(
        state.spot_book_mut(MARKET).resting_orders_of(maker).len(),
        1,
        "rehydrated from the rows, the maker is back"
    );
}
