//! A spot market order fills, and the tick size the gateway publishes is the
//! chain's.
//!
//! Found on a hand run on 2026-10-10: every `type: market` spot order sat
//! `pending` for the rest of the session while GTC, IOC and FOK limit orders
//! filled. The gateway rounded the market order's protective price to a tick
//! it had guessed (one wire unit, 0.000001 USD on BTC-USD) rather than the
//! one genesis gives the market (0.1), and the executor refused the order
//! with `price must be a multiple of tick_size`. The sequencer drops a
//! refused transaction from the block, so the gateway never saw a verdict.
//!
//! Only the real binaries show this. The gateway's own suites run against a
//! mock node that enforces no tick, and a limit order typed on a round price
//! happens to be a multiple of any tick, which is why the journey test passed
//! throughout.

use cb_e2e::flows::{await_book_level, funded_user, rest_limit};
use cb_e2e::money::dec;
use cb_e2e::{Journal, Stack, StackConfig};
use serde_json::json;

// On the 0.1 tick, and chosen so that the gateway's 5% protective bound for
// a market sell (47500.095) is *not*: a gateway rounding to a guessed finer
// tick sends that price and the executor refuses it. A round price would
// pass with or without the fix.
const PRICE: &str = "50000.10";
const QTY: &str = "0.01000000";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_spot_market_order_fills_against_resting_liquidity() {
    // The shipped refresh period, not the harness's one-second one. The
    // catalogue's first periodic refresh fires before the node is connected
    // and fails, so with 30 s between attempts the chain's tick reaches the
    // gateway promptly only because connecting triggers a refresh of its own.
    // The harness waits at boot until the catalogue has been read from the
    // sequencer, so without that trigger this stack takes the whole period
    // to come up, and `make start` refused every market order placed in its
    // first half minute. The boot time is therefore the assertion.
    let booted = std::time::Instant::now();
    let stack = Stack::start_with(StackConfig {
        market_refresh_seconds: 30,
        ..StackConfig::default()
    })
    .await;
    let boot = booted.elapsed();
    let api = &stack.api;
    let mut log = Journal::new("spot market order");
    log.check(
        boot < std::time::Duration::from_secs(15),
        "the gateway reads the chain's market listing as soon as it connects, not a refresh period later",
        format!("the stack took {boot:.1?} to serve a catalogue read from the sequencer"),
    );

    let (_maker_session, maker) = funded_user(api, 0, "mkt_maker").await;
    let (_taker_session, taker) = funded_user(api, 1, "mkt_taker").await;

    // ---- the published increments are the chain's -----------------------
    //
    // genesis-full.jsonl lists btc_usd with tick_size 100000 and min_qty 1000
    // in the wire scales: 0.1 USD and 0.00001 BTC. Anything else here means
    // the gateway is guessing, and its market orders will be refused below.
    let markets = maker.get_ok("/markets").await;
    let btc = markets.ok(200)["markets"]
        .as_array()
        .expect("a markets array")
        .iter()
        .find(|market| market["symbol"] == "BTC-USD")
        .cloned()
        .expect("BTC-USD is listed");
    // Compared as numbers: the gateway renders a tick at the market's display
    // precision ("0.10"), and the fact under test is the value, not the text.
    log.check_eq(
        btc["tick_size"].as_str().map(dec).unwrap_or_default(),
        dec("0.1"),
        "the gateway publishes the tick size genesis configured, not a guess",
    );
    log.check_eq(
        btc["lot_size"].as_str().map(dec).unwrap_or_default(),
        dec("0.00001"),
        "the gateway publishes the lot size genesis configured",
    );

    // ---- a market sell into a resting bid --------------------------------
    rest_limit(&maker, "BTC-USD", "buy", PRICE, QTY).await;

    let btc_before =
        cb_e2e::client::balance_entry(maker.get_ok("/account").await.ok(200), "BTC", "total");

    let placed = taker
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "sell", "type": "market", "qty": QTY }),
        )
        .await;
    log.check(
        placed.status == 201,
        "a spot market sell is accepted",
        format!("HTTP {} {}", placed.status, placed.raw),
    );
    let order_id = placed.body["order_id"]
        .as_str()
        .expect("an accepted order has an id")
        .to_string();

    // The protective price the gateway chose is on the chain's tick, or the
    // executor refuses the order and it never leaves `pending`.
    let order = taker.await_order_status(&order_id, &["filled"]).await;
    log.check_eq(
        order["status"].as_str().unwrap_or(""),
        "filled",
        "the market order reaches `filled` rather than a permanent `pending`",
    );
    log.check(
        order["filled_qty"].as_str().map(dec) == Some(dec(QTY)),
        "the whole quantity filled",
        format!("filled_qty was {}", order["filled_qty"]),
    );

    // The fill is real when the base asset has moved.
    let settled = maker
        .await_account("the bought BTC to arrive", |account| {
            cb_e2e::client::balance_entry(account, "BTC", "total") >= btc_before + dec(QTY)
        })
        .await;
    log.check(
        cb_e2e::client::balance_entry(&settled, "BTC", "total") >= btc_before + dec(QTY),
        "the fill moved the base asset to the maker",
        format!(
            "BTC went from {btc_before} to {}",
            cb_e2e::client::balance_entry(&settled, "BTC", "total")
        ),
    );
    await_book_level(&maker, "BTC-USD", "buy", PRICE, false).await;

    log.finish();
}
