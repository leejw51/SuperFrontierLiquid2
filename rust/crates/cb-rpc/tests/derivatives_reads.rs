//! End-to-end test for the M6 derivative-read RPC surface.
//!
//! Exercises every new method on `SuperFrontierLiquid` (getPositions, getMarginAccount,
//! getFundingIndex, getOraclePrice, getMarkPrice, getPerpMarket,
//! listPerpMarkets, getFutureMarket, listFutureMarkets, getLiquidationQueue,
//! getPerpOrderBook) against an in-memory `SequencerServer` wired to a
//! tempdir RocksDB seeded with known fixtures. The handlers return the same
//! determinism-shaped responses the leader will emit in production:
//! deterministic ordering, default-on-missing, no panics on empty CFs.

use cb_rpc::{conv, server::SequencerServer};
use cb_schema::rpc_capnp;
use cb_state::{
    perps::ORACLE_KIND_ORACLE, Database, FundingIndices, FuturesMarkets, LiquidationQueue,
    MarginAccounts, Meta, OracleState, PerpBookIndex, PerpMarkets, PerpOrders, Positions,
    CF_LIQUIDATION_QUEUE,
};
use cb_types::{
    Address, AssetId, DerivId, FundingIndex, FutureMarket, LiquidationEntry, MarginAccount,
    MarkPrice, OraclePrice, PerpMarket, PerpOrder, Position, Side, Signed, Tif, H256,
};
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

fn sample_perp_market(idx: u16) -> PerpMarket {
    PerpMarket {
        id: DerivId::perp(idx),
        underlying: AssetId::Btc,
        tick_size: 100,
        lot_size: 1_000,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 3_600,
        funding_rate_cap_q9: 1_000_000,
        oracle_authority: Address([7; 20]),
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 50_001 * 100_000_000,
        last_trade_height: 5,
        paused_reduce_only_only: false,
    }
}

fn sample_future_market(idx: u16, expiry: u64) -> FutureMarket {
    FutureMarket {
        id: DerivId::future(idx),
        underlying: AssetId::Eth,
        tick_size: 100,
        lot_size: 1_000,
        max_leverage_x100: 2_500,
        initial_margin_bps: 400,
        maintenance_margin_bps: 200,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        expiry_height: expiry,
        settlement_method: 0,
        settlement_window_blocks: 600,
        oracle_authority: Address([8; 20]),
        paused: false,
        final_settlement_px_q8: None,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    }
}

#[test]
fn get_positions_returns_all_positions_for_owner_in_deriv_id_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let alice = Address([1; 20]);
    let bob = Address([2; 20]);

    {
        let ps = Positions::new(&db);
        ps.put(&Position {
            owner: alice,
            market: DerivId::perp(2),
            side: Side::Buy,
            qty: 200,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 50_000 * 100_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
        ps.put(&Position {
            owner: alice,
            market: DerivId::perp(0),
            side: Side::Sell,
            qty: 100,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 49_000 * 100_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
        // Bob's position should NOT appear in Alice's response.
        ps.put(&Position {
            owner: bob,
            market: DerivId::perp(0),
            side: Side::Buy,
            qty: 999,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 0,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        });
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_positions_request();
        conv::address_to_capnp(req.get().init_addr(), &alice);
        let resp = req.send().promise.await.unwrap();
        let list = resp.get().unwrap().get_positions().unwrap();
        assert_eq!(list.len(), 2, "alice has two positions");
        // Order is deriv_id byte order: perp(0) before perp(2).
        let p0 = conv::position_from_capnp(list.get(0)).unwrap();
        let p1 = conv::position_from_capnp(list.get(1)).unwrap();
        assert_eq!(p0.market, DerivId::perp(0));
        assert_eq!(p0.qty, 100);
        assert_eq!(p1.market, DerivId::perp(2));
        assert_eq!(p1.qty, 200);
    });
}

#[test]
fn get_margin_account_defaults_to_zero_when_unfunded() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let alice = Address([1; 20]);
    let funded = Address([2; 20]);
    {
        let mas = MarginAccounts::new(&db);
        mas.put(&MarginAccount {
            owner: funded,
            collateral: 10_000_000,
            unrealized_pnl: Signed::from_i128(-2_500),
            bad_debt: 0,
        });
    }

    run_local(|| async move {
        let cap = make_server(db);
        // Funded address.
        let mut req = cap.get_margin_account_request();
        conv::address_to_capnp(req.get().init_addr(), &funded);
        let resp = req.send().promise.await.unwrap();
        let m = conv::margin_account_from_capnp(resp.get().unwrap().get_margin().unwrap()).unwrap();
        assert_eq!(m.collateral, 10_000_000);
        assert_eq!(m.unrealized_pnl, Signed::from_i128(-2_500));

        // Unfunded address gets a zero margin account.
        let mut req = cap.get_margin_account_request();
        conv::address_to_capnp(req.get().init_addr(), &alice);
        let resp = req.send().promise.await.unwrap();
        let m = conv::margin_account_from_capnp(resp.get().unwrap().get_margin().unwrap()).unwrap();
        assert_eq!(m.collateral, 0);
        assert_eq!(m.bad_debt, 0);
    });
}

#[test]
fn get_funding_index_defaults_to_zero_when_market_has_no_history() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market_a = DerivId::perp(0);
    let market_b = DerivId::perp(1);
    {
        FundingIndices::new(&db).put(&FundingIndex {
            market: market_a,
            cumulative: Signed::from_i128(98_765),
            last_tick_height: 7_200,
            rate_q9: 250,
        });
    }

    run_local(|| async move {
        let cap = make_server(db);

        let mut req = cap.get_funding_index_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &market_a);
        let resp = req.send().promise.await.unwrap();
        let f = conv::funding_index_from_capnp(resp.get().unwrap().get_index().unwrap()).unwrap();
        assert_eq!(f.cumulative, Signed::from_i128(98_765));
        assert_eq!(f.rate_q9, 250);

        let mut req = cap.get_funding_index_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &market_b);
        let resp = req.send().promise.await.unwrap();
        let f = conv::funding_index_from_capnp(resp.get().unwrap().get_index().unwrap()).unwrap();
        assert_eq!(f.market, market_b);
        assert_eq!(f.cumulative, Signed::ZERO);
        assert_eq!(f.rate_q9, 0);
    });
}

