//! §5 and §6 — the account views and the endpoints that move money.
//!
//! Every documented error code on the order path has a test here. These are
//! the responses a trading UI branches on, so a wrong one is not a cosmetic
//! difference: it is the difference between "your size is too small" and a
//! spinner.

mod common;

use cb_gateway::node::{MarginSnapshot, PositionSnapshot, SubmitOutcome, SubmitRejection};
use cb_types::{AssetId, DerivId, Side};
use common::Api;
use serde_json::{json, Value};

/// A well-formed limit order.
fn order() -> Value {
    json!({
        "symbol": "BTC-PERP",
        "side": "buy",
        "type": "limit",
        "price": "64000.00",
        "qty": "1.0000",
        "tif": "gtc",
        "post_only": false,
        "reduce_only": false
    })
}

/// A harness with a funded, collateralised player on a live market.
async fn ready() -> (Api, cb_gateway::testing::Player) {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000_000);
    api.collateralise(&player, 1_000_000_000_000);
    (api, player)
}

#[tokio::test]
async fn placing_an_order_returns_the_contracts_shape() {
    let (api, player) = ready().await;
    let reply = api.post_as("/api/v1/orders", &player, order()).await;
    let body = reply.assert_ok(201);
    assert!(body["order_id"].as_str().expect("id").starts_with("0x"));
    assert!(body["tx_hash"].as_str().expect("hash").starts_with("0x"));
    // No block producer in the harness, so the order stays pending and is
    // addressed by its transaction hash. That is what the contract's
    // `pending` status is for.
    assert_eq!(body["status"], "pending");
    assert_eq!(body["order_id"], body["tx_hash"]);
    assert!(body["accepted_at"].as_str().is_some());
    assert_eq!(body["immediate_fills"], json!([]));

    // A real signed transaction went to the chain.
    let placed = api
        .harness
        .node
        .submitted_bodies()
        .into_iter()
        .any(|b| matches!(b, cb_tx::TxBody::PlacePerpOrder { .. }));
    assert!(placed, "the order must reach the sequencer");
}

