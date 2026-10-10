//! The live feed: subscribe, snapshot, delta, and the private channel.
//!
//! The websocket is the half of the product that never appears in a REST test,
//! and it is the half that decides whether a trading screen is usable. A
//! terminal that has to poll is a terminal that is always a second behind, and
//! the fault list opens with a socket that never connected at all.
//!
//! Three properties are worth more than the rest and are each asserted on their
//! own here:
//!
//! * **A snapshot precedes every delta.** A delta is a patch, so a client that
//!   receives one before it has a base has no way to know what the book is. The
//!   contract promises a fresh snapshot on every subscribe and every reconnect.
//! * **A zero quantity removes a level.** Both sides agreeing on that sentence
//!   is the whole delta protocol. A client that treats `"0"` as a level with no
//!   size renders a book with a phantom price in it forever.
//! * **The private channel is private.** An `account` subscription without a
//!   token must deliver nothing, and with a token must deliver only that
//!   account's events. A feed that leaks one user's fills to another is worse
//!   than a feed that does not work.
//!
//! Absence is proved against a clock the server owns, never a sleep: the test
//! waits for a *later* public event on the same socket and then asserts the
//! private one never arrived. That is a synchronisation point rather than a
//! guess about timing.

use std::time::Duration;

use cb_e2e::flows::{funded_user, rest_limit, sign_in};
use cb_e2e::ws::{frontend_websocket_url, Ws};
use cb_e2e::Stack;
use serde_json::json;

/// Long enough for a block to be produced and broadcast several times over.
const FEED_TIMEOUT: Duration = Duration::from_secs(20);

/// Opens a socket the way the frontend does.
async fn dial(stack: &Stack) -> Ws {
    let url = frontend_websocket_url(&stack.api_base());
    Ws::connect(&url)
        .await
        .unwrap_or_else(|err| panic!("cannot open the websocket at {url}: {err}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn subscribing_to_a_book_delivers_a_snapshot_before_any_delta() {
    let stack = Stack::start().await;
    let mut socket = dial(&stack).await;

    socket.subscribe(&["orderbook:BTC-USD"]).await;

    // The guarantee is about state, not about frame order in general: no delta
    // may reach a client that has not been given a base to apply it to. The
    // gateway also sends a `subscribed` acknowledgement, which the contract does
    // not document and which carries no book state, so it is neither a snapshot
    // nor a delta and is not what this is about.
    let snapshot = socket
        .await_message(
            "the order-book snapshot for BTC-USD",
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("orderbook:BTC-USD")
                    && matches!(message["type"].as_str(), Some("snapshot") | Some("delta"))
            },
        )
        .await;

    assert_eq!(
        snapshot["type"].as_str(),
        Some("snapshot"),
        "the first order-book frame carrying state after subscribing was a `{}`, not a \
         snapshot. A delta is a patch: a client that gets one first has nothing to apply it \
         to and cannot tell that it is missing state. Frame: {snapshot}",
        snapshot["type"].as_str().unwrap_or("?")
    );
    for side in ["bids", "asks"] {
        assert!(
            snapshot["data"][side].is_array(),
            "the snapshot has no `{side}` array: {snapshot}"
        );
    }
    assert!(
        snapshot["data"]["height"].is_number(),
        "the snapshot does not say which height it describes, so a client cannot order it \
         against the deltas that follow: {snapshot}"
    );

    // Resubscribing has to produce another snapshot: the contract says a fresh
    // one arrives on every subscribe, which is a client's only recovery from a
    // missed sequence.
    socket.subscribe(&["orderbook:BTC-USD"]).await;
    let again = socket
        .await_message(
            "a second snapshot after resubscribing",
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("orderbook:BTC-USD")
                    && message["type"].as_str() == Some("snapshot")
            },
        )
        .await;
    assert!(again["data"]["height"].is_number(), "{again}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_new_order_arrives_as_a_delta_and_cancelling_it_removes_the_level_with_a_zero() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, trader) = funded_user(api, 0, "ws_book").await;

    let mut socket = dial(&stack).await;
    socket.subscribe(&["orderbook:BTC-USD"]).await;
    socket
        .await_message("the initial snapshot", FEED_TIMEOUT, |message| {
            message["channel"].as_str() == Some("orderbook:BTC-USD")
                && message["type"].as_str() == Some("snapshot")
        })
        .await;

    // A price nothing else in the suite uses, so the level is unmistakably ours.
    let price = "43210.00";
    let order = rest_limit(&trader, "BTC-USD", "buy", price, "0.02000000").await;

    let appeared = socket
        .await_message(
            &format!("a delta adding the {price} bid"),
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("orderbook:BTC-USD")
                    && message["type"].as_str() == Some("delta")
                    && level_quantity(message, "bids", price).is_some_and(|qty| qty != "0")
            },
        )
        .await;
    let quantity = level_quantity(&appeared, "bids", price).expect("the matched frame has it");
    assert_eq!(
        cb_e2e::money::dec(&quantity),
        cb_e2e::money::dec("0.02"),
        "the delta reported {quantity} at {price}; the order was for 0.02. Frame: {appeared}"
    );

    trader.delete_ok(&format!("/orders/{order}")).await.ok(200);

    // The removal has to arrive as a zero at that price, not as a silence and
    // not as the level missing from a later snapshot. A client applying deltas
    // has no other way to learn the level is gone.
    let removed = socket
        .await_message(
            &format!("a delta removing the {price} bid with a zero quantity"),
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("orderbook:BTC-USD")
                    && message["type"].as_str() == Some("delta")
                    && level_quantity(message, "bids", price)
                        .is_some_and(|qty| cb_e2e::money::dec(&qty).is_zero())
            },
        )
        .await;
    assert!(
        level_quantity(&removed, "bids", price).is_some(),
        "frame matched but carries no {price} level: {removed}"
    );
}

