//! A leader that seals its blocks on the writer thread must produce a chain
//! every replica accepts, and the same chain a synchronous leader produces.
//!
//! `execute_and_hand_off` executes a block, publishes its rows as an overlay
//! and returns; the diff hash, header, signature and body are made later, on
//! the writer thread (ADR 0007). Two things could go wrong. The next block
//! could be built without seeing the previous block's rows, height or
//! timestamp, which would fork the chain. And the sealed root could differ
//! from the one `build_and_apply` computes for the same rows. The first test
//! pins that a pipelined chain, held several blocks deep in the overlay,
//! replays on a replica block for block. The second pins that, drained after
//! every block, it is the synchronous chain byte for byte.

use cb_block::{
    build_and_apply, execute_and_hand_off, verify_and_commit, Block, ExecCtx, SealChain,
};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Accounts, Blocks, Database, MarketConfig, Markets, Meta};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

const MARKET: MarketId = MarketId::BtcUsd;

fn setup() -> (tempfile::TempDir, Database, Vec<(SecretKey, Address)>) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Markets::new(&db).put(&MarketConfig {
        id: MARKET,
        base: AssetId::Btc,
        quote: AssetId::Usd,
        tick_size: 1,
        min_qty: 1,
        fee_bps: 10,
    });
    let mut actors = Vec::new();
    for i in 0..6u8 {
        let (sk, pk) = keypair_from_seed(format!("pipelined-seal-{i}").as_bytes());
        let addr = address_from_pubkey(&pk);
        let accounts = Accounts::new(&db);
        let mut s = accounts.get(&addr).unwrap_or_default();
        s.add(AssetId::Usd, 1_000_000_000_000_000).unwrap();
        s.add(AssetId::Btc, 1_000_000_000_000).unwrap();
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

/// Makers rest, a taker crosses, a transfer moves quote, every third block a
/// maker cancels, every fifth a pauper's order fails and rolls back. The same
/// mix `spot_book_cache.rs` uses, plus the transfer, so account rows, order
/// rows, index rows and the head all change every block.
fn block_txs(actors: &[(SecretKey, Address)], nonces: &mut [u64], n: u64) -> Vec<VerifiedTx> {
    let mut txs = Vec::new();
    let mid = 50_000u128;
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
    let (sk, _) = &actors[4];
    let side = if n % 2 == 0 { Side::Buy } else { Side::Sell };
    let price = match side {
        Side::Buy => mid + 50,
        Side::Sell => mid - 50,
    };
    txs.push(place(sk, nonces[4], side, price, 1_500, Tif::Ioc, 40));
    nonces[4] += 1;
    let (sk, _) = &actors[5];
    txs.push(sign(
        sk,
        nonces[5],
        TxBody::Transfer {
            to: actors[0].1,
            asset: AssetId::Usd,
            amount: 1_000 + n as u128,
        },
    ));
    nonces[5] += 1;
    if n % 3 == 2 {
        let (sk, _) = &actors[0];
        txs.push(sign(sk, nonces[0], TxBody::BatchCancel { market: MARKET }));
        nonces[0] += 1;
    }
    if n % 5 == 4 {
        let (sk, _) = keypair_from_seed(b"pipelined-seal-pauper");
        txs.push(place(&sk, 0, Side::Buy, mid + 60, 2_000, Tif::Ioc, 99));
    }
    txs
}

fn ctx(db: &Database) -> ExecCtx {
    ExecCtx {
        treasury_signer: Meta::new(db).company_addr(),
        ..Default::default()
    }
}

/// Build `blocks` blocks through the pipelined hand-off in manual write mode,
/// draining one queued block every `drain_every` blocks, then flush.
fn pipelined(blocks: u64, drain_every: u64) -> (tempfile::TempDir, Database, Vec<(u64, H256)>) {
    let (dir, db, actors) = setup();
    db.enable_manual_writes();
    let chain = Arc::new(Mutex::new(SealChain::at_head(&db)));
    let durable: Arc<Mutex<Vec<(u64, H256)>>> = Arc::default();
    let mut nonces = vec![0u64; actors.len()];
    for n in 0..blocks {
        let txs = block_txs(&actors, &mut nonces, n);
        let seen = Arc::clone(&durable);
        let out = execute_and_hand_off(
            &db,
            &ctx(&db),
            txs,
            1_000 + n,
            0,
            Vec::new(),
            Arc::clone(&chain),
            |_| vec![0u8; 65],
            move |h, hash| seen.lock().unwrap().push((h, hash)),
            |_, _, _| {},
        )
        .expect("block executes");
        assert_eq!(out.height, n + 1);
        // The next block must see this one's head at once, sealed or not.
        assert_eq!(Meta::new(&db).head_height(), n + 1);
        if n % drain_every == drain_every - 1 {
            assert!(db.drain_one(), "a sealed write was queued");
        }
    }
    db.flush_pending().unwrap();
    let durable = durable.lock().unwrap().clone();
    (dir, db, durable)
}

#[test]
fn a_pipelined_chain_replays_on_a_replica_block_for_block() {
    let (_dir, leader, durable) = pipelined(30, 4);
    assert_eq!(
        durable.len(),
        30,
        "every block announced once, after its write"
    );
    assert!(
        durable.windows(2).all(|w| w[0].0 + 1 == w[1].0),
        "announced in height order"
    );

    let (_rdir, replica, _) = setup();
    let ctx = ctx(&replica);
    let durable_view = leader.durable_view();
    for h in 1..=30u64 {
        let bytes = Blocks::new(&durable_view)
            .get(h)
            .expect("sealed body on disk");
        let block = Block::decode(&bytes).unwrap();
        assert_eq!(block.header.hash(), durable[(h - 1) as usize].1);
        let txs: Vec<VerifiedTx> = block
            .txs
            .iter()
            .cloned()
            .zip(block.senders.iter().copied())
            .map(|(signed, sender)| VerifiedTx { signed, sender })
            .collect();
        let root = verify_and_commit(
            &replica,
            &ctx,
            &txs,
            h,
            block.header.hash(),
            &bytes,
            block.header.state_root,
            block.header.timestamp,
            &[],
        )
        .unwrap_or_else(|e| panic!("replica refused pipelined block {h}: {e:?}"));
        assert_eq!(root, block.header.state_root);
    }
    assert_eq!(
        Meta::new(&replica).state_root(),
        Meta::new(&durable_view).state_root()
    );
    for i in 0..6u8 {
        let (_, pk) = keypair_from_seed(format!("pipelined-seal-{i}").as_bytes());
        let addr = address_from_pubkey(&pk);
        assert_eq!(
            Accounts::new(&replica).get(&addr),
            Accounts::new(&durable_view).get(&addr)
        );
    }
}

#[test]
fn drained_every_block_it_is_the_synchronous_chain() {
    // With every block sealed before the next executes, the ordering seed
    // (the latest sealed head hash) is the synchronous leader's seed too, so
    // the two chains must be identical, not merely both valid.
    let (_dir, _db, durable) = pipelined(20, 1);
    let (_sdir, sdb, actors) = setup();
    let mut nonces = vec![0u64; actors.len()];
    let mut hashes = Vec::new();
    for n in 0..20 {
        let txs = block_txs(&actors, &mut nonces, n);
        let res = build_and_apply(&sdb, &ctx(&sdb), txs, 1_000 + n, 0, Vec::new(), |_| {
            vec![0u8; 65]
        })
        .expect("block builds");
        hashes.push(res.block.header.hash());
    }
    let piped: Vec<H256> = durable.iter().map(|(_, h)| *h).collect();
    assert_eq!(piped, hashes);
}
