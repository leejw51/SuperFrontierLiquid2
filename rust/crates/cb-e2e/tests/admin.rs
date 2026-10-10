//! The operator surface: who may reach it, and what it does when they do.
//!
//! Two different things are being tested here and they are worth separating.
//!
//! The first is **who the operator is**. A role is a claim about a person, and
//! the only safe way to make that claim is against something the person cannot
//! choose. Granting it by matching a self-chosen handle means the first
//! stranger to register `admin` owns the exchange — they can halt markets,
//! suspend accounts and mint from the faucet. That escalation was found against
//! a live stack, and `a_stranger_who_registers_the_handle_admin_gets_no_powers`
//! exists so it cannot come back quietly.
//!
//! The second is **what the operator surface promises**: that halting a market
//! stops new orders without trapping the ones already resting, and that a dry
//! run tells you what a change would do without doing it. Both are safety
//! features for the operator, and both are the kind of thing that silently
//! stops working.
//!
//! Every test here boots its own stack, so "the first account to register" is
//! always an account this test created.

use cb_e2e::flows::{funded_user, has_role, rest_limit, sign_in, sign_in_admin};
use cb_e2e::money::assert_number_conventions;
use cb_e2e::stack::{admin_address, gateway_startup_with_secret, AdminGrant};
use cb_e2e::{Stack, StackConfig};
use serde_json::json;

// ---------------------------------------------------------------------------
// Who the operator is
// ---------------------------------------------------------------------------

/// The operator role must not be obtainable by choosing a handle.
///
/// The escalation: the role was granted by string-matching a handle the user
/// picks at sign-up, so whoever registered `admin` first became the operator.
/// There is no exploit to write — you sign up, you type a name, and the admin
/// navigation appears. The fix binds the role to an address, which nobody can
/// claim without the key.
///
/// The stranger here is a different account from the one this stack trusts, and
/// it registers the handle `admin` deliberately. It must get nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_stranger_who_registers_the_handle_admin_gets_no_operator_powers() {
    let stack = Stack::start().await;
    let api = &stack.api;

    // Account index 7 is nobody. It picks the most obvious handle there is.
    let stranger = sign_in(api, 7, "admin").await;
    let stranger_api = api.as_user(&stranger);

    let session = stranger_api.get_ok("/auth/session").await;
    let roles = session.ok(200)["roles"].clone();
    assert!(
        !has_role(&roles, "admin"),
        "an ordinary account that registered the handle `admin` was given the admin role \
         (roles: {roles}).\n\
         The stack grants the operator role to {:?}, and this account is not it. A role that \
         can be claimed by typing a name at sign-up is not a role: whoever signs up first \
         can halt every market, suspend every account and mint from the faucet.",
        stack.admin_grant()
    );

    // And the role is not the boundary anyway — the contract says the gateway
    // enforces it on every request regardless of what the client believes. So
    // the endpoints themselves are tried.
    for path in [
        "/admin/overview",
        "/admin/users",
        "/admin/markets",
        "/admin/faucet",
        "/admin/audit",
    ] {
        let reply = stranger_api.get_ok(path).await;
        assert_eq!(
            reply.status, 403,
            "GET {path} as an account holding the handle `admin` returned {}: {}",
            reply.status, reply.raw
        );
        assert_eq!(reply.error_code(), Some("FORBIDDEN"), "{}", reply.raw);
    }

    // Mutating routes too. Being unable to read the dashboard is no comfort if
    // you can still halt a market.
    let halt = stranger_api
        .post_ok(
            "/admin/markets/BTC-USD/halt",
            json!({ "reason": "because I can" }),
        )
        .await;
    assert_eq!(
        halt.status, 403,
        "an account holding the handle `admin` was able to halt a market: {} {}",
        halt.status, halt.raw
    );
}

