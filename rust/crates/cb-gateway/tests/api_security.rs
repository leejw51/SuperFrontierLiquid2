//! The authentication and request-boundary controls, at their edges.
//!
//! `api_auth` covers the ordinary login flow and the obvious refusals. This
//! suite goes after the edges the security review names: the exact instant a
//! challenge or session stops working, a signature's malleable twin, a
//! signature that is garbage in each of the ways a hex string can be, one
//! captured signature presented twice at once, a header a client can forge,
//! a browser origin nobody allowed, and the rate limit on the endpoints an
//! attacker would hammer first.

mod common;

use axum::body::Body;
use axum::http::Request;
use cb_gateway::auth::eth;
use cb_gateway::testing::Harness;
use common::Api;
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                       abandon abandon abandon about";

/// A challenge for mnemonic index `index`, and the signature over it.
async fn challenge_and_sign(api: &Api, index: u32) -> (String, String, String) {
    let (secret, address) = eth::derive_from_mnemonic(ABANDON, index).expect("derive");
    let rendered = eth::checksum_address(&address);
    let reply = api
        .post("/api/v1/auth/challenge", json!({ "address": rendered }))
        .await;
    let message = reply.assert_ok(200)["message"]
        .as_str()
        .expect("message")
        .to_string();
    let signature = eth::sign_personal_sign(&secret, &message).expect("sign");
    (rendered, message, signature)
}

/// secp256k1's group order, big-endian.
const N: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE,
    0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x41,
];

/// The malleable twin of a 65-byte `r || s || v` signature: `(r, N - s, v ^ 1)`,
/// which recovers the same signer and is exactly what the low-s rule exists
/// to refuse.
fn high_s_twin(signature: &str) -> String {
    let bytes = hex::decode(signature.trim_start_matches("0x")).expect("hex");
    assert_eq!(bytes.len(), 65);
    let mut twin = bytes.clone();
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let diff = N[i] as i16 - bytes[32 + i] as i16 - borrow;
        let (digit, next) = if diff < 0 { (diff + 256, 1) } else { (diff, 0) };
        twin[32 + i] = digit as u8;
        borrow = next;
    }
    twin[64] = match bytes[64] {
        27 => 28,
        28 => 27,
        0 => 1,
        1 => 0,
        other => panic!("unexpected v {other}"),
    };
    format!("0x{}", hex::encode(twin))
}

// ---------------------------------------------------------------------------
// Challenges
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_challenge_works_until_its_last_second_and_not_at_its_expiry() {
    // Five minutes, per the contract; the store keeps a challenge while
    // `expires_at > now`, so the expiry instant itself is already too late.
    let api = Api::new();
    let (address, _message, signature) = challenge_and_sign(&api, 0).await;
    api.harness
        .clock
        .advance(chrono::Duration::minutes(5) - chrono::Duration::seconds(1));
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "satoshi" }),
    )
    .await
    .assert_ok(200);

    let (address, _message, signature) = challenge_and_sign(&api, 1).await;
    api.harness.clock.advance(chrono::Duration::minutes(5));
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "hal" }),
    )
    .await
    .assert_error(401, "CHALLENGE_EXPIRED");
}

#[tokio::test]
async fn one_addresss_signed_challenge_does_not_log_in_another_address() {
    // B has a live challenge of its own. A's signature over A's challenge,
    // presented as B, must match nothing: the recovered signer is compared
    // in full against the claimed address.
    let api = Api::new();
    let (_a, _message, a_signature) = challenge_and_sign(&api, 0).await;
    let (_secret, b) = eth::derive_from_mnemonic(ABANDON, 1).expect("derive");
    let b = eth::checksum_address(&b);
    api.post("/api/v1/auth/challenge", json!({ "address": b }))
        .await
        .assert_ok(200);
    api.post(
        "/api/v1/auth/login",
        json!({ "address": b, "signature": a_signature, "handle": "mallory" }),
    )
    .await
    .assert_error(401, "BAD_SIGNATURE");
}

#[tokio::test]
async fn the_high_s_twin_of_a_real_signature_is_refused_and_retires_nothing() {
    // Finding 6, over HTTP. The twin recovers the same address; it must be
    // refused anyway, and — per finding 7's fix — its failure must not burn
    // the challenge the real signature is still waiting to use.
    let api = Api::new();
    let (address, _message, signature) = challenge_and_sign(&api, 0).await;
    let twin = high_s_twin(&signature);
    assert_ne!(twin, signature);
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": twin, "handle": "satoshi" }),
    )
    .await
    .assert_error(401, "BAD_SIGNATURE");
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "satoshi" }),
    )
    .await
    .assert_ok(200);
}

