//! Integration test for the M14 dashboard `/api/perps/*` endpoints.
//!
//! Builds the axum router from `cb_dashboard::server::build_router`, drives
//! it via `tower::ServiceExt::oneshot` (no listener), and asserts that each
//! endpoint emits the expected JSON shape against a seeded RocksDB.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use cb_dashboard::server::{build_router, PruningInfo};
use cb_dashboard::Metrics;
use cb_state::{
    Database, FundingIndices, LiquidationQueue, MarginAccounts, Meta, OracleState, PerpMarkets,
    Positions,
};
use cb_types::{
    Address, AssetId, DerivId, FundingIndex, LiquidationEntry, MarginAccount, MarkPrice,
    OraclePrice, PerpMarket, Position, Side, Signed, H256,
};
use http_body_util::BodyExt;
use std::sync::Arc;
use tempfile::tempdir;
use tower::ServiceExt;

/// Spin up a Database, seed one perp market with funding/oracle/mark/position/
/// liquidation/insurance state, and return the populated DB. The caller drives
/// the router against the same DB.
fn seeded_db() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);

    let m = PerpMarket {
        id: DerivId::perp(0),
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
    };
    PerpMarkets::new(&db).put(&m);

    Meta::new(&db).set_head(50, H256::ZERO, H256::ZERO);

    FundingIndices::new(&db).put(&FundingIndex {
        market: m.id,
        cumulative: Signed::ZERO,
        last_tick_height: 40,
        rate_q9: 1234,
    });

    OracleState::new(&db).put_oracle(&OraclePrice {
        market: m.id,
        px_q8: 50_000 * 100_000_000,
        source_height: 49,
        authority: Address([7; 20]),
    });
    OracleState::new(&db).put_mark(&MarkPrice {
        market: m.id,
        px_q8: 50_500 * 100_000_000, // long is in profit vs entry below
        computed_at_height: 50,
        premium_ema_q9: 100,
    });

    let owner = Address([1; 20]);
    Positions::new(&db).put(&Position {
        owner,
        market: m.id,
        side: Side::Buy,
        qty: 100_000_000,
        entry_funding_index: Signed::ZERO,
        entry_px_q8: 50_000 * 100_000_000,
        margin_mode: 0,
        isolated_margin: 0,
        last_funding_height: 0,
    });
    MarginAccounts::new(&db).put(&MarginAccount {
        owner,
        collateral: 5_000_000,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    LiquidationQueue::new(&db).put(&LiquidationEntry {
        owner,
        market: m.id,
        triggered_by: Address([2; 20]),
        triggered_at_height: 49,
        mark_at_trigger_q8: 50_400 * 100_000_000,
        seq: 0,
    });

    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: 1_000_000_000,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    (dir, db)
}

async fn fetch_json(db: Database, path: &str) -> serde_json::Value {
    let app = build_router(
        db,
        Arc::new(Metrics::new()),
        PruningInfo {
            enabled: false,
            keep_recent: 0,
        },
    );
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "expected 200 from {path}"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response body must be JSON")
}

#[tokio::test]
async fn api_perps_funding_returns_seeded_market() {
    let (_dir, db) = seeded_db();
    let body = fetch_json(db, "/api/perps/funding").await;
    let arr = body.as_array().expect("funding endpoint returns array");
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["market"], "perp:0");
    assert_eq!(row["rate_q9"], 1234);
    assert_eq!(row["last_tick_height"], 40);
    assert_eq!(row["next_tick_height"], 40 + 3_600);
    assert_eq!(row["interval_blocks"], 3_600);
    assert_eq!(row["cap_q9"], 1_000_000);
}

#[tokio::test]
async fn api_perps_positions_returns_top_position_with_pnl() {
    let (_dir, db) = seeded_db();
    let body = fetch_json(db, "/api/perps/positions").await;
    let arr = body.as_array().expect("positions endpoint returns array");
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["market"], "perp:0");
    assert_eq!(row["side"], "long");
    // Owner truncated in renderer, but JSON keeps full form.
    assert!(row["owner"].as_str().unwrap().starts_with("0x"));
    // One BTC long from 50,000 marked at 50,500: $500 of profit, which is
    // 500,000,000 micro-USD. Pinned exactly: this test used to check only
    // `> 0`, and passed while the dashboard showed the gain as $50 billion.
    let abs: u128 = row["abs_unrealized_pnl"].as_str().unwrap().parse().unwrap();
    assert_eq!(abs, 500_000_000, "long should be $500 up when mark > entry");
    assert_eq!(row["unrealized_pnl"], "500000000");
    // im_util_bps populated for cross positions.
    assert!(row["im_util_bps"].is_number());
}

#[tokio::test]
async fn api_perps_oracle_returns_freshness_metadata() {
    let (_dir, db) = seeded_db();
    let body = fetch_json(db, "/api/perps/oracle").await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["market"], "perp:0");
    assert_eq!(row["source_height"], 49);
    assert_eq!(row["blocks_since_update"], 1);
    assert_eq!(row["stale"], false);
}

#[tokio::test]
async fn api_perps_liquidations_returns_seeded_event() {
    let (_dir, db) = seeded_db();
    let body = fetch_json(db, "/api/perps/liquidations").await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["triggered_at_height"], 49);
    assert_eq!(row["seq"], 0);
    assert_eq!(row["market"], "perp:0");
}

#[tokio::test]
async fn api_perps_insurance_returns_balance_and_sparkline() {
    let (_dir, db) = seeded_db();
    let body = fetch_json(db, "/api/perps/insurance").await;
    assert_eq!(body["collateral"], "1000000000");
    assert_eq!(body["bad_debt"], "0");
    let sparkline = body["sparkline"].as_array().unwrap();
    let heights = body["sparkline_heights"].as_array().unwrap();
    assert_eq!(sparkline.len(), heights.len());
    // head_height = 50 < INSURANCE_SPARKLINE_BLOCKS (100), so window == 50.
    assert_eq!(sparkline.len(), 50);
    assert_eq!(heights.first().unwrap(), 1);
    assert_eq!(heights.last().unwrap(), 50);
}

#[tokio::test]
async fn api_perps_endpoints_return_empty_collections_on_fresh_db() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let app = build_router(
        db,
        Arc::new(Metrics::new()),
        PruningInfo {
            enabled: false,
            keep_recent: 0,
        },
    );
    for path in [
        "/api/perps/funding",
        "/api/perps/positions",
        "/api/perps/oracle",
        "/api/perps/liquidations",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body.as_array().expect("array").len(),
            0,
            "{} should be empty on fresh db",
            path
        );
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/perps/insurance")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["collateral"], "0");
    assert_eq!(body["bad_debt"], "0");
}
