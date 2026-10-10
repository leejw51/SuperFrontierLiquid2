//! §10 — the operator surface.
//!
//! Two properties matter more than any individual field here: the role gate
//! must be enforced server-side on every request, and every mutating action
//! must leave an audit record naming the operator.

mod common;

use cb_gateway::testing::Harness;
use cb_types::AssetId;
use common::Api;
use serde_json::json;

/// A gateway where `operator` holds the admin role, with one market listed.
async fn with_operator() -> (
    Api,
    cb_gateway::testing::Player,
    cb_gateway::testing::Player,
) {
    let api = Api::from(Harness::builder().admins(&["operator"]).build());
    api.list_btc_perp().await;
    let operator = api.player("operator", 0).await;
    let player = api.player("satoshi", 1).await;
    (api, operator, player)
}

/// A gateway whose operator is named by **address**, which is how a real
/// deployment is configured.
async fn with_operator_address() -> (
    Api,
    cb_gateway::testing::Player,
    cb_gateway::testing::Player,
) {
    let (_secret, operator_address) =
        cb_gateway::auth::eth::derive_from_mnemonic(Harness::MNEMONIC, 0).expect("derive");
    let api = Api::from(
        Harness::builder()
            .admin_addresses(&[operator_address])
            .build(),
    );
    api.list_btc_perp().await;
    let operator = api.player("operator", 0).await;
    let stranger = api.player("satoshi", 1).await;
    (api, operator, stranger)
}

