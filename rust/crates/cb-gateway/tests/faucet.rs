//! §3 — the faucet, at its boundaries.
//!
//! The faucet is the only mint in the system, and `total_granted` is what a
//! conservation audit reconciles against. So every claim must be paid once,
//! recorded once and cooled down exactly as long as advertised — and a grant
//! the ledger cannot represent must be refused before anything is written or
//! minted, rather than recorded as zero and minted in full.

mod common;

use cb_gateway::faucet::{FaucetConfig, Grant};
use cb_gateway::testing::{Harness, Player};
use cb_tx::TxBody;
use cb_types::AssetId;
use common::Api;
use serde_json::json;

/// Every deposit the gateway has submitted, as (recipient, asset, amount).
fn deposits(api: &Api) -> Vec<(cb_types::Address, AssetId, u128)> {
    api.harness
        .node
        .submitted_bodies()
        .into_iter()
        .filter_map(|b| match b {
            TxBody::Deposit {
                user,
                asset,
                amount,
                ..
            } => Some((user, asset, amount)),
            _ => None,
        })
        .collect()
}

async fn claims_made(api: &Api, player: &Player) -> u64 {
    api.get_as("/api/v1/faucet", player).await.assert_ok(200)["claims_made"]
        .as_u64()
        .expect("a count")
}

