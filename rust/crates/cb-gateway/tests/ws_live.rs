//! The WebSocket, over a real socket.
//!
//! The hub's own behaviour — fan-out, backpressure, the private channel — is
//! covered by unit tests in `cb_gateway::ws`, which can drive it directly.
//! What those cannot cover is the upgrade itself and the shape of the frames
//! that actually leave the process, so this suite binds a port and speaks the
//! protocol.

mod common;

use cb_gateway::node::{BookLevel, BookSnapshot};
use cb_types::DerivId;
use common::Api;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// A gateway listening on a real port, and the socket to reach it on.
struct Live {
    api: Api,
    address: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Live {
    async fn start(api: Api) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = listener.local_addr().expect("an address");
        let router = api.harness.router();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            api,
            address,
            server,
        }
    }

    async fn connect(&self) -> Socket {
        let url = format!("ws://{}/ws", self.address);
        let (stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .expect("the gateway upgrades the connection");
        Socket { stream }
    }

    fn stop(self) {
        self.server.abort();
    }
}

struct Socket {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Socket {
    async fn send(&mut self, value: Value) {
        self.stream
            .send(Message::Text(value.to_string()))
            .await
            .expect("the frame is sent");
    }

    /// The next JSON frame, or `None` if none arrives in time.
    async fn next(&mut self) -> Option<Value> {
        loop {
            let received = tokio::time::timeout(Duration::from_secs(2), self.stream.next()).await;
            match received {
                Ok(Some(Ok(Message::Text(text)))) => {
                    return Some(serde_json::from_str(&text).expect("frames are JSON"))
                }
                // Protocol-level frames are not the gateway's envelopes.
                Ok(Some(Ok(_))) => continue,
                _ => return None,
            }
        }
    }

