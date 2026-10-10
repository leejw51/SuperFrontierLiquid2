//! An order's whole life under execute-after-commit (ADR 0008), end to end.
//!
//! Under BFT ordering a block carries every committed transaction whether or
//! not the executor applied it, and says nothing about which. The gateway
//! learns the answer from the sender's nonce, which it can only read — and
//! the read can fail, the mempool can lose a transaction, a retry can hand a
//! nonce back. Each of those is a path by which an order can end up in the
//! wrong state, and each is driven here through the real router, the real
//! watcher and the real tracker, with only the node replaced.
//!
//! Every step checks two views of the same order against each other: the
//! REST listing a page loads, and the `order_update` frame a live page is
//! pushed. The contract says they are the same object field for field, so a
//! test that checked only one would let them drift.

mod common;

use cb_gateway::node::{SubmitOutcome, SubmitRejection};
use cb_gateway::testing::Player;
use cb_types::AssetId;
use common::{hex_hash, last_submitted, settle, spot_buy, AccountSocket, Api};
use serde_json::{json, Value};

/// A market, a funded player and a socket on their account channel.
async fn ready() -> (Api, Player, AccountSocket) {
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 1_000_000_000_000);
    let socket = AccountSocket::attach(&api, &player);
    (api, player, socket)
}

/// Place one spot order and return the transaction hash it is known by.
async fn place(api: &Api, player: &Player, price: &str) -> String {
    let reply = api.post_as("/api/v1/orders", player, spot_buy(price)).await;
    let body = reply.assert_ok(201);
    // No block arrives inside the harness's 20 ms wait, so every placement
    // here answers `pending` and is addressed by its hash until one does.
    assert_eq!(
        body["status"], "pending",
        "no block has answered yet: {body}"
    );
    assert_eq!(body["order_id"], body["tx_hash"]);
    body["tx_hash"].as_str().expect("a hash").to_string()
}

/// The last `order_update` must be exactly what REST now says about the
/// order: the contract's "field for field" promise.
async fn assert_socket_matches_rest(
    api: &Api,
    player: &Player,
    socket: &mut AccountSocket,
    tx: &str,
) {
    let rest = api.order(player, tx).await;
    let pushed: Vec<Value> = socket
        .order_updates()
        .into_iter()
        .filter(|u| u["order_id"] == rest["order_id"])
        .collect();
    let last = pushed
        .last()
        .unwrap_or_else(|| panic!("no order_update for {} was pushed", rest["order_id"]));
    assert_eq!(last, &rest, "the pushed order and the REST order disagree");
}