/// The quantity a frame reports at one price on one side, if it mentions it.
fn level_quantity(message: &serde_json::Value, side: &str, price: &str) -> Option<String> {
    let wanted = cb_e2e::money::dec(price);
    message["data"][side].as_array()?.iter().find_map(|level| {
        let level = level.as_array()?;
        let at = level.first()?.as_str()?;
        (cb_e2e::money::dec(at) == wanted).then(|| level.get(1)?.as_str().map(str::to_string))?
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_chain_channel_delivers_new_block_headers() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, trader) = funded_user(api, 0, "ws_chain").await;

    let mut socket = dial(&stack).await;
    socket.subscribe(&["chain"]).await;

    // This chain does not produce empty blocks, so something has to happen for
    // there to be a block at all. Placing an order is that something.
    rest_limit(&trader, "BTC-USD", "buy", "44000.00", "0.02000000").await;

    let block = socket
        .await_message(
            "a block header on the chain channel",
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("chain")
                    && message["type"].as_str() == Some("block")
            },
        )
        .await;
    for field in ["height", "hash", "tx_count", "at"] {
        assert!(
            block["data"].get(field).is_some(),
            "the block header is missing `{field}`: {block}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_ping_is_answered_with_a_pong() {
    let stack = Stack::start().await;
    let mut socket = dial(&stack).await;
    socket.send(json!({ "op": "ping" })).await;
    let pong = socket
        .await_message("a pong", FEED_TIMEOUT, |message| {
            message["type"].as_str() == Some("pong")
        })
        .await;
    assert_eq!(pong["channel"].as_str(), Some("system"), "{pong}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_private_channel_refuses_an_unauthenticated_subscriber() {
    let stack = Stack::start().await;
    let mut socket = dial(&stack).await;

    socket.subscribe(&["account"]).await;
    let error = socket
        .await_message(
            "an error refusing the private channel",
            FEED_TIMEOUT,
            |message| {
                message["channel"].as_str() == Some("system")
                    && message["type"].as_str() == Some("error")
            },
        )
        .await;
    assert_eq!(
        error["data"]["code"].as_str(),
        Some("UNAUTHORIZED"),
        "subscribing to `account` with no token was refused with the wrong code: {error}"
    );
}

/// The private feed carries this account's events and nobody else's.
///
/// The absence half is the point, and absence needs a clock. Two other accounts
/// trade with each other while a third account listens; the listener waits for
/// its own faucet grant to show up, which is an event on its own channel and
/// therefore proof the channel is live and current. Any fill belonging to the
/// other two that was going to leak would have leaked by then.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_private_channel_never_carries_another_accounts_activity() {
    let stack = Stack::start().await;
    let api = &stack.api;

    let (_, maker) = funded_user(api, 0, "ws_maker").await;
    let (_, taker) = funded_user(api, 1, "ws_taker").await;
    let onlooker = sign_in(api, 2, "ws_onlooker").await;
    let onlooker_api = api.as_user(&onlooker);

    let mut socket = dial(&stack).await;
    socket.auth(&onlooker.token).await;
    socket.subscribe(&["account", "chain"]).await;

    // Two strangers trade.
    rest_limit(&maker, "BTC-USD", "buy", "47000.00", "0.02000000").await;
    taker
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "sell", "type": "limit",
                    "price": "47000.00", "qty": "0.02000000" }),
        )
        .await
        .ok(201);

    // The onlooker then does something of its own, which is the synchronisation
    // point: once its own event has arrived, everything the strangers did is in
    // the past as far as this socket is concerned.
    onlooker_api
        .post_ok("/faucet/claim", json!({}))
        .await
        .ok(200);
    socket
        .await_message(
            "the onlooker's own account event, which pins 'by now'",
            FEED_TIMEOUT,
            |message| message["channel"].as_str() == Some("account"),
        )
        .await;
    // Drain whatever else is already queued, so the judgement covers everything
    // the server has decided to send rather than only what has been read.
    socket.drain(Duration::from_millis(300)).await;

    let leaked: Vec<&serde_json::Value> = socket
        .history()
        .iter()
        .filter(|message| message["channel"].as_str() == Some("account"))
        .filter(|message| {
            matches!(
                message["type"].as_str(),
                Some("fill") | Some("order_update") | Some("position_update")
            )
        })
        .collect();

    assert!(
        leaked.is_empty(),
        "the onlooker's private channel carried {} order or fill event(s) for a trade \
         between two other accounts:\n{}\n\nThe onlooker has never placed an order.",
        leaked.len(),
        leaked
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// An authenticated account does receive its own events.
///
/// Without this, the previous test passes on a feed that delivers nothing at
/// all — which would be private, and useless.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn an_authenticated_account_receives_its_own_events() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let session = sign_in(api, 0, "ws_own_events").await;
    let user = api.as_user(&session);

    let mut socket = dial(&stack).await;
    socket.auth(&session.token).await;
    socket.subscribe(&["account"]).await;

    user.post_ok("/faucet/claim", json!({})).await.ok(200);

    let event = socket
        .await_message(
            "an account event for the caller's own faucet grant",
            FEED_TIMEOUT,
            |message| message["channel"].as_str() == Some("account"),
        )
        .await;
    assert!(
        event["type"].as_str().is_some(),
        "an account frame with no `type`: {event}"
    );
}
