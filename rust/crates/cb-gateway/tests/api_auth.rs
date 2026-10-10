//! §2 — identity and session, driven through the real router.

mod common;

use cb_gateway::auth::eth;
use cb_gateway::testing::Harness;
use common::Api;
use serde_json::json;

const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                       abandon abandon abandon about";
const ABANDON_ADDRESS: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

/// Sign a challenge the way a wallet would.
async fn signed_login(api: &Api, index: u32) -> (String, String) {
    let (secret, address) = eth::derive_from_mnemonic(ABANDON, index).expect("derive");
    let rendered = eth::checksum_address(&address);
    let reply = api
        .post("/api/v1/auth/challenge", json!({ "address": rendered }))
        .await;
    let message = reply.assert_ok(200)["message"]
        .as_str()
        .expect("a message to sign")
        .to_string();
    let signature = eth::sign_personal_sign(&secret, &message).expect("sign");
    (rendered, signature)
}

#[tokio::test]
async fn a_challenge_names_the_address_and_carries_an_expiry() {
    let api = Api::new();
    let reply = api
        .post(
            "/api/v1/auth/challenge",
            json!({ "address": ABANDON_ADDRESS }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert!(body["nonce"]
        .as_str()
        .expect("nonce")
        .starts_with("cbl-login-"));
    assert!(body["message"]
        .as_str()
        .expect("message")
        .contains(ABANDON_ADDRESS));
    assert!(body["expires_at"].as_str().is_some());
}

#[tokio::test]
async fn a_challenge_for_a_malformed_address_is_rejected() {
    let api = Api::new();
    api.post("/api/v1/auth/challenge", json!({ "address": "0x1234" }))
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_mixed_case_address_with_a_broken_checksum_is_rejected() {
    let api = Api::new();
    // One digit's case flipped: a typo, and crediting a different account is
    // what accepting it would mean.
    let corrupted = "0x9858EFFD232B4033E47d90003D41EC34EcaEda94";
    api.post("/api/v1/auth/challenge", json!({ "address": corrupted }))
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_first_login_creates_the_account_and_returns_a_session() {
    let api = Api::new();
    let (address, signature) = signed_login(&api, 0).await;
    let reply = api
        .post(
            "/api/v1/auth/login",
            json!({ "address": address, "signature": signature, "handle": "satoshi" }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["handle"], "satoshi");
    assert_eq!(body["address"], ABANDON_ADDRESS);
    assert_eq!(body["is_new"], true);
    assert!(body["session_token"]
        .as_str()
        .expect("token")
        .starts_with("sess_"));
    assert!(body["user_id"].as_str().expect("id").starts_with("usr_"));
}

#[tokio::test]
async fn a_first_login_without_a_handle_is_handle_required() {
    let api = Api::new();
    let (address, signature) = signed_login(&api, 0).await;
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature }),
    )
    .await
    .assert_error(422, "HANDLE_REQUIRED");
}

#[tokio::test]
async fn a_second_login_reports_that_the_account_is_not_new() {
    let api = Api::new();
    let (address, signature) = signed_login(&api, 0).await;
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "satoshi" }),
    )
    .await
    .assert_ok(200);

    let (address, signature) = signed_login(&api, 0).await;
    let reply = api
        .post(
            "/api/v1/auth/login",
            json!({ "address": address, "signature": signature }),
        )
        .await;
    assert_eq!(reply.assert_ok(200)["is_new"], false);
}

#[tokio::test]
async fn a_signature_that_recovers_to_another_address_is_a_bad_signature() {
    let api = Api::new();
    // Challenge index 0, sign with index 1's key.
    let (_secret, claimed) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
    let (other, _address) = eth::derive_from_mnemonic(ABANDON, 1).expect("derive");
    let rendered = eth::checksum_address(&claimed);
    let reply = api
        .post("/api/v1/auth/challenge", json!({ "address": rendered }))
        .await;
    let message = reply.assert_ok(200)["message"]
        .as_str()
        .expect("message")
        .to_string();
    let signature = eth::sign_personal_sign(&other, &message).expect("sign");

    let reply = api
        .post(
            "/api/v1/auth/login",
            json!({ "address": rendered, "signature": signature, "handle": "satoshi" }),
        )
        .await;
    reply.assert_error(401, "BAD_SIGNATURE");
    assert!(
        !reply.body.to_string().contains("0x6Fac"),
        "the reply must not name the address it did recover: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_login_without_a_challenge_is_challenge_expired() {
    let api = Api::new();
    let (secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
    let signature = eth::sign_personal_sign(&secret, "not the challenge").expect("sign");
    api.post(
        "/api/v1/auth/login",
        json!({
            "address": eth::checksum_address(&address),
            "signature": signature,
            "handle": "satoshi"
        }),
    )
    .await
    .assert_error(401, "CHALLENGE_EXPIRED");
}

#[tokio::test]
async fn a_replayed_signature_is_refused() {
    let api = Api::new();
    let (address, signature) = signed_login(&api, 0).await;
    let body = json!({ "address": address, "signature": signature, "handle": "satoshi" });
    api.post("/api/v1/auth/login", body.clone())
        .await
        .assert_ok(200);
    api.post("/api/v1/auth/login", body)
        .await
        .assert_error(401, "CHALLENGE_EXPIRED");
}

#[tokio::test]
async fn a_taken_handle_is_a_conflict() {
    let api = Api::new();
    let (address, signature) = signed_login(&api, 0).await;
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "satoshi" }),
    )
    .await
    .assert_ok(200);

    let (address, signature) = signed_login(&api, 1).await;
    api.post(
        "/api/v1/auth/login",
        json!({ "address": address, "signature": signature, "handle": "SATOSHI" }),
    )
    .await
    .assert_error(409, "HANDLE_TAKEN");
}

