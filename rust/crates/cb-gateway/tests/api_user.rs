//! §9 — the user page.

mod common;

use cb_types::AssetId;
use common::Api;
use serde_json::json;

#[tokio::test]
async fn the_profile_reports_the_account_and_its_statistics() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let reply = api.get_as("/api/v1/user/profile", &player).await;
    let body = reply.assert_ok(200);

    assert_eq!(body["handle"], "satoshi");
    assert_eq!(
        body["address"],
        "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
    );
    assert_eq!(body["is_bot"], false);
    assert_eq!(body["roles"], json!(["user"]));
    assert!(body["joined_at"].as_str().is_some());
    assert!(body["last_seen_at"].as_str().is_some());

    let stats = &body["stats"];
    assert_eq!(stats["trades"], 0);
    assert_eq!(stats["volume_30d"], "0.00");
    assert_eq!(stats["realized_pnl_30d"], "0.00");
    assert_eq!(stats["markets_traded"], 0);
    assert_eq!(stats["liquidations"], 0);
    assert_eq!(stats["faucet_claims"], 0);
    assert_eq!(stats["best_rank"], json!(null));
}

#[tokio::test]
async fn the_profile_needs_a_session() {
    let api = Api::new();
    api.get("/api/v1/user/profile")
        .await
        .assert_error(401, "UNAUTHORIZED");
}

#[tokio::test]
async fn the_faucet_claim_count_reaches_the_profile() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    let reply = api.get_as("/api/v1/user/profile", &player).await;
    assert_eq!(reply.assert_ok(200)["stats"]["faucet_claims"], 1);
}

#[tokio::test]
async fn the_handle_can_be_changed_and_nothing_else_can() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;

    let reply = api
        .patch_as(
            "/api/v1/user/profile",
            &player,
            json!({ "handle": "satoshi_v2" }),
        )
        .await;
    let body = reply.assert_ok(200);
    assert_eq!(body["handle"], "satoshi_v2");
    assert_eq!(
        body["address"], "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
        "the address is the identity and does not move"
    );

    // The new handle is what the session reports from now on.
    let session = api.get_as("/api/v1/auth/session", &player).await;
    assert_eq!(session.assert_ok(200)["handle"], "satoshi_v2");
}

#[tokio::test]
async fn changing_to_a_taken_handle_is_a_conflict() {
    let api = Api::new();
    let alice = api.player("alice", 0).await;
    let _bob = api.player("bob", 1).await;
    api.patch_as("/api/v1/user/profile", &alice, json!({ "handle": "bob" }))
        .await
        .assert_error(409, "HANDLE_TAKEN");
}

#[tokio::test]
async fn an_invalid_handle_is_rejected_and_an_empty_patch_says_so() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.patch_as("/api/v1/user/profile", &player, json!({ "handle": "no" }))
        .await
        .assert_error(400, "VALIDATION_FAILED");
    api.patch_as("/api/v1/user/profile", &player, json!({}))
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn the_session_list_marks_the_current_session() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let reply = api.get_as("/api/v1/user/sessions", &player).await;
    let sessions = reply.assert_ok(200)["sessions"]
        .as_array()
        .expect("sessions")
        .clone();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["current"], true);
    assert!(sessions[0]["session_id"]
        .as_str()
        .expect("id")
        .starts_with("ses_"));
    assert!(sessions[0]["issued_at"].as_str().is_some());
    assert!(sessions[0]["last_used_at"].as_str().is_some());
}

#[tokio::test]
async fn a_second_login_appears_in_the_session_list() {
    let api = Api::new();
    let first = api.player("satoshi", 0).await;
    let second = api.harness.login("satoshi", 0).await;

    let reply = api.get_as("/api/v1/user/sessions", &second).await;
    let sessions = reply.assert_ok(200)["sessions"]
        .as_array()
        .expect("sessions")
        .clone();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions.iter().filter(|s| s["current"] == true).count(),
        1,
        "exactly one session is the one asking"
    );
    let _ = first;
}

