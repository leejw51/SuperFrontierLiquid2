//! End-to-end test for the S4 spot `getOrderBook` RPC surface.
//!
//! Mirrors `derivatives_reads.rs::get_perp_order_book_*` against the spot
//! book index. We seed `CF_ORDERS` + `CF_BOOK_INDEX` directly (per the S4
//! plan; the executor wiring is exercised by the S3 test suite) and assert
//! the RPC surfaces correctly aggregated, depth-capped levels in
//! best-first order per side.

use cb_rpc::{conv, server::SequencerServer};
use cb_schema::rpc_capnp;
use cb_state::{spot::SpotOrder, BookIndex, Database, Meta, Orders};
use cb_types::{Address, MarketId, Side, Tif, H256};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

fn run_local<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f()))
}

fn make_server(db: Database) -> rpc_capnp::super_frontier_liquid::Client {
    let notify = Arc::new(Notify::new());
    let server = SequencerServer::without_voting(db, notify);
    capnp_rpc::new_client(server)
}

fn mk_order(market: MarketId, oid: u8, side: Side, price: u128, qty: u128, seq: u64) -> SpotOrder {
    SpotOrder {
        order_id: H256([oid; 32]),
        market,
        owner: Address([oid; 20]),
        side,
        price,
        qty_total: qty,
        qty_remaining: qty,
        seq,
        tif: Tif::Gtc,
        post_only: false,
        client_order_id: H256([0; 32]),
        created_height: 1,
    }
}

#[test]
fn get_order_book_aggregates_levels_with_correct_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let market = MarketId::BtcUsd;

    {
        let orders = Orders::new(&db);
        let book = BookIndex::new(&db);
        let to_insert = [
            mk_order(market, 1, Side::Buy, 100, 10, 1),
            mk_order(market, 2, Side::Buy, 100, 5, 2), // same price → aggregates
            mk_order(market, 3, Side::Buy, 99, 7, 3),
            mk_order(market, 4, Side::Sell, 101, 20, 4),
            mk_order(market, 5, Side::Sell, 102, 30, 5),
        ];
        for o in &to_insert {
            orders.put(o);
            book.put(o.market, o.side, o.price, o.seq, &o.order_id);
        }
        // Different market should NOT leak into the snapshot.
        let other = mk_order(MarketId::EthUsd, 9, Side::Buy, 100, 999, 0);
        orders.put(&other);
        book.put(
            other.market,
            other.side,
            other.price,
            other.seq,
            &other.order_id,
        );
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_order_book_request();
        let mut p = req.get();
        p.set_market(conv::market_to_capnp(market));
        p.set_depth(10);
        let resp = req.send().promise.await.unwrap();
        let snap = conv::spot_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert_eq!(snap.market, market);
        // Bids: best (highest) first; price=100 aggregates qty 15.
        assert_eq!(snap.bids.len(), 2);
        assert_eq!(snap.bids[0].price, 100);
        assert_eq!(snap.bids[0].qty, 15);
        assert_eq!(snap.bids[1].price, 99);
        assert_eq!(snap.bids[1].qty, 7);
        // Asks: best (lowest) first.
        assert_eq!(snap.asks.len(), 2);
        assert_eq!(snap.asks[0].price, 101);
        assert_eq!(snap.asks[0].qty, 20);
        assert_eq!(snap.asks[1].price, 102);
        assert_eq!(snap.asks[1].qty, 30);
    });
}

#[test]
fn get_order_book_respects_depth_cap() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market = MarketId::BtcUsd;
    {
        let orders = Orders::new(&db);
        let book = BookIndex::new(&db);
        for i in 0..5u8 {
            let o = mk_order(market, i + 1, Side::Sell, 100 + i as u128, 10, i as u64);
            orders.put(&o);
            book.put(o.market, o.side, o.price, o.seq, &o.order_id);
        }
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_order_book_request();
        let mut p = req.get();
        p.set_market(conv::market_to_capnp(market));
        p.set_depth(2);
        let resp = req.send().promise.await.unwrap();
        let snap = conv::spot_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert_eq!(snap.asks.len(), 2);
        assert_eq!(snap.asks[0].price, 100);
        assert_eq!(snap.asks[1].price, 101);
        assert!(snap.bids.is_empty());
    });
}

#[test]
fn get_order_book_empty_book_returns_empty_sides() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_order_book_request();
        let mut p = req.get();
        p.set_market(conv::market_to_capnp(MarketId::BtcUsd));
        p.set_depth(5);
        let resp = req.send().promise.await.unwrap();
        let snap = conv::spot_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert!(snap.bids.is_empty());
        assert!(snap.asks.is_empty());
        assert_eq!(snap.market, MarketId::BtcUsd);
    });
}

#[test]
fn a_depth_of_u32_max_is_held_to_the_ceiling_rather_than_allocated() {
    // The spot index reserved `depth` levels before reading one, and `depth`
    // is the client's: `u32::MAX` asked the allocator for about 137 GB and
    // aborted the node. The request must be answered, capped.
    use cb_rpc::server::MAX_BOOK_DEPTH;
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market = MarketId::BtcUsd;
    let levels = MAX_BOOK_DEPTH as u128 + 5;
    {
        let orders = Orders::new(&db);
        let book = BookIndex::new(&db);
        for i in 0..levels {
            let mut o = mk_order(market, 1, Side::Buy, 1_000 + i, 1, i as u64);
            o.order_id = H256({
                let mut id = [0u8; 32];
                id[..16].copy_from_slice(&i.to_be_bytes());
                id
            });
            orders.put(&o);
            book.put(o.market, o.side, o.price, o.seq, &o.order_id);
        }
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_order_book_request();
        let mut p = req.get();
        p.set_market(conv::market_to_capnp(market));
        p.set_depth(u32::MAX);
        let resp = req.send().promise.await.expect("answered, not aborted");
        let snap = conv::spot_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert_eq!(snap.bids.len(), MAX_BOOK_DEPTH as usize);
        assert_eq!(snap.bids[0].price, 1_000 + levels - 1, "best first");
    });
}