// ---------------------------------------------------------------------------
// The ordinary path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_order_opens_then_a_cancel_the_chain_applies_closes_it_and_both_views_agree_throughout()
{
    // placed → pending → open → (cancel accepted) → cancelled. The cancel
    // endpoint answers `cancelled` on mempool acceptance, but the order is not
    // moved until the chain answers for the cancel; REST must not run ahead
    // of the chain, and the socket must say so exactly once when it does.
    let (api, player, mut socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;
    assert_eq!(api.order(&player, &tx).await["status"], "pending");
    assert!(
        socket.order_updates().is_empty(),
        "nothing has changed state yet"
    );

    settle(&api, 11).await;
    let open = api.order(&player, &tx).await;
    assert_eq!(open["status"], "open");
    assert_eq!(
        open["order_id"],
        hex_hash(&cb_gateway::tracker::derive_order_id(
            player.trading_address,
            11,
            0,
            0
        )),
        "the on-chain id is the engine's (sender, height, index) derivation"
    );
    assert_socket_matches_rest(&api, &player, &mut socket, &tx).await;
    assert_eq!(
        socket.order_updates().len(),
        1,
        "one frame for one transition"
    );
    assert_eq!(api.harness.state.submitter.in_flight_count(), 0);

    let order_id = open["order_id"].as_str().expect("id").to_string();
    let cancel = api
        .delete_as(&format!("/api/v1/orders/{order_id}"), &player)
        .await;
    assert_eq!(cancel.assert_ok(200)["status"], "cancelled");
    assert_eq!(
        api.order(&player, &tx).await["status"],
        "open",
        "the order is moved by the chain's answer, not by the mempool's"
    );
    socket.forget();

    settle(&api, 12).await;
    let cancelled = api.order(&player, &tx).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert_socket_matches_rest(&api, &player, &mut socket, &tx).await;
    assert_eq!(socket.order_updates().len(), 1);
    assert!(
        api.harness
            .state
            .tracker
            .addresses_with_live_orders()
            .is_empty(),
        "a closed order stops the account being polled"
    );

    // A second cancel of a closed order is refused and signs nothing.
    let before = api.harness.node.submitted().len();
    api.delete_as(&format!("/api/v1/orders/{order_id}"), &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
    assert_eq!(api.harness.node.submitted().len(), before);
}

#[tokio::test]
async fn a_pending_order_cannot_be_cancelled_because_it_has_no_on_chain_id_yet() {
    // A cancel names an on-chain order id, which does not exist until the
    // block carrying the order has been answered for. Signing a cancel for a
    // made-up id would burn a nonce on a transaction that cannot apply.
    let (api, player, _socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;
    let before = api.harness.node.submitted().len();
    api.delete_as(&format!("/api/v1/orders/{tx}"), &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
    assert_eq!(api.harness.node.submitted().len(), before, "nothing signed");
}

// ---------------------------------------------------------------------------
// The verdict cannot be read when the block arrives
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_order_whose_verdict_cannot_be_read_stays_pending_and_opens_on_a_later_block() {
    // The block carrying the order arrives while the sender's account cannot
    // be read. Nothing is known, so nothing may be claimed: the order stays
    // `pending` — included, but unanswered — and the client is told nothing.
    // The next block's account read answers for it, and the order opens under
    // the id of the block that actually carried it, not the one that
    // happened to answer.
    let (api, player, mut socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;

    api.harness
        .node
        .edit(|s| s.account_read_error = Some("node is restarting".into()));
    settle(&api, 11).await;
    assert_eq!(api.order(&player, &tx).await["status"], "pending");
    assert!(socket.order_updates().is_empty(), "no verdict, no frame");
    assert!(
        !api.harness
            .state
            .tracker
            .addresses_with_live_orders()
            .is_empty(),
        "an unanswered order keeps its account watched, which is how it gets answered"
    );

    api.harness.node.edit(|s| s.account_read_error = None);
    settle(&api, 12).await;
    let open = api.order(&player, &tx).await;
    assert_eq!(open["status"], "open");
    assert_eq!(
        open["order_id"],
        hex_hash(&cb_gateway::tracker::derive_order_id(
            player.trading_address,
            11,
            0,
            0
        )),
        "the id comes from the carrying block (11), not the answering one (12)"
    );
    assert_socket_matches_rest(&api, &player, &mut socket, &tx).await;
    assert_eq!(socket.order_updates().len(), 1);
}

#[tokio::test]
async fn a_refused_order_whose_verdict_could_not_be_read_is_rejected_on_a_later_block() {
    // The same, for an order the executor refused: the later read finds the
    // nonce unmoved and the order is rejected, never passing through `open`.
    let (api, player, mut socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;
    api.harness.node.refuse(last_submitted(&api));

    api.harness
        .node
        .edit(|s| s.account_read_error = Some("node is restarting".into()));
    settle(&api, 11).await;
    assert_eq!(api.order(&player, &tx).await["status"], "pending");

    api.harness.node.edit(|s| s.account_read_error = None);
    settle(&api, 12).await;
    assert_eq!(api.order(&player, &tx).await["status"], "rejected");
    assert_socket_matches_rest(&api, &player, &mut socket, &tx).await;
    assert!(
        socket.order_updates().iter().all(|u| u["status"] != "open"),
        "a refused order is never announced as open"
    );
}

#[tokio::test]
async fn an_order_answered_on_a_later_block_is_not_given_up_by_the_hourly_janitor() {
    // An order resolved by the late read (above) is still an answered order.
    // The submitter's hourly sweep gives up submissions *nothing* answered
    // for; if the late answer left the submission registered as unanswered,
    // the sweep would declare an open, resting order dead an hour later and
    // flip it to `rejected` while it is still on the book.
    let (api, player, mut socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;
    api.harness
        .node
        .edit(|s| s.account_read_error = Some("node is restarting".into()));
    settle(&api, 11).await;
    api.harness.node.edit(|s| s.account_read_error = None);
    settle(&api, 12).await;
    assert_eq!(api.order(&player, &tx).await["status"], "open");
    socket.forget();

    api.harness.clock.advance(chrono::Duration::minutes(61));
    api.harness
        .state
        .submitter
        .prune(api.harness.state.clock.now());
    settle(&api, 13).await;

    assert_eq!(
        api.order(&player, &tx).await["status"],
        "open",
        "an order the chain applied is still resting an hour later"
    );
    assert!(
        socket.order_updates().is_empty(),
        "and nobody is told otherwise"
    );
}

// ---------------------------------------------------------------------------
// Submissions the chain never answers for
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_placement_no_block_ever_carries_is_rejected_by_the_hourly_janitor_and_not_before() {
    // The node accepted the order into its mempool and then lost it — a
    // restart, say — with no later submission from the account to expose the
    // loss. Nothing will ever answer for it. It must not read `pending`
    // forever, and it must not be given up early either: an hour is the
    // documented bound.
    let (api, player, mut socket) = ready().await;
    let tx = place(&api, &player, "50000.00").await;
    api.harness.node.edit(|s| s.submitted.clear());
    let start = api.harness.state.clock.now();

    api.harness
        .state
        .submitter
        .prune(start + chrono::Duration::minutes(59));
    settle(&api, 11).await;
    assert_eq!(
        api.order(&player, &tx).await["status"],
        "pending",
        "not yet"
    );
    assert!(socket.order_updates().is_empty());

    api.harness.clock.advance(chrono::Duration::minutes(61));
    api.harness
        .state
        .submitter
        .prune(api.harness.state.clock.now());
    settle(&api, 12).await;
    let rejected = api.order(&player, &tx).await;
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(
        rejected["order_id"],
        tx.as_str(),
        "no on-chain id was ever issued"
    );
    assert_socket_matches_rest(&api, &player, &mut socket, &tx).await;
    assert_eq!(socket.order_updates().len(), 1, "told once");
    assert_eq!(api.harness.state.submitter.in_flight_count(), 0);
    assert!(api
        .harness
        .state
        .tracker
        .addresses_with_live_orders()
        .is_empty());
    api.delete_as(&format!("/api/v1/orders/{tx}"), &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
}

#[tokio::test]
async fn a_bad_nonce_retry_rejects_the_earlier_order_the_chain_handed_its_nonce_back_from() {
    // Order A is accepted at nonce 0 and then dropped at block build, so the
    // chain still expects 0. Order B is signed at the cached nonce 1, refused
    // `badNonce`, re-read, and accepted at 0. That retry is the proof that A
    // can never apply — its nonce now belongs to B — and A must be rejected
    // rather than left pending, while B opens normally.
    let (api, player, mut socket) = ready().await;
    let a = place(&api, &player, "50000.00").await;
    api.harness.node.edit(|s| {
        s.submitted.clear(); // A never reaches a block.
        s.submit_replies.push_back(SubmitOutcome::Rejected {
            code: SubmitRejection::BadNonce,
            message: "expected nonce 0".into(),
        });
    });

    let b = place(&api, &player, "49000.00").await;
    let attempts = api.harness.node.submitted();
    assert_eq!(
        attempts.iter().map(|t| t.tx.nonce).collect::<Vec<_>>(),
        vec![1, 0],
        "the stale cached nonce is tried once, then the chain's"
    );
    assert_eq!(
        hex_hash(&attempts[1].hash()),
        b,
        "B is known by the retry's hash"
    );
    let b_record = api.harness.state.tracker.find(&b).expect("B is tracked");
    assert_eq!(b_record.nonce, 0);
    // The mempool refused the first attempt, so no block carries it.
    api.harness.node.edit(|s| {
        s.submitted.remove(0);
    });

    settle(&api, 11).await;
    assert_eq!(api.order(&player, &a).await["status"], "rejected");
    assert_eq!(api.order(&player, &b).await["status"], "open");
    assert_socket_matches_rest(&api, &player, &mut socket, &a).await;
    assert_socket_matches_rest(&api, &player, &mut socket, &b).await;
    assert_eq!(socket.order_updates().len(), 2, "one frame per order");
    assert_eq!(api.harness.state.submitter.in_flight_count(), 0);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_placements_from_one_account_use_every_nonce_exactly_once() {
    // Eight orders from one account at once. The submitter serialises them
    // per address; were it not to, two would sign the same nonce and the
    // chain would keep one. Every order must get its own nonce, its own
    // transaction, and — once the block lands — its own on-chain id.
    let (api, player, mut socket) = ready().await;
    const N: usize = 8;
    let prices: Vec<String> = (0..N).map(|i| format!("{}.00", 40_000 + i * 100)).collect();
    let replies = futures_util::future::join_all(
        prices
            .iter()
            .map(|price| api.post_as("/api/v1/orders", &player, spot_buy(price))),
    )
    .await;

    let mut hashes = std::collections::BTreeSet::new();
    for reply in &replies {
        hashes.insert(
            reply.assert_ok(201)["tx_hash"]
                .as_str()
                .expect("hash")
                .to_string(),
        );
    }
    assert_eq!(hashes.len(), N, "eight distinct transactions");
    let mut nonces: Vec<u64> = api
        .harness
        .node
        .submitted()
        .iter()
        .map(|t| t.tx.nonce)
        .collect();
    nonces.sort_unstable();
    assert_eq!(nonces, (0..N as u64).collect::<Vec<_>>());

    settle(&api, 11).await;
    let orders = api.all_orders(&player).await;
    assert_eq!(orders.len(), N);
    assert!(orders.iter().all(|o| o["status"] == "open"), "{orders:?}");
    let ids: std::collections::BTreeSet<String> = orders
        .iter()
        .map(|o| o["order_id"].as_str().expect("id").to_string())
        .collect();
    assert_eq!(ids.len(), N, "distinct on-chain ids");
    for hash in &hashes {
        assert_socket_matches_rest(&api, &player, &mut socket, hash).await;
    }
    assert_eq!(socket.order_updates().len(), N);
    assert_eq!(api.harness.state.submitter.in_flight_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_placements_with_one_client_order_id_place_one_order_and_sign_one_transaction() {
    // Idempotency under a real race rather than a sequential retry: every
    // request carries the same `client_order_id`, exactly one transaction is
    // signed, and every reply names the same order.
    let (api, player, _socket) = ready().await;
    let mut body = spot_buy("50000.00");
    body["client_order_id"] = json!("cli_race");
    let replies = futures_util::future::join_all(
        (0..6).map(|_| api.post_as("/api/v1/orders", &player, body.clone())),
    )
    .await;
    let ids: std::collections::BTreeSet<String> = replies
        .iter()
        .map(|r| {
            r.assert_ok(201)["tx_hash"]
                .as_str()
                .expect("hash")
                .to_string()
        })
        .collect();
    assert_eq!(ids.len(), 1, "one order");
    assert_eq!(api.harness.node.submitted().len(), 1, "one signature");
}

// ---------------------------------------------------------------------------
// Cancel-all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_all_the_chain_applies_closes_every_order_and_reports_each() {
    let (api, player, mut socket) = ready().await;
    let mut txs = Vec::new();
    for price in ["48000.00", "49000.00", "50000.00"] {
        txs.push(place(&api, &player, price).await);
    }
    settle(&api, 11).await;
    socket.forget();

    let reply = api
        .post_as(
            "/api/v1/orders/cancel-all",
            &player,
            json!({ "symbol": "BTC-USD" }),
        )
        .await;
    assert_eq!(reply.assert_ok(200)["cancelled"], 3);
    assert_eq!(
        api.harness.node.submitted().len(),
        1,
        "spot has a batch cancel: one transaction, one nonce"
    );
    settle(&api, 12).await;
    for tx in &txs {
        assert_eq!(api.order(&player, tx).await["status"], "cancelled");
        assert_socket_matches_rest(&api, &player, &mut socket, tx).await;
    }
    assert_eq!(socket.order_updates().len(), 3);
}

#[tokio::test]
async fn a_cancel_all_the_chain_refuses_leaves_every_order_open_and_corrects_the_client() {
    // The endpoint said `cancelled: 3`. The executor refused the batch, so
    // all three are still resting, and each one's owner is told its real
    // status rather than being left to believe the endpoint.
    let (api, player, mut socket) = ready().await;
    let mut txs = Vec::new();
    for price in ["48000.00", "49000.00", "50000.00"] {
        txs.push(place(&api, &player, price).await);
    }
    settle(&api, 11).await;
    socket.forget();

    api.post_as("/api/v1/orders/cancel-all", &player, json!({}))
        .await
        .assert_ok(200);
    api.harness.node.refuse(last_submitted(&api));
    settle(&api, 12).await;
    for tx in &txs {
        assert_eq!(api.order(&player, tx).await["status"], "open");
        assert_socket_matches_rest(&api, &player, &mut socket, tx).await;
    }
    assert_eq!(socket.order_updates().len(), 3);
    assert_eq!(
        api.all_orders(&player).await.len(),
        3,
        "and they can still be cancelled: nothing was lost"
    );
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_transfer_writes_no_ledger_row_even_when_its_verdict_read_failed() {
    // The refused-transfer case in `account_stream` reads the verdict
    // cleanly. Here the one read the verdict comes from fails transiently,
    // and the account reads a moment later — for the balances — succeed. A
    // transaction the gateway itself signed and has no answer for is not a
    // transaction the chain applied, and must not be written to either
    // party's ledger as a movement of money.
    let api = Api::with_markets().await;
    let sender = api.player("satoshi", 0).await;
    let recipient = api.player("hal", 1).await;
    api.fund(&sender, AssetId::Usd, 1_000_000_000);

    api.post_as(
        "/api/v1/transfers",
        &sender,
        json!({
            "to": cb_gateway::http::format_address(&recipient.trading_address),
            "asset": "USD",
            "amount": "100.00"
        }),
    )
    .await
    .assert_ok(200);
    api.harness.node.refuse(last_submitted(&api));
    api.harness.node.edit(|s| s.failing_account_reads = 1);
    settle(&api, 11).await;

    for player in [&sender, &recipient] {
        let ledger = api.get_as("/api/v1/account/transfers", player).await;
        assert_eq!(
            ledger.assert_ok(200)["entries"],
            json!([]),
            "a refused transfer is not a movement, whatever the verdict read did"
        );
    }
}

#[tokio::test]
async fn an_applied_transfer_writes_exactly_one_row_per_party_even_when_the_block_is_folded_twice()
{
    // The control for the test above, plus replay: the same block through the
    // watcher twice (a gateway replaying from `--start-height`) must not
    // double anybody's history.
    let api = Api::with_markets().await;
    let sender = api.player("satoshi", 0).await;
    let recipient = api.player("hal", 1).await;
    api.fund(&sender, AssetId::Usd, 1_000_000_000);
    api.post_as(
        "/api/v1/transfers",
        &sender,
        json!({
            "to": cb_gateway::http::format_address(&recipient.trading_address),
            "asset": "USD",
            "amount": "100.00"
        }),
    )
    .await
    .assert_ok(200);
    let block = api.harness.node.mine(11);
    for _ in 0..2 {
        cb_gateway::watcher::on_block(&api.harness.state, &block)
            .await
            .expect("processed");
    }
    let out = api.get_as("/api/v1/account/transfers", &sender).await;
    let out = out.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0]["kind"], "transfer_out");
    let inbound = api.get_as("/api/v1/account/transfers", &recipient).await;
    let inbound = inbound.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    assert_eq!(inbound.len(), 1, "{inbound:?}");
    assert_eq!(inbound[0]["kind"], "transfer_in");
}