#[test]
fn get_oracle_and_mark_round_trip_or_error_when_missing() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market = DerivId::perp(0);
    {
        let os = OracleState::new(&db);
        os.put_oracle(&OraclePrice {
            market,
            px_q8: 50_000 * 100_000_000,
            source_height: 1234,
            authority: Address([7; 20]),
        });
        os.put_mark(&MarkPrice {
            market,
            px_q8: 50_010 * 100_000_000,
            computed_at_height: 1234,
            premium_ema_q9: 12_345,
        });
    }
    let _ = ORACLE_KIND_ORACLE; // keep import live in case cb-state changes shape.

    run_local(|| async move {
        let cap = make_server(db);

        let mut req = cap.get_oracle_price_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &market);
        let resp = req.send().promise.await.unwrap();
        let o = conv::oracle_price_from_capnp(resp.get().unwrap().get_oracle().unwrap()).unwrap();
        assert_eq!(o.px_q8, 50_000 * 100_000_000);
        assert_eq!(o.source_height, 1234);

        let mut req = cap.get_mark_price_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &market);
        let resp = req.send().promise.await.unwrap();
        let m = conv::mark_price_from_capnp(resp.get().unwrap().get_mark().unwrap()).unwrap();
        assert_eq!(m.px_q8, 50_010 * 100_000_000);

        // Missing market => RPC error (caller can decide whether to surface or
        // hide). Asserting error here pins the contract.
        let mut req = cap.get_oracle_price_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::perp(99));
        assert!(req.send().promise.await.is_err());
    });
}

#[test]
fn perp_market_get_and_list_in_deriv_index_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    {
        let pms = PerpMarkets::new(&db);
        // Insert out of order to verify the response is canonical (DerivId byte order).
        pms.put(&sample_perp_market(2));
        pms.put(&sample_perp_market(0));
        pms.put(&sample_perp_market(1));
    }

    run_local(|| async move {
        let cap = make_server(db);

        // Single get.
        let mut req = cap.get_perp_market_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::perp(1));
        let resp = req.send().promise.await.unwrap();
        let m = conv::perp_market_from_capnp(resp.get().unwrap().get_perp().unwrap()).unwrap();
        assert_eq!(m.id, DerivId::perp(1));

        // List.
        let req = cap.list_perp_markets_request();
        let resp = req.send().promise.await.unwrap();
        let list = resp.get().unwrap().get_markets().unwrap();
        assert_eq!(list.len(), 3);
        let m0 = conv::perp_market_from_capnp(list.get(0)).unwrap();
        let m1 = conv::perp_market_from_capnp(list.get(1)).unwrap();
        let m2 = conv::perp_market_from_capnp(list.get(2)).unwrap();
        assert_eq!(m0.id, DerivId::perp(0));
        assert_eq!(m1.id, DerivId::perp(1));
        assert_eq!(m2.id, DerivId::perp(2));

        // Unknown market => RPC error.
        let mut req = cap.get_perp_market_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::perp(99));
        assert!(req.send().promise.await.is_err());
    });
}

#[test]
fn future_market_get_and_list_in_deriv_index_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    {
        let fms = FuturesMarkets::new(&db);
        fms.put(&sample_future_market(0, 1_000));
        fms.put(&sample_future_market(1, 2_000));
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_future_market_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::future(1));
        let resp = req.send().promise.await.unwrap();
        let m = conv::future_market_from_capnp(resp.get().unwrap().get_future().unwrap()).unwrap();
        assert_eq!(m.expiry_height, 2_000);

        let req = cap.list_future_markets_request();
        let resp = req.send().promise.await.unwrap();
        let list = resp.get().unwrap().get_markets().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            conv::future_market_from_capnp(list.get(0)).unwrap().id,
            DerivId::future(0)
        );
        assert_eq!(
            conv::future_market_from_capnp(list.get(1)).unwrap().id,
            DerivId::future(1)
        );
    });
}