#[tokio::test]
async fn placing_an_order_needs_a_session() {
    let api = Api::with_markets().await;
    api.post("/api/v1/orders", order())
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn an_order_on_an_unlisted_market_is_a_404() {
    let (api, player) = ready().await;
    let mut bad = order();
    bad["symbol"] = json!("ZZZ-PERP");
    api.post_as("/api/v1/orders", &player, bad)
        .await
        .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn an_order_off_the_tick_grid_is_a_validation_failure() {
    let (api, player) = ready().await;
    let mut bad = order();
    bad["price"] = json!("64000.25");
    let reply = api.post_as("/api/v1/orders", &player, bad).await;
    reply.assert_error(400, "VALIDATION_FAILED");
    assert!(
        reply.body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("tick size"),
        "the message must say what to fix: {}",
        reply.body
    );
}

#[tokio::test]
async fn an_order_off_the_lot_grid_is_a_validation_failure() {
    let (api, player) = ready().await;
    let mut bad = order();
    bad["qty"] = json!("1.00005");
    api.post_as("/api/v1/orders", &player, bad)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_size_finer_than_the_market_is_rejected_rather_than_truncated() {
    let (api, player) = ready().await;
    let mut bad = order();
    // Nine decimal places against a market that can hold eight.
    bad["qty"] = json!("1.000000001");
    let reply = api.post_as("/api/v1/orders", &player, bad).await;
    reply.assert_error(400, "VALIDATION_FAILED");
    // Nothing was sent: truncating a user's size is the bug this prevents.
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn an_order_below_the_minimum_notional_is_rejected_with_the_threshold() {
    let (api, player) = ready().await;
    let mut small = order();
    small["qty"] = json!("0.0001");
    small["price"] = json!("0.10");
    let reply = api.post_as("/api/v1/orders", &player, small).await;
    reply.assert_error(400, "VALIDATION_FAILED");
    assert!(
        reply.body["error"]["details"]["min_notional"].is_string(),
        "the frontend needs the threshold: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_derivative_order_far_from_the_mark_has_its_own_code() {
    // The chain refuses a derivative order priced more than 10% from the
    // mark (`ExecError::PriceOutOfBand`); the gateway refuses it first, with
    // the band's edges for the client to show.
    let (api, player) = ready().await;
    let mut far = order();
    far["price"] = json!("1000.00");
    let reply = api.post_as("/api/v1/orders", &player, far).await;
    reply.assert_error(422, "PRICE_OUT_OF_BAND");
    let details = &reply.body["error"]["details"];
    assert!(
        details["min_price"].is_string() && details["max_price"].is_string(),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_post_only_order_that_would_cross_has_its_own_code() {
    let (api, player) = ready().await;
    let mut crossing = order();
    crossing["post_only"] = json!(true);
    crossing["price"] = json!("64252.00");
    api.post_as("/api/v1/orders", &player, crossing)
        .await
        .assert_error(422, "POST_ONLY_WOULD_CROSS");
}

#[tokio::test]
async fn a_reduce_only_order_that_would_increase_a_position_has_its_own_code() {
    let (api, player) = ready().await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![PositionSnapshot {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 100_000_000,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
    });

    // Buying while long grows the position.
    let mut growing = order();
    growing["reduce_only"] = json!(true);
    api.post_as("/api/v1/orders", &player, growing)
        .await
        .assert_error(422, "REDUCE_ONLY_WOULD_INCREASE");

    // Selling within it is what the flag is for.
    let mut reducing = order();
    reducing["reduce_only"] = json!(true);
    reducing["side"] = json!("sell");
    reducing["qty"] = json!("0.5000");
    api.post_as("/api/v1/orders", &player, reducing)
        .await
        .assert_ok(201);
}

#[tokio::test]
async fn an_under_margined_order_has_its_own_code() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    // One BTC at $64,000 needs 2% = $1,280. Offer $100.
    api.collateralise(&player, 100_000_000);
    let reply = api.post_as("/api/v1/orders", &player, order()).await;
    reply.assert_error(422, "INSUFFICIENT_MARGIN");
    assert_eq!(reply.body["error"]["details"]["required"], "1280.00");
}

#[tokio::test]
async fn a_halted_market_refuses_orders() {
    let (api, player) = ready().await;
    api.harness
        .state
        .markets
        .amend_override("BTC-PERP", |patch| patch.status = Some("halted".into()));
    api.post_as("/api/v1/orders", &player, order())
        .await
        .assert_error(422, "MARKET_HALTED");
}

#[tokio::test]
async fn a_stale_mark_price_refuses_orders() {
    let (api, player) = ready().await;
    // Push the head far past the mark's height.
    api.harness
        .node
        .edit(|state| state.health.last_height = 1_000_000);
    api.post_as("/api/v1/orders", &player, order())
        .await
        .assert_error(422, "ORACLE_STALE");
}

#[tokio::test]
async fn a_sequencer_rejection_maps_to_the_contracts_code() {
    let (api, player) = ready().await;
    api.harness.node.edit(|state| {
        state.submit_reply = Some(SubmitOutcome::Rejected {
            code: SubmitRejection::InsufficientBalance,
            message: "not enough".into(),
        })
    });
    api.post_as("/api/v1/orders", &player, order())
        .await
        .assert_error(422, "INSUFFICIENT_BALANCE");
}

#[tokio::test]
async fn an_unreachable_sequencer_is_a_503_on_the_order_path() {
    let (api, player) = ready().await;
    api.harness.node.take_offline("restarting");
    api.post_as("/api/v1/orders", &player, order())
        .await
        .assert_error(503, "NODE_UNAVAILABLE");
}

#[tokio::test]
async fn leverage_and_margin_mode_are_accepted_on_a_derivative_order() {
    // The contract carries both on the order, so a conformant client sends
    // them. This chain sets both on the position, so they have no effect here
    // — but refusing them would fail every order such a client makes.
    let (api, player) = ready().await;
    let mut with_leverage = order();
    with_leverage["leverage"] = json!("10");
    with_leverage["margin_mode"] = json!("cross");
    api.post_as("/api/v1/orders", &player, with_leverage)
        .await
        .assert_ok(201);
}

#[tokio::test]
async fn a_nonsensical_margin_mode_is_still_refused() {
    let (api, player) = ready().await;
    let mut bad = order();
    bad["margin_mode"] = json!("sideways");
    api.post_as("/api/v1/orders", &player, bad)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn leverage_on_a_spot_order_is_refused_because_it_is_meaningless() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000_000);
    let mut spot = order();
    spot["symbol"] = json!("BTC-USD");
    spot["price"] = json!("64000.00");
    spot["leverage"] = json!("10");
    api.post_as("/api/v1/orders", &player, spot)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_size_written_with_trailing_zeros_is_accepted() {
    // A browser formatting with `toFixed(8)` writes zeros it means nothing by.
    let (api, player) = ready().await;
    let mut padded = order();
    padded["qty"] = json!("1.00000000");
    padded["price"] = json!("64000.00000000");
    api.post_as("/api/v1/orders", &player, padded)
        .await
        .assert_ok(201);
}

#[tokio::test]
async fn one_client_order_id_places_exactly_one_order() {
    let (api, player) = ready().await;
    let mut idempotent = order();
    idempotent["client_order_id"] = json!("cli_a1b2c3");

    let first = api
        .post_as("/api/v1/orders", &player, idempotent.clone())
        .await;
    let first_body = first.assert_ok(201).clone();
    let second = api.post_as("/api/v1/orders", &player, idempotent).await;
    let second_body = second.assert_ok(201);

    assert_eq!(first_body["order_id"], second_body["order_id"]);
    assert_eq!(first_body["tx_hash"], second_body["tx_hash"]);
    assert_eq!(second_body["client_order_id"], "cli_a1b2c3");

    let placements = api
        .harness
        .node
        .submitted_bodies()
        .into_iter()
        .filter(|b| matches!(b, cb_tx::TxBody::PlacePerpOrder { .. }))
        .count();
    assert_eq!(placements, 1, "the second request must not place an order");
}

#[tokio::test]
async fn two_players_may_use_the_same_client_order_id() {
    let api = Api::with_markets().await;
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;
    for player in [&alice, &bob] {
        api.fund(player, AssetId::Usd, 1_000_000_000_000);
        api.collateralise(player, 1_000_000_000_000);
    }

    let mut body = order();
    body["client_order_id"] = json!("cli_1");
    let a = api.post_as("/api/v1/orders", &alice, body.clone()).await;
    let b = api.post_as("/api/v1/orders", &bob, body).await;
    assert_ne!(a.assert_ok(201)["order_id"], b.assert_ok(201)["order_id"]);
}

#[tokio::test]
async fn an_order_appears_in_the_placing_accounts_order_list_and_nobody_elses() {
    let api = Api::with_markets().await;
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;
    api.collateralise(&alice, 1_000_000_000_000);
    api.post_as("/api/v1/orders", &alice, order())
        .await
        .assert_ok(201);

    let hers = api.get_as("/api/v1/account/orders", &alice).await;
    let orders = hers.assert_ok(200)["orders"]
        .as_array()
        .expect("orders")
        .clone();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["symbol"], "BTC-PERP");
    assert_eq!(orders[0]["side"], "buy");
    assert_eq!(orders[0]["type"], "limit");
    assert_eq!(orders[0]["price"], "64000.00");
    assert_eq!(orders[0]["qty"], "1.0000");
    assert_eq!(orders[0]["remaining_qty"], "1.0000");
    assert_eq!(orders[0]["status"], "pending");
    assert_eq!(orders[0]["tif"], "gtc");

    let his = api.get_as("/api/v1/account/orders", &bob).await;
    assert_eq!(his.assert_ok(200)["orders"], json!([]));
}

#[tokio::test]
async fn the_order_status_filter_follows_the_contracts_vocabulary() {
    let (api, player) = ready().await;
    api.post_as("/api/v1/orders", &player, order())
        .await
        .assert_ok(201);

    for status in ["open", "filled", "cancelled", "all"] {
        api.get_as(&format!("/api/v1/account/orders?status={status}"), &player)
            .await
            .assert_ok(200);
    }
    api.get_as("/api/v1/account/orders?status=sideways", &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn cancelling_an_order_that_is_not_yours_is_forbidden() {
    let api = Api::with_markets().await;
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;
    api.collateralise(&alice, 1_000_000_000_000);
    let placed = api.post_as("/api/v1/orders", &alice, order()).await;
    let order_id = placed.assert_ok(201)["order_id"]
        .as_str()
        .expect("id")
        .to_string();

    api.delete_as(&format!("/api/v1/orders/{order_id}"), &bob)
        .await
        .assert_error(403, "FORBIDDEN");
}

#[tokio::test]
async fn cancelling_an_unknown_order_is_a_404() {
    let (api, player) = ready().await;
    let unknown = format!("0x{}", "11".repeat(32));
    api.delete_as(&format!("/api/v1/orders/{unknown}"), &player)
        .await
        .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn cancel_all_with_nothing_resting_reports_zero() {
    let (api, player) = ready().await;
    let reply = api
        .post_as("/api/v1/orders/cancel-all", &player, json!({}))
        .await;
    assert_eq!(reply.assert_ok(200)["cancelled"], 0);
}

#[tokio::test]
async fn the_account_view_reports_balances_and_a_portfolio() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);
    api.harness.node.edit(|state| {
        state.margins.insert(
            player.trading_address,
            MarginSnapshot {
                collateral: 0,
                unrealized_pnl: 4_250_000_000,
                bad_debt: 0,
            },
        );
    });

    let reply = api.get_as("/api/v1/account", &player).await;
    let body = reply.assert_ok(200);
    // §7 of the flow review. The contract's account view publishes BOTH
    // addresses, labelled. It used to publish one field called `address`
    // holding the trading address, which the browser client cannot decode at
    // all — it requires `login_address` and `trading_address` — so the account
    // store stayed empty and the Place Order button was disabled for every
    // order in every market.
    assert_eq!(
        body["login_address"],
        json!(cb_gateway::http::format_address(&player.login_address)),
        "the address the user signed in with"
    );
    assert_eq!(
        body["trading_address"],
        json!(cb_gateway::http::format_address(&player.trading_address)),
        "the account holding the balances below"
    );
    assert_ne!(body["login_address"], body["trading_address"]);
    assert!(
        body.get("address").is_none(),
        "one field named `address` cannot say which of the two it is"
    );
    let usd = body["balances"]
        .as_array()
        .expect("balances")
        .iter()
        .find(|b| b["asset"] == "USD")
        .expect("a USD balance")
        .clone();
    assert_eq!(usd["free"], "100000.00");
    assert_eq!(usd["locked"], "0.00");
    assert_eq!(usd["total"], "100000.00");

    let portfolio = &body["portfolio"];
    assert_eq!(portfolio["equity"], "104250.00");
    assert_eq!(portfolio["unrealized_pnl"], "4250.00");
    assert_eq!(portfolio["liquidation_risk"], "low");
}

#[tokio::test]
async fn positions_are_rendered_with_every_documented_field() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![PositionSnapshot {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 150_000_000,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
    });

    let reply = api.get_as("/api/v1/account/positions", &player).await;
    let positions = reply.assert_ok(200)["positions"]
        .as_array()
        .expect("positions")
        .clone();
    assert_eq!(positions.len(), 1);
    let position = &positions[0];
    assert_eq!(position["symbol"], "BTC-PERP");
    assert_eq!(position["side"], "long");
    assert_eq!(position["qty"], "1.5000");
    assert_eq!(position["entry_price"], "63000.00");
    assert_eq!(position["mark_price"], "64250.00");
    assert_eq!(position["margin_mode"], "cross");
    for field in [
        "liquidation_price",
        "notional",
        "leverage",
        "margin",
        "unrealized_pnl",
        "unrealized_pnl_pct",
        "funding_paid",
    ] {
        assert!(position[field].is_string(), "{field} must be a string");
    }
    // Long from 63,000 marked at 64,250 is 1,875 up on 1.5 contracts.
    assert_eq!(position["unrealized_pnl"], "1875.00");
}

#[tokio::test]
async fn a_flat_position_row_is_not_reported_as_open() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![PositionSnapshot {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 0,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
    });
    let reply = api.get_as("/api/v1/account/positions", &player).await;
    assert_eq!(reply.assert_ok(200)["positions"], json!([]));
}

#[tokio::test]
async fn closing_a_position_without_one_is_a_404() {
    let (api, player) = ready().await;
    api.post_as(
        "/api/v1/positions/close",
        &player,
        json!({ "symbol": "BTC-PERP" }),
    )
    .await
    .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn closing_more_than_the_position_holds_is_refused() {
    let (api, player) = ready().await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![PositionSnapshot {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 100_000_000,
                entry_px_q8: 6_300_000_000_000,
                margin_mode: 0,
                isolated_margin: 0,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
    });
    api.post_as(
        "/api/v1/positions/close",
        &player,
        json!({ "symbol": "BTC-PERP", "qty": "5.0000" }),
    )
    .await
    .assert_error(422, "REDUCE_ONLY_WOULD_INCREASE");

    // Closing it fully is fine, and submits a real transaction.
    let reply = api
        .post_as(
            "/api/v1/positions/close",
            &player,
            json!({ "symbol": "BTC-PERP" }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["closed_qty"], "1.0000");
    assert_eq!(body["avg_price"], "64250.00");
    assert_eq!(body["realized_pnl"], "1250.00");
    assert!(api
        .harness
        .node
        .submitted_bodies()
        .into_iter()
        .any(|b| matches!(b, cb_tx::TxBody::ClosePosition { .. })));
}

#[tokio::test]
async fn margin_cannot_be_moved_at_all_and_the_reason_given_is_the_real_one() {
    // §9 of the flow review, and the cause is upstream of this crate: the
    // executor answers `TxBody::AdjustMargin` with `ExecError::Unsupported`, so
    // nothing bridges a spot balance and margin collateral and every account
    // created through the gateway has zero collateral forever.
    //
    // What was in the gateway's gift was the answer. It used to check for an
    // open position first and reply "no open position on BTC-PERP", which sends
    // the user to open one — with margin they cannot fund. And it submitted a
    // transaction the chain refuses, which is a transaction that currently
    // takes its whole block down.
    let (api, player) = ready().await;
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![PositionSnapshot {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 100_000_000,
                entry_px_q8: 6_300_000_000_000,
                // Isolated: the one shape the old code would have submitted.
                margin_mode: 1,
                isolated_margin: 1_000_000_000,
                entry_funding_index: 0,
                last_funding_height: 0,
            }],
        );
    });
    let refused = api
        .post_as(
            "/api/v1/positions/margin",
            &player,
            json!({ "symbol": "BTC-PERP", "action": "add", "amount": "500.00" }),
        )
        .await;
    refused.assert_error(501, "NOT_SUPPORTED");
    assert!(
        !api.harness
            .node
            .submitted_bodies()
            .into_iter()
            .any(|b| matches!(b, cb_tx::TxBody::AdjustMargin { .. })),
        "nothing may be submitted for a transaction the executor refuses"
    );

    // The request is still validated: a malformed one is a 400, not a 501.
    api.post_as(
        "/api/v1/positions/margin",
        &player,
        json!({ "symbol": "BTC-PERP", "action": "sideways", "amount": "500.00" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn leverage_beyond_the_markets_maximum_is_refused() {
    let (api, player) = ready().await;
    api.post_as(
        "/api/v1/positions/leverage",
        &player,
        json!({ "symbol": "BTC-PERP", "leverage": "500" }),
    )
    .await
    .assert_error(422, "INSUFFICIENT_MARGIN");

    api.post_as(
        "/api/v1/positions/leverage",
        &player,
        json!({ "symbol": "BTC-PERP", "leverage": "20", "margin_mode": "isolated" }),
    )
    .await
    .assert_ok(200);
}

#[tokio::test]
async fn a_transfer_checks_the_balance_before_it_reaches_the_chain() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let recipient = format!("0x{}", "22".repeat(20));

    let reply = api
        .post_as(
            "/api/v1/transfers",
            &player,
            json!({ "to": recipient, "asset": "USD", "amount": "100.00" }),
        )
        .await;
    reply.assert_error(422, "INSUFFICIENT_BALANCE");
    assert_eq!(reply.body["error"]["details"]["required"], "100.00");
    assert_eq!(reply.body["error"]["details"]["available"], "0.00");
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn a_transfer_reaches_the_ledger_only_once_the_chain_has_applied_it() {
    // §20 of the flow review. `POST /transfers` used to append a `transfer_out`
    // entry the moment the transaction was accepted into the *mempool*, with a
    // `balance_after` it had computed from the pre-submission balance. A
    // transaction that then died at block build left a permanent row in the one
    // place a person looks to answer "where did my money go", describing a
    // transfer that never happened and a balance that never existed.
    let api = Api::with_markets().await;
    let sender = api.player("satoshi", 0).await;
    let recipient = api.player("hal", 1).await;
    api.fund(&sender, AssetId::Usd, 1_000_000_000);

    let reply = api
        .post_as(
            "/api/v1/transfers",
            &sender,
            json!({
                "to": cb_gateway::http::format_address(&recipient.trading_address),
                "asset": "USD",
                "amount": "100.00",
                "memo": "gg"
            }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert!(body["tx_hash"].as_str().expect("hash").starts_with("0x"));
    assert!(api
        .harness
        .node
        .submitted_bodies()
        .into_iter()
        .any(|b| matches!(b, cb_tx::TxBody::Transfer { .. })));

    let pending = api.get_as("/api/v1/account/transfers", &sender).await;
    assert_eq!(
        pending.assert_ok(200)["entries"],
        json!([]),
        "submitted is not applied: nothing is recorded until a block says so"
    );

    // The block lands.
    let block = api.harness.node.mine(11);
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");

    let ledger = api.get_as("/api/v1/account/transfers", &sender).await;
    let entries = ledger.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "transfer_out");
    assert_eq!(entries[0]["amount"], "-100.00");
    assert_eq!(entries[0]["memo"], "gg");
    assert_eq!(
        entries[0]["balance_after"], "900.00",
        "the balance the chain reports after the block, not one the gateway assumed"
    );

    // And the recipient gets the other half, which nothing used to write.
    let theirs = api.get_as("/api/v1/account/transfers", &recipient).await;
    let received = theirs.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    assert_eq!(
        received.len(),
        1,
        "`transfer_in` is in the contract's own kind vocabulary"
    );
    assert_eq!(received[0]["kind"], "transfer_in");
    assert_eq!(received[0]["amount"], "100.00");
    assert_eq!(received[0]["balance_after"], "100.00");
}

#[tokio::test]
async fn a_transfer_to_a_malformed_address_is_rejected() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.post_as(
        "/api/v1/transfers",
        &player,
        json!({ "to": "0x1234", "asset": "USD", "amount": "1.00" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_transfer_amount_finer_than_the_asset_is_rejected() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000);
    let recipient = format!("0x{}", "22".repeat(20));
    api.post_as(
        "/api/v1/transfers",
        &player,
        // USD has six decimals; this has seven.
        json!({ "to": recipient, "asset": "USD", "amount": "1.0000001" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn fills_are_empty_until_something_fills_and_are_scoped_to_the_caller() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let reply = api.get_as("/api/v1/account/fills", &player).await;
    assert_eq!(reply.assert_ok(200)["fills"], json!([]));

    api.get_as("/api/v1/account/fills?symbol=ZZZ-PERP", &player)
        .await
        .assert_error(404, "NOT_FOUND");
}

#[tokio::test]
async fn mutating_endpoints_are_rate_limited_and_reads_are_not() {
    let api = Api::from(
        cb_gateway::testing::Harness::builder()
            .rate_limits(2, 1_000)
            .build(),
    );
    api.list_btc_perp().await;
    let player = api.player("satoshi", 0).await;

    // Two mutating requests are allowed, the third is not.
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    let reply = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    reply.assert_error(429, "RATE_LIMITED");
    assert!(reply.body["error"]["details"]["retry_after_seconds"].is_number());

    // Reads are untouched: a trading UI polls constantly.
    for _ in 0..20 {
        api.get_as("/api/v1/account", &player).await.assert_ok(200);
    }
}

#[tokio::test]
async fn free_plus_locked_equals_total_through_a_rest_a_fill_and_a_cancel() {
    // The chain keeps `locked` as a *subset* of the balance: locking an order's
    // collateral records how much of the balance is spoken for, it does not
    // debit it. The gateway read the balance as though it meant free and then
    // added the lock to make a total, so a rested order inflated the account by
    // exactly the amount it had reserved — and the number the interface labels
    // spendable included money the user had already committed.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);

    let usd = |body: &serde_json::Value| -> (f64, f64, f64) {
        let row = body["balances"]
            .as_array()
            .expect("balances")
            .iter()
            .find(|b| b["asset"] == "USD")
            .expect("a USD balance")
            .clone();
        let number = |key: &str| -> f64 {
            row[key]
                .as_str()
                .expect("a decimal string")
                .parse()
                .expect("a number")
        };
        (number("free"), number("locked"), number("total"))
    };

    let before = api.get_as("/api/v1/account", &player).await;
    let (free, locked, total) = usd(before.assert_ok(200));
    assert_eq!((free, locked, total), (100_000.0, 0.0, 100_000.0));

    // Rest an order worth 1,000: the chain locks the quote, and the balance —
    // which is the total — does not move.
    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.02000000"
        }),
    )
    .await
    .assert_ok(201);
    api.harness
        .node
        .lock(player.trading_address, AssetId::Usd, 1_000_000_000);

    let rested = api.get_as("/api/v1/account", &player).await;
    let body = rested.assert_ok(200);
    let (free, locked, total) = usd(body);
    assert_eq!(free, 99_000.0, "the reserved 1,000 is not spendable");
    assert_eq!(locked, 1_000.0);
    assert_eq!(total, 100_000.0, "nothing has left the account yet");
    assert_eq!(free + locked, total);
    assert_eq!(
        body["portfolio"]["equity"]
            .as_str()
            .and_then(|e| e.parse::<f64>().ok()),
        Some(100_000.0),
        "equity inherited the double count"
    );

    // Half of it fills: 500 of quote leaves, and the lock behind it goes with it.
    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        *account.balances.entry(AssetId::Usd).or_insert(0) -= 500_000_000;
        account.locked.insert(AssetId::Usd, 500_000_000);
        *account.balances.entry(AssetId::Btc).or_insert(0) += 1_000_000;
    });
    let partial = api.get_as("/api/v1/account", &player).await;
    let (free, locked, total) = usd(partial.assert_ok(200));
    assert_eq!((free, locked, total), (99_000.0, 500.0, 99_500.0));
    assert_eq!(free + locked, total);

    // The rest is cancelled: the lock is released and nothing else changes.
    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        account.locked.remove(&AssetId::Usd);
    });
    let cancelled = api.get_as("/api/v1/account", &player).await;
    let (free, locked, total) = usd(cancelled.assert_ok(200));
    assert_eq!((free, locked, total), (99_500.0, 0.0, 99_500.0));
    assert_eq!(free + locked, total);
}

#[tokio::test]
async fn a_spot_order_the_account_cannot_pay_for_is_refused() {
    // It used to be accepted: `201 pending`, and pending for ever, because the
    // sequencer refused it and the gateway had already answered. The contract
    // documents `422 INSUFFICIENT_BALANCE` and the client renders it.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000); // 100,000 USD

    let reply = api
        .post_as(
            "/api/v1/orders",
            &player,
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": "50000.00", "qty": "1000.00000000"
            }),
        )
        .await;
    reply.assert_error(422, "INSUFFICIENT_BALANCE");
    assert_eq!(reply.body["error"]["details"]["asset"], "USD");
    assert!(reply.body["error"]["details"]["available"].is_string());
    assert!(
        api.harness.node.submitted_bodies().is_empty(),
        "nothing may be submitted for an order the chain would refuse"
    );

    // A sell of base the account does not hold is refused the same way.
    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "sell", "type": "limit",
            "price": "50000.00", "qty": "1.00000000"
        }),
    )
    .await
    .assert_error(422, "INSUFFICIENT_BALANCE");

    // And an order the account *can* pay for still goes through.
    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
}

