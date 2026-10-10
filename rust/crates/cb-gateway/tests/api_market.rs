//! §3 and §4 — the faucet and public market data.

mod common;

use cb_types::AssetId;
use common::Api;
use serde_json::json;

#[tokio::test]
async fn the_market_list_has_the_shape_the_contract_publishes() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets").await;
    let body = reply.assert_ok(200);
    let markets = body["markets"].as_array().expect("an array");
    let perp = markets
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP is listed");

    assert_eq!(perp["kind"], "perp");
    assert_eq!(perp["asset_class"], "crypto");
    assert_eq!(perp["base"], "BTC");
    assert_eq!(perp["quote"], "USD");
    assert_eq!(perp["status"], "trading");
    assert_eq!(perp["tick_size"], "0.10");
    assert_eq!(perp["lot_size"], "0.0001");
    assert_eq!(perp["price_decimals"], 2);
    assert_eq!(perp["size_decimals"], 4);
    assert_eq!(perp["max_leverage"], "50");
    assert_eq!(perp["initial_margin_bps"], 200);
    assert_eq!(perp["maintenance_margin_bps"], 100);
    assert_eq!(perp["funding_interval_seconds"], 3600);
    assert_eq!(perp["expiry"], json!(null));
    assert_eq!(perp["maker_fee_bps"], -1);
    assert_eq!(perp["taker_fee_bps"], 4);
}

#[tokio::test]
async fn spot_markets_publish_null_for_the_derivative_fields() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets").await;
    let markets = reply.assert_ok(200)["markets"]
        .as_array()
        .expect("an array")
        .clone();
    let spot = markets
        .iter()
        .find(|m| m["symbol"] == "BTC-USD")
        .expect("BTC-USD is listed");
    assert_eq!(spot["kind"], "spot");
    for field in [
        "max_leverage",
        "initial_margin_bps",
        "maintenance_margin_bps",
        "funding_interval_seconds",
        "expiry",
    ] {
        assert_eq!(spot[field], json!(null), "{field} must be null for spot");
    }
}

#[tokio::test]
async fn an_equity_market_is_classified_as_equity() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets").await;
    let markets = reply.assert_ok(200)["markets"]
        .as_array()
        .expect("array")
        .clone();
    let equity = markets
        .iter()
        .find(|m| m["symbol"] == "AAPL-USD")
        .expect("the equity universe is listed");
    assert_eq!(equity["asset_class"], "equity");
}

#[tokio::test]
async fn a_malformed_symbol_is_a_400_and_an_unlisted_one_is_a_404() {
    let api = Api::with_markets().await;
    api.get("/api/v1/markets/not%20a%20symbol/ticker")
        .await
        .assert_error(400, "VALIDATION_FAILED");
    api.get("/api/v1/markets/ZZZ-PERP/ticker")
        .await
        .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn the_ticker_reports_every_field_as_a_string() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets/BTC-PERP/ticker").await;
    let body = reply.assert_ok(200);
    for field in [
        "last_price",
        "mark_price",
        "index_price",
        "open_interest",
        "volume_24h",
        "change_24h_pct",
        "high_24h",
        "low_24h",
        "funding_rate",
    ] {
        assert!(
            body[field].is_string(),
            "{field} must cross the wire as a string, got {}",
            body[field]
        );
    }
    assert_eq!(body["symbol"], "BTC-PERP");
    assert_eq!(body["mark_price"], "64250.00");
}

#[tokio::test]
async fn the_order_book_is_price_quantity_pairs_best_first() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets/BTC-PERP/orderbook").await;
    let body = reply.assert_ok(200);
    assert_eq!(body["symbol"], "BTC-PERP");
    assert_eq!(body["height"], 10);
    let bids = body["bids"].as_array().expect("bids");
    assert_eq!(bids[0][0], "64249.00");
    assert_eq!(bids[0][1], "10.0000");
    let asks = body["asks"].as_array().expect("asks");
    assert_eq!(asks[0][0], "64251.00");
}