/// The gateway must refuse to run with the server secret the example ships.
///
/// Every account's trading key is `HKDF-SHA256(server_secret, login_address)`.
/// The secret is therefore not a configuration detail; it is the master key to
/// every account on the deployment. `.env.example` ships a working constant so
/// the simulator boots out of the box, which means anyone who copies the
/// example verbatim is running with a key that is published in this repository.
///
/// A warning in a log is not enough, because by the time anyone reads it the
/// keys have already been derived. The binary has to refuse.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts the gateway binary; run with `make e2e`"]
async fn the_gateway_refuses_to_start_with_a_placeholder_or_short_server_secret() {
    let shipped = cb_e2e::shipped::shipped_env();
    let example = shipped
        .get("GATEWAY_SERVER_SECRET")
        .cloned()
        .expect("the shipped environment sets a server secret");

    let mut accepted = Vec::new();
    let mut unexplained = Vec::new();
    for (what, secret) in [
        ("the value shipped in .env.example", example.as_str()),
        ("an empty secret", ""),
        ("a four-character secret", "abcd"),
        ("an obvious placeholder", "changeme"),
        ("another obvious placeholder", "secret"),
    ] {
        let outcome = gateway_startup_with_secret(secret).await;
        if !outcome.refused() {
            accepted.push(format!("  {what} ({secret:?})"));
            continue;
        }
        // It has to refuse *for this reason*. A gateway that fell over for an
        // unrelated cause — a port clash, a missing file — would satisfy a bare
        // "did it exit?" check and leave the secret unvalidated.
        if !outcome.output().to_lowercase().contains("secret") {
            unexplained.push(format!(
                "  {what} ({secret:?}) exited without mentioning the secret:\n{}",
                outcome.output()
            ));
        }
    }
    assert!(
        unexplained.is_empty(),
        "the gateway exited for {} configuration(s), but not visibly because of the \
         secret:\n{}",
        unexplained.len(),
        unexplained.join("\n"),
    );

    assert!(
        accepted.is_empty(),
        "the gateway started with {} server secret(s) it should have refused:\n{}\n\n\
         Every trading key on the deployment is HKDF(server_secret, address). A published \
         or guessable secret is every account's key, and the operator finds out only when \
         the money moves.",
        accepted.len(),
        accepted.join("\n"),
    );

    // The refusal must be a refusal to *run*, not a refusal of every request:
    // a strong secret still has to work, or the check is just broken.
    let good = gateway_startup_with_secret(cb_e2e::stack::E2E_SERVER_SECRET).await;
    assert!(
        !good.refused(),
        "the gateway also refused a long, non-placeholder secret, so the check rejects \
         everything: {}",
        good.output()
    );
}

/// The operator role reaches the operator surface, and an ordinary account does
/// not. The positive half matters as much as the negative: a suite where admin
/// is refused for everybody would pass the escalation test and prove nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_operator_reaches_the_admin_surface_and_an_ordinary_account_does_not() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let (_, user) = funded_user(api, 3, "ordinary").await;

    let overview = admin.get_ok("/admin/overview").await;
    let body = overview.ok(200);
    assert_number_conventions(body, "GET /admin/overview");
    for block in [
        "chain", "users", "markets", "volume", "faucet", "risk", "health",
    ] {
        assert!(
            body.get(block).is_some(),
            "GET /admin/overview is missing the documented `{block}` block: {}",
            overview.raw
        );
    }
    assert!(
        body["health"].as_array().is_some_and(|h| !h.is_empty()),
        "the overview reports no health components at all: {}",
        overview.raw
    );

    for path in [
        "/admin/users",
        "/admin/markets",
        "/admin/faucet",
        "/admin/audit",
    ] {
        let reply = admin.get_ok(path).await;
        assert_number_conventions(reply.ok(200), &format!("GET {path}"));
        user.get_ok(path).await.error(403, "FORBIDDEN");
    }
}

// ---------------------------------------------------------------------------
// What the operator surface does
// ---------------------------------------------------------------------------

/// Halting a market stops new orders and still lets people out.
///
/// The contract is explicit about why: "trapping someone's order with no way
/// out would be worse than the condition that caused the halt". A halt that
/// also blocked cancels would turn an oracle problem into a hostage situation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn halting_a_market_refuses_new_orders_and_still_allows_cancels() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let (_, trader) = funded_user(api, 3, "halt_trader").await;

    let resting = rest_limit(&trader, "BTC-USD", "buy", "41000.00", "0.02000000").await;

    let halt = admin
        .post_ok(
            "/admin/markets/BTC-USD/halt",
            json!({ "reason": "oracle feed stale" }),
        )
        .await;
    assert!(
        (200..300).contains(&halt.status),
        "halting BTC-USD failed: {} {}",
        halt.status,
        halt.raw
    );

    let listed = api.get_ok("/markets").await;
    let status = listed.ok(200)["markets"]
        .as_array()
        .expect("a market list")
        .iter()
        .find(|m| m["symbol"].as_str() == Some("BTC-USD"))
        .map(|m| m["status"].clone())
        .expect("BTC-USD is listed");
    assert_eq!(
        status.as_str(),
        Some("halted"),
        "the market list still reports BTC-USD as {status} after it was halted"
    );

    trader
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "41000.00", "qty": "0.02000000" }),
        )
        .await
        .error(422, "MARKET_HALTED");

    let cancel = trader.delete_ok(&format!("/orders/{resting}")).await;
    assert_eq!(
        cancel.status, 200,
        "cancelling a resting order on a halted market was refused with {} {}.\n\
         A halt stops new risk; it must not trap the risk already there.",
        cancel.status, cancel.raw
    );

    // Resuming puts the market back.
    let resume = admin
        .post_ok("/admin/markets/BTC-USD/resume", json!({}))
        .await;
    assert!(
        (200..300).contains(&resume.status),
        "resuming BTC-USD failed: {} {}",
        resume.status,
        resume.raw
    );
    trader
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "41000.00", "qty": "0.02000000" }),
        )
        .await
        .ok(201);
}

