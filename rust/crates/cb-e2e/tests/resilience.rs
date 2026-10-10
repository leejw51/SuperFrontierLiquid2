//! What the gateway does when the chain goes away, and when it comes back.
//!
//! The gateway already has a reconnect path and it already has a test — against
//! a mock connector that returns an error when told to. That proves the code
//! handles an error value. It does not prove what happens when a real
//! Cap'n Proto session dies mid-request with a half-written frame on the wire,
//! which is the only version of this that ever actually occurs.
//!
//! Two things have to be true, and the second is the one people forget:
//!
//! 1. **The gateway answers.** A request that needs the sequencer must fail
//!    fast with `503 NODE_UNAVAILABLE`, not hang. Every client here has a ten
//!    second timeout, so a hang is a test failure rather than a slow pass — to
//!    a browser, a request that never returns and a request that returns an
//!    error are completely different experiences, and only one of them can be
//!    shown to a user.
//! 2. **It recovers by itself.** An operator restarting the sequencer should
//!    not also have to restart the gateway. If the gateway has to be bounced,
//!    every session is dropped and every user is signed out for a fault that
//!    was not theirs.
//!
//! The node is killed with SIGKILL rather than asked politely, because a clean
//! shutdown is the case that was already working.

use std::time::Duration;

use cb_e2e::flows::{funded_user, rest_limit};
use cb_e2e::wait::expect_within;
use cb_e2e::Stack;
use serde_json::json;

/// Long enough for a reconnect backoff and a fresh RocksDB open.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots and kills the real node; run with `make e2e`"]
async fn the_gateway_reports_503_while_the_node_is_down_and_recovers_when_it_returns() {
    let mut stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "outage").await;

    // Prove the system works before breaking it, or a 503 afterwards proves
    // nothing about the outage.
    rest_limit(&user, "BTC-USD", "buy", "40000.00", "0.02000000").await;

    stack.kill_node();

    // `GET /account` needs the chain: the balances come from state the gateway
    // does not hold. `/chain/status` is deliberately not used here — it may
    // legitimately serve a cached height and would report health that no longer
    // exists.
    let down = expect_within(
        "the gateway to answer 503 NODE_UNAVAILABLE with the sequencer dead",
        RECOVERY_TIMEOUT,
        || async {
            match user.get("/account").await {
                Ok(reply) if reply.status == 503 => Ok(reply),
                Ok(reply) => Err(format!("status {} body {}", reply.status, reply.raw)),
                // A transport error here is the failure this test exists to
                // catch: the request neither completed nor failed cleanly.
                Err(err) if err.is_timeout() => Err(format!(
                    "the request hung and timed out client-side: {err}. A browser shows this \
                     as a spinner that never stops, with nothing to retry and nothing to \
                     report."
                )),
                Err(err) => Err(format!("transport error {err}")),
            }
        },
    )
    .await;
    assert_eq!(
        down.error_code(),
        Some("NODE_UNAVAILABLE"),
        "with the sequencer dead, GET /account answered 503 with the wrong code: {}",
        down.raw
    );

    // Placing an order must fail the same way, and must not be accepted into a
    // void. Telling a client `201` for an order that reached nothing is worse
    // than refusing it.
    let order = user
        .post(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                                 "price": "40000.00", "qty": "0.02000000" }),
        )
        .await;
    match order {
        Ok(reply) => {
            assert_eq!(
                reply.status, 503,
                "an order was answered {} while the sequencer was dead: {}",
                reply.status, reply.raw
            );
            assert_eq!(
                reply.error_code(),
                Some("NODE_UNAVAILABLE"),
                "{}",
                reply.raw
            );
        }
        Err(err) => panic!("POST /orders hung or failed at the transport during an outage: {err}"),
    }

    // Public data that the gateway holds itself should keep working. A dead
    // sequencer is not a reason for the market list to disappear.
    let markets = stack.api.get_ok("/markets").await;
    assert_eq!(
        markets.status, 200,
        "the market catalogue went away with the node: {}",
        markets.raw
    );

    // ---- and back ------------------------------------------------------
    stack.restart_node().await;

    let recovered = expect_within(
        "the gateway to serve the account again after the sequencer restarts",
        RECOVERY_TIMEOUT,
        || async {
            match user.get("/account").await {
                Ok(reply) if reply.status == 200 => Ok(reply),
                Ok(reply) => Err(format!("status {} body {}", reply.status, reply.raw)),
                Err(err) => Err(format!("transport error {err}")),
            }
        },
    )
    .await;
    assert!(
        recovered.body["balances"].is_array(),
        "the account came back without balances: {}",
        recovered.raw
    );

    // Recovery means the whole path, not just reads: the account must be able
    // to trade again without signing in again.
    let after = user
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "39000.00", "qty": "0.02000000" }),
        )
        .await;
    assert_eq!(
        after.status, 201,
        "trading did not resume after the sequencer came back: {} {}",
        after.status, after.raw
    );
    cb_e2e::flows::await_book_level(&user, "BTC-USD", "buy", "39000.00", true).await;
}

/// The chain the gateway serves after a restart is the same chain.
///
/// A restarted sequencer re-opens the same data directory, so the height must
/// carry on rather than start again. A gateway that resynchronised from zero
/// would replay the whole chain into its candle and ledger views and double
/// every entry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots and kills the real node; run with `make e2e`"]
async fn the_chain_carries_on_from_where_it_was_after_a_sequencer_restart() {
    let mut stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "restart_height").await;
    rest_limit(&user, "BTC-USD", "buy", "40000.00", "0.02000000").await;

    let before = stack.height().await;
    assert!(before > 0, "nothing had been written before the restart");

    stack.kill_node();
    stack.restart_node().await;

    let after = expect_within(
        "the chain to report its height again after a restart",
        RECOVERY_TIMEOUT,
        || async {
            match stack.api.get("/chain/status").await {
                Ok(reply) if reply.status == 200 => {
                    let height = reply.body["height"].as_u64().unwrap_or(0);
                    if height > 0 {
                        Ok(height)
                    } else {
                        Err("height 0".to_string())
                    }
                }
                Ok(reply) => Err(format!("status {} body {}", reply.status, reply.raw)),
                Err(err) => Err(format!("transport error {err}")),
            }
        },
    )
    .await;

    assert!(
        after >= before,
        "the chain was at height {before} before the sequencer was killed and reports \
         {after} after it restarted. A chain that rewinds is one whose blocks were never \
         durable, and every view built from it is now wrong."
    );

    // The order placed before the outage is still on the book, which is the
    // user-visible form of the same claim.
    cb_e2e::flows::await_book_level(&user, "BTC-USD", "buy", "40000.00", true).await;
}