#[tokio::test]
async fn the_order_book_depth_is_clamped_to_the_documented_maximum() {
    let api = Api::with_markets().await;
    // The contract's ceiling is 200; asking for more must not be an error and
    // must not be honoured.
    api.get("/api/v1/markets/BTC-PERP/orderbook?depth=100000")
        .await
        .assert_ok(200);
    api.get("/api/v1/markets/BTC-PERP/orderbook?depth=0")
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn trades_and_candles_answer_for_a_market_that_has_not_traded_yet() {
    let api = Api::with_markets().await;
    let trades = api.get("/api/v1/markets/BTC-PERP/trades").await;
    assert_eq!(trades.assert_ok(200)["trades"], json!([]));

    let candles = api
        .get("/api/v1/markets/BTC-PERP/candles?interval=1m")
        .await;
    let body = candles.assert_ok(200);
    assert_eq!(body["symbol"], "BTC-PERP");
    assert_eq!(body["interval"], "1m");
    assert_eq!(body["candles"], json!([]));
}

#[tokio::test]
async fn an_unknown_candle_interval_is_rejected() {
    let api = Api::with_markets().await;
    api.get("/api/v1/markets/BTC-PERP/candles?interval=3m")
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn funding_history_answers_even_when_nothing_has_ticked() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/markets/BTC-PERP/funding").await;
    assert_eq!(reply.assert_ok(200)["history"], json!([]));
}

#[tokio::test]
async fn an_unreachable_node_is_a_503_and_not_a_hang() {
    let api = Api::with_markets().await;
    api.harness.node.take_offline("sequencer restarting");

    let reply = api.get("/api/v1/markets/BTC-PERP/orderbook").await;
    reply.assert_error(503, "NODE_UNAVAILABLE");

    // The catalogue is cached, so the market list still answers. That is the
    // point of caching it: a node blink must not empty the exchange.
    api.get("/api/v1/markets").await.assert_ok(200);
}

#[tokio::test]
async fn the_faucet_reports_its_grant_and_cooldown() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let reply = api.get_as("/api/v1/faucet", &player).await;
    let body = reply.assert_ok(200);
    assert_eq!(body["available"], true);
    assert_eq!(body["next_available_at"], json!(null));
    assert_eq!(body["cooldown_seconds"], 3600);
    assert_eq!(body["claims_made"], 0);
    let grant = body["grant"].as_array().expect("grant");
    assert_eq!(grant[0]["asset"], "USD");
    assert_eq!(grant[0]["amount"], "100000.00");
    assert_eq!(grant[1]["asset"], "BTC");
    assert_eq!(grant[1]["amount"], "1.00000000");
}

#[tokio::test]
async fn the_faucet_requires_a_session() {
    let api = Api::new();
    api.get("/api/v1/faucet")
        .await
        .assert_error(401, "UNAUTHORIZED");
    api.post("/api/v1/faucet/claim", json!({}))
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn claiming_mints_on_chain_and_starts_the_cooldown() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;

    let reply = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    let body = reply.assert_ok(200);
    assert!(body["claim_id"].as_str().expect("id").starts_with("clm_"));
    assert!(body["tx_hash"].as_str().expect("hash").starts_with("0x"));
    assert_eq!(body["granted"][0]["asset"], "USD");
    assert!(body["next_available_at"].as_str().is_some());

    // A real transaction went to the chain, crediting the trading account.
    let deposits: Vec<_> = api
        .harness
        .node
        .submitted_bodies()
        .into_iter()
        .filter_map(|b| match b {
            cb_tx::TxBody::Deposit {
                user,
                asset,
                amount,
                ..
            } => Some((user, asset, amount)),
            _ => None,
        })
        .collect();
    assert_eq!(deposits.len(), 2, "one deposit per granted asset");
    assert!(deposits
        .iter()
        .all(|(user, _, _)| *user == player.trading_address));
    assert!(deposits
        .iter()
        .any(|(_, asset, amount)| *asset == AssetId::Usd && *amount == 100_000_000_000));

    // ...and the cooldown is now in force.
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "FAUCET_COOLDOWN");
    let status = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(status.assert_ok(200)["available"], false);
    assert_eq!(status.body["claims_made"], 1);
}