    /// Wait for a frame on a given channel and type.
    async fn wait_for(&mut self, channel: &str, kind: &str) -> Value {
        for _ in 0..16 {
            let Some(frame) = self.next().await else {
                panic!("no `{channel}` `{kind}` frame arrived");
            };
            if frame["channel"] == channel && frame["type"] == kind {
                return frame;
            }
        }
        panic!("no `{channel}` `{kind}` frame among the first sixteen");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ping_is_answered_with_a_pong() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket.send(json!({ "op": "ping" })).await;
    let frame = socket.wait_for("system", "pong").await;
    assert_eq!(frame["channel"], "system");
    assert_eq!(frame["type"], "pong");

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_frame_is_reported_and_the_connection_survives() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .stream
        .send(Message::Text("not json".into()))
        .await
        .expect("sent");
    let frame = socket.wait_for("system", "error").await;
    assert_eq!(frame["data"]["code"], "VALIDATION_FAILED");

    // Still usable.
    socket.send(json!({ "op": "ping" })).await;
    socket.wait_for("system", "pong").await;

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribing_to_a_book_yields_a_snapshot_before_any_delta() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "subscribe", "channels": ["orderbook:BTC-PERP"] }))
        .await;

    let snapshot = socket.wait_for("orderbook:BTC-PERP", "snapshot").await;
    let data = &snapshot["data"];
    assert_eq!(data["height"], 10);
    assert_eq!(data["bids"][0][0], "64249.00");
    assert_eq!(data["bids"][0][1], "10.0000");
    assert_eq!(data["asks"][0][0], "64251.00");

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_book_change_arrives_as_a_delta_with_zero_meaning_removed() {
    let api = Api::with_markets().await;
    let live = Live::start(api).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "subscribe", "channels": ["orderbook:BTC-PERP"] }))
        .await;
    socket.wait_for("orderbook:BTC-PERP", "snapshot").await;

    // Move the book: the top bid vanishes and a new one appears.
    live.api.harness.node.edit(|state| {
        state.deriv_books.insert(
            DerivId::perp(0),
            BookSnapshot {
                bids: vec![BookLevel {
                    price: 6_424_800_000_000,
                    qty: 2_000_000_000,
                }],
                asks: vec![BookLevel {
                    price: 6_425_100_000_000,
                    qty: 1_000_000_000,
                }],
                height: 11,
            },
        );
    });
    // Drive one block through the watcher, which is what the running gateway
    // does on every block.
    let block = cb_gateway::node::BlockSummary {
        height: 11,
        hash: cb_types::H256([11; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000,
        proposer: None,
        txs: Vec::new(),
    };
    cb_gateway::watcher::on_block(&live.api.harness.state, &block)
        .await
        .expect("the block is processed");

    let delta = socket.wait_for("orderbook:BTC-PERP", "delta").await;
    let data = &delta["data"];
    assert_eq!(data["height"], 11);
    let bids: Vec<(String, String)> = data["bids"]
        .as_array()
        .expect("bids")
        .iter()
        .map(|level| {
            (
                level[0].as_str().expect("price").to_string(),
                level[1].as_str().expect("qty").to_string(),
            )
        })
        .collect();
    assert!(
        bids.iter()
            .any(|(price, qty)| price == "64249.00" && qty == "0.0000"),
        "a removed level must be reported with quantity zero, got {bids:?}"
    );
    assert!(bids.iter().any(|(price, _)| price == "64248.00"));

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_block_reaches_the_chain_channel() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "subscribe", "channels": ["chain"] }))
        .await;
    socket.wait_for("chain", "subscribed").await;

    let block = cb_gateway::node::BlockSummary {
        height: 42,
        hash: cb_types::H256([42; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000,
        proposer: None,
        txs: Vec::new(),
    };
    cb_gateway::watcher::on_block(&live.api.harness.state, &block)
        .await
        .expect("the block is processed");

    let frame = socket.wait_for("chain", "block").await;
    assert_eq!(frame["data"]["height"], 42);
    assert_eq!(frame["data"]["tx_count"], 0);
    assert!(frame["data"]["hash"]
        .as_str()
        .expect("hash")
        .starts_with("0x"));

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_private_channel_needs_a_session_and_then_works() {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let live = Live::start(api).await;
    let mut socket = live.connect().await;

    // Without a session the subscription is refused.
    socket
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    let error = socket.wait_for("system", "error").await;
    assert_eq!(error["data"]["code"], "UNAUTHORIZED");

    // With one it is accepted.
    socket
        .send(json!({ "op": "auth", "token": player.token }))
        .await;
    let authenticated = socket.wait_for("system", "authenticated").await;
    assert_eq!(authenticated["data"]["handle"], "satoshi");

    socket
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    let subscribed = socket.wait_for("account", "subscribed").await;
    assert_eq!(subscribed["data"]["channel"], "account");

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_token_over_the_socket_is_refused_without_closing_it() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "auth", "token": "sess_nope" }))
        .await;
    let error = socket.wait_for("system", "error").await;
    assert_eq!(error["data"]["code"], "UNAUTHORIZED");

    // The connection is still usable, which is what lets a client retry.
    socket.send(json!({ "op": "ping" })).await;
    socket.wait_for("system", "pong").await;

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribing_stops_the_frames() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "subscribe", "channels": ["chain"] }))
        .await;
    socket.wait_for("chain", "subscribed").await;
    socket
        .send(json!({ "op": "unsubscribe", "channels": ["chain"] }))
        .await;
    socket.wait_for("chain", "unsubscribed").await;

    let block = cb_gateway::node::BlockSummary {
        height: 7,
        hash: cb_types::H256([7; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000,
        proposer: None,
        txs: Vec::new(),
    };
    cb_gateway::watcher::on_block(&live.api.harness.state, &block)
        .await
        .expect("the block is processed");

    // A ping proves the socket is alive; the block must not have arrived.
    socket.send(json!({ "op": "ping" })).await;
    let frame = socket.next().await.expect("the pong arrives");
    assert_eq!(frame["type"], "pong", "no chain frame should precede it");

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_channel_is_reported_and_the_rest_still_subscribe() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;

    socket
        .send(json!({ "op": "subscribe", "channels": ["nonsense", "chain"] }))
        .await;
    let error = socket.wait_for("system", "error").await;
    assert_eq!(error["data"]["code"], "VALIDATION_FAILED");
    socket.wait_for("chain", "subscribed").await;

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_that_moves_an_account_reaches_that_players_private_channel() {
    // The watcher sees *trading* addresses in a block, while a user row stores
    // the *login* address. Resolving the two is what makes the private channel
    // work at all; without it this test receives nothing.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let state = std::sync::Arc::clone(&api.harness.state);
    let trading = player.trading_address;
    let live = Live::start(api).await;

    let mut socket = live.connect().await;
    socket
        .send(json!({ "op": "auth", "token": player.token }))
        .await;
    socket.wait_for("system", "authenticated").await;
    socket
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    socket.wait_for("account", "subscribed").await;

    // First block: the watcher records the account as it is.
    let empty = cb_gateway::node::BlockSummary {
        height: 11,
        hash: cb_types::H256([11; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000,
        proposer: None,
        txs: vec![cb_gateway::node::BlockTx {
            applied: None,
            hash: cb_types::H256([1; 32]),
            sender: cb_types::Address([9; 20]),
            index: 0,
            body: cb_tx::TxBody::Deposit {
                user: trading,
                asset: cb_types::AssetId::Usd,
                amount: 1,
                external_tx_ref: "seed".into(),
            },
        }],
    };
    cb_gateway::watcher::on_block(&state, &empty)
        .await
        .expect("processed");
    assert_eq!(
        state.mirror.len(),
        1,
        "the deposit's subject must resolve to the account that owns the trading address"
    );

    // Second block: the balance has moved, so a balance update must publish.
    live.api
        .harness
        .node
        .fund(trading, cb_types::AssetId::Usd, 100_000_000_000);
    let credited = cb_gateway::node::BlockSummary {
        height: 12,
        hash: cb_types::H256([12; 32]),
        ..empty.clone()
    };
    cb_gateway::watcher::on_block(&state, &credited)
        .await
        .expect("processed");

    let frame = socket.wait_for("account", "balance_update").await;
    // The contract's `balance_update` payload is the array itself, which is
    // what the browser deserialises; it used to be an object wrapping one.
    let balances = frame["data"].as_array().expect("balances");
    assert!(balances
        .iter()
        .any(|b| b["asset"] == "USD" && b["free"] == "100000.00"));

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_players_account_frames_never_reach_another_socket() {
    let api = Api::with_markets().await;
    let alice = api.player("alice", 0).await;
    let bob = api.player("bob", 1).await;
    let state = std::sync::Arc::clone(&api.harness.state);
    let live = Live::start(api).await;

    let mut hers = live.connect().await;
    let mut his = live.connect().await;
    for (socket, player) in [(&mut hers, &alice), (&mut his, &bob)] {
        socket
            .send(json!({ "op": "auth", "token": player.token }))
            .await;
        socket.wait_for("system", "authenticated").await;
        socket
            .send(json!({ "op": "subscribe", "channels": ["account"] }))
            .await;
        socket.wait_for("account", "subscribed").await;
    }

    // A fill addressed at alice.
    state.hub.publish_to_user(
        &alice.user_id,
        &cb_gateway::ws::Envelope::new(
            "account",
            "fill",
            json!({ "symbol": "BTC-PERP", "qty": "1.0000", "who": "alice" }),
        ),
    );

    let received = hers.wait_for("account", "fill").await;
    assert_eq!(received["data"]["who"], "alice");

    // Bob gets nothing but his own pong.
    his.send(json!({ "op": "ping" })).await;
    let frame = his.next().await.expect("a frame");
    assert_eq!(
        frame["type"], "pong",
        "bob must not receive alice's fill: {frame}"
    );

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_expires_mid_connection_has_its_socket_closed() {
    // §14 of the flow review, over a real socket and through the whole loop.
    // Revocation is pushed to the connection as it happens; expiry has nobody
    // to push it, so an open socket rechecks the session it authenticated with.
    // Before that, `user_id` was written into the connection at `auth` and never
    // looked at again: a session could expire, or be revoked out of band, and
    // the socket went on delivering balances, fills and positions for as long
    // as the client kept pinging — which the browser does every thirty seconds,
    // so the idle close never fired either.
    let harness = cb_gateway::testing::Harness::builder()
        .settings(cb_gateway::state::Settings {
            // Fast enough to test, and the only thing this changes.
            ws_session_recheck: Duration::from_millis(200),
            ..Default::default()
        })
        .build();
    let api = Api::from(harness);
    let player = api.player("satoshi", 0).await;
    let live = Live::start(api).await;

    let mut socket = live.connect().await;
    socket
        .send(json!({ "op": "auth", "token": player.token }))
        .await;
    socket.wait_for("system", "authenticated").await;
    socket
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    socket.wait_for("account", "subscribed").await;

    // The session goes away without anything telling the socket.
    live.api
        .harness
        .state
        .db
        .sessions()
        .revoke(&cb_gateway::auth::session_token_hash(&player.token))
        .await
        .expect("the session is revoked");

    let frame = socket.wait_for("system", "error").await;
    assert_eq!(frame["data"]["code"], "UNAUTHORIZED");

    // And the connection is gone: nothing further can be delivered to it.
    for _ in 0..20 {
        if live.api.harness.state.hub.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        live.api.harness.state.hub.is_empty(),
        "the socket must be closed, not merely told"
    );

    live.stop();
}

// ---------------------------------------------------------------------------
// Limits, malformed input, slow consumers and reconnects, over a real socket
// ---------------------------------------------------------------------------

/// An empty block at `height`, to drive the watcher.
fn empty_block(height: u64) -> cb_gateway::node::BlockSummary {
    cb_gateway::node::BlockSummary {
        height,
        hash: cb_types::H256([height as u8; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000 + height,
        proposer: None,
        txs: Vec::new(),
    }
}

/// Wait until the hub holds exactly `n` connections, or fail.
async fn wait_for_connections(api: &Api, n: usize) {
    for _ in 0..100 {
        if api.harness.state.hub.len() == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "expected {n} live connections, the hub holds {}",
        api.harness.state.hub.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_socket_is_capped_at_its_subscription_limit_and_survives_hitting_it() {
    // Finding 5 over the wire: one session-less socket asks for one more
    // channel than the cap in a single frame. Exactly the cap is granted, the
    // rest is refused with `RATE_LIMITED`, and the connection stays usable —
    // including for a subscription made after freeing a slot. The queue is
    // the production default (256): every reply to one frame is queued before
    // the writer drains any, and the harness's smaller test queue would drop
    // the socket as slow before the cap was reached.
    let harness = cb_gateway::testing::Harness::builder()
        .ws_buffer(256)
        .build();
    let api = Api::from(harness);
    api.list_btc_perp().await;
    let live = Live::start(api).await;
    let mut socket = live.connect().await;
    let cap = cb_gateway::ws::MAX_SUBSCRIPTIONS_PER_CONNECTION;
    let channels: Vec<String> = (0..=cap).map(|i| format!("trades:S{i}-PERP")).collect();
    socket
        .send(json!({ "op": "subscribe", "channels": channels }))
        .await;

    let (mut subscribed, mut limited) = (0, 0);
    while subscribed + limited < cap + 1 {
        let frame = socket.next().await.expect("one reply per channel");
        match (frame["channel"].as_str(), frame["type"].as_str()) {
            (Some("system"), Some("error")) => {
                assert_eq!(frame["data"]["code"], "RATE_LIMITED");
                limited += 1;
            }
            (_, Some("subscribed")) => subscribed += 1,
            other => panic!("unexpected frame {other:?}: {frame}"),
        }
    }
    assert_eq!((subscribed, limited), (cap, 1));

    // Re-subscribing to a channel already held is not a new slot.
    socket
        .send(json!({ "op": "subscribe", "channels": ["trades:S0-PERP"] }))
        .await;
    socket.wait_for("trades:S0-PERP", "subscribed").await;

    socket
        .send(json!({ "op": "unsubscribe", "channels": ["trades:S0-PERP"] }))
        .await;
    socket.wait_for("trades:S0-PERP", "unsubscribed").await;
    socket
        .send(json!({ "op": "subscribe", "channels": ["chain"] }))
        .await;
    socket.wait_for("chain", "subscribed").await;

    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_kind_of_malformed_frame_is_answered_and_none_closes_the_socket() {
    let live = Live::start(Api::with_markets().await).await;
    let mut socket = live.connect().await;
    let frames = [
        "{}",
        "[]",
        "null",
        "42",
        r#"{"op":"dance"}"#,
        r#"{"op":7}"#,
        r#"{"op":"auth"}"#,
        r#"{"op":"auth","token":7}"#,
        r#"{"op":"subscribe"}"#,
        r#"{"op":"subscribe","channels":"ticker"}"#,
        r#"{"op":"subscribe","channels":{"a":1}}"#,
        "{\"op\":\"ping\"",
    ];
    for text in frames {
        socket
            .stream
            .send(Message::Text(text.to_string()))
            .await
            .expect("sent");
        let frame = socket.wait_for("system", "error").await;
        assert_eq!(frame["data"]["code"], "VALIDATION_FAILED", "for {text}");
    }
    socket
        .stream
        .send(Message::Binary(vec![0, 159, 146, 150]))
        .await
        .expect("sent");
    let frame = socket.wait_for("system", "error").await;
    assert_eq!(frame["data"]["code"], "VALIDATION_FAILED");

    // Channel names that are too long or badly shaped are refused one by one
    // while the good one in the same frame still subscribes.
    let long = format!("trades:{}-PERP", "A".repeat(80));
    socket
        .send(json!({ "op": "subscribe",
                      "channels": [long, "orderbook:btc perp", "candles:BTC-PERP:7m", 5, "chain"] }))
        .await;
    socket.wait_for("chain", "subscribed").await;

    socket.send(json!({ "op": "ping" })).await;
    socket.wait_for("system", "pong").await;
    assert_eq!(live.api.harness.state.hub.len(), 1, "still connected");
    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_that_stops_reading_is_dropped_and_the_others_get_every_frame() {
    // One client stops reading. Its queue fills once its TCP window does, and
    // it is dropped; the fan-out never waits on it, and a client that keeps
    // up receives every frame in order.
    let harness = cb_gateway::testing::Harness::builder().ws_buffer(4).build();
    let live = Live::start(Api::from(harness)).await;
    let mut stalled = live.connect().await;
    let mut healthy = live.connect().await;
    for socket in [&mut stalled, &mut healthy] {
        socket
            .send(json!({ "op": "subscribe", "channels": ["chain"] }))
            .await;
        socket.wait_for("chain", "subscribed").await;
    }

    // Frames big enough that a stalled reader's socket buffers fill quickly.
    let padding = "x".repeat(256 * 1024);
    let hub = std::sync::Arc::clone(&live.api.harness.state.hub);
    const FRAMES: u64 = 400;
    let mut published = 0;
    let mut dropped_at = None;
    for seq in 0..FRAMES {
        hub.publish(
            &cb_gateway::ws::Channel::Chain,
            &cb_gateway::ws::Envelope::new("chain", "block", json!({ "seq": seq, "pad": padding })),
        );
        published += 1;
        // The healthy client keeps pace: every frame, in order.
        let frame = healthy.wait_for("chain", "block").await;
        assert_eq!(frame["data"]["seq"], seq, "in order, none skipped");
        if dropped_at.is_none() && hub.dropped_slow_clients() > 0 {
            dropped_at = Some(seq);
        }
        // Keep publishing a while after the drop, to show the healthy
        // socket is unaffected by it.
        if dropped_at.is_some_and(|at| seq >= at + 20) {
            break;
        }
    }
    assert!(dropped_at.is_some(), "the stalled client was never dropped");
    assert_eq!(
        hub.dropped_slow_clients(),
        1,
        "the stalled client, and only it"
    );
    wait_for_connections(&live.api, 1).await;

    // The healthy socket is still subscribed and still answered.
    healthy.send(json!({ "op": "ping" })).await;
    healthy.wait_for("system", "pong").await;

    // The stalled client, reading at last, finds a stream that ends short.
    let mut received = 0;
    while let Ok(Some(message)) =
        tokio::time::timeout(Duration::from_secs(2), stalled.stream.next()).await
    {
        match message {
            Ok(Message::Text(_)) => received += 1,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        received < published,
        "the dropped client must not have been buffered everything ({received} of {published})"
    );
    live.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnecting_client_rebuilds_the_current_book_and_must_authenticate_again() {
    // The contract: a client that misses frames resubscribes, and gets a
    // fresh snapshot on every subscribe. What the client rebuilds from that
    // snapshot plus the deltas after it must be the book the REST endpoint
    // serves — even though the book moved while nobody was watching it.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let live = Live::start(api).await;

    let mut first = live.connect().await;
    first
        .send(json!({ "op": "auth", "token": player.token }))
        .await;
    first.wait_for("system", "authenticated").await;
    first
        .send(json!({ "op": "subscribe", "channels": ["orderbook:BTC-PERP", "account"] }))
        .await;
    first.wait_for("orderbook:BTC-PERP", "snapshot").await;
    first.stream.close(None).await.expect("closes");
    drop(first);
    wait_for_connections(&live.api, 0).await;

    // The book moves while the client is away, and a block passes.
    live.api.harness.node.edit(|state| {
        state.deriv_books.insert(
            DerivId::perp(0),
            BookSnapshot {
                bids: vec![
                    BookLevel {
                        price: 6_424_000_000_000,
                        qty: 3_000_000_000,
                    },
                    BookLevel {
                        price: 6_423_000_000_000,
                        qty: 1_000_000_000,
                    },
                ],
                asks: vec![BookLevel {
                    price: 6_426_000_000_000,
                    qty: 5_000_000_000,
                }],
                height: 11,
            },
        );
    });
    cb_gateway::watcher::on_block(&live.api.harness.state, &empty_block(11))
        .await
        .expect("processed");

    let mut second = live.connect().await;
    second
        .send(json!({ "op": "subscribe", "channels": ["orderbook:BTC-PERP"] }))
        .await;
    let snapshot = second.wait_for("orderbook:BTC-PERP", "snapshot").await;
    let mut book = LocalBook::from_snapshot(&snapshot["data"]);

    // One more block: whatever the snapshot was, the deltas after it must
    // bring the client to the chain's book.
    cb_gateway::watcher::on_block(&live.api.harness.state, &empty_block(12))
        .await
        .expect("processed");
    second.send(json!({ "op": "ping" })).await;
    loop {
        let frame = second.next().await.expect("a frame");
        if frame["type"] == "pong" {
            break;
        }
        if frame["channel"] == "orderbook:BTC-PERP" && frame["type"] == "delta" {
            book.apply(&frame["data"]);
        }
    }
    let rest = live.api.get("/api/v1/markets/BTC-PERP/orderbook").await;
    let rest = rest.assert_ok(200).clone();
    assert_eq!(
        book.bids(),
        levels(&rest["bids"]),
        "rebuilt bids match REST"
    );
    assert_eq!(
        book.asks(),
        levels(&rest["asks"]),
        "rebuilt asks match REST"
    );

    // The session does not follow the socket: the private channel is refused
    // until the new connection authenticates.
    second
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    let refused = second.wait_for("system", "error").await;
    assert_eq!(refused["data"]["code"], "UNAUTHORIZED");
    second
        .send(json!({ "op": "auth", "token": player.token }))
        .await;
    second.wait_for("system", "authenticated").await;
    second
        .send(json!({ "op": "subscribe", "channels": ["account"] }))
        .await;
    second.wait_for("account", "subscribed").await;

    live.stop();
}

/// A client-side book, rebuilt the way the contract tells a client to.
struct LocalBook {
    bids: std::collections::BTreeMap<String, String>,
    asks: std::collections::BTreeMap<String, String>,
}

impl LocalBook {
    fn from_snapshot(data: &Value) -> Self {
        let collect = |side: &Value| {
            side.as_array()
                .expect("levels")
                .iter()
                .map(|l| {
                    (
                        l[0].as_str().unwrap().to_string(),
                        l[1].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        };
        Self {
            bids: collect(&data["bids"]),
            asks: collect(&data["asks"]),
        }
    }

    fn apply(&mut self, data: &Value) {
        for (side, levels) in [
            (&mut self.bids, &data["bids"]),
            (&mut self.asks, &data["asks"]),
        ] {
            for level in levels.as_array().expect("levels") {
                let price = level[0].as_str().unwrap().to_string();
                let qty = level[1].as_str().unwrap().to_string();
                if qty
                    .trim_start_matches('0')
                    .trim_start_matches('.')
                    .trim_matches('0')
                    .is_empty()
                {
                    side.remove(&price);
                } else {
                    side.insert(price, qty);
                }
            }
        }
    }

    fn bids(&self) -> std::collections::BTreeSet<(String, String)> {
        self.bids.clone().into_iter().collect()
    }

    fn asks(&self) -> std::collections::BTreeSet<(String, String)> {
        self.asks.clone().into_iter().collect()
    }
}

fn levels(side: &Value) -> std::collections::BTreeSet<(String, String)> {
    side.as_array()
        .expect("levels")
        .iter()
        .map(|l| {
            (
                l[0].as_str().unwrap().to_string(),
                l[1].as_str().unwrap().to_string(),
            )
        })
        .collect()
}