#[tokio::test]
async fn an_order_on_a_market_with_no_oracle_price_is_oracle_stale_not_a_500() {
    // The default state of a freshly started chain: a perpetual is listed and
    // no oracle has priced it. The node answers "no mark price for perp:0",
    // which was propagated as `500 INTERNAL` — a code the client treats as the
    // gateway having broken, for a condition that is ordinary and that the
    // contract gives `422 ORACLE_STALE` to.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.collateralise(&player, 100_000_000_000);
    api.harness.node.edit(|state| state.marks.clear());

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-PERP", "side": "buy", "type": "limit",
            "price": "64000.00", "qty": "0.10000000"
        }),
    )
    .await
    .assert_error(422, "ORACLE_STALE");
}

#[tokio::test]
async fn an_order_refused_in_the_block_it_waited_for_is_answered_rejected() {
    // `POST /orders` waits, bounded, for the block carrying the order. Under
    // BFT ordering that block can carry an order the executor refused, and the
    // reply used to say `open` because the order was in a block. It now reads
    // the sender's nonce and says `rejected`.
    let harness = cb_gateway::testing::Harness::builder()
        .settings(cb_gateway::state::Settings {
            order_confirm: std::time::Duration::from_secs(5),
            ..Default::default()
        })
        .build();
    let api = Api::from(harness);
    api.list_btc_perp().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000_000);
    api.collateralise(&player, 1_000_000_000_000);

    // The block producer: as soon as the order is in the mempool, commit it
    // in a block whose executor refuses it.
    let node = std::sync::Arc::clone(&api.harness.node);
    let producer = tokio::spawn(async move {
        for _ in 0..500 {
            if let Some(tx) = node.submitted().last().cloned() {
                node.refuse(tx.hash());
                let block = node.mine(11);
                node.emit(cb_gateway::node::NodeEvent::Block(std::sync::Arc::new(
                    block,
                )));
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        false
    });

    let reply = api.post_as("/api/v1/orders", &player, order()).await;
    assert!(
        producer.await.expect("join"),
        "the order reached the mempool"
    );
    let body = reply.assert_ok(201);
    assert_eq!(body["status"], "rejected");
    assert_eq!(
        body["order_id"], body["tx_hash"],
        "no on-chain id was issued"
    );
}
