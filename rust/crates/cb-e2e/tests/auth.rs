//! Signing in, and every way of failing to.
//!
//! The rest of the suite signs in through `/auth/dev-login`, which the contract
//! provides for exactly that purpose. This file does not: it runs the real
//! flow — request a challenge, sign it with a key derived from a mnemonic,
//! present the signature — because a suite that only ever used the shortcut
//! would leave the production login path untested while reporting that
//! authentication works.
//!
//! The negative cases matter more than the positive one. An exchange that
//! accepts a signature recovering to the wrong address, or lets a nonce be
//! replayed, hands one account's money to another. Each is asserted on both the
//! status *and* the documented error code, because the frontend branches on the
//! code: a 401 with the wrong code sends the user to the wrong screen.

use cb_e2e::flows::{funded_user, sign_in, TEST_ADDRESS_INDEX_0, TEST_MNEMONIC};
use cb_e2e::Stack;
use cb_gateway::auth::eth::{checksum_address, derive_from_mnemonic, sign_personal_sign};
use serde_json::json;

/// Asks for a challenge and signs it with the key at `index`.
///
/// Returns the address claimed and the signature, so a test can mismatch them
/// deliberately.
async fn challenge_and_sign(api: &cb_e2e::Api, index: u32) -> (String, String) {
    let (key, address) = derive_from_mnemonic(TEST_MNEMONIC, index).expect("a derivable index");
    let address = checksum_address(&address);
    let challenge = api
        .post_ok("/auth/challenge", json!({ "address": address }))
        .await;
    let message = challenge.ok(200)["message"]
        .as_str()
        .expect("a challenge carries the exact message to sign")
        .to_string();
    let signature = sign_personal_sign(&key, &message).expect("signing a challenge");
    (address, signature)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn signing_a_challenge_with_the_matching_key_signs_a_user_in() {
    let stack = Stack::start().await;
    let api = &stack.api;

    let (address, signature) = challenge_and_sign(api, 0).await;
    assert_eq!(
        address, TEST_ADDRESS_INDEX_0,
        "the canonical mnemonic derived {address} at index 0. Every EVM tool derives \
         {TEST_ADDRESS_INDEX_0}; a different value means this exchange has its own \
         derivation and no existing wallet can sign in."
    );

    let login = api
        .post_ok(
            "/auth/login",
            json!({ "address": address, "signature": signature, "handle": "real_login" }),
        )
        .await;
    let body = login.ok(200);
    assert_eq!(body["address"].as_str(), Some(address.as_str()));
    assert_eq!(body["is_new"].as_bool(), Some(true));
    let token = body["session_token"]
        .as_str()
        .expect("a successful login returns a session token")
        .to_string();

    // The session it issued has to actually work.
    let session = api.with_token(token).get_ok("/auth/session").await;
    assert_eq!(
        session.ok(200)["login_address"].as_str(),
        Some(address.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_signature_that_recovers_to_a_different_address_is_refused() {
    let stack = Stack::start().await;
    let api = &stack.api;

    // A challenge for address 0, signed by the key for address 1. This is the
    // shape of the attack: the attacker knows the victim's address and has some
    // other valid signature.
    let (victim, _) = challenge_and_sign(api, 0).await;
    let (_, other_signature) = challenge_and_sign(api, 1).await;

    let (_, impostor_address) = derive_from_mnemonic(TEST_MNEMONIC, 1).expect("a derivable index");
    let impostor_address = checksum_address(&impostor_address);

    let reply = api
        .post_ok(
            "/auth/login",
            json!({ "address": victim, "signature": other_signature, "handle": "impostor" }),
        )
        .await;

    // The contract says two things about this case and they do not quite agree:
    // section 2 calls it `401 UNAUTHORIZED`, while the error table defines
    // `BAD_SIGNATURE` (also 401) as exactly "signature does not recover to the
    // claimed address". Both are documented codes for this condition, so the
    // status is pinned hard and the code is accepted as either — with the
    // ambiguity reported rather than silently resolved by whichever one the
    // server happens to send.
    assert_eq!(
        reply.status, 401,
        "a signature that recovers to a different address returned {} {}",
        reply.status, reply.raw
    );
    assert!(
        matches!(
            reply.error_code(),
            Some("UNAUTHORIZED") | Some("BAD_SIGNATURE")
        ),
        "a mismatched signature returned an undocumented error code: {}",
        reply.raw
    );

    // Whichever code it is, it must not say whose signature it was. Telling an
    // attacker which address a signature recovered to turns a failed login into
    // an oracle.
    assert!(
        !reply
            .raw
            .to_lowercase()
            .contains(&impostor_address.to_lowercase()),
        "the rejection named the address the signature actually recovered to: {}",
        reply.raw
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_challenge_cannot_be_used_twice() {
    let stack = Stack::start().await;
    let api = &stack.api;

    let (address, signature) = challenge_and_sign(api, 2).await;
    let first = api
        .post_ok(
            "/auth/login",
            json!({ "address": address, "signature": signature, "handle": "replay_me" }),
        )
        .await;
    first.ok(200);

    // The same address, the same nonce, the same signature: a replay of a
    // request someone could have watched go past.
    let second = api
        .post_ok(
            "/auth/login",
            json!({ "address": address, "signature": signature }),
        )
        .await;
    second.error(401, "CHALLENGE_EXPIRED");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_first_login_without_a_handle_is_told_a_handle_is_required() {
    let stack = Stack::start().await;
    let api = &stack.api;

    let (address, signature) = challenge_and_sign(api, 3).await;
    let reply = api
        .post_ok(
            "/auth/login",
            json!({ "address": address, "signature": signature }),
        )
        .await;
    reply.error(422, "HANDLE_REQUIRED");

    // The documented remedy has to work: retry with a handle.
    let (address, signature) = challenge_and_sign(api, 3).await;
    let retry = api
        .post_ok(
            "/auth/login",
            json!({ "address": address, "signature": signature, "handle": "second_try" }),
        )
        .await;
    retry.ok(200);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_handle_someone_else_holds_is_refused() {
    let stack = Stack::start().await;
    let api = &stack.api;
    sign_in(api, 0, "taken_handle").await;

    let reply = api
        .post_ok(
            "/auth/dev-login",
            json!({ "mnemonic": TEST_MNEMONIC, "index": 1, "handle": "TAKEN_HANDLE" }),
        )
        .await;
    reply.error(409, "HANDLE_TAKEN");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn requests_without_a_usable_token_are_rejected_as_unauthorized() {
    let stack = Stack::start().await;
    let api = &stack.api;

    // Four ways of not having a session, all of which the contract answers the
    // same way. They are listed together because a handler that treats them
    // differently is leaking: "that token does not exist" and "that token
    // expired" tell an attacker which guesses were close.
    for (what, client) in [
        ("no Authorization header at all", api.anonymous()),
        (
            "a token that is not even the right shape",
            api.with_token("not-a-token"),
        ),
        (
            "a well-formed token that was never issued",
            api.with_token("sess_00000000000000000000000000000000000000000000000000000000000000"),
        ),
        ("an empty bearer token", api.with_token("")),
    ] {
        let reply = client.get_ok("/account").await;
        assert_eq!(
            reply.status, 401,
            "GET /account with {what} returned {} — expected 401. Body: {}",
            reply.status, reply.raw
        );
        assert_eq!(
            reply.error_code(),
            Some("UNAUTHORIZED"),
            "GET /account with {what} returned the wrong error code: {}",
            reply.raw
        );
    }
}

// A session that has passed its expiry is NOT covered here, and the omission is
// deliberate rather than an oversight. `cb-gateway/src/main.rs` floors the
// configured lifetime at `session_ttl_hours.max(1)`, so the shortest session any
// invocation can issue lasts an hour; `--session-ttl-hours 0` yields a
// one-hour session, not an expired one. Enforcement itself is present —
// `AuthService` rejects a session once `expires_at <= now` — but provoking it
// needs either a clock the test can move or a way to age a stored session, and
// neither is exposed. Rather than sleep for an hour, or reach into the
// gateway's database and stop testing the API, the gap is written down: in
// `docs/testing.md`, and here. The two adjacent cases that *are* reachable —
// a session the user ended, and a session an operator revoked — are below.

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn logging_out_stops_the_token_working() {
    let stack = Stack::start().await;
    let session = sign_in(&stack.api, 0, "logs_out").await;
    let user = stack.api.as_user(&session);

    user.get_ok("/account").await.ok(200);
    let logout = user.post_ok("/auth/logout", json!({})).await;
    assert_eq!(
        logout.status, 204,
        "the contract documents POST /auth/logout as 204; got {} {}",
        logout.status, logout.raw
    );
    user.get_ok("/account").await.error(401, "UNAUTHORIZED");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn one_users_token_cannot_touch_another_users_order() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, alice) = funded_user(api, 0, "alice_orders").await;
    let (_, bob) = funded_user(api, 1, "bob_orders").await;

    let order_id =
        cb_e2e::flows::rest_limit(&alice, "BTC-USD", "buy", "40000.00", "0.02000000").await;

    // Bob sees nothing of Alice's.
    let bobs_orders = bob.get_ok("/account/orders?status=all").await;
    let listed = bobs_orders.ok(200)["orders"]
        .as_array()
        .expect("an orders list")
        .iter()
        .any(|order| order["order_id"].as_str() == Some(order_id.as_str()));
    assert!(
        !listed,
        "Bob's own order list contains Alice's order {order_id}. Body: {}",
        bobs_orders.raw
    );

    // And cannot act on it. The contract's table offers FORBIDDEN for
    // "authenticated but not permitted" and NOT_FOUND for an unknown order;
    // either is a defensible answer, and a 2xx is not.
    let attempt = bob.delete_ok(&format!("/orders/{order_id}")).await;
    assert!(
        matches!(attempt.status, 403 | 404),
        "Bob cancelling Alice's order returned {} {}. Expected 403 FORBIDDEN or 404 NOT_FOUND.",
        attempt.status,
        attempt.raw
    );
    assert!(
        matches!(attempt.error_code(), Some("FORBIDDEN") | Some("NOT_FOUND")),
        "Bob cancelling Alice's order returned an undocumented error code: {}",
        attempt.raw
    );

    // The order is still Alice's, and still resting. A refusal that happened to
    // cancel the order anyway would be worse than an allowed cancel.
    cb_e2e::flows::await_book_level(&alice, "BTC-USD", "buy", "40000.00", true).await;
    let still = alice.await_order_status(&order_id, &["open"]).await;
    assert_eq!(still["status"].as_str(), Some("open"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_revoked_session_stops_working_and_the_current_one_survives() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let first = sign_in(api, 0, "two_sessions").await;
    // The same address signing in again is a second session, not a second user.
    let second = api
        .post_ok(
            "/auth/dev-login",
            json!({ "mnemonic": TEST_MNEMONIC, "index": 0 }),
        )
        .await;
    let second_token = second.ok(200)["session_token"]
        .as_str()
        .expect("a second session token")
        .to_string();

    let keeper = api.with_token(second_token);
    let revoked = keeper.post_ok("/user/sessions/revoke-all", json!({})).await;
    let count = revoked.ok(200)["revoked"].as_u64().unwrap_or(0);
    assert!(
        count >= 1,
        "revoke-all reported {count} sessions revoked; the other session should have been \
         one of them. Body: {}",
        revoked.raw
    );

    api.with_token(first.token)
        .get_ok("/account")
        .await
        .error(401, "UNAUTHORIZED");
    keeper.get_ok("/account").await.ok(200);
}