#[tokio::test]
async fn every_malformed_signature_is_a_bad_signature_never_a_500() {
    let api = Api::new();
    let (address, _message, signature) = challenge_and_sign(&api, 0).await;
    let body = signature.trim_start_matches("0x");
    let mut v29 = hex::decode(body).expect("hex");
    v29[64] = 29;
    let malformed = vec![
        String::new(),
        "0x".to_string(),
        "0x00".to_string(),
        format!("0x{}", &body[..128]),      // 64 bytes: no v
        format!("0x{body}00"),              // 66 bytes
        format!("0x{}", "zz".repeat(65)),   // not hex
        format!("0x{}", "0".repeat(130)),   // r = s = 0
        format!("0x{}", "f".repeat(130)),   // r, s >= N
        format!("0x{}", hex::encode(&v29)), // v out of range
        format!("{}x", &signature[..131]),  // odd length
        format!("0x{}", "é".repeat(65)),    // non-ASCII
        "not a signature".to_string(),
        "0".repeat(10_000),
    ];
    for bad in &malformed {
        let reply = api
            .post(
                "/api/v1/auth/login",
                json!({ "address": address, "signature": bad, "handle": "satoshi" }),
            )
            .await;
        reply.assert_error(401, "BAD_SIGNATURE");
    }
    // None of it cost the real signer their challenge.
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "satoshi" }),
    )
    .await
    .assert_ok(200);
}

