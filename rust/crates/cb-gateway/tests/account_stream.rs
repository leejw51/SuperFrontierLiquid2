//! What the gateway derives from a block, and what it tells the owner.
//!
//! The account channel and the order tracker are both *derivations*: the node
//! has no per-account order query, no owner on a fill and no liquidation
//! notification, so everything here is inferred by reading a user's own state
//! before and after a block. That makes this suite the one that decides whether
//! the gateway is telling the truth — an invented fill and a missed one look
//! identical from inside the process, and only a test that drives real blocks
//! through the real watcher can tell them apart.
//!
//! Frames are collected straight from the hub rather than over a socket. The
//! socket itself is covered by `ws_live`; what matters here is which frames are
//! produced, for whom, and what is in them.

mod common;

use cb_gateway::node::{MarginSnapshot, PositionSnapshot};
use cb_gateway::testing::Player;
use cb_gateway::ws::Channel;
use cb_types::{AssetId, DerivId, Side};
use common::Api;
use serde_json::{json, Value};

/// A subscriber on the private channel, as a browser would be.
struct AccountSocket {
    #[allow(dead_code)] // kept for debugging output
    id: u64,
    frames: tokio::sync::mpsc::Receiver<String>,
    /// Everything received so far. Kept because a test asks about several
    /// frame types in turn, and reading the queue consumes it.
    seen: Vec<Value>,
}

impl AccountSocket {
    /// Attach to the hub as `player`, authenticated by their real session.
    fn attach(api: &Api, player: &Player) -> Self {
        let hub = &api.harness.state.hub;
        let (id, frames) = hub.register();
        hub.authenticate(
            id,
            &player.user_id,
            &cb_gateway::auth::session_token_hash(&player.token),
        );
        hub.subscribe(id, Channel::Account)
            .expect("an authenticated connection may take the account channel");
        Self {
            id,
            frames,
            seen: Vec::new(),
        }
    }

    /// Everything delivered so far, decoded.
    fn drain(&mut self) -> &[Value] {
        while let Ok(text) = self.frames.try_recv() {
            self.seen
                .push(serde_json::from_str(&text).expect("the gateway sends JSON"));
        }
        &self.seen
    }

    /// Forget what has been seen, so a later assertion is about later frames.
    fn forget(&mut self) {
        self.drain();
        self.seen.clear();
    }

    /// Every frame of one type.
    fn of_type(&mut self, kind: &str) -> Vec<Value> {
        self.drain()
            .iter()
            .filter(|frame| frame["type"] == kind)
            .cloned()
            .collect()
    }
}

/// Drive the block carrying everything submitted so far through the watcher.
async fn settle(api: &Api, height: u64) {
    let block = api.harness.node.mine(height);
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");
}

/// One open BTC-PERP position of `qty` contracts.
fn long(qty: u128, entry_q8: u128) -> PositionSnapshot {
    PositionSnapshot {
        market: DerivId::perp(0),
        side: Side::Buy,
        qty,
        entry_px_q8: entry_q8,
        margin_mode: 0,
        isolated_margin: 0,
        entry_funding_index: 0,
        last_funding_height: 0,
    }
}

// ---------------------------------------------------------------------------
// §15 — the mirror must not invent history when it has none
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_restart_does_not_invent_a_fill_for_a_position_that_was_already_open() {
    // §15 of the flow review. The watcher derives a fill from a position whose
    // size changed. With an empty mirror — which is the state at start-up, on
    // reconnect and after a lagged subscription — every open position looked
    // like it had grown from nothing, so a restart wrote a fabricated "taker"
    // fill for every position anybody held, into the history they read and into
    // the volume figures the operator dashboard reports.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    let mut socket = AccountSocket::attach(&api, &player);

    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![long(150_000_000, 6_300_000_000_000)],
        );
    });

    // A block that makes the gateway look at this account: their own faucet
    // claim. The mirror is empty, exactly as it is one block after a restart.
    api.post_as("/api/v1/faucet/claim", &player, json!({}))
        .await
        .assert_ok(200);
    settle(&api, 11).await;

    let fills = api.get_as("/api/v1/account/fills", &player).await;
    assert_eq!(
        fills.assert_ok(200)["fills"],
        json!([]),
        "a position that was already open is not a trade that just happened"
    );
    assert!(
        socket.of_type("fill").is_empty(),
        "and nobody is told they were filled"
    );

    // The seeding must not disable the derivation: a real change in the NEXT
    // block is still a fill.
    api.harness.node.edit(|state| {
        state.positions.insert(
            player.trading_address,
            vec![long(200_000_000, 6_320_000_000_000)],
        );
    });
    settle(&api, 12).await;

    let fills = api.get_as("/api/v1/account/fills", &player).await;
    let fills = fills.assert_ok(200)["fills"]
        .as_array()
        .expect("fills")
        .clone();
    assert_eq!(fills.len(), 1, "the position grew by half a contract");
    assert_eq!(fills[0]["qty"], "0.5000");
    assert_eq!(fills[0]["side"], "buy");
}