/// A risk change reports whom it affects, and a dry run changes nothing.
///
/// Raising a margin requirement can put open accounts below maintenance the
/// moment it lands. The contract's answer is that the operator finds out before
/// the liquidation engine does: the response names the accounts, and `dry_run`
/// computes the consequence without applying it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_risk_change_reports_who_it_affects_and_a_dry_run_changes_nothing() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;

    let before = market_risk(api, "BTC-PERP").await;

    let dry = admin
        .patch_ok(
            "/admin/markets/BTC-PERP/risk?dry_run=true",
            json!({ "max_leverage": "20", "initial_margin_bps": 500,
                    "maintenance_margin_bps": 250 }),
        )
        .await;
    let dry_body = dry.ok(200);
    assert_number_conventions(dry_body, "PATCH /admin/markets/{symbol}/risk?dry_run=true");
    assert_eq!(
        dry_body["applied"].as_bool(),
        Some(false),
        "a dry run reported itself as applied: {}",
        dry.raw
    );
    for field in ["accounts_now_below_maintenance", "affected"] {
        assert!(
            dry_body.get(field).is_some(),
            "the dry run does not report `{field}`, so the operator learns nothing from it: {}",
            dry.raw
        );
    }
    assert!(
        dry_body["affected"].is_array(),
        "`affected` must be the list of accounts, not just a count: {}",
        dry.raw
    );

    let after_dry = market_risk(api, "BTC-PERP").await;
    assert_eq!(
        after_dry, before,
        "a dry run changed the market's risk parameters from {before:?} to {after_dry:?}. \
         The whole point is to find out what a change would do without doing it."
    );

    // And the real thing does apply.
    let applied = admin
        .patch_ok(
            "/admin/markets/BTC-PERP/risk",
            json!({ "max_leverage": "20", "initial_margin_bps": 500,
                    "maintenance_margin_bps": 250 }),
        )
        .await;
    assert_eq!(
        applied.ok(200)["applied"].as_bool(),
        Some(true),
        "{}",
        applied.raw
    );

    let after = market_risk(api, "BTC-PERP").await;
    assert_ne!(
        after, before,
        "the risk change reported `applied: true` and the market still reads {after:?}"
    );

    // Every mutating action is audited, so "who changed this" has an answer.
    let audit = admin.get_ok("/admin/audit?limit=20").await;
    let records = audit.ok(200)["records"].as_array().expect("an audit list");
    let found = records
        .iter()
        .find(|record| record["subject"].as_str() == Some("BTC-PERP"));
    let record = found.unwrap_or_else(|| {
        panic!(
            "the risk change left no audit record. Records: {}",
            audit.raw
        )
    });
    for field in ["record_id", "actor", "action", "subject", "detail", "at"] {
        assert!(
            record.get(field).is_some(),
            "an audit record is missing `{field}`: {record}"
        );
    }
}

/// The market's risk parameters as a client sees them.
async fn market_risk(api: &cb_e2e::Api, symbol: &str) -> (String, i64, i64) {
    let reply = api.get_ok("/markets").await;
    let market = reply.ok(200)["markets"]
        .as_array()
        .expect("a market list")
        .iter()
        .find(|m| m["symbol"].as_str() == Some(symbol))
        .cloned()
        .unwrap_or_else(|| panic!("{symbol} is not listed"));
    (
        market["max_leverage"].as_str().unwrap_or("").to_string(),
        market["initial_margin_bps"].as_i64().unwrap_or(-1),
        market["maintenance_margin_bps"].as_i64().unwrap_or(-1),
    )
}