#[tokio::test]
async fn a_malformed_or_missing_address_on_login_is_a_validation_failure() {
    let api = Api::new();
    let (_address, _message, signature) = challenge_and_sign(&api, 0).await;
    for address in [
        "",
        "0x",
        "0x1234",
        "9858EfFD232B4033E47d90003D41EC34EcaEda9",
        "0xZZ58EfFD232B4033E47d90003D41EC34EcaEda94",
    ] {
        api.post(
            "/api/v1/auth/login",
            json!({ "address": address, "signature": signature }),
        )
        .await
        .assert_error(400, "VALIDATION_FAILED");
    }
    api.post("/api/v1/auth/login", json!({ "signature": signature }))
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_captured_signature_presented_concurrently_mints_exactly_one_session() {
    // The retire is the single-use point: both presentations see the live
    // challenge and both verify, and exactly one of them may remove it.
    let api = Api::new();
    let (address, _message, signature) = challenge_and_sign(&api, 0).await;
    let body = json!({ "address": address, "signature": signature, "handle": "satoshi" });
    let replies = futures_util::future::join_all(
        (0..8).map(|_| api.post("/api/v1/auth/login", body.clone())),
    )
    .await;
    let ok = replies.iter().filter(|r| r.status.as_u16() == 200).count();
    assert_eq!(ok, 1, "one session from one signature");
    for reply in replies.iter().filter(|r| r.status.as_u16() != 200) {
        reply.assert_error(401, "CHALLENGE_EXPIRED");
    }
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_works_until_its_last_second_and_not_at_its_expiry() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.harness
        .clock
        .advance(chrono::Duration::hours(24) - chrono::Duration::seconds(1));
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_ok(200);
    api.harness.clock.advance(chrono::Duration::seconds(1));
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn a_revoked_session_cannot_trade_and_a_second_logout_is_refused() {
    // Revocation has to stop the endpoints that move money, not just the
    // session endpoint the existing logout test checks.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, cb_types::AssetId::Usd, 1_000_000_000_000);
    let reply = api.post_as("/api/v1/auth/logout", &player, json!({})).await;
    assert_eq!(reply.status.as_u16(), 204);

    api.post_as("/api/v1/orders", &player, common::spot_buy("50000.00"))
        .await
        .assert_error(401, "UNAUTHORIZED");
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(401, "UNAUTHORIZED");
    api.post_as("/api/v1/auth/logout", &player, json!({}))
        .await
        .assert_error(401, "UNAUTHORIZED");
    assert!(
        api.harness.node.submitted().is_empty(),
        "nothing was signed"
    );
}

#[tokio::test]
async fn a_new_login_issues_a_new_token_and_does_not_revoke_the_old_one() {
    // No session fixation (tokens are always minted), and two devices may be
    // signed in at once.
    let api = Api::new();
    let first = api.player("satoshi", 0).await;
    let second = api.harness.login("satoshi", 0).await;
    assert_ne!(first.token, second.token);
    api.get_as("/api/v1/auth/session", &first)
        .await
        .assert_ok(200);
    api.get_as("/api/v1/auth/session", &second)
        .await
        .assert_ok(200);
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_revoked_operator_session_loses_the_admin_surface() {
    // The role is recomputed from the server's record on every request; a
    // session that no longer exists carries no role at all.
    let api = Api::from(Harness::builder().admins(&["root"]).build());
    let operator = api.player("root", 0).await;
    api.get_as("/api/v1/admin/overview", &operator)
        .await
        .assert_ok(200);
    api.post_as("/api/v1/auth/logout", &operator, json!({}))
        .await;
    api.get_as("/api/v1/admin/overview", &operator)
        .await
        .assert_error(401, "UNAUTHORIZED");
}

// ---------------------------------------------------------------------------
// Forwarded headers
// ---------------------------------------------------------------------------

/// A request that arrived over a real connection from `peer`.
fn from_peer(
    method: &str,
    path: &str,
    peer: &str,
    forwarded: Option<&str>,
    body: serde_json::Value,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    let mut request = request.body(Body::from(body.to_string())).expect("request");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
            peer.parse().expect("a socket address"),
        ));
    request
}

#[tokio::test]
async fn a_forwarded_header_from_an_untrusted_peer_does_not_choose_where_a_session_came_from() {
    // Finding 9: "where you signed in from" is shown to the user and would be
    // leaned on by an audit. A client that forges `x-forwarded-for` from an
    // unconfigured peer must be recorded at its real address.
    let api = Api::new();
    let request = from_peer(
        "POST",
        "/api/v1/auth/dev-login",
        "203.0.113.9:40000",
        Some("10.9.8.7"),
        json!({ "mnemonic": ABANDON, "index": 0, "handle": "satoshi" }),
    );
    let reply = api.dispatch(request).await;
    let token = reply.assert_ok(200)["session_token"]
        .as_str()
        .expect("token")
        .to_string();
    let sessions = api
        .get_with_token("/api/v1/user/sessions", &format!("Bearer {token}"))
        .await;
    let recorded = sessions.assert_ok(200)["sessions"][0]["ip_prefix"].to_string();
    assert!(
        !recorded.contains("10.9"),
        "the forged address was recorded: {recorded}"
    );
    assert!(
        recorded.contains("203.0"),
        "the real peer was not recorded: {recorded}"
    );
}

#[tokio::test]
async fn the_per_address_limit_counts_the_real_peer_whatever_it_forwards() {
    // Three login attempts per window per peer; every one carries a new
    // forged `x-forwarded-for`, and the fourth is still refused. A different
    // real peer has its own budget.
    let api = Api::from(Harness::builder().rate_limits(0, 3).build());
    let address = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";
    let body = json!({ "address": address, "signature": "0x00" });
    for i in 0..3 {
        let forged = format!("198.51.100.{i}");
        let reply = api
            .dispatch(from_peer(
                "POST",
                "/api/v1/auth/login",
                "203.0.113.9:1",
                Some(&forged),
                body.clone(),
            ))
            .await;
        assert_ne!(reply.status.as_u16(), 429, "attempt {i} is within budget");
    }
    let reply = api
        .dispatch(from_peer(
            "POST",
            "/api/v1/auth/login",
            "203.0.113.9:2",
            Some("198.51.100.200"),
            body.clone(),
        ))
        .await;
    reply.assert_error(429, "RATE_LIMITED");
    let reply = api
        .dispatch(from_peer(
            "POST",
            "/api/v1/auth/login",
            "192.0.2.1:1",
            None,
            body.clone(),
        ))
        .await;
    assert_ne!(reply.status.as_u16(), 429, "another peer is not punished");
}

// ---------------------------------------------------------------------------
// Rate limits on the endpoints an attacker hammers first
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_is_rate_limited_with_a_retry_after_and_the_window_rolls_over() {
    let api = Api::from(Harness::builder().rate_limits(0, 4).build());
    let body =
        json!({ "address": "0x9858EfFD232B4033E47d90003D41EC34EcaEda94", "signature": "0x00" });
    for _ in 0..4 {
        api.post("/api/v1/auth/login", body.clone())
            .await
            .assert_error(401, "CHALLENGE_EXPIRED");
    }
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = api
        .harness
        .router()
        .oneshot(request)
        .await
        .expect("answers");
    assert_eq!(response.status().as_u16(), 429);
    let retry_after: u64 = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("a 429 says when to come back");
    assert!((1..=60).contains(&retry_after), "retry-after {retry_after}");

    api.harness.clock.advance(chrono::Duration::seconds(60));
    api.post("/api/v1/auth/login", body)
        .await
        .assert_error(401, "CHALLENGE_EXPIRED");
}

#[tokio::test]
async fn faucet_claims_are_rate_limited_per_session_before_the_cooldown_is_consulted() {
    // The cooldown is the faucet's own limit; the request limiter is in
    // front of it. Hammering the endpoint with a session is refused as
    // `RATE_LIMITED` without reading the faucet at all.
    let api = Api::from(Harness::builder().rate_limits(2, 0).build());
    let player = api.player("satoshi", 0).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "FAUCET_COOLDOWN");
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "RATE_LIMITED");
    assert_eq!(
        api.harness.node.submitted().len(),
        2,
        "one claim's two deposits, and nothing more"
    );
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