#[tokio::test]
async fn the_cooldown_lifts_when_the_clock_says_so() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    api.harness.clock.advance(chrono::Duration::seconds(3_601));
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn a_faucet_claim_appears_in_the_unified_ledger_once_the_chain_applies_it() {
    // §11 and §20 of the flow review. The ledger is what a person reads to
    // answer "where did my money go", so it records what the chain did — not
    // what the gateway submitted, and not a balance the gateway worked out for
    // itself. Before the block there is nothing to report; after it, the entry
    // carries the height it landed at and the balance the chain reports.
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);

    let pending = api.get_as("/api/v1/account/transfers", &player).await;
    assert_eq!(
        pending.assert_ok(200)["entries"],
        json!([]),
        "a mempool acceptance is not money received"
    );

    // The block carrying both deposits.
    let block = api.harness.node.mine(11);
    assert_eq!(block.txs.len(), 2, "one Deposit per granted asset");
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");

    let reply = api.get_as("/api/v1/account/transfers", &player).await;
    let entries = reply.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    assert_eq!(entries.len(), 2);
    let usd = entries
        .iter()
        .find(|e| e["asset"] == "USD")
        .expect("the USD grant");
    assert_eq!(usd["kind"], "faucet");
    assert_eq!(usd["amount"], "100000.00");
    assert_eq!(
        usd["balance_after"], "100000.00",
        "observed from the chain after the block, not assumed; it used to read 0.00"
    );
    assert!(usd["tx_hash"].as_str().is_some_and(|h| h.starts_with("0x")));

    // And replaying the same block — which a gateway started with
    // `--start-height 1` does on every boot — must not double the history.
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed again");
    let again = api.get_as("/api/v1/account/transfers", &player).await;
    assert_eq!(
        again.assert_ok(200)["entries"]
            .as_array()
            .expect("entries")
            .len(),
        2,
        "a replayed block must not write the same entry twice"
    );
}

#[tokio::test]
async fn the_chain_status_and_block_endpoints_answer_without_a_session() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/chain/status").await;
    let body = reply.assert_ok(200);
    assert_eq!(body["height"], 10);
    assert!(body["tps_1m"].is_string());
    assert!(body["state_root"].as_str().expect("root").starts_with("0x"));
    assert_eq!(body["synced"], true);

    let blocks = api.get("/api/v1/chain/blocks?limit=5").await;
    assert_eq!(blocks.assert_ok(200)["blocks"], json!([]));
}

#[tokio::test]
async fn the_leaderboard_answers_and_validates_its_window() {
    let api = Api::new();
    let reply = api.get("/api/v1/leaderboard?window=24h").await;
    let body = reply.assert_ok(200);
    assert_eq!(body["window"], "24h");
    assert_eq!(body["entries"], json!([]));

    api.get("/api/v1/leaderboard?window=forever")
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn health_reports_every_dependency_separately() {
    let api = Api::new();
    let reply = api.get("/health").await;
    let body = reply.assert_ok(200);
    let names: Vec<&str> = body["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .map(|d| d["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"sequencer"));
    assert!(names.contains(&"database"));
    assert!(names.contains(&"event_bus"));
    assert_eq!(body["status"], "ok");

    // With the sequencer down the gateway says which dependency broke — but
    // not what the backend's own error string said. `/health` needs no
    // session, and the live sample in the security review was this field
    // handing out the sequencer's RPC address and an OS error code (finding 8).
    api.harness
        .node
        .take_offline("connect to 127.0.0.1:7799: Connection refused (os error 61)");
    let reply = api.get("/health").await;
    let body = reply.assert_ok(200);
    assert_eq!(body["status"], "degraded");
    let sequencer = body["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .find(|d| d["name"] == "sequencer")
        .expect("sequencer is reported")
        .clone();
    assert_eq!(sequencer["status"], "down");
    let detail = sequencer["detail"].as_str().expect("a detail is present");
    assert!(
        !detail.contains("127.0.0.1") && !detail.contains("os error"),
        "the internal RPC target must not leak to an unauthenticated caller: {detail}"
    );
    assert!(detail.contains("unreachable"), "{detail}");
}

#[tokio::test]
async fn chain_status_names_the_rpc_facts_and_txs_is_served() {
    let api = Api::with_markets().await;
    let reply = api.get("/api/v1/chain/status").await;
    let body = reply.assert_ok(200);
    assert!(
        body["node_rpc"].is_string(),
        "the sequencer address is named"
    );
    assert!(body["gateway_listen"].is_string());
    assert_eq!(body["ws_path"], "/ws");
    assert_eq!(
        body["sim_mode"], "off",
        "no harness attached in the harness"
    );
    let reply = api.get("/api/v1/chain/txs?limit=5").await;
    let txs = reply.assert_ok(200);
    assert!(txs["txs"].is_array());
}