/// One claim of a multi-asset grant is one claim on the operator dashboard too.
///
/// The user-facing counter (`GET /faucet`'s `claims_made`) is right. The
/// operator's is not: a single claim of the shipped two-asset grant is reported
/// as two, because the dashboard counts ledger rows rather than claims. Every
/// faucet number an operator uses to decide whether the faucet is being abused
/// is therefore inflated by the size of the grant — and the size of the grant
/// is a setting they can change, so the inflation moves under them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn one_multi_asset_claim_is_one_claim_on_the_operator_dashboard() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;

    // Exactly one account, claiming exactly once, on a chain nobody else has
    // touched. Anything other than 1 is counting something that is not claims.
    let (_, user) = funded_user(api, 3, "single_claimer").await;
    let grant_assets = user.get_ok("/faucet").await.ok(200)["grant"]
        .as_array()
        .expect("the faucet advertises its grant")
        .len();
    assert!(
        grant_assets >= 2,
        "the stack's grant has {grant_assets} asset(s); the miscount only appears with more \
         than one"
    );

    let faucet = admin.get_ok("/admin/faucet").await;
    let faucet_body = faucet.ok(200);
    assert_number_conventions(faucet_body, "GET /admin/faucet");
    for field in [
        "enabled",
        "cooldown_seconds",
        "grant",
        "max_claims_per_user",
        "claims_24h",
        "granted_total",
        "authority_balance",
    ] {
        assert!(
            faucet_body.get(field).is_some(),
            "GET /admin/faucet is missing the documented `{field}`: {}",
            faucet.raw
        );
    }

    let overview = admin.get_ok("/admin/overview").await;
    let claims = overview.ok(200)["faucet"]["claims_24h"].as_u64();
    assert_eq!(
        claims,
        Some(1),
        "one account claimed once, of a {grant_assets}-asset grant, and the operator \
         dashboard reports {} claims in 24 hours. The user's own `claims_made` says 1. \
         Counting ledger rows scales every faucet figure an operator reads by the number of \
         assets in the grant.",
        overview.body["faucet"]["claims_24h"]
    );
}

/// Changing the faucet settings changes what users are offered.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn changing_the_faucet_settings_changes_what_a_user_is_offered() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let session = sign_in(api, 3, "faucet_watcher").await;
    let user = api.as_user(&session);

    let patched = admin
        .patch_ok(
            "/admin/faucet",
            json!({ "enabled": true, "cooldown_seconds": 1800,
                    "grant": [ { "asset": "USD", "amount": "250000.00" } ] }),
        )
        .await;
    patched.ok(200);

    let offered = user.get_ok("/faucet").await;
    let body = offered.ok(200);
    assert_eq!(
        body["cooldown_seconds"].as_u64(),
        Some(1800),
        "the operator set the cooldown to 1800 and the user is still told {}: {}",
        body["cooldown_seconds"],
        offered.raw
    );
    let grant = body["grant"].as_array().expect("a grant list");
    assert_eq!(
        grant.len(),
        1,
        "the operator set a one-asset grant and the user is offered {}: {}",
        grant.len(),
        offered.raw
    );
    assert_eq!(
        grant[0]["amount"].as_str(),
        Some("250000.00"),
        "{}",
        offered.raw
    );
}

/// The operator can open one account's full record.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_operator_can_open_one_users_full_record() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let (session, user) = funded_user(api, 3, "inspected").await;
    rest_limit(&user, "BTC-USD", "buy", "38000.00", "0.02000000").await;

    let listing = admin.get_ok("/admin/users?limit=100").await;
    let listed = listing.ok(200)["users"]
        .as_array()
        .expect("a users list")
        .iter()
        .find(|u| u["user_id"].as_str() == Some(session.user_id.as_str()))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "the account is not in the operator's user list: {}",
                listing.raw
            )
        });
    for field in [
        "user_id",
        "handle",
        "address",
        "is_bot",
        "joined_at",
        "last_seen_at",
        "equity",
        "open_positions",
        "trades_24h",
        "faucet_claims",
        "status",
    ] {
        assert!(
            listed.get(field).is_some(),
            "a user row is missing the documented `{field}`: {listed}"
        );
    }
    assert_eq!(listed["status"].as_str(), Some("active"), "{listed}");

    let detail = admin
        .get_ok(&format!("/admin/users/{}", session.user_id))
        .await;
    assert_number_conventions(
        detail.ok(200),
        &format!("GET /admin/users/{}", session.user_id),
    );

    // A search by handle has to find them, or the operator cannot act on a
    // report that names a person.
    let searched = admin.get_ok("/admin/users?query=inspected").await;
    assert!(
        searched.ok(200)["users"]
            .as_array()
            .is_some_and(|users| users
                .iter()
                .any(|u| u["user_id"].as_str() == Some(session.user_id.as_str()))),
        "searching the operator's user list for the handle found nothing: {}",
        searched.raw
    );
}