#[tokio::test]
async fn the_cooldown_holds_until_its_last_second_and_lifts_exactly_at_its_end() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let first = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    let next_at = first.assert_ok(200)["next_available_at"]
        .as_str()
        .expect("next_available_at")
        .to_string();

    api.harness.clock.advance(chrono::Duration::seconds(3_599));
    let refused = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    refused.assert_error(429, "FAUCET_COOLDOWN");
    let details = &refused.body["error"]["details"];
    assert_eq!(
        details["retry_after_seconds"], 1,
        "one second left: {details}"
    );
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(details["next_available_at"].as_str().expect("at"))
            .expect("rfc3339"),
        chrono::DateTime::parse_from_rfc3339(&next_at).expect("rfc3339"),
        "the refusal names the instant the claim receipt promised"
    );
    let status = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(status.assert_ok(200)["available"], false);

    api.harness.clock.advance(chrono::Duration::seconds(1));
    let status = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(
        status.assert_ok(200)["available"],
        true,
        "the end is inclusive"
    );
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    assert_eq!(claims_made(&api, &player).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_from_one_user_pay_exactly_once() {
    // A double-click, or a script. The per-user lock is held across read,
    // record and mint, so the second request reads the first one's row.
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    let replies = futures_util::future::join_all(
        (0..8).map(|_| api.post_as("/api/v1/faucet/claim", &player, json!({}))),
    )
    .await;
    let paid = replies.iter().filter(|r| r.status.as_u16() == 200).count();
    assert_eq!(paid, 1, "paid once");
    for reply in replies.iter().filter(|r| r.status.as_u16() != 200) {
        reply.assert_error(429, "FAUCET_COOLDOWN");
    }
    assert_eq!(deposits(&api).len(), 2, "one claim's USD and BTC, once");
    assert_eq!(claims_made(&api, &player).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_from_many_users_share_one_treasury_key_without_losing_a_nonce() {
    // Every grant is signed by the one treasury key. Users do not contend
    // for the faucet lock, so this is the signer's per-address serialisation
    // under real contention: every deposit must carry its own nonce, or the
    // chain would keep one of two and a user would be told they were paid.
    let api = Api::new();
    let mut players = Vec::new();
    for i in 0..6 {
        players.push(api.player(&format!("player_{i}"), i).await);
    }
    let replies = futures_util::future::join_all(
        players
            .iter()
            .map(|p| api.post_as("/api/v1/faucet/claim", p, json!({}))),
    )
    .await;
    for reply in &replies {
        reply.assert_ok(200);
    }
    let submitted = api.harness.node.submitted();
    let mut nonces: Vec<u64> = submitted.iter().map(|t| t.tx.nonce).collect();
    nonces.sort_unstable();
    assert_eq!(
        nonces,
        (0..12).collect::<Vec<u64>>(),
        "twelve deposits, twelve nonces"
    );
    for player in &players {
        let mine = deposits(&api)
            .into_iter()
            .filter(|(user, _, _)| *user == player.trading_address)
            .count();
        assert_eq!(mine, 2, "{} was paid exactly once", player.user_id);
    }
}

#[tokio::test]
async fn a_grant_the_ledger_cannot_record_is_refused_before_anything_is_written_or_minted() {
    // The faucet's configuration can hold a base-unit amount that fits the
    // chain's u128 but not the ledger's decimal. The claim is refused as a
    // validation failure, no deposit is signed, and — because no row was
    // written — no cooldown started: once the configuration is corrected
    // the same user claims immediately.
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.harness.state.faucet.reconfigure(FaucetConfig {
        grants: vec![
            Grant {
                asset: AssetId::Usd,
                amount: 100_000_000_000,
            },
            Grant {
                asset: AssetId::Btc,
                amount: u128::MAX,
            },
        ],
        ..FaucetConfig::default()
    });
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(400, "VALIDATION_FAILED");
    assert!(
        deposits(&api).is_empty(),
        "not even the representable USD half"
    );
    assert_eq!(
        claims_made(&api, &player).await,
        0,
        "no row, so no cooldown"
    );

    api.harness
        .state
        .faucet
        .reconfigure(FaucetConfig::default());
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
}

#[tokio::test]
async fn an_operator_cannot_configure_or_grant_an_amount_the_ledger_cannot_record() {
    // The same amount through the operator's own endpoints. Accepting it into
    // the configuration would turn every player's claim into a validation
    // error about a request they did not write; it is the operator's input
    // and the operator should be told.
    let api = Api::from(Harness::builder().admins(&["root"]).build());
    let operator = api.player("root", 0).await;
    let player = api.player("satoshi", 1).await;
    let too_large = format!("1{}.00", "0".repeat(32)); // 10^32 USD: fits u128 base units, not Decimal

    api.patch_as(
        "/api/v1/admin/faucet",
        &operator,
        json!({ "grant": [ { "asset": "USD", "amount": too_large } ] }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
    let status = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(
        status.assert_ok(200)["grant"][0]["amount"],
        "100000.00",
        "the configuration is unchanged"
    );

    api.post_as(
        "/api/v1/admin/faucet/grant",
        &operator,
        json!({ "user_id": player.user_id,
                "grants": [ { "asset": "USD", "amount": too_large } ],
                "reason": "typo" }),
    )
    .await
    .assert_error(400, "VALIDATION_FAILED");
    assert!(deposits(&api).is_empty());
}

#[tokio::test]
async fn a_claim_that_fails_to_mint_keeps_its_cooldown_and_says_so() {
    // Record-first: a mint failure after the row is written costs the user
    // one cooldown window rather than letting them claim in a loop. The
    // error says the cooldown started, and the next attempt is refused.
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.harness.node.edit(|s| {
        s.submit_reply = Some(cb_gateway::node::SubmitOutcome::Rejected {
            code: cb_gateway::node::SubmitRejection::MempoolFull,
            message: "full".into(),
        })
    });
    let reply = api
        .post_as("/api/v1/faucet/claim", &player, json!({}))
        .await;
    reply.assert_error(429, "RATE_LIMITED");
    assert_eq!(reply.body["error"]["details"]["cooldown_started"], true);

    api.harness.node.edit(|s| s.submit_reply = None);
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "FAUCET_COOLDOWN");
}

#[tokio::test]
async fn a_lifetime_claim_quota_is_enforced_even_after_the_cooldown() {
    let api = Api::new();
    let player = api.player("satoshi", 0).await;
    api.harness.state.faucet.reconfigure(FaucetConfig {
        max_claims_per_user: Some(2),
        cooldown_seconds: 10,
        ..FaucetConfig::default()
    });
    for _ in 0..2 {
        api.post_as("/api/v1/faucet/claim", &player, json!({}))
            .await
            .assert_ok(200);
        api.harness.clock.advance(chrono::Duration::seconds(10));
    }
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_error(429, "FAUCET_COOLDOWN");
    let status = api.get_as("/api/v1/faucet", &player).await;
    assert_eq!(status.assert_ok(200)["available"], false);
    assert_eq!(deposits(&api).len(), 4);
}