#[tokio::test]
async fn claiming_the_handle_admin_does_not_make_you_the_operator() {
    // Finding 1: the operator role used to be granted by comparing a
    // self-chosen handle, and handles are first-come. A stranger who registers
    // `admin` on a fresh store must get nothing.
    let (api, operator, _stranger) = with_operator_address().await;
    let squatter = api.player("admin", 5).await;

    let session = api.get_as("/api/v1/auth/session", &squatter).await;
    assert_eq!(
        session.assert_ok(200)["roles"],
        json!(["user"]),
        "registering the handle `admin` must not grant the operator role"
    );
    api.get_as("/api/v1/admin/users", &squatter)
        .await
        .assert_error(403, "FORBIDDEN");
    api.post_as(
        "/api/v1/admin/faucet/grant",
        &squatter,
        json!({ "user_id": operator.user_id, "asset": "USD", "amount": "1000" }),
    )
    .await
    .assert_error(403, "FORBIDDEN");

    // And the configured address holds it, whatever handle it goes by.
    api.get_as("/api/v1/admin/users", &operator)
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn renaming_into_an_operator_handle_grants_nothing() {
    // The escalation-by-rename half of finding 1: the `Admin` extractor
    // recomputes the role on every request, so with a handle binding a rename
    // used to promote an existing session in place.
    let (api, _operator, stranger) = with_operator_address().await;
    api.get_as("/api/v1/admin/users", &stranger)
        .await
        .assert_error(403, "FORBIDDEN");

    let renamed = api
        .patch_as(
            "/api/v1/user/profile",
            &stranger,
            json!({ "handle": "admin" }),
        )
        .await;
    renamed.assert_ok(200);

    // Same session token, still not an operator.
    api.get_as("/api/v1/admin/users", &stranger)
        .await
        .assert_error(403, "FORBIDDEN");
    assert_eq!(
        api.get_as("/api/v1/auth/session", &stranger).await.body["roles"],
        json!(["user"])
    );
}

#[tokio::test]
async fn every_admin_endpoint_refuses_a_non_operator() {
    let (api, _operator, player) = with_operator().await;
    for path in [
        "/api/v1/admin/overview",
        "/api/v1/admin/users",
        "/api/v1/admin/markets",
        "/api/v1/admin/faucet",
        "/api/v1/admin/audit",
        "/api/v1/admin/sim",
    ] {
        let reply = api.get_as(path, &player).await;
        reply.assert_error(403, "FORBIDDEN");
        assert_eq!(
            reply.body["error"]["message"], "not permitted",
            "{path} must not reveal that it exists for somebody else"
        );
    }
}

#[tokio::test]
async fn admin_endpoints_also_refuse_an_anonymous_caller() {
    let (api, _operator, _player) = with_operator().await;
    api.get("/api/v1/admin/overview")
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn the_operator_role_appears_in_the_session_and_the_profile() {
    let (api, operator, player) = with_operator().await;
    let session = api.get_as("/api/v1/auth/session", &operator).await;
    assert_eq!(session.assert_ok(200)["roles"], json!(["user", "admin"]));

    let theirs = api.get_as("/api/v1/auth/session", &player).await;
    assert_eq!(theirs.assert_ok(200)["roles"], json!(["user"]));
}

#[tokio::test]
async fn the_overview_paints_the_whole_dashboard_in_one_call() {
    let (api, operator, _player) = with_operator().await;
    let reply = api.get_as("/api/v1/admin/overview", &operator).await;
    let body = reply.assert_ok(200);

    for section in [
        "chain", "users", "markets", "volume", "faucet", "risk", "health",
    ] {
        assert!(!body[section].is_null(), "{section} must be present");
    }
    assert_eq!(body["chain"]["height"], 10);
    assert_eq!(body["users"]["total"], 2);
    assert_eq!(body["users"]["humans"], 2);
    assert!(body["markets"]["total"].as_u64().expect("count") > 0);
    assert_eq!(body["markets"]["halted"], 0);
    assert!(body["volume"]["volume_24h"].is_string());
    assert_eq!(body["risk"]["accounts_below_maintenance"], 0);

    let components: Vec<&str> = body["health"]
        .as_array()
        .expect("health")
        .iter()
        .map(|c| c["component"].as_str().expect("name"))
        .collect();
    assert!(components.contains(&"node"));
    assert!(components.contains(&"database"));
}

#[tokio::test]
async fn the_user_list_searches_by_handle_and_by_address() {
    let (api, operator, player) = with_operator().await;
    let all = api.get_as("/api/v1/admin/users", &operator).await;
    assert_eq!(
        all.assert_ok(200)["users"].as_array().expect("users").len(),
        2
    );

    let by_handle = api
        .get_as("/api/v1/admin/users?query=satoshi", &operator)
        .await;
    let found = by_handle.assert_ok(200)["users"]
        .as_array()
        .expect("users")
        .clone();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["handle"], "satoshi");
    assert_eq!(found[0]["status"], "active");

    let address = cb_gateway::http::format_address(&player.login_address);
    let by_address = api
        .get_as(&format!("/api/v1/admin/users?query={address}"), &operator)
        .await;
    assert_eq!(
        by_address.assert_ok(200)["users"]
            .as_array()
            .expect("users")
            .len(),
        1
    );
}

#[tokio::test]
async fn the_user_detail_shows_both_addresses_and_the_balances() {
    let (api, operator, player) = with_operator().await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);

    let reply = api
        .get_as(
            &format!("/api/v1/admin/users/{}", player.user_id),
            &operator,
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["handle"], "satoshi");
    assert_eq!(
        body["address"],
        json!(cb_gateway::http::format_address(&player.login_address))
    );
    assert_eq!(
        body["trading_address"],
        json!(cb_gateway::http::format_address(&player.trading_address))
    );
    assert_eq!(body["status"], "active");
    let usd = body["balances"]
        .as_array()
        .expect("balances")
        .iter()
        .find(|b| b["asset"] == "USD")
        .expect("USD")
        .clone();
    assert_eq!(usd["free"], "100000.00");
}