async fn preflight(router: axum::Router, origin: &str) -> axum::http::HeaderMap {
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/orders")
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "authorization,content-type",
        )
        .body(Body::empty())
        .expect("preflight");
    router
        .oneshot(request)
        .await
        .expect("answers")
        .headers()
        .clone()
}

async fn simple_get(router: axum::Router, origin: &str) -> axum::http::HeaderMap {
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/markets")
        .header("origin", origin)
        .body(Body::empty())
        .expect("request");
    router
        .oneshot(request)
        .await
        .expect("answers")
        .headers()
        .clone()
}

#[tokio::test]
async fn an_allowed_origin_is_echoed_and_any_other_origin_gets_no_allow_origin() {
    let harness = Harness::new();
    let origins = vec!["http://127.0.0.1:3000".to_string()];
    let router = || cb_gateway::http::router_with(Arc::clone(&harness.state), &origins, false);

    let allowed = preflight(router(), "http://127.0.0.1:3000").await;
    assert_eq!(
        allowed
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("http://127.0.0.1:3000")
    );
    for evil in [
        "http://evil.example",
        "http://127.0.0.1:3001",
        "null",
        "http://127.0.0.1:3000.evil.example",
    ] {
        let headers = preflight(router(), evil).await;
        assert!(
            headers.get("access-control-allow-origin").is_none(),
            "{evil} was allowed by preflight: {headers:?}"
        );
        let headers = simple_get(router(), evil).await;
        assert!(
            headers.get("access-control-allow-origin").is_none(),
            "{evil} was allowed on a simple request: {headers:?}"
        );
    }
}

#[tokio::test]
async fn an_empty_origin_list_allows_no_origin_rather_than_every_origin() {
    // Security review finding 10 is recorded as fixed: "An empty origin list
    // no longer means any origin; only an explicit `*` is permissive." An
    // operator who configured nothing must not have silently opened the API
    // to every site on the web.
    let harness = Harness::new();
    let router = || cb_gateway::http::router_with(Arc::clone(&harness.state), &[], false);
    let headers = preflight(router(), "http://evil.example").await;
    assert!(
        headers.get("access-control-allow-origin").is_none(),
        "an empty list allowed an arbitrary origin: {headers:?}"
    );
    let headers = simple_get(router(), "http://evil.example").await;
    assert!(headers.get("access-control-allow-origin").is_none());

    // An explicit `*` is still the documented way to ask for any origin.
    let star = vec!["*".to_string()];
    let headers = preflight(
        cb_gateway::http::router_with(Arc::clone(&harness.state), &star, false),
        "http://anything.example",
    )
    .await;
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

// ---------------------------------------------------------------------------
// The development login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_development_login_is_absent_from_a_production_router_whatever_the_request_claims() {
    // Mounting is the only guard the endpoint has (see the report on S2): a
    // production router must not answer it at all, from any peer, with any
    // forwarded header.
    let api = Api::new();
    for (peer, forwarded) in [("127.0.0.1:1", None), ("203.0.113.9:1", Some("127.0.0.1"))] {
        let request = from_peer(
            "POST",
            "/api/v1/auth/dev-login",
            peer,
            forwarded,
            json!({ "mnemonic": ABANDON, "index": 0, "handle": "satoshi" }),
        );
        let response = api
            .harness
            .router_without_dev_login()
            .oneshot(request)
            .await
            .expect("answers");
        assert_eq!(response.status().as_u16(), 404);
    }
}

#[test]
fn the_development_login_refuses_to_mount_on_any_non_loopback_bind() {
    use clap::Parser;
    for listen in ["0.0.0.0:0", "[::]:0", "192.168.1.10:0", "10.0.0.1:0"] {
        let config = cb_gateway::config::Config::try_parse_from([
            "cb-gateway",
            "--listen",
            listen,
            "--dev-login",
            "--server-secret",
            "a-test-secret-that-is-long-enough-0123456789",
            "--faucet-treasury-key",
            "0101010101010101010101010101010101010101010101010101010101010101",
        ])
        .expect("parses");
        assert!(
            config.dev_login_allowed().is_err(),
            "--dev-login on {listen} must be a start-up error"
        );
    }
    for listen in ["127.0.0.1:0", "[::1]:0"] {
        let config = cb_gateway::config::Config::try_parse_from([
            "cb-gateway",
            "--listen",
            listen,
            "--dev-login",
            "--server-secret",
            "a-test-secret-that-is-long-enough-0123456789",
            "--faucet-treasury-key",
            "0101010101010101010101010101010101010101010101010101010101010101",
        ])
        .expect("parses");
        assert!(config.dev_login_allowed().expect("loopback"));
    }
}
