//! Every trading endpoint refuses bad input with a 4xx and a stable code.
//!
//! The contract promises two things about a refusal: the status is in the
//! 4xx range, because the fault is the caller's, and the body is the error
//! envelope with a SCREAMING_SNAKE `code` the frontend branches on. A `500`
//! for a typo is a gateway bug, and a bare text body — which is what an
//! unmapped extractor rejection produces — is one the client cannot branch
//! on at all. This suite throws malformed decimals, unknown enums, wrong JSON
//! types and plain garbage at every endpoint that moves money, and a property
//! test throws arbitrary JSON at `POST /orders`.

mod common;

use cb_gateway::testing::Player;
use cb_types::AssetId;
use common::{Api, Reply};
use proptest::prelude::*;
use serde_json::{json, Value};

/// A market, and a funded, collateralised player.
async fn ready() -> (Api, Player) {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000_000);
    api.collateralise(&player, 1_000_000_000_000);
    (api, player)
}

/// A well-formed perpetual limit order, to be broken one field at a time.
fn perp_order() -> Value {
    json!({
        "symbol": "BTC-PERP", "side": "buy", "type": "limit",
        "price": "64000.00", "qty": "1.0000", "tif": "gtc"
    })
}

/// The refusal the contract describes: a 4xx and a code in the envelope.
fn assert_refused(reply: &Reply, what: &str) {
    let status = reply.status.as_u16();
    assert!(
        (400..500).contains(&status),
        "{what}: expected a 4xx, got {status} with body {}",
        reply.body
    );
    let code = reply.error_code().unwrap_or_else(|| {
        panic!(
            "{what}: a {status} must carry the error envelope, got body {}",
            reply.body
        )
    });
    assert!(
        !code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
        "{what}: `{code}` is not a stable SCREAMING_SNAKE code"
    );
}

/// Decimal strings no endpoint may accept as an amount, and why.
const BAD_DECIMALS: &[(&str, &str)] = &[
    ("", "empty"),
    ("   ", "blank"),
    ("-1", "negative"),
    ("-0.50", "negative fraction"),
    ("1e3", "exponent"),
    ("1E-2", "negative exponent"),
    ("0x10", "hex"),
    ("1.2.3", "two points"),
    ("1,000", "thousands separator"),
    ("1 000", "inner whitespace"),
    ("NaN", "not a number"),
    ("inf", "infinity"),
    ("١٢٣", "Arabic-Indic digits"),
    ("１２３", "fullwidth digits"),
    ("²", "superscript digit"),
    ("12abc", "trailing letters"),
    ("0.0000000000000000001", "more decimals than any market has"),
    (
        "1000000000000000000000000000000000000000000",
        "wider than u128",
    ),
];

// ---------------------------------------------------------------------------
// POST /orders
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_malformed_decimal_in_price_or_qty_is_a_validation_failure() {
    let (api, player) = ready().await;
    for (bad, why) in BAD_DECIMALS {
        for field in ["price", "qty"] {
            let mut body = perp_order();
            body[field] = json!(bad);
            let reply = api.post_as("/api/v1/orders", &player, body).await;
            reply.assert_error(400, "VALIDATION_FAILED");
            let _ = why;
        }
    }
    assert!(
        api.harness.node.submitted().is_empty(),
        "nothing was signed"
    );
}