#[tokio::test]
async fn a_session_can_be_revoked_by_id() {
    let api = Api::new();
    let first = api.player("satoshi", 0).await;
    let second = api.harness.login("satoshi", 0).await;

    let sessions = api.get_as("/api/v1/user/sessions", &second).await;
    let target = sessions.assert_ok(200)["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|s| s["current"] == false)
        .expect("the other session")
        .clone();
    let session_id = target["session_id"].as_str().expect("id");

    let reply = api
        .delete_as(&format!("/api/v1/user/sessions/{session_id}"), &second)
        .await;
    assert_eq!(reply.status.as_u16(), 204);

    // The revoked one no longer authenticates; the current one still does.
    api.get_as("/api/v1/auth/session", &first)
        .await
        .assert_error(401, "UNAUTHORIZED");
    api.get_as("/api/v1/auth/session", &second)
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn one_player_cannot_revoke_anothers_session() {
    let api = Api::new();
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;

    let alices = api.get_as("/api/v1/user/sessions", &alice).await;
    let session_id = alices.assert_ok(200)["sessions"][0]["session_id"]
        .as_str()
        .expect("id")
        .to_string();

    api.delete_as(&format!("/api/v1/user/sessions/{session_id}"), &bob)
        .await
        .assert_error(404, "NOT_FOUND");
    // Alice's session is untouched.
    api.get_as("/api/v1/auth/session", &alice)
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn revoking_all_sessions_keeps_the_one_asking() {
    let api = Api::new();
    let first = api.player("satoshi", 0).await;
    let second = api.harness.login("satoshi", 0).await;
    let third = api.harness.login("satoshi", 0).await;

    let reply = api
        .post_as("/api/v1/user/sessions/revoke-all", &third, json!({}))
        .await;
    assert_eq!(reply.assert_ok(200)["revoked"], 2);

    api.get_as("/api/v1/auth/session", &third)
        .await
        .assert_ok(200);
    for revoked in [&first, &second] {
        api.get_as("/api/v1/auth/session", revoked)
            .await
            .assert_error(401, "UNAUTHORIZED");
    }
}

/// Drive the block carrying everything submitted so far through the watcher.
///
/// The activity feed reads the ledger, and the ledger records what the chain
/// applied rather than what the gateway submitted (§20 of the flow review), so
/// a test that wants to see a grant in the feed has to let the block land.
async fn settle_the_block(api: &Api, height: u64) {
    let block = api.harness.node.mine(height);
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");
}

#[tokio::test]
async fn the_activity_feed_merges_everything_newest_first() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    settle_the_block(&api, 11).await;

    let reply = api.get_as("/api/v1/user/activity", &player).await;
    let body = reply.assert_ok(200);
    let entries = body["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "one per granted asset");
    assert_eq!(entries[0]["kind"], "faucet");
    assert!(entries[0]["summary"]
        .as_str()
        .expect("summary")
        .contains("faucet"));
    assert!(entries[0]["id"].as_str().expect("id").starts_with("act_"));
    assert_eq!(body["next_cursor"], json!(null), "one page holds it all");
}

#[tokio::test]
async fn the_activity_feed_pages_with_an_opaque_cursor() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    // Several claims across time, so there is something to page through.
    for round in 0..3 {
        api.post_as("/api/v1/faucet/claim", &player, json!({}))
            .await
            .assert_ok(200);
        settle_the_block(&api, 11 + round).await;
        api.harness.clock.advance(chrono::Duration::hours(2));
    }

    let first = api.get_as("/api/v1/user/activity?limit=2", &player).await;
    let body = first.assert_ok(200);
    assert_eq!(body["entries"].as_array().expect("entries").len(), 2);
    let cursor = body["next_cursor"].as_str().expect("a cursor").to_string();

    let second = api
        .get_as(
            &format!("/api/v1/user/activity?limit=2&cursor={cursor}"),
            &player,
        )
        .await;
    let more = second.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .len();
    assert!(more > 0, "the cursor must advance rather than repeat");
}

#[tokio::test]
async fn a_corrupt_cursor_is_rejected_rather_than_ignored() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.get_as("/api/v1/user/activity?cursor=nonsense", &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn one_players_activity_never_shows_anothers() {
    let api = Api::new();
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;
    api.post_as("/api/v1/faucet/claim", &alice, json!({}))
        .await
        .assert_ok(200);
    settle_the_block(&api, 11).await;

    let hers = api.get_as("/api/v1/user/activity", &alice).await;
    assert!(!hers.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .is_empty());
    let his = api.get_as("/api/v1/user/activity", &bob).await;
    assert_eq!(his.assert_ok(200)["entries"], json!([]));
    let _ = AssetId::Usd;
}

#[tokio::test]
async fn revoking_a_session_closes_the_sockets_it_opened() {
    // §14 of the flow review, through the handler rather than the hub: sign in
    // twice, open the private channel on both, then revoke everything but the
    // session making the request. The revoked tab's REST calls already failed;
    // what it kept was the socket, and the socket is where the private data
    // actually flows.
    let api = Api::new();
    let first = api.player("satoshi", 0).await;
    let second = api.harness.login("satoshi", 0).await;
    assert_eq!(first.user_id, second.user_id, "one account, two sessions");

    let hub = &api.harness.state.hub;
    let attach = |player: &cb_gateway::testing::Player| {
        let (id, rx) = hub.register();
        hub.authenticate(
            id,
            &player.user_id,
            &cb_gateway::auth::session_token_hash(&player.token),
        );
        hub.subscribe(id, cb_gateway::ws::Channel::Account)
            .expect("the account channel");
        (id, rx)
    };
    let (old_tab, mut old_frames) = attach(&first);
    let (current_tab, _current_frames) = attach(&second);

    let reply = api
        .post_as("/api/v1/user/sessions/revoke-all", &second, json!({}))
        .await;
    assert_eq!(reply.assert_ok(200)["revoked"], 1);

    assert!(hub.user_of(old_tab).is_none(), "the revoked tab is closed");
    assert_eq!(
        hub.user_of(current_tab).as_deref(),
        Some(second.user_id.as_str()),
        "the tab making the request keeps its socket"
    );
    let mut told = false;
    while let Ok(frame) = old_frames.try_recv() {
        told |= frame.contains("UNAUTHORIZED");
    }
    assert!(told, "the closed tab is told why");

    // And logging out closes the remaining one.
    api.post_as("/api/v1/auth/logout", &second, json!({}))
        .await
        .assert_ok(204);
    assert!(hub.user_of(current_tab).is_none());
}

/// The failure this pins: the faucet is usually the first transaction an
/// account makes, and a claim that did not prime the account mirror left the
/// watcher with nothing to difference the grant against. It seeded silently,
/// no balance update was published, and the browser's equity read zero.
#[tokio::test]
async fn a_faucet_claim_primes_the_account_mirror_so_the_grant_is_published() {
    let api = Api::with_markets().await;
    let player = api.player("primed", 3).await;
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    let user_id = player.user_id.clone();
    assert!(
        api.harness.state.mirror.baseline(&user_id).is_some(),
        "the claim must leave a baseline for the watcher to difference the grant against"
    );
}