/// A suspended account cannot trade or claim, and its positions are left alone.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn suspending_an_account_stops_it_trading_without_touching_its_orders() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let (session, trader) = funded_user(api, 3, "suspendee").await;

    let resting = rest_limit(&trader, "BTC-USD", "buy", "42000.00", "0.02000000").await;

    let suspend = admin
        .post_ok(
            &format!("/admin/users/{}/suspend", session.user_id),
            json!({ "reason": "abusive order flow" }),
        )
        .await;
    assert!(
        (200..300).contains(&suspend.status),
        "suspending {} failed: {} {}",
        session.user_id,
        suspend.status,
        suspend.raw
    );

    let order = trader
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "42000.00", "qty": "0.02000000" }),
        )
        .await;
    assert_eq!(
        order.status, 403,
        "a suspended account placed an order: {} {}",
        order.status, order.raw
    );
    assert_eq!(
        order.error_code(),
        Some("ACCOUNT_SUSPENDED"),
        "{}",
        order.raw
    );

    trader
        .post_ok("/faucet/claim", json!({}))
        .await
        .error(403, "ACCOUNT_SUSPENDED");

    // The existing order is still there, seen from the public book — which is
    // the view that does not depend on the suspended account's own access. The
    // contract is explicit that force-closing someone's position is a different
    // and much larger decision than stopping them trading.
    cb_e2e::flows::await_book_level(api, "BTC-USD", "buy", "42000.00", true).await;

    // A suspended account must still be able to *look*. The contract suspends
    // two specific abilities — placing orders and claiming the faucet — and
    // says in the same breath that existing positions stay open. Someone who
    // cannot see the position they are still carrying cannot manage the risk
    // they have been left holding, and has no way to find out what happened.
    let readable = trader.get_ok("/account/orders?status=open").await;
    assert_eq!(
        readable.status, 200,
        "a suspended account cannot read its own order list: {} {}.\n\
         Suspension is documented as stopping new orders and faucet claims, with existing \
         positions deliberately left open. Blocking the read as well leaves the account \
         carrying risk it cannot see.",
        readable.status, readable.raw
    );
    let still = readable.ok(200)["orders"]
        .as_array()
        .expect("an orders list")
        .iter()
        .find(|order| order["order_id"].as_str() == Some(resting.as_str()))
        .cloned()
        .expect("the resting order is still the account's own");
    assert_eq!(still["status"].as_str(), Some("open"));

    let unsuspend = admin
        .post_ok(
            &format!("/admin/users/{}/unsuspend", session.user_id),
            json!({}),
        )
        .await;
    assert!(
        (200..300).contains(&unsuspend.status),
        "unsuspending failed: {} {}",
        unsuspend.status,
        unsuspend.raw
    );
    trader
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "42000.00", "qty": "0.02000000" }),
        )
        .await
        .ok(201);
}

/// An operator grant bypasses the cooldown and is audited like everything else.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn an_operator_faucet_grant_bypasses_the_cooldown_and_is_audited() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, admin) = sign_in_admin(&stack).await;
    let (session, user) = funded_user(api, 3, "granted_to").await;

    // The user is inside their cooldown: their own claim is refused.
    user.post_ok("/faucet/claim", json!({}))
        .await
        .error(429, "FAUCET_COOLDOWN");

    let before = user.free_balance("USD").await;
    let grant = admin
        .post_ok(
            "/admin/faucet/grant",
            json!({ "user_id": session.user_id,
                    "grants": [ { "asset": "USD", "amount": "1000000.00" } ],
                    "reason": "load test" }),
        )
        .await;
    let body = grant.ok(200);
    assert!(
        body.get("claim_id").is_some() && body.get("tx_hash").is_some(),
        "an operator grant should report a claim id and a transaction: {}",
        grant.raw
    );

    user.await_account("the operator grant to arrive", |account| {
        cb_e2e::client::balance_entry(account, "USD", "free") > before
    })
    .await;
}

/// A stack booted with the operator bound to an address is the configuration
/// that should ship, and this says so out loud.
///
/// It is not an assertion about behaviour so much as a tripwire: when the
/// gateway gains `--admin-address`, this starts passing, and if the flag is
/// ever removed again it starts failing. Until then it records the state of the
/// world in the one place somebody running the suite will see it.
#[test]
#[ignore = "inspects the gateway binary; run with `make e2e`"]
fn the_operator_role_can_be_bound_to_an_address_rather_than_a_handle() {
    let config = StackConfig::default();
    assert_eq!(
        config.admin,
        AdminGrant::Address(admin_address().to_string()),
        "the installed cb-gateway has no `--admin-address` flag, so the operator role can \
         only be granted by matching a handle the user chooses at sign-up. Whoever registers \
         that handle first becomes the operator. Run `cb-gateway --help` to confirm."
    );
}