#[tokio::test]
async fn zero_price_or_zero_qty_is_refused_not_submitted() {
    let (api, player) = ready().await;
    for (field, zero) in [
        ("price", "0"),
        ("qty", "0"),
        ("price", "0.00"),
        ("qty", "0.0000"),
    ] {
        let mut body = perp_order();
        body[field] = json!(zero);
        api.post_as("/api/v1/orders", &player, body)
            .await
            .assert_error(400, "VALIDATION_FAILED");
    }
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn amounts_at_the_edge_of_u128_are_refused_without_overflowing() {
    // A value that parses into a `u128` and then overflows the notional
    // arithmetic is the one most likely to panic a handler. u128::MAX and its
    // neighbours, as both price and quantity, on spot and on perp.
    let (api, player) = ready().await;
    let max = u128::MAX.to_string();
    let edges = [
        max.as_str(),
        "340282366920938463463374607431768211454",
        "340282366920938463463.374607431768211455",
        "18446744073709551616", // 2^64
        "99999999999999999999.99",
    ];
    for symbol in ["BTC-PERP", "BTC-USD"] {
        for edge in edges {
            for field in ["price", "qty"] {
                let mut body = perp_order();
                body["symbol"] = json!(symbol);
                body[field] = json!(edge);
                let reply = api.post_as("/api/v1/orders", &player, body).await;
                assert_refused(&reply, &format!("{symbol} {field}={edge}"));
            }
        }
    }
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn unknown_enum_values_are_validation_failures() {
    let (api, player) = ready().await;
    let cases: &[(&str, Value)] = &[
        ("side", json!("BUY")),
        ("side", json!("long")),
        ("side", json!("")),
        ("type", json!("stop")),
        ("type", json!("LIMIT")),
        ("tif", json!("day")),
        ("tif", json!("GTC ")),
        ("margin_mode", json!("portfolio")),
    ];
    for (field, value) in cases {
        let mut body = perp_order();
        body[*field] = value.clone();
        let reply = api.post_as("/api/v1/orders", &player, body).await;
        assert_eq!(
            reply.status.as_u16(),
            400,
            "{field}={value}: {}",
            reply.body
        );
        assert_eq!(
            reply.error_code(),
            Some("VALIDATION_FAILED"),
            "{field}={value}"
        );
    }
}

#[tokio::test]
async fn an_unknown_market_is_not_found_and_a_malformed_symbol_is_invalid() {
    let (api, player) = ready().await;
    for symbol in ["DOGE-USD", "BTC-EUR", "ZZZ-PERP", "BTC-991231"] {
        let mut body = perp_order();
        body["symbol"] = json!(symbol);
        api.post_as("/api/v1/orders", &player, body)
            .await
            .assert_error(404, "NOT_FOUND");
    }
    for symbol in [
        "",
        "btc perp",
        "BTC/USD",
        "BTC-PERP; DROP TABLE users",
        &"A".repeat(300),
    ] {
        let mut body = perp_order();
        body["symbol"] = json!(symbol);
        let reply = api.post_as("/api/v1/orders", &player, body).await;
        assert_refused(&reply, &format!("symbol {symbol:?}"));
    }
}

#[tokio::test]
async fn a_market_order_with_a_price_and_a_limit_order_without_one_are_refused() {
    let (api, player) = ready().await;
    let mut market = perp_order();
    market["type"] = json!("market");
    api.post_as("/api/v1/orders", &player, market)
        .await
        .assert_error(400, "VALIDATION_FAILED");

    let mut limit = perp_order();
    limit.as_object_mut().expect("object").remove("price");
    api.post_as("/api/v1/orders", &player, limit)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_spot_order_takes_the_same_decimal_rules_through_its_own_validation_path() {
    // Spot orders are checked against the account's free balance rather than
    // margin, which is a different path through the handler; the decimal
    // rules must not depend on which path an order takes.
    let (api, player) = ready().await;
    for (bad, _why) in BAD_DECIMALS {
        for field in ["price", "qty"] {
            let mut body = json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                                   "price": "50000.00", "qty": "0.50000000" });
            body[field] = json!(bad);
            api.post_as("/api/v1/orders", &player, body)
                .await
                .assert_error(400, "VALIDATION_FAILED");
        }
    }
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn a_body_that_is_not_the_right_shape_still_gets_the_error_envelope() {
    // These never reach the handler: the JSON extractor refuses them. Its
    // default refusal is a bare-text 400/415/422 — a status the client may not
    // expect and a body with no code to branch on.
    let (api, player) = ready().await;
    let shapes: Vec<(&str, Value)> = vec![
        (
            "missing qty",
            json!({ "symbol": "BTC-PERP", "side": "buy", "type": "limit", "price": "1" }),
        ),
        (
            "qty as a number",
            json!({ "symbol": "BTC-PERP", "side": "buy", "type": "limit", "price": "64000.00", "qty": 1 }),
        ),
        (
            "side as a number",
            json!({ "symbol": "BTC-PERP", "side": 1, "type": "limit", "price": "64000.00", "qty": "1" }),
        ),
        (
            "post_only as a string",
            json!({ "symbol": "BTC-PERP", "side": "buy", "type": "limit", "price": "64000.00", "qty": "1", "post_only": "yes" }),
        ),
        ("an array", json!([1, 2, 3])),
        ("a string", json!("buy me bitcoin")),
        ("null", Value::Null),
        ("an empty object", json!({})),
    ];
    for (what, body) in shapes {
        let reply = api.post_as("/api/v1/orders", &player, body).await;
        reply.assert_error(400, "VALIDATION_FAILED");
        let _ = what;
    }
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn a_body_that_is_not_json_or_not_declared_json_gets_the_error_envelope() {
    let (api, player) = ready().await;
    let cases: &[(&str, Option<&str>)] = &[
        ("{\"symbol\": \"BTC-PERP\",", Some("application/json")),
        ("not json at all", Some("application/json")),
        ("", Some("application/json")),
        (r#"{"symbol":"BTC-PERP"}"#, Some("text/plain")),
        (r#"{"symbol":"BTC-PERP"}"#, None),
    ];
    for (raw, content_type) in cases {
        let reply = api
            .raw_as(
                "POST",
                "/api/v1/orders",
                &player,
                content_type.as_deref(),
                raw,
            )
            .await;
        reply.assert_error(400, "VALIDATION_FAILED");
    }
}

#[tokio::test]
async fn deeply_nested_json_is_refused_cleanly() {
    let (api, player) = ready().await;
    let raw = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let reply = api
        .raw_as(
            "POST",
            "/api/v1/orders",
            &player,
            Some("application/json"),
            &raw,
        )
        .await;
    assert_refused(&reply, "10k-deep nesting");
}

#[tokio::test]
async fn an_oversized_body_is_refused_as_too_large_with_the_envelope() {
    let (api, player) = ready().await;
    let raw = format!(r#"{{"symbol":"{}"}}"#, "A".repeat(3 * 1024 * 1024));
    let reply = api
        .raw_as(
            "POST",
            "/api/v1/orders",
            &player,
            Some("application/json"),
            &raw,
        )
        .await;
    assert_eq!(reply.status.as_u16(), 413, "{}", reply.body);
    assert_eq!(reply.error_code(), Some("VALIDATION_FAILED"));
}

// ---------------------------------------------------------------------------
// The other endpoints that move money
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_transfer_with_a_bad_amount_asset_or_recipient_is_refused() {
    let (api, player) = ready().await;
    let to = cb_gateway::http::format_address(&cb_types::Address([7u8; 20]));
    for (bad, _why) in BAD_DECIMALS {
        api.post_as(
            "/api/v1/transfers",
            &player,
            json!({ "to": to, "asset": "USD", "amount": bad }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
    }
    for amount in ["0", "0.00"] {
        api.post_as(
            "/api/v1/transfers",
            &player,
            json!({ "to": to, "asset": "USD", "amount": amount }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
    }
    for asset in ["DOGE", "usd ", "", "U\u{0}SD"] {
        let reply = api
            .post_as(
                "/api/v1/transfers",
                &player,
                json!({ "to": to, "asset": asset, "amount": "1.00" }),
            )
            .await;
        assert_refused(&reply, &format!("asset {asset:?}"));
    }
    for recipient in ["", "0x", "0x1234", "not an address", &format!("{to}00")] {
        api.post_as(
            "/api/v1/transfers",
            &player,
            json!({ "to": recipient, "asset": "USD", "amount": "1.00" }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
    }
    let reply = api
        .post_as(
            "/api/v1/transfers",
            &player,
            json!({ "to": to, "asset": "USD" }),
        )
        .await;
    reply.assert_error(400, "VALIDATION_FAILED");
    assert!(
        api.harness.node.submitted().is_empty(),
        "nothing was signed"
    );
}

#[tokio::test]
async fn closing_leverage_and_margin_refuse_malformed_numbers_and_unknown_markets() {
    let (api, player) = ready().await;
    for (bad, _why) in BAD_DECIMALS.iter().filter(|(b, _)| !b.trim().is_empty()) {
        let close = api
            .post_as(
                "/api/v1/positions/close",
                &player,
                json!({ "symbol": "BTC-PERP", "qty": bad }),
            )
            .await;
        assert_refused(&close, &format!("close qty {bad:?}"));
        api.post_as(
            "/api/v1/positions/leverage",
            &player,
            json!({ "symbol": "BTC-PERP", "leverage": bad }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
        api.post_as(
            "/api/v1/positions/margin",
            &player,
            json!({ "symbol": "BTC-PERP", "action": "add", "amount": bad }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
    }
    for path in [
        "/api/v1/positions/close",
        "/api/v1/positions/leverage",
        "/api/v1/positions/margin",
    ] {
        let reply = api
            .post_as(
                path,
                &player,
                json!({ "symbol": "DOGE-PERP", "qty": "1", "leverage": "2",
                        "action": "add", "amount": "1" }),
            )
            .await;
        reply.assert_error(404, "NOT_FOUND");
        api.post_as(path, &player, json!({}))
            .await
            .assert_error(400, "VALIDATION_FAILED");
    }
    api.post_as(
        "/api/v1/positions/margin",
        &player,
        json!({ "symbol": "BTC-PERP", "action": "steal", "amount": "1" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
    api.post_as(
        "/api/v1/positions/leverage",
        &player,
        json!({ "symbol": "BTC-PERP", "leverage": "0" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn cancel_endpoints_refuse_malformed_ids_and_symbols_without_signing() {
    let (api, player) = ready().await;
    let long = "f".repeat(4096);
    for id in [
        "0xzz",
        "0x",
        "12",
        "0x00",
        &format!("0x{}", "0".repeat(63)),
        &long,
    ] {
        let reply = api
            .delete_as(&format!("/api/v1/orders/{id}"), &player)
            .await;
        reply.assert_error(404, "NOT_FOUND");
    }
    api.post_as(
        "/api/v1/orders/cancel-all",
        &player,
        json!({ "symbol": "DOGE-USD" }),
    )
    .await
    .assert_error(404, "NOT_FOUND");
    let reply = api
        .post_as(
            "/api/v1/orders/cancel-all",
            &player,
            json!({ "symbol": "btc perp!" }),
        )
        .await;
    assert_refused(&reply, "cancel-all malformed symbol");
    assert!(api.harness.node.submitted().is_empty());
}

#[tokio::test]
async fn every_trading_endpoint_refuses_an_anonymous_caller_before_reading_the_body() {
    let api = Api::with_markets().await;
    for path in [
        "/api/v1/orders",
        "/api/v1/orders/cancel-all",
        "/api/v1/positions/close",
        "/api/v1/positions/margin",
        "/api/v1/positions/leverage",
        "/api/v1/transfers",
        "/api/v1/faucet/claim",
    ] {
        api.post(path, json!({ "garbage": true }))
            .await
            .assert_error(401, "UNAUTHORIZED");
    }
}

// ---------------------------------------------------------------------------
// Fuzzing POST /orders
// ---------------------------------------------------------------------------

/// A JSON value of the kind a confused or hostile client sends.
fn junk_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>().prop_map(|f| json!(f)),
        "\\PC{0,24}".prop_map(Value::String),
        "[0-9]{0,45}(\\.[0-9]{0,24})?".prop_map(Value::String),
        "-?[0-9]{1,6}(\\.[0-9]{1,10})?([eE][+-]?[0-9]{1,3})?".prop_map(Value::String),
        prop::sample::select(vec![
            "buy",
            "sell",
            "limit",
            "market",
            "gtc",
            "ioc",
            "fok",
            "cross",
            "isolated",
            "BTC-PERP",
            "BTC-USD",
            "ETH-260626",
            "0",
            "64000.00",
            "1.0000",
        ])
        .prop_map(|s| Value::String(s.to_string())),
    ];
    leaf.prop_recursive(2, 8, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[a-z_]{1,8}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// An order body with each documented field present, absent or replaced.
fn junk_order() -> impl Strategy<Value = Value> {
    let fields = [
        "symbol",
        "side",
        "type",
        "price",
        "qty",
        "tif",
        "post_only",
        "reduce_only",
        "leverage",
        "margin_mode",
        "client_order_id",
    ];
    prop::collection::vec(prop::option::of(junk_value()), fields.len()).prop_map(move |values| {
        let mut body = perp_order();
        let object = body.as_object_mut().expect("object");
        for (field, value) in fields.iter().zip(values) {
            match value {
                Some(Value::Null) => {
                    object.remove(*field);
                }
                Some(value) => {
                    object.insert(field.to_string(), value);
                }
                None => {}
            }
        }
        body
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 192,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    /// Whatever arrives, `POST /orders` answers below 500, and every
    /// refusal carries the envelope.
    #[test]
    fn no_order_body_makes_the_gateway_fail(body in junk_order()) {
        let runtime = fuzz_runtime();
        let (api, player) = &*FUZZ_TARGET;
        let reply = runtime.block_on(api.post_as("/api/v1/orders", player, body.clone()));
        let status = reply.status.as_u16();
        prop_assert!(status < 500, "{status} for {body}: {}", reply.body);
        if status >= 400 {
            prop_assert!(
                reply.error_code().is_some(),
                "{status} without the envelope for {body}: {}",
                reply.body
            );
        }
    }
}

fn fuzz_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime")
    })
}

/// One gateway for every fuzz case: building one per case would spend the
/// budget on setup instead of on inputs.
static FUZZ_TARGET: std::sync::LazyLock<(Api, Player)> =
    std::sync::LazyLock::new(|| fuzz_runtime().block_on(ready()));