#[tokio::test]
async fn an_unknown_user_is_a_404() {
    let (api, operator, _player) = with_operator().await;
    api.get_as("/api/v1/admin/users/usr_nobody", &operator)
        .await
        .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn suspending_an_account_stops_it_trading_and_is_audited() {
    let (api, operator, player) = with_operator().await;

    let reply = api
        .post_as(
            &format!("/api/v1/admin/users/{}/suspend", player.user_id),
            &operator,
            json!({ "reason": "abusive order flow" }),
        )
        .await;
    assert_eq!(reply.status.as_u16(), 204);

    // Every *action* from that account is now refused: placing, cancelling,
    // transferring, claiming.
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(403, "ACCOUNT_SUSPENDED");
    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.01000000"
        }),
    )
    .await
    .assert_error(403, "ACCOUNT_SUSPENDED");

    // Its reads are not. The contract leaves a suspended account's positions
    // open, so it is still carrying risk — a position that can be liquidated,
    // collateral that moves with the mark — and refusing the reads as well left
    // somebody holding exposure they were not allowed to look at.
    api.get_as("/api/v1/account", &player).await.assert_ok(200);
    api.get_as("/api/v1/account/positions", &player)
        .await
        .assert_ok(200);
    api.get_as("/api/v1/account/orders?status=open", &player)
        .await
        .assert_ok(200);

    // The operator's own view shows why.
    let detail = api
        .get_as(
            &format!("/api/v1/admin/users/{}", player.user_id),
            &operator,
        )
        .await;
    let body = detail.assert_ok(200);
    assert_eq!(body["status"], "suspended");
    assert_eq!(body["suspension_reason"], "abusive order flow");

    // ...and the action left a trace naming the operator.
    let audit = api.get_as("/api/v1/admin/audit", &operator).await;
    let records = audit.assert_ok(200)["records"]
        .as_array()
        .expect("records")
        .clone();
    let record = records
        .iter()
        .find(|r| r["action"] == "user.suspend")
        .expect("the suspension is audited");
    assert_eq!(
        record["actor"],
        json!(cb_gateway::http::format_address(&operator.login_address))
    );
    assert_eq!(record["subject"], json!(player.user_id));
    assert_eq!(record["detail"]["reason"], "abusive order flow");
}

#[tokio::test]
async fn a_suspension_needs_a_reason_and_can_be_lifted() {
    let (api, operator, player) = with_operator().await;
    api.post_as(
        &format!("/api/v1/admin/users/{}/suspend", player.user_id),
        &operator,
        json!({ "reason": "   " }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");

    api.post_as(
        &format!("/api/v1/admin/users/{}/suspend", player.user_id),
        &operator,
        json!({ "reason": "testing" }),
    )
    .await
    .assert_ok(204);

    let reply = api
        .post_as(
            &format!("/api/v1/admin/users/{}/unsuspend", player.user_id),
            &operator,
            json!({}),
        )
        .await;
    assert_eq!(reply.status.as_u16(), 204);
    api.get_as("/api/v1/account", &player).await.assert_ok(200);
}

#[tokio::test]
async fn the_market_list_shows_live_operational_state() {
    let (api, operator, _player) = with_operator().await;
    let reply = api.get_as("/api/v1/admin/markets", &operator).await;
    let markets = reply.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .clone();
    let perp = markets
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP");
    assert_eq!(perp["status"], "trading");
    assert_eq!(perp["overridden"], false);
    assert_eq!(perp["resting_levels"], 2);
    assert!(perp["oracle_fresh"].is_boolean());
}

#[tokio::test]
async fn halting_a_market_stops_new_orders_but_still_permits_cancellation() {
    let (api, operator, player) = with_operator().await;
    api.collateralise(&player, 1_000_000_000_000);

    // Rest an order before the halt.
    let placed = api
        .post_as(
            "/api/v1/orders",
            &player,
            json!({
                "symbol": "BTC-PERP", "side": "buy", "type": "limit",
                "price": "64000.00", "qty": "1.0000"
            }),
        )
        .await;
    let order_id = placed.assert_ok(201)["order_id"]
        .as_str()
        .expect("id")
        .to_string();

    api.post_as(
        "/api/v1/admin/markets/BTC-PERP/halt",
        &operator,
        json!({ "reason": "oracle feed stale" }),
    )
    .await
    .assert_ok(204);

    // New orders are refused...
    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-PERP", "side": "buy", "type": "limit",
            "price": "64000.00", "qty": "1.0000"
        }),
    )
    .await
    .assert_error(422, "MARKET_HALTED");

    // ...and the resting order can still be got out of. Trapping somebody's
    // order with no way out would be worse than whatever caused the halt.
    let cancel = api
        .delete_as(&format!("/api/v1/orders/{order_id}"), &player)
        .await;
    assert_ne!(
        cancel.status.as_u16(),
        422,
        "cancellation must not be blocked by a halt: {}",
        cancel.body
    );
    api.post_as("/api/v1/orders/cancel-all", &player, json!({}))
        .await
        .assert_ok(200);

    // Resuming puts it back.
    api.post_as(
        "/api/v1/admin/markets/BTC-PERP/resume",
        &operator,
        json!({}),
    )
    .await
    .assert_ok(204);
    let markets = api.get("/api/v1/markets").await;
    let perp = markets.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP")
        .clone();
    assert_eq!(perp["status"], "trading");
}

