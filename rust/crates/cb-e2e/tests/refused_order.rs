//! An order the executor refuses is reported `rejected`, not left `pending`.
//!
//! Under the sequencer scheme a refused transaction is dropped from the block
//! and never recorded on chain, so until the node published its refusals
//! (`streamRejected`) nothing told the gateway that a submission had died.
//! The order sat `pending` until a later nonce from the same account proved
//! the point, or the hourly sweep gave it up. Found on 2026-10-10, hunting
//! why every spot market order was stuck: the tick-size defect was the
//! trigger, this was why it was silent.
//!
//! A self-trade is the cleanest refusal to provoke: the gateway has no
//! pre-check for it, the node admits the transaction, and the executor
//! refuses it when the order would match the sender's own resting bid.

use cb_e2e::flows::{await_book_level, funded_user, rest_limit};
use cb_e2e::money::dec;
use cb_e2e::{Journal, Stack};
use serde_json::json;

const PRICE: &str = "50000.00";
const QTY: &str = "0.01000000";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn an_order_the_executor_refuses_is_reported_rejected_not_left_pending() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let mut log = Journal::new("refused order");

    let (_session, user) = funded_user(api, 0, "selftrader").await;
    rest_limit(&user, "BTC-USD", "buy", PRICE, QTY).await;
    let before = user.get_ok("/account").await.ok(200).clone();

    // ---- a sell that would match the account's own bid ------------------
    let placed = user
        .post_ok(
            "/orders",
            json!({
                "symbol": "BTC-USD", "side": "sell", "type": "limit",
                "price": PRICE, "qty": QTY, "tif": "gtc"
            }),
        )
        .await;
    log.check(
        placed.status == 201,
        "the self-crossing sell is admitted: nothing before the executor refuses it",
        format!("HTTP {} {}", placed.status, placed.raw),
    );
    let order_id = placed.body["order_id"]
        .as_str()
        .expect("an accepted order has an id")
        .to_string();

    // The refusal has to reach the order record on its own. Without the
    // refusal stream this waits the full 30 s and sees `pending`.
    let order = user.await_order_status(&order_id, &["rejected"]).await;
    log.check_eq(
        order["status"].as_str().unwrap_or(""),
        "rejected",
        "the executor's refusal is reported on the order rather than left `pending`",
    );
    log.check(
        placed.body["status"] == "rejected",
        "the placement reply already says so when the refusal lands inside the confirm window",
        format!("the reply said {}", placed.body["status"]),
    );

    // ---- nothing happened on chain ----------------------------------------
    await_book_level(&user, "BTC-USD", "buy", PRICE, true).await;
    let after = user.get_ok("/account").await.ok(200).clone();
    for (asset, field) in [
        ("BTC", "free"),
        ("BTC", "locked"),
        ("USD", "free"),
        ("USD", "locked"),
    ] {
        log.check_eq(
            cb_e2e::client::balance_entry(&after, asset, field),
            cb_e2e::client::balance_entry(&before, asset, field),
            format!("{asset} {field} is untouched by a refused order"),
        );
    }

    // ---- and the account is not wedged -----------------------------------
    //
    // The refused transaction consumed no nonce. A gateway whose cached nonce
    // ran one ahead would have its next submission refused at admission and
    // only then re-sign; this one is told at once.
    let later = user
        .post_ok(
            "/orders",
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": "49000.00", "qty": QTY, "tif": "gtc"
            }),
        )
        .await;
    log.check(
        later.status == 201,
        "the next order from the same account is accepted",
        format!("HTTP {} {}", later.status, later.raw),
    );
    let later_id = later.body["order_id"]
        .as_str()
        .expect("an accepted order has an id")
        .to_string();
    let later_order = user.await_order_status(&later_id, &["open"]).await;
    log.check_eq(
        later_order["status"].as_str().unwrap_or(""),
        "open",
        "the next order reaches the book: the refusal left the nonce where the chain has it",
    );
    let _ = dec;

    log.finish();
}