#[tokio::test]
async fn the_development_login_derives_the_canonical_account() {
    let api = Api::new();
    let reply = api
        .post(
            "/api/v1/auth/dev-login",
            json!({ "mnemonic": ABANDON, "index": 0, "handle": "tester" }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["address"], ABANDON_ADDRESS);
    assert_eq!(body["handle"], "tester");
}

#[tokio::test]
async fn the_development_login_is_not_mounted_in_a_production_router() {
    // `Harness::router_without_dev_login` is what `main.rs` mounts when the
    // flag is absent. The route must simply not exist.
    let api = Api::new();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/auth/dev-login")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({ "mnemonic": ABANDON, "handle": "tester" }).to_string(),
        ))
        .expect("request");
    let response = tower::ServiceExt::oneshot(api.harness.router_without_dev_login(), request)
        .await
        .expect("router answers");
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_session_endpoint_reports_both_addresses_and_the_roles() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let reply = api.get_as("/api/v1/auth/session", &player).await;
    let body = reply.assert_ok(200);
    assert_eq!(body["handle"], "satoshi");
    assert_eq!(body["login_address"], ABANDON_ADDRESS);
    assert_ne!(
        body["trading_address"], body["login_address"],
        "the two addresses are different accounts"
    );
    assert_eq!(body["roles"], json!(["user"]));
    assert!(body["issued_at"].as_str().is_some());
    assert!(body["expires_at"].as_str().is_some());
}

#[tokio::test]
async fn an_authenticated_endpoint_refuses_every_bad_token_the_same_way() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;

    // No header at all.
    api.get("/api/v1/auth/session")
        .await
        .assert_error(401, "UNAUTHORIZED");
    // Not a bearer token.
    api.get_with_token("/api/v1/auth/session", "Basic abc")
        .await
        .assert_error(401, "UNAUTHORIZED");
    // A bearer token nobody issued.
    api.get_with_token("/api/v1/auth/session", "Bearer sess_nope")
        .await
        .assert_error(401, "UNAUTHORIZED");
    // Empty.
    api.get_with_token("/api/v1/auth/session", "Bearer ")
        .await
        .assert_error(401, "UNAUTHORIZED");

    // ...and the real one still works.
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn an_expired_session_is_refused() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_ok(200);

    api.harness.clock.advance(chrono::Duration::hours(25));
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn logging_out_revokes_the_session_it_was_made_with() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let reply = api.post_as("/api/v1/auth/logout", &player, json!({})).await;
    assert_eq!(reply.status.as_u16(), 204);
    api.get_as("/api/v1/auth/session", &player)
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn one_players_token_never_reaches_another_players_account() {
    let api = Api::new();
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;

    let alices = api.get_as("/api/v1/auth/session", &alice).await;
    let bobs = api.get_as("/api/v1/auth/session", &bob).await;
    assert_eq!(alices.assert_ok(200)["handle"], "alice");
    assert_eq!(bobs.assert_ok(200)["handle"], "bob");
    assert_ne!(
        alices.body["trading_address"], bobs.body["trading_address"],
        "two accounts must not share a trading account"
    );
}

#[tokio::test]
async fn every_response_carries_a_request_id() {
    let api = Api::new();
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/health")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = tower::ServiceExt::oneshot(api.harness.router(), request)
        .await
        .expect("router answers");
    assert!(
        response.headers().get("x-request-id").is_some(),
        "a report of a lost order has to be followable through the logs"
    );
}

#[tokio::test]
async fn a_client_supplied_request_id_is_echoed_back() {
    let api = Api::new();
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-request-id", "trace-me")
        .body(axum::body::Body::empty())
        .expect("request");
    let response = tower::ServiceExt::oneshot(api.harness.router(), request)
        .await
        .expect("router answers");
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("trace-me")
    );
}

#[tokio::test]
async fn a_rotating_forwarded_header_does_not_evade_the_rate_limit() {
    // Finding 3: the per-address key was taken from `x-forwarded-for`, which
    // an unauthenticated client fully controls, so twelve requests carrying
    // twelve forged values all passed a limit of five. The key now comes from
    // the connection, and no proxy is trusted unless one is configured.
    let api = Api::from(Harness::builder().rate_limits(0, 5).build());
    let address = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

    let mut allowed = 0;
    let mut limited = 0;
    for i in 0..12 {
        let forged = format!("198.51.100.{i}");
        let reply = api
            .post_with_headers(
                "/api/v1/auth/challenge",
                json!({ "address": address }),
                &[("x-forwarded-for", forged.as_str())],
            )
            .await;
        match reply.status.as_u16() {
            200 => allowed += 1,
            429 => limited += 1,
            other => panic!("unexpected status {other}: {}", reply.body),
        }
    }
    assert_eq!(allowed, 5, "the configured budget, and not one more");
    assert_eq!(limited, 7);

    // A header kilobytes long is not a key either: the reviewer grew the
    // limiter map by ~40 MB with 5,000 of them.
    let huge = "a".repeat(8 * 1024);
    let reply = api
        .post_with_headers(
            "/api/v1/auth/challenge",
            json!({ "address": address }),
            &[("x-forwarded-for", huge.as_str())],
        )
        .await;
    assert_eq!(reply.status.as_u16(), 429, "{}", reply.body);
}