#[test]
fn get_liquidation_queue_filters_by_market() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let m_a = DerivId::perp(0);
    let m_b = DerivId::perp(1);
    {
        let q = LiquidationQueue::new(&db);
        q.put(&LiquidationEntry {
            owner: Address([1; 20]),
            market: m_a,
            triggered_by: Address([9; 20]),
            triggered_at_height: 100,
            mark_at_trigger_q8: 50_000 * 100_000_000,
            seq: 0,
        });
        q.put(&LiquidationEntry {
            owner: Address([2; 20]),
            market: m_b,
            triggered_by: Address([9; 20]),
            triggered_at_height: 100,
            mark_at_trigger_q8: 0,
            seq: 1,
        });
        q.put(&LiquidationEntry {
            owner: Address([3; 20]),
            market: m_a,
            triggered_by: Address([9; 20]),
            triggered_at_height: 101,
            mark_at_trigger_q8: 0,
            seq: 0,
        });
    }
    let _ = CF_LIQUIDATION_QUEUE;

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_liquidation_queue_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &m_a);
        let resp = req.send().promise.await.unwrap();
        let list = resp.get().unwrap().get_entries().unwrap();
        assert_eq!(list.len(), 2);
        // Order is `(height, seq)` ascending = lex byte order on the BE keys.
        let a = conv::liquidation_entry_from_capnp(list.get(0)).unwrap();
        let b = conv::liquidation_entry_from_capnp(list.get(1)).unwrap();
        assert_eq!(a.triggered_at_height, 100);
        assert_eq!(b.triggered_at_height, 101);
    });
}

#[test]
fn get_perp_order_book_aggregates_levels_with_correct_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market = DerivId::perp(0);

    // Insert resting orders directly into the CFs; bypass the executor since
    // we're testing the read surface.
    let orders = PerpOrders::new(&db);
    let book = PerpBookIndex::new(&db);
    let mk_order = |oid: u8, side: Side, px: u128, qty: u128, seq: u64| PerpOrder {
        order_id: H256([oid; 32]),
        owner: Address([oid; 20]),
        market,
        side,
        price_q8: px,
        qty,
        remaining_qty: qty,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: false,
        seq,
        placed_at_height: 1,
        client_order_id: H256([0; 32]),
    };
    let to_insert = [
        mk_order(1, Side::Buy, 100, 10, 1),
        mk_order(2, Side::Buy, 100, 5, 2), // same price
        mk_order(3, Side::Buy, 99, 7, 3),
        mk_order(4, Side::Sell, 101, 20, 4),
        mk_order(5, Side::Sell, 102, 30, 5),
    ];
    for o in &to_insert {
        orders.put(o);
        book.put(&market, o.side, o.price_q8, o.seq, &o.order_id);
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_perp_order_book_request();
        let mut p = req.get();
        conv::deriv_id_to_capnp(p.reborrow().init_market(), &market);
        p.set_depth(10);
        let resp = req.send().promise.await.unwrap();
        let snap = conv::perp_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert_eq!(snap.market, market);
        // Bids: best (highest) first; price=100 aggregates qty 15.
        assert_eq!(snap.bids.len(), 2);
        assert_eq!(snap.bids[0].price_q8, 100);
        assert_eq!(snap.bids[0].qty, 15);
        assert_eq!(snap.bids[1].price_q8, 99);
        // Asks: best (lowest) first.
        assert_eq!(snap.asks.len(), 2);
        assert_eq!(snap.asks[0].price_q8, 101);
        assert_eq!(snap.asks[0].qty, 20);
        assert_eq!(snap.asks[1].price_q8, 102);
    });
}

#[test]
fn get_perp_order_book_respects_depth_cap() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let market = DerivId::perp(0);
    let orders = PerpOrders::new(&db);
    let book = PerpBookIndex::new(&db);
    for i in 0..5u8 {
        let o = PerpOrder {
            order_id: H256([i + 1; 32]),
            owner: Address([1; 20]),
            market,
            side: Side::Sell,
            price_q8: 100 + i as u128,
            qty: 10,
            remaining_qty: 10,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            seq: i as u64,
            placed_at_height: 1,
            client_order_id: H256([0; 32]),
        };
        orders.put(&o);
        book.put(&market, o.side, o.price_q8, o.seq, &o.order_id);
    }

    run_local(|| async move {
        let cap = make_server(db);
        let mut req = cap.get_perp_order_book_request();
        let mut p = req.get();
        conv::deriv_id_to_capnp(p.reborrow().init_market(), &market);
        p.set_depth(2);
        let resp = req.send().promise.await.unwrap();
        let snap = conv::perp_book_snapshot_from_capnp(resp.get().unwrap().get_snapshot().unwrap())
            .unwrap();
        assert_eq!(snap.asks.len(), 2);
        assert_eq!(snap.asks[0].price_q8, 100);
        assert_eq!(snap.asks[1].price_q8, 101);
    });
}