#[tokio::test]
async fn a_halt_needs_a_reason_and_names_a_real_market() {
    let (api, operator, _player) = with_operator().await;
    api.post_as(
        "/api/v1/admin/markets/BTC-PERP/halt",
        &operator,
        json!({ "reason": "" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");

    api.post_as(
        "/api/v1/admin/markets/ZZZ-PERP/halt",
        &operator,
        json!({ "reason": "x" }),
    )
    .await
    .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn raising_margin_reports_how_many_accounts_it_just_broke() {
    let (api, operator, player) = with_operator().await;
    // A position that is comfortable at 1% maintenance and not at 50%.
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![cb_gateway::node::PositionSnapshot {
                market: cb_types::DerivId::perp(0),
                side: cb_types::Side::Buy,
                qty: 100_000_000,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
        state.margins.insert(
            player.trading_address,
            cb_gateway::node::MarginSnapshot {
                collateral: 1_000_000_000, // $1,000
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });

    let reply = api
        .patch_as(
            "/api/v1/admin/markets/BTC-PERP/risk",
            &operator,
            json!({ "max_leverage": "20", "initial_margin_bps": 5000, "maintenance_margin_bps": 5000 }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["applied"], true);
    assert_eq!(
        body["accounts_now_below_maintenance"], 1,
        "the operator must find out before the liquidation engine does"
    );
    let affected = body["affected"].as_array().expect("affected").clone();
    assert_eq!(affected.len(), 1);
    assert_eq!(affected[0]["user_id"], json!(player.user_id));
    assert_eq!(
        affected[0]["handle"], "satoshi",
        "a bare count does not say whom to warn"
    );
    // One BTC marked at $64,250 against $1,000 of equity: comfortable at the
    // market's 1% maintenance, far past the line at 50%.
    assert_eq!(affected[0]["margin_ratio_before"], "0.642");
    assert_eq!(affected[0]["margin_ratio_after"], "32.125");

    // The new parameters are what the market now publishes.
    let markets = api.get("/api/v1/markets").await;
    let perp = markets.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP")
        .clone();
    assert_eq!(perp["max_leverage"], "20");
    assert_eq!(perp["initial_margin_bps"], 5000);
}

#[tokio::test]
async fn a_risk_dry_run_computes_the_consequence_and_changes_nothing() {
    let (api, operator, player) = with_operator().await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![cb_gateway::node::PositionSnapshot {
                market: cb_types::DerivId::perp(0),
                side: cb_types::Side::Buy,
                qty: 100_000_000,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
        state.margins.insert(
            player.trading_address,
            cb_gateway::node::MarginSnapshot {
                collateral: 1_000_000_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });

    let dry = api
        .patch_as(
            "/api/v1/admin/markets/BTC-PERP/risk?dry_run=true",
            &operator,
            json!({ "maintenance_margin_bps": 5000 }),
        )
        .await;
    let body = dry.assert_ok(200);
    assert_eq!(body["applied"], false, "a dry run changes nothing");
    assert_eq!(body["accounts_now_below_maintenance"], 1);
    assert_eq!(body["affected"][0]["handle"], "satoshi");

    // The market is untouched...
    let markets = api.get("/api/v1/markets").await;
    let perp = markets.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP")
        .clone();
    assert_eq!(
        perp["maintenance_margin_bps"], 100,
        "a dry run must not move the market"
    );

    // ...and nothing was audited, because nothing happened.
    let audit = api
        .get_as("/api/v1/admin/audit?action=market.risk", &operator)
        .await;
    assert_eq!(
        audit.assert_ok(200)["records"]
            .as_array()
            .expect("records")
            .len(),
        0
    );

    // The real change reports the same consequence, and does audit.
    let applied = api
        .patch_as(
            "/api/v1/admin/markets/BTC-PERP/risk",
            &operator,
            json!({ "maintenance_margin_bps": 5000 }),
        )
        .await;
    let applied_body = applied.assert_ok(200);
    assert_eq!(applied_body["applied"], true);
    assert_eq!(
        applied_body["affected"], body["affected"],
        "the dry run must predict exactly what the change does"
    );
    let audit = api
        .get_as("/api/v1/admin/audit?action=market.risk", &operator)
        .await;
    assert_eq!(
        audit.assert_ok(200)["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_risk_change_that_breaks_nobody_reports_an_empty_list() {
    let (api, operator, _player) = with_operator().await;
    let reply = api
        .patch_as(
            "/api/v1/admin/markets/BTC-PERP/risk?dry_run=true",
            &operator,
            json!({ "maintenance_margin_bps": 9000 }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["accounts_now_below_maintenance"], 0);
    assert_eq!(body["affected"], json!([]));
}

#[tokio::test]
async fn nonsensical_risk_parameters_are_refused() {
    let (api, operator, _player) = with_operator().await;
    for body in [
        json!({ "initial_margin_bps": 0 }),
        json!({ "maintenance_margin_bps": 20000 }),
        json!({ "initial_margin_bps": 100, "maintenance_margin_bps": 500 }),
        json!({ "max_leverage": "0" }),
    ] {
        api.patch_as("/api/v1/admin/markets/BTC-PERP/risk", &operator, body)
            .await
            .assert_error(400, "VALIDATION_FAILED");
    }
    // Spot has no margin requirements at all.
    api.patch_as(
        "/api/v1/admin/markets/BTC-USD/risk",
        &operator,
        json!({ "initial_margin_bps": 500 }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn settling_a_future_requires_it_to_be_expired_or_halted() {
    let (api, operator, _player) = with_operator().await;
    api.list_btc_future(1_000_000).await;
    let symbol = api.get("/api/v1/markets").await.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["kind"] == "future")
        .expect("a future is listed")["symbol"]
        .as_str()
        .expect("symbol")
        .to_string();

    api.post_as(
        &format!("/api/v1/admin/markets/{symbol}/settle"),
        &operator,
        json!({ "settlement_price": "64250.00" }),
    )
    .await
    .assert_error(422, "MARKET_HALTED");

    // Halt it, then settle.
    api.post_as(
        &format!("/api/v1/admin/markets/{symbol}/halt"),
        &operator,
        json!({ "reason": "settling early" }),
    )
    .await
    .assert_ok(204);

    // A wild price is refused unless forced, because this is irreversible.
    let wild = api
        .post_as(
            &format!("/api/v1/admin/markets/{symbol}/settle"),
            &operator,
            json!({ "settlement_price": "1.00" }),
        )
        .await;
    wild.assert_error(400, "VALIDATION_FAILED");
    assert!(wild.body["error"]["details"]["deviation_bps"].is_number());

    // A sane price is validated and then refused, because this chain cannot
    // force-settle at all: `FuturesSettle` carries no price and is accepted
    // only from the block proposer. What it must never do is submit a
    // transaction it knows will fail and then audit a settlement that did not
    // happen (§18 of the flow review).
    let refused = api
        .post_as(
            &format!("/api/v1/admin/markets/{symbol}/settle"),
            &operator,
            json!({ "settlement_price": "64250.00" }),
        )
        .await;
    refused.assert_error(501, "NOT_SUPPORTED");
    assert!(
        !api.harness
            .node
            .submitted_bodies()
            .into_iter()
            .any(|b| matches!(b, cb_tx::TxBody::FuturesSettle { .. })),
        "nothing may be submitted for a settlement the chain will refuse"
    );

    // And the audit trail says refused, not settled.
    let audit = api
        .get_as("/api/v1/admin/audit?action=market.settle", &operator)
        .await;
    let records = audit.assert_ok(200)["records"]
        .as_array()
        .expect("records")
        .clone();
    let settle = records
        .iter()
        .find(|r| r["action"] == "market.settle")
        .expect("the attempt is audited");
    assert_eq!(settle["detail"]["outcome"], "refused");
}

#[tokio::test]
async fn a_future_with_no_mark_price_cannot_be_settled_at_all() {
    // Both guards around an irreversible action are computed from the mark: the
    // 10% band, and the force confirmation the UI hangs off it. On a chain
    // whose oracle has never posted, the mark is zero, and a deviation computed
    // from zero is not "0%" — it is nothing at all. The two safety devices fail
    // open together, exactly when the market is least understood.
    let (api, operator, _player) = with_operator().await;
    api.list_btc_future(1_000_000).await;
    let symbol = api.get("/api/v1/markets").await.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["kind"] == "future")
        .expect("a future is listed")["symbol"]
        .as_str()
        .expect("symbol")
        .to_string();
    api.post_as(
        &format!("/api/v1/admin/markets/{symbol}/halt"),
        &operator,
        json!({ "reason": "no oracle" }),
    )
    .await
    .assert_ok(204);

    api.harness.node.edit(|state| state.marks.clear());

    api.post_as(
        &format!("/api/v1/admin/markets/{symbol}/settle"),
        &operator,
        json!({ "settlement_price": "1.00" }),
    )
    .await
    .assert_error(422, "ORACLE_STALE");
}

#[tokio::test]
async fn a_spot_market_cannot_be_settled() {
    let (api, operator, _player) = with_operator().await;
    api.post_as(
        "/api/v1/admin/markets/BTC-USD/settle",
        &operator,
        json!({ "settlement_price": "64250.00" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn the_faucet_view_shows_what_is_left_to_give() {
    let (api, operator, _player) = with_operator().await;
    let reply = api.get_as("/api/v1/admin/faucet", &operator).await;
    let body = reply.assert_ok(200);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["cooldown_seconds"], 3600);
    assert_eq!(body["max_claims_per_user"], json!(null));
    assert!(!body["grant"].as_array().expect("grant").is_empty());
    assert!(body["authority_balance"].is_array());
}

#[tokio::test]
async fn the_faucet_can_be_reconfigured_and_closed() {
    let (api, operator, player) = with_operator().await;
    let reply = api
        .patch_as(
            "/api/v1/admin/faucet",
            &operator,
            json!({ "cooldown_seconds": 1800, "grant": [ { "asset": "USD", "amount": "250000.00" } ] }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["cooldown_seconds"], 1800);
    assert_eq!(body["grant"][0]["amount"], "250000.00");

    // A player now gets the new grant.
    let claim = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    assert_eq!(claim.assert_ok(200)["granted"][0]["amount"], "250000.00");

    // Closing it stops claims.
    api.patch_as(
        "/api/v1/admin/faucet",
        &operator,
        json!({ "enabled": false }),
    )
    .await
    .assert_ok(200);
    api.harness.clock.advance(chrono::Duration::hours(2));
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(403, "FORBIDDEN");
}

#[tokio::test]
async fn a_faucet_that_grants_nothing_is_refused() {
    let (api, operator, _player) = with_operator().await;
    api.patch_as("/api/v1/admin/faucet", &operator, json!({ "grant": [] }))
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_manual_grant_bypasses_the_cooldown_and_is_audited() {
    let (api, operator, player) = with_operator().await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    // On cooldown for a normal claim...
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "FAUCET_COOLDOWN");

    // ...but an operator may still grant.
    let reply = api
        .post_as(
            "/api/v1/admin/faucet/grant",
            &operator,
            json!({
                "user_id": player.user_id,
                "grants": [ { "asset": "USD", "amount": "1000000.00" } ],
                "reason": "load test"
            }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert!(body["claim_id"].as_str().expect("id").starts_with("clm_"));
    assert!(body["tx_hash"].as_str().expect("hash").starts_with("0x"));

    let audit = api
        .get_as("/api/v1/admin/audit?action=faucet.grant", &operator)
        .await;
    let records = audit.assert_ok(200)["records"]
        .as_array()
        .expect("records")
        .clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["detail"]["reason"], "load test");
}

#[tokio::test]
async fn a_manual_grant_needs_a_reason_and_a_real_user() {
    let (api, operator, player) = with_operator().await;
    api.post_as(
        "/api/v1/admin/faucet/grant",
        &operator,
        json!({ "user_id": player.user_id, "grants": [ { "asset": "USD", "amount": "1.00" } ], "reason": " " }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");

    api.post_as(
        "/api/v1/admin/faucet/grant",
        &operator,
        json!({ "user_id": "usr_nobody", "grants": [ { "asset": "USD", "amount": "1.00" } ], "reason": "x" }),
    )
    .await
    .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn the_audit_trail_filters_by_action_and_subject() {
    let (api, operator, player) = with_operator().await;
    api.post_as(
        "/api/v1/admin/markets/BTC-PERP/halt",
        &operator,
        json!({ "reason": "one" }),
    )
    .await
    .assert_ok(204);
    api.post_as(
        &format!("/api/v1/admin/users/{}/suspend", player.user_id),
        &operator,
        json!({ "reason": "two" }),
    )
    .await
    .assert_ok(204);

    let halts = api
        .get_as("/api/v1/admin/audit?action=market.halt", &operator)
        .await;
    let records = halts.assert_ok(200)["records"]
        .as_array()
        .expect("records")
        .clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["subject"], "BTC-PERP");

    let by_subject = api
        .get_as(
            &format!("/api/v1/admin/audit?subject={}", player.user_id),
            &operator,
        )
        .await;
    assert_eq!(
        by_subject.assert_ok(200)["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
}

#[tokio::test]
async fn the_simulation_endpoints_report_and_validate() {
    let (api, operator, _player) = with_operator().await;
    let reply = api.get_as("/api/v1/admin/sim", &operator).await;
    let body = reply.assert_ok(200);
    assert_eq!(body["running"], false);
    assert_eq!(body["scenario"], "calm");
    assert!(body["bots"].is_object());

    // An unknown scenario is a typo, and is reported as one.
    api.post_as(
        "/api/v1/admin/sim",
        &operator,
        json!({ "action": "start", "scenario": "moon" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");

    // An unknown bot kind likewise.
    api.post_as(
        "/api/v1/admin/sim",
        &operator,
        json!({ "action": "start", "bots": { "wizard": 3 } }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");

    // Starting with no harness attached says so rather than pretending.
    api.post_as(
        "/api/v1/admin/sim",
        &operator,
        json!({ "action": "start", "scenario": "crash", "seed": 42 }),
    )
    .await
    .assert_error(403, "FORBIDDEN");

    // Stopping something that is not running is not an error.
    api.post_as("/api/v1/admin/sim", &operator, json!({ "action": "stop" }))
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn an_operator_control_that_only_binds_this_process_says_so() {
    // §19 of the flow review. A halt and a risk change live in gateway memory:
    // the chain has no transaction for either, so they stop *this* gateway
    // accepting orders and change what it publishes, and bind nothing else. A
    // bot on the sequencer's RPC keeps trading a "halted" market at full speed,
    // the executor margins against its own table and never reads a changed
    // requirement, and both vanish on restart.
    //
    // None of that is fixable here — it needs transactions the chain does not
    // have. What is fixable is presenting it as an exchange-wide action when it
    // is not, so every surface that carries one now carries its scope.
    let (api, operator, _player) = with_operator().await;

    api.post_as(
        "/api/v1/admin/markets/BTC-PERP/halt",
        &operator,
        json!({ "reason": "oracle feed stale" }),
    )
    .await
    .assert_ok(204);

    let markets = api.get_as("/api/v1/admin/markets", &operator).await;
    let row = markets.assert_ok(200)["markets"]
        .as_array()
        .expect("markets")
        .iter()
        .find(|m| m["symbol"] == "BTC-PERP")
        .expect("BTC-PERP is listed")
        .clone();
    assert_eq!(row["status"], "halted");
    assert_eq!(row["overridden"], true);
    assert_eq!(
        row["override_scope"], "gateway_local",
        "the operator has to be able to see how far their halt reaches"
    );

    let risk = api
        .patch_as(
            "/api/v1/admin/markets/BTC-PERP/risk",
            &operator,
            json!({ "maintenance_margin_bps": 800 }),
        )
        .await;
    let body = risk.assert_ok(200);
    assert_eq!(body["applied"], true);
    assert_eq!(body["scope"], "gateway_local");
    let enforcement = body["enforcement"].as_str().expect("an explanation");
    assert!(
        enforcement.contains("executor") && enforcement.contains("restart"),
        "the reply must say what enforces the change and that it is not durable: {enforcement}"
    );

    // Both are audited with the same scope, so an investigation reading the
    // audit trail is told too.
    let audit = api.get_as("/api/v1/admin/audit", &operator).await;
    let records = audit.assert_ok(200)["records"]
        .as_array()
        .expect("records")
        .clone();
    for action in ["market.halt", "market.risk"] {
        let record = records
            .iter()
            .find(|r| r["action"] == action)
            .unwrap_or_else(|| panic!("{action} is audited"));
        assert_eq!(record["detail"]["scope"], "gateway_local", "{action}");
    }
}

#[tokio::test]
async fn the_operator_faucet_figures_count_claims_rather_than_rows() {
    // One claim of a two-asset grant writes two rows. Counting rows reported
    // twice the usage — and would have scaled with the size of the grant rather
    // than with how much the faucet was used — while the user's own
    // `claims_made` correctly said one. Two counters of the same thing that
    // disagree is a counter nobody can act on.
    let (api, operator, player) = with_operator().await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);

    let theirs = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(theirs.assert_ok(200)["claims_made"], 1);

    let faucet = api.get_as("/api/v1/admin/faucet", &operator).await;
    assert_eq!(
        faucet.assert_ok(200)["claims_24h"],
        1,
        "one claim, however many assets it granted"
    );

    let overview = api.get_as("/api/v1/admin/overview", &operator).await;
    let body = overview.assert_ok(200);
    assert_eq!(body["faucet"]["claims_24h"], 1);
    assert_eq!(
        body["faucet"]["granted_24h"]
            .as_array()
            .expect("granted")
            .len(),
        2,
        "the amounts are still per asset: that is what a conservation check needs"
    );
}
