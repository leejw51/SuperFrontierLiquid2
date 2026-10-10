//! A chain whose blocks are written to disk after the fact must be the same
//! chain as one whose blocks are written before the next begins.
//!
//! `Database::enable_async_writes` lets a sequencer commit block N+1 while a
//! writer thread is still putting block N into RocksDB; until then N's rows are
//! served from an overlay. Every read the block builder makes — the head,
//! balances, resting orders, the books' index, the tries' nodes — goes through
//! that overlay, and if any one of them did not, block N+1 would be built on
//! stale state and its root would differ from a synchronous node's. That is a
//! consensus split, so the property to pin is the same one `spot_book_cache.rs`
//! pins for the carried books: run identical transactions both ways and the
//! chains must agree block for block, and once flushed the disk must agree
//! with what the overlay said.
//!
//! Manual drain mode makes the test deterministic: writes queue until the
//! test drains them, so several blocks are held pending at once and the
//! overlay is exercised at depth, not just for one block.

use cb_block::{build_and_apply, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Accounts, BookIndex, Database, MarketConfig, Markets, Meta, Orders};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, MarketId, Side, Tif, H256};
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
        let (sk, pk) = keypair_from_seed(format!("async-writes-{i}").as_bytes());
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
        let (sk, _) = keypair_from_seed(b"async-writes-pauper");
        txs.push(place(&sk, 0, Side::Buy, mid + 60, 2_000, Tif::Ioc, 99));
    }
    txs
}

struct Run {
    roots: Vec<H256>,
    hashes: Vec<H256>,
    db: Database,
    _dir: tempfile::TempDir,
}

/// `drain_every == 0` means synchronous writes. Otherwise writes queue and
/// the test drains one block's batch every `drain_every` blocks, so the
/// overlay carries up to `drain_every` blocks at a time.
fn run(blocks: u64, drain_every: u64) -> Run {
    let (dir, db, actors) = setup();
    if drain_every > 0 {
        db.enable_manual_writes();
    }
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        ..Default::default()
    };
    let mut nonces = vec![0u64; actors.len()];
    let (mut roots, mut hashes) = (Vec::new(), Vec::new());
    for n in 0..blocks {
        let txs = block_txs(&actors, &mut nonces, n);
        let res = build_and_apply(&db, &ctx, txs, 1_000 + n, 0, Vec::new(), |_| vec![0u8; 65])
            .expect("block builds");
        roots.push(res.block.header.state_root);
        hashes.push(res.block.header.hash());
        if drain_every > 0 && n % drain_every == drain_every - 1 {
            assert!(db.drain_one(), "a batch was queued for this block");
        }
    }
    Run {
        roots,
        hashes,
        db,
        _dir: dir,
    }
}

#[test]
fn a_chain_written_late_is_the_same_chain() {
    let sync = run(30, 0);
    let lazy = run(30, 4);
    assert_eq!(sync.roots, lazy.roots, "state roots diverged");
    assert_eq!(sync.hashes, lazy.hashes, "block hashes diverged");

    // Up to four blocks were still pending when the run ended. The overlay
    // already reports the final head; the disk does not yet.
    assert!(
        lazy.db.pending_depth() > 0,
        "the test should end with writes in flight"
    );
    assert_eq!(Meta::new(&lazy.db).head_height(), 30);
    assert!(Meta::new(&lazy.db.durable_view()).head_height() < 30);

    // Flush, and the disk agrees with the overlay and with the synchronous
    // chain on the head, every balance, and every resting order.
    lazy.db.flush_pending().unwrap();
    assert_eq!(lazy.db.pending_depth(), 0);
    let durable = lazy.db.durable_view();
    assert_eq!(Meta::new(&durable).head_height(), 30);
    assert_eq!(
        Meta::new(&durable).head_hash(),
        *sync.hashes.last().unwrap()
    );
    for i in 0..6u8 {
        let (_, pk) = keypair_from_seed(format!("async-writes-{i}").as_bytes());
        let addr = address_from_pubkey(&pk);
        assert_eq!(
            Accounts::new(&durable).get(&addr),
            Accounts::new(&sync.db).get(&addr),
            "account {i} differs on disk"
        );
    }
    let resting =
        |db: &Database| BookIndex::new(db).list_orders_for_market(&Orders::new(db), MARKET);
    assert_eq!(resting(&durable), resting(&sync.db));
}

#[test]
fn the_durable_view_never_shows_a_head_the_disk_lacks() {
    let lazy = run(9, 3);
    // `drain_one` writes one block's batch. Three drains over nine blocks
    // leave blocks 1..=3 on disk and 4..=9 pending.
    let visible = Meta::new(&lazy.db).head_height();
    let on_disk = Meta::new(&lazy.db.durable_view()).head_height();
    assert_eq!(visible, 9);
    assert_eq!(on_disk, 3);
    assert_eq!(lazy.db.pending_depth(), 6);
    // Any block the durable view reports a height for, it can hand over;
    // any it does not, it will not — while the pending view has them all.
    for h in 1..=on_disk {
        assert!(cb_state::Blocks::new(&lazy.db.durable_view())
            .get(h)
            .is_some());
    }
    assert!(cb_state::Blocks::new(&lazy.db.durable_view())
        .get(4)
        .is_none());
    assert!(cb_state::Blocks::new(&lazy.db).get(4).is_some());
    assert!(cb_state::Blocks::new(&lazy.db).get(9).is_some());
}