// ---------------------------------------------------------------------------
// §10 — a spot fill has to move the order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_spot_fill_moves_the_order_and_is_recorded() {
    // §10 of the flow review. Fills were derived only from *position* deltas,
    // which exist on derivative markets alone, so a spot order that filled on
    // chain stayed `open` with `filled_qty` zero for ever. The user then
    // cancelled the phantom the screen still showed them, and the cancel of an
    // order the engine had already retired was an error that took the whole
    // block down.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000); // 100,000 USD
    let mut socket = AccountSocket::attach(&api, &player);

    let placed = api
        .post_as(
            "/api/v1/orders",
            &player,
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": "50000.00", "qty": "0.50000000"
            }),
        )
        .await;
    placed.assert_ok(201);
    settle(&api, 11).await;

    // The chain fills it: 0.5 BTC in, 25,000 USD out.
    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        *account.balances.entry(AssetId::Usd).or_insert(0) -= 25_000_000_000;
        *account.balances.entry(AssetId::Btc).or_insert(0) += 50_000_000;
    });
    settle(&api, 12).await;

    let orders = api
        .get_as("/api/v1/account/orders?status=all", &player)
        .await;
    let orders = orders.assert_ok(200)["orders"]
        .as_array()
        .expect("orders")
        .clone();
    assert_eq!(orders.len(), 1, "one order was placed");
    let order = orders[0].clone();
    // The block gave it its real, engine-derived id; until then it was known by
    // the hash of the transaction that placed it.
    let order_id = order["order_id"].as_str().expect("an order id").to_string();
    assert_eq!(order["status"], "filled");
    assert_eq!(order["filled_qty"], "0.50000000");
    assert_eq!(order["remaining_qty"], "0.00000000");

    let fills = api.get_as("/api/v1/account/fills", &player).await;
    let fills = fills.assert_ok(200)["fills"]
        .as_array()
        .expect("fills")
        .clone();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0]["symbol"], "BTC-USD");
    assert_eq!(fills[0]["side"], "buy");
    assert_eq!(fills[0]["qty"], "0.50000000");
    // Recovered from the two balance movements the trade caused, and rendered
    // with the market's own price precision.
    let reference = api.get("/api/v1/markets/BTC-USD/ticker").await;
    let reference = reference.assert_ok(200)["last_price"].clone();
    assert_eq!(fills[0]["price"], reference);
    assert_eq!(
        fills[0]["price"]
            .as_str()
            .and_then(|p| p.parse::<f64>().ok()),
        Some(50_000.0)
    );
    assert_eq!(fills[0]["order_id"], json!(order_id));

    // And the owner is told, on the channel the contract gives them.
    assert_eq!(socket.of_type("fill").len(), 1);
}

#[tokio::test]
async fn a_cancelled_spot_order_is_not_reported_as_a_fill() {
    // The other half of the same inference. Cancelling a resting order releases
    // a lock: the totals do not move, and nothing was traded. Reading a release
    // as a trade would put an invented fill in the history of every user who
    // ever changed their mind.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
    // The chain locks the quote against the resting order: `locked` grows and
    // the balance, which is the total, does not move.
    api.harness
        .node
        .lock(player.trading_address, AssetId::Usd, 25_000_000_000);
    settle(&api, 11).await;

    // The cancel releases it again.
    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        account.locked.remove(&AssetId::Usd);
    });
    settle(&api, 12).await;

    let fills = api.get_as("/api/v1/account/fills", &player).await;
    assert_eq!(
        fills.assert_ok(200)["fills"],
        json!([]),
        "a lock and its release are not a trade"
    );
}

// ---------------------------------------------------------------------------
// §17 and §16(d) — the events that move money without the user's participation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_liquidation_reaches_the_victim_who_has_no_resting_order() {
    // §17 and §16 of the flow review. Only accounts with a live order were
    // refreshed, and a liquidation is triggered by a keeper: the victim is
    // neither the sender of the transaction nor the holder of a resting order,
    // so the one event that empties their account was the one event the gateway
    // could not see. Nothing published a `liquidation` frame, nothing wrote a
    // `liquidation` ledger entry, and the operator dashboard's count of them
    // sat at zero for ever.
    let api = Api::with_markets().await;
    let victim = api.player("satoshi", 0).await;
    let keeper = api.player("keeper", 1).await;
    let mut socket = AccountSocket::attach(&api, &victim);

    api.harness.node.edit(|state| {
        state.positions.insert(
            victim.trading_address,
            vec![long(150_000_000, 6_300_000_000_000)],
        );
        state.margins.insert(
            victim.trading_address,
            MarginSnapshot {
                collateral: 9_000_000_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });

    // Block one: the gateway sees the account for the first time and seeds.
    api.post_as("/api/v1/faucet/claim", &victim, json!({}))
        .await
        .assert_ok(200);
    settle(&api, 11).await;
    socket.forget();

    // Block two: somebody else's keeper closes the position out.
    api.harness.node.edit(|state| {
        state.positions.insert(victim.trading_address, Vec::new());
        state.margins.insert(
            victim.trading_address,
            MarginSnapshot {
                collateral: 1_800_000_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });
    let block = cb_gateway::node::BlockSummary {
        height: 12,
        hash: cb_types::H256([12; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_001_200,
        proposer: None,
        txs: vec![cb_gateway::node::BlockTx {
            applied: None,
            hash: cb_types::H256([99; 32]),
            // The keeper sends it. The victim is not in this block at all.
            sender: keeper.trading_address,
            index: 0,
            body: cb_tx::TxBody::LiquidationTrigger {
                owner: victim.trading_address,
                market: DerivId::perp(0),
            },
        }],
    };
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");

    let liquidations = socket.of_type("liquidation");
    assert_eq!(liquidations.len(), 1, "the victim is told");
    let data = &liquidations[0]["data"];
    assert_eq!(data["symbol"], "BTC-PERP");
    assert_eq!(data["qty"], "1.5000");
    assert_eq!(
        data["loss"], "-7200.00",
        "the collateral the account no longer has, as the chain reports it"
    );
    assert!(data["price"].is_string() && data["at"].is_string());

    // The position row is published as a zero, which is how a client is told to
    // remove it; publishing nothing would leave the blotter showing a position
    // that no longer exists.
    let positions = socket.of_type("position_update");
    assert_eq!(positions.len(), 1);
    assert_eq!(
        positions[0]["data"]["qty"]
            .as_str()
            .and_then(|q| q.parse::<f64>().ok()),
        Some(0.0)
    );

    // It is not reported as a fill: the user placed no order.
    let fills = api.get_as("/api/v1/account/fills", &victim).await;
    assert_eq!(fills.assert_ok(200)["fills"], json!([]));

    // And the ledger has the entry the dashboard counts.
    let ledger = api.get_as("/api/v1/account/transfers", &victim).await;
    let entries = ledger.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    let liquidation = entries
        .iter()
        .find(|e| e["kind"] == "liquidation")
        .expect("a liquidation ledger entry");
    assert_eq!(liquidation["amount"], "-7200.00");
}

// ---------------------------------------------------------------------------
// §16 — the frames themselves
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_account_frame_is_the_shape_the_client_deserialises() {
    // §16 of the flow review: all four account frames failed to decode in the
    // browser, so the private channel delivered nothing usable at all.
    // `balance_update` was an object where an array was expected,
    // `position_update` carried four of fifteen required fields, `fill` omitted
    // `realized_pnl`, and `order_update` was emitted by nobody.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);
    let mut socket = AccountSocket::attach(&api, &player);

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
    settle(&api, 11).await;

    // `order_update`, on the transition from pending to open.
    let orders = socket.of_type("order_update");
    assert!(
        !orders.is_empty(),
        "an order that opened is an order update"
    );
    let order = &orders[0]["data"];
    for field in [
        "order_id",
        "symbol",
        "side",
        "type",
        "price",
        "qty",
        "filled_qty",
        "remaining_qty",
        "status",
        "tif",
        "post_only",
        "reduce_only",
        "created_at",
        "updated_at",
    ] {
        assert!(
            order.get(field).is_some(),
            "order_update is missing `{field}`: {order}"
        );
    }
    assert_eq!(order["status"], "open");

    // A fill, a balance change and a position, in one block.
    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        *account.balances.entry(AssetId::Usd).or_insert(0) -= 25_000_000_000;
        *account.balances.entry(AssetId::Btc).or_insert(0) += 50_000_000;
        state.positions.insert(
            player.trading_address,
            vec![long(150_000_000, 6_300_000_000_000)],
        );
    });
    settle(&api, 12).await;

    let balances = socket.of_type("balance_update");
    assert!(!balances.is_empty(), "the balance moved");
    let payload = balances[0]["data"]
        .as_array()
        .expect("balance_update carries the array of balances, not an object wrapping it");
    let usd = payload
        .iter()
        .find(|b| b["asset"] == "USD")
        .expect("a USD balance");
    for field in ["asset", "free", "locked", "total"] {
        assert!(usd.get(field).is_some(), "balance is missing `{field}`");
    }

    // Two fills: the spot trade the balances imply, and the perpetual position
    // that appeared. Both carry the same shape.
    let fills = socket.of_type("fill");
    assert_eq!(fills.len(), 2);
    let fill = &fills[0]["data"];
    for field in [
        "fill_id",
        "order_id",
        "symbol",
        "side",
        "price",
        "qty",
        "fee",
        "fee_asset",
        "role",
        "realized_pnl",
        "at",
        "height",
    ] {
        assert!(
            fill.get(field).is_some(),
            "fill is missing `{field}`: {fill}"
        );
    }

    let positions = socket.of_type("position_update");
    assert_eq!(positions.len(), 1, "the position appeared");
    let position = &positions[0]["data"];
    for field in [
        "symbol",
        "side",
        "qty",
        "entry_price",
        "mark_price",
        "liquidation_price",
        "notional",
        "margin_mode",
        "leverage",
        "margin",
        "unrealized_pnl",
        "unrealized_pnl_pct",
        "funding_paid",
        "opened_at",
    ] {
        assert!(
            position.get(field).is_some(),
            "position_update is missing `{field}`: {position}"
        );
    }
}

// ---------------------------------------------------------------------------
// §8 — the ticker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ticker_frame_carries_every_field_the_contract_names() {
    // §8 of the flow review. The frame carried `symbol` and `last_price` and
    // nothing else; the browser's ticker type requires `mark_price` and
    // `change_24h_pct` with no defaults, so every update was dropped, every
    // price on the terminal froze at its page-load value and wore the stale
    // treatment permanently — while the connection indicator still read "Live".
    let api = Api::with_markets().await;
    let (id, mut frames) = api.harness.state.hub.register();
    api.harness
        .state
        .hub
        .subscribe(id, Channel::Ticker)
        .expect("the ticker needs no session");

    // A derivative prints a trade: its published last trade moves.
    api.harness.node.edit(|state| {
        if let Some(market) = state.perp_markets.first_mut() {
            market.last_trade_px_q8 = 6_430_000_000_000;
            market.last_trade_height = 11;
        }
    });
    settle(&api, 11).await;

    let mut ticker = None;
    while let Ok(text) = frames.try_recv() {
        let frame: Value = serde_json::from_str(&text).expect("JSON");
        if frame["channel"] == "ticker" && frame["type"] == "update" {
            ticker = Some(frame);
        }
    }
    let ticker = ticker.expect("a trade print publishes a ticker update");
    let data = &ticker["data"];
    for field in [
        "symbol",
        "last_price",
        "mark_price",
        "change_24h_pct",
        "funding_rate",
    ] {
        assert!(
            data.get(field).is_some_and(|v| v.is_string()),
            "ticker is missing `{field}` as a string: {data}"
        );
    }
    assert_eq!(data["symbol"], "BTC-PERP");
    assert_eq!(data["last_price"], "64300.00");
}

#[tokio::test]
async fn a_spot_market_publishes_a_ticker_from_a_fill_it_derived() {
    // The other half of §8: the ticker was published only when a *derivative*
    // printed a trade, so a spot market never emitted one at all and its quote
    // was whatever the page loaded with. The chain publishes no last trade for
    // a spot market, so the only price the gateway can ever print for one is a
    // fill it derived from a user's own balances — which is a real trade at a
    // real price, and is labelled as the estimate of quantity it is.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);
    let (id, mut frames) = api.harness.state.hub.register();
    api.harness
        .state
        .hub
        .subscribe(id, Channel::Ticker)
        .expect("the ticker needs no session");

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
    settle(&api, 11).await;
    while frames.try_recv().is_ok() {}

    api.harness.node.edit(|state| {
        let account = state.accounts.entry(player.trading_address).or_default();
        *account.balances.entry(AssetId::Usd).or_insert(0) -= 25_000_000_000;
        *account.balances.entry(AssetId::Btc).or_insert(0) += 50_000_000;
    });
    settle(&api, 12).await;

    let mut spot_ticker = None;
    while let Ok(text) = frames.try_recv() {
        let frame: Value = serde_json::from_str(&text).expect("JSON");
        if frame["channel"] == "ticker" && frame["data"]["symbol"] == "BTC-USD" {
            spot_ticker = Some(frame);
        }
    }
    let ticker = spot_ticker.expect("a spot market publishes a ticker once it has a price");
    assert_eq!(
        ticker["data"]["last_price"]
            .as_str()
            .and_then(|p| p.parse::<f64>().ok()),
        Some(50_000.0)
    );
    assert_eq!(
        ticker["data"]["mark_price"]
            .as_str()
            .and_then(|p| p.parse::<f64>().ok()),
        Some(0.0),
        "spot has no mark price and says so, rather than borrowing the last trade"
    );
}

#[tokio::test]
async fn a_funding_tick_reaches_the_holder_who_did_not_send_it() {
    // The other half of §17. A `FundingTick` is a system transaction the
    // proposer sends; it moves only the holder's margin collateral, and the
    // holder appears nowhere in the block. Nothing watched them, nothing
    // published the change, and the contract's `funding` ledger kind had no
    // producer at all — so the money left the account and the account's own
    // history said nothing happened.
    let api = Api::with_markets().await;
    let holder = api.player("satoshi", 0).await;

    api.harness.node.edit(|state| {
        state.positions.insert(
            holder.trading_address,
            vec![long(150_000_000, 6_300_000_000_000)],
        );
        state.margins.insert(
            holder.trading_address,
            MarginSnapshot {
                collateral: 9_000_000_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });

    // Block one seeds the baseline.
    api.post_as("/api/v1/faucet/claim", &holder, json!({}))
        .await
        .assert_ok(200);
    settle(&api, 11).await;

    // Block two: funding is charged. The holder has no resting order and sent
    // nothing.
    api.harness.node.edit(|state| {
        state.margins.insert(
            holder.trading_address,
            MarginSnapshot {
                collateral: 8_998_500_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });
    let block = cb_gateway::node::BlockSummary {
        height: 12,
        hash: cb_types::H256([12; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_001_200,
        proposer: None,
        txs: vec![cb_gateway::node::BlockTx {
            applied: None,
            hash: cb_types::H256([77; 32]),
            sender: cb_types::Address([9; 20]),
            index: 0,
            body: cb_tx::TxBody::FundingTick {
                market: DerivId::perp(0),
            },
        }],
    };
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");

    let ledger = api.get_as("/api/v1/account/transfers", &holder).await;
    let entries = ledger.assert_ok(200)["entries"]
        .as_array()
        .expect("entries")
        .clone();
    let funding = entries
        .iter()
        .find(|e| e["kind"] == "funding")
        .expect("a funding entry");
    // 9,000.00 of collateral less 8,998.50: one and a half dollars of funding.
    assert_eq!(funding["amount"], "-1.50", "collateral the account lost");
    assert_eq!(funding["balance_after"], "8998.50");
    assert!(funding["memo"]
        .as_str()
        .is_some_and(|m| m.contains("BTC-PERP")));

    // A collateral change with no funding tick in the block is not funding: it
    // is a fill, a fee or a liquidation, and each of those is recorded by the
    // code that can tell which.
    api.harness.node.edit(|state| {
        state.margins.insert(
            holder.trading_address,
            MarginSnapshot {
                collateral: 8_000_000_000,
                unrealized_pnl: 0,
                bad_debt: 0,
            },
        );
    });
    settle(&api, 13).await;
    let ledger = api.get_as("/api/v1/account/transfers", &holder).await;
    assert_eq!(
        ledger.assert_ok(200)["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter(|e| e["kind"] == "funding")
            .count(),
        1,
        "only the block that carried a tick writes a funding entry"
    );
}

// ---------------------------------------------------------------------------
// ADR 0008 — a committed transaction the executor refused
// ---------------------------------------------------------------------------

/// The hash of the one transaction submitted since the last block.
fn last_submitted(api: &Api) -> cb_types::H256 {
    api.harness
        .node
        .submitted()
        .last()
        .expect("something was submitted")
        .hash()
}

#[tokio::test]
async fn an_order_the_chain_committed_and_refused_is_rejected_not_open() {
    // ADR 0008, *Consequences*. Under BFT ordering a block keeps every
    // committed transaction, applied or not, and nothing in it says which. The
    // gateway used to promote any placement it saw in a block to `open`, so an
    // order the executor refused sat on the user's blotter, resting and
    // cancellable, until nothing ever happened to it. The sender's nonce is
    // the chain's answer: a refused transaction does not consume it.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);
    let mut socket = AccountSocket::attach(&api, &player);

    let placed = api
        .post_as(
            "/api/v1/orders",
            &player,
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": "50000.00", "qty": "0.50000000"
            }),
        )
        .await;
    let tx_hash = placed.assert_ok(201)["tx_hash"]
        .as_str()
        .expect("a hash")
        .to_string();
    assert_eq!(
        format!("0x{}", hex::encode(last_submitted(&api).0)),
        tx_hash
    );

    // The block carries it; the executor refuses it.
    api.harness.node.refuse(last_submitted(&api));
    settle(&api, 11).await;

    let orders = api
        .get_as("/api/v1/account/orders?status=all", &player)
        .await;
    let orders = orders.assert_ok(200)["orders"]
        .as_array()
        .expect("orders")
        .clone();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["status"], "rejected");
    assert_eq!(
        orders[0]["order_id"], tx_hash,
        "no on-chain id: the chain never issued one, so it is still known by its hash"
    );
    let open = api.get_as("/api/v1/account/orders", &player).await;
    assert_eq!(open.assert_ok(200)["orders"], json!([]), "not open");

    let updates = socket.of_type("order_update");
    assert_eq!(updates.len(), 1, "the owner is told once");
    assert_eq!(updates[0]["data"]["status"], "rejected");
    assert!(
        updates.iter().all(|u| u["data"]["status"] != "open"),
        "and is never told it opened"
    );

    // A cancel of it is refused rather than signed.
    api.delete_as(&format!("/api/v1/orders/{tx_hash}"), &player)
        .await
        .assert_error(400, "VALIDATION_FAILED");
    assert!(
        api.harness
            .state
            .tracker
            .addresses_with_live_orders()
            .is_empty(),
        "the account is no longer polled for an order that does not exist"
    );
}

#[tokio::test]
async fn an_order_the_chain_applied_still_opens() {
    // The control for the test above: the same flow without the refusal.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
    settle(&api, 11).await;

    let orders = api.get_as("/api/v1/account/orders", &player).await;
    let orders = orders.assert_ok(200)["orders"]
        .as_array()
        .expect("orders")
        .clone();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["status"], "open");
    assert_eq!(
        api.harness.state.submitter.in_flight_count(),
        0,
        "answered for"
    );
}

#[tokio::test]
async fn a_transfer_the_chain_refused_writes_no_ledger_entry() {
    // The ledger records what the chain did. A committed transfer the
    // executor refused moved nothing, and a `transfer_out` for it would be the
    // lie §20 of the flow review removed, back by another route.
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
    settle(&api, 11).await;

    for player in [&sender, &recipient] {
        let ledger = api.get_as("/api/v1/account/transfers", player).await;
        assert_eq!(
            ledger.assert_ok(200)["entries"],
            json!([]),
            "a refused transfer is not a movement"
        );
    }
}

#[tokio::test]
async fn a_cancel_the_chain_refused_puts_the_order_back_on_the_blotter() {
    // `DELETE /orders/{id}` answers `cancelled` on mempool acceptance. When
    // the executor refuses the cancel, the order is still resting, and the
    // owner has to be told so rather than left believing it is gone.
    let api = Api::with_markets().await;
    let player = api.player("satoshi", 0).await;
    api.fund(&player, AssetId::Usd, 100_000_000_000);
    let mut socket = AccountSocket::attach(&api, &player);

    api.post_as(
        "/api/v1/orders",
        &player,
        json!({
            "symbol": "BTC-USD", "side": "buy", "type": "limit",
            "price": "50000.00", "qty": "0.50000000"
        }),
    )
    .await
    .assert_ok(201);
    settle(&api, 11).await;
    let orders = api.get_as("/api/v1/account/orders", &player).await;
    let order_id = orders.assert_ok(200)["orders"][0]["order_id"]
        .as_str()
        .expect("an id")
        .to_string();

    let reply = api
        .delete_as(&format!("/api/v1/orders/{order_id}"), &player)
        .await;
    assert_eq!(reply.assert_ok(200)["status"], "cancelled");
    socket.forget();

    api.harness.node.refuse(last_submitted(&api));
    settle(&api, 12).await;

    let orders = api.get_as("/api/v1/account/orders", &player).await;
    assert_eq!(orders.assert_ok(200)["orders"][0]["status"], "open");
    let updates = socket.of_type("order_update");
    assert_eq!(updates.len(), 1, "the owner is corrected");
    assert_eq!(updates[0]["data"]["status"], "open");
}

/// Under BFT a block says which of its transactions applied — everyone's,
/// not only the ones this gateway signed — and the watcher must read that
/// for a stranger's transfer too. A refused one used to be taken for
/// applied and written into the recipient's ledger (audit finding B1).
#[tokio::test]
async fn a_strangers_refused_transfer_writes_no_ledger_row() {
    let api = Api::with_markets().await;
    let victim = api.player("satoshi", 0).await;
    let stranger = cb_types::Address([0xee; 20]);
    let block = |height: u64, applied: bool| cb_gateway::node::BlockSummary {
        height,
        hash: cb_types::H256([height as u8; 32]),
        state_root: cb_types::H256([1; 32]),
        timestamp_ms: 1_700_000_000_000 + height,
        proposer: None,
        txs: vec![cb_gateway::node::BlockTx {
            applied: Some(applied),
            hash: cb_types::H256([0xd0 + height as u8; 32]),
            sender: stranger,
            index: 0,
            body: cb_tx::TxBody::Transfer {
                to: victim.trading_address,
                asset: AssetId::Usd,
                amount: 5_000_000,
            },
        }],
    };
    let transfers_in = |ledger: &Value| -> usize {
        ledger["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .filter(|e| e["kind"] == "transfer_in")
            .count()
    };

    cb_gateway::watcher::on_block(&api.harness.state, &block(12, false))
        .await
        .expect("the block is processed");
    let ledger = api.get_as("/api/v1/account/transfers", &victim).await;
    assert_eq!(
        transfers_in(ledger.assert_ok(200)),
        0,
        "a transfer the executor refused is not in the recipient's ledger"
    );

    cb_gateway::watcher::on_block(&api.harness.state, &block(13, true))
        .await
        .expect("the block is processed");
    let ledger = api.get_as("/api/v1/account/transfers", &victim).await;
    assert_eq!(transfers_in(ledger.assert_ok(200)), 1, "an applied one is");
}
