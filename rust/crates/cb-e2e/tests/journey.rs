//! The journey the product promises, against the binaries the product ships.
//!
//! This is the single most important test in the repository, because it is the
//! only one that asks the question a user asks: *does the thing work?* Every
//! step below is something the interface offers and a person will do in their
//! first two minutes — sign in with a mnemonic, claim test money, watch it
//! arrive on chain, place an order, see it on the book, cancel it, get the
//! money back, then trade against someone.
//!
//! It is one test rather than seven because the sequence is the subject. Seven
//! independent tests would not notice that the cancel only refunds when nothing
//! has filled, or that the balance view is right at rest and wrong while an
//! order is live. A journey notices.
//!
//! Observations are recorded in a [`Journal`] and reported together at the end,
//! so one broken step does not hide the five after it — see `journal.rs` for
//! why that is not a weaker assertion.
//!
//! # Why spot, and not a perpetual
//!
//! A fresh chain has no oracle price for any derivative, and the contract is
//! explicit that a derivative order without a fresh mark is `422 ORACLE_STALE`.
//! Putting the headline journey on a perpetual would therefore be testing the
//! oracle harness rather than the product. `BTC-USD` needs no mark, and the
//! shipped faucet grant — 100,000 USD and 1 BTC — is exactly the pair of
//! balances two accounts need to trade it with each other. The derivative path
//! is covered where it belongs, in `contract.rs`.

use cb_e2e::flows::{await_book_level, funded_user, TEST_ADDRESS_INDEX_0, TEST_MNEMONIC};
use cb_e2e::money::dec;
use cb_e2e::{Journal, Stack};
use serde_json::json;

/// Price and size for the whole journey. 0.02 BTC at 50,000 is 1,000 USD of
/// notional: comfortably over the 10.00 minimum, comfortably inside a
/// 100,000 USD grant, and a round number in every balance assertion.
const PRICE: &str = "50000.00";
const QTY: &str = "0.02000000";
const NOTIONAL: &str = "1000.00";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_new_user_can_sign_in_claim_test_money_trade_and_get_their_money_back() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let mut log = Journal::new("the product journey");

    // ---- sign in with a mnemonic ---------------------------------------
    //
    // The contract pins the derivation to the wider EVM ecosystem: this phrase
    // at index 0 must be 0x9858EfFD…, the same address MetaMask shows. A
    // different address here means this exchange has invented its own
    // derivation and no existing wallet can sign in.
    let maker_session = api.dev_login(TEST_MNEMONIC, 0, "journey_maker").await;
    log.check_eq(
        maker_session.address.as_str(),
        TEST_ADDRESS_INDEX_0,
        "signing in with the canonical mnemonic gives the address every EVM wallet gives",
    );
    log.check(
        maker_session.is_new,
        "a first sign-in reports the account as new",
        format!("is_new was {}", maker_session.is_new),
    );
    let maker = api.as_user(&maker_session);

    // The session endpoint is what the frontend uses to decide what to show.
    let session = maker.get_ok("/auth/session").await;
    let session = session.ok(200);
    log.check(
        session["login_address"].as_str() == Some(TEST_ADDRESS_INDEX_0),
        "the session names the login address the user signed in with",
        format!("login_address was {}", session["login_address"]),
    );
    log.check(
        session["trading_address"]
            .as_str()
            .is_some_and(|address| address != TEST_ADDRESS_INDEX_0),
        "the session names a distinct trading address, as the two-address design requires",
        format!("trading_address was {}", session["trading_address"]),
    );

    // ---- claim the faucet ----------------------------------------------
    //
    // This is the click that found fault 1 and fault 2 by hand. The status call
    // first, because the button is drawn from it.
    let faucet = maker.get_ok("/faucet").await;
    let faucet = faucet.ok(200);
    log.check(
        faucet["available"].as_bool() == Some(true),
        "a fresh account is told the faucet is available",
        format!("available was {}", faucet["available"]),
    );
    let granted_assets: Vec<String> = faucet["grant"]
        .as_array()
        .map(|grants| {
            grants
                .iter()
                .filter_map(|g| g["asset"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let claim = maker.post_ok("/faucet/claim", json!({})).await;
    log.check(
        claim.status == 200,
        "claiming the faucet succeeds",
        format!("HTTP {} {}", claim.status, claim.raw),
    );
    log.check(
        claim.body["tx_hash"].as_str().is_some(),
        "the claim names the chain transaction that minted the money",
        format!("tx_hash was {}", claim.body["tx_hash"]),
    );

    // ---- see the money arrive ON CHAIN ---------------------------------
    //
    // Not "the gateway said 200": the grant is a Deposit signed by the treasury
    // and it either executed or it did not. Fault 1 was a treasury nobody had a
    // key for, which returns a happy answer and mints nothing.
    let account = maker
        .await_account("the faucet grant to arrive on chain", |account| {
            cb_e2e::client::balance_entry(account, "USD", "total") > dec("0")
        })
        .await;
    for asset in &granted_assets {
        let total = cb_e2e::client::balance_entry(&account, asset, "total");
        log.check(
            total > dec("0"),
            format!("the granted {asset} is on chain and visible in the balance"),
            format!("{asset} total was {total}; the grant promised some"),
        );
    }
    let usd_before = cb_e2e::client::balance_entry(&account, "USD", "free");
    let usd_total_before = cb_e2e::client::balance_entry(&account, "USD", "total");

    // ---- place a limit order, and see it rest --------------------------
    let placed = maker
        .post_ok(
            "/orders",
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": PRICE, "qty": QTY, "tif": "gtc",
                "client_order_id": "cli_journey_1"
            }),
        )
        .await;
    log.check(
        placed.status == 201,
        "a limit order is accepted",
        format!("HTTP {} {}", placed.status, placed.raw),
    );
    let order_id = placed.body["order_id"]
        .as_str()
        .unwrap_or_else(|| panic!("POST /orders returned no order_id: {}", placed.raw))
        .to_string();

    let resting = maker.await_order_status(&order_id, &["open"]).await;
    log.check(
        dec(resting["remaining_qty"].as_str().unwrap_or("0")) == dec(QTY),
        "the resting order still has its whole quantity outstanding",
        format!("remaining_qty was {}", resting["remaining_qty"]),
    );

    // The book is the market's view, and it is the one that matters: an order
    // only its owner can see is not liquidity.
    await_book_level(&maker, "BTC-USD", "buy", PRICE, true).await;
    let book = maker.get_ok("/markets/BTC-USD/orderbook?depth=50").await;
    let bids = book.ok(200)["bids"].clone();
    log.check(
        bids.as_array().is_some_and(|levels| !levels.is_empty()),
        "the order appears on the public order book",
        format!("bids were {bids}"),
    );

    // ---- the money is locked, not conjured ------------------------------
    //
    // The contract's own example is free 94,000 + locked 6,000 = total 100,000
    // for an account that deposited 100,000. Locking collateral moves money
    // between two columns of one holding; it does not create any. An account
    // whose total rises when it places an order is reporting money that does
    // not exist, and every equity, margin and leaderboard number built on that
    // total is wrong.
    let live = maker
        .await_account("the order's collateral to be locked", |account| {
            cb_e2e::client::balance_entry(account, "USD", "locked") > dec("0")
        })
        .await;
    let usd_free_live = cb_e2e::client::balance_entry(&live, "USD", "free");
    let usd_locked_live = cb_e2e::client::balance_entry(&live, "USD", "locked");
    let usd_total_live = cb_e2e::client::balance_entry(&live, "USD", "total");

    log.check(
        usd_locked_live >= dec(NOTIONAL),
        "at least the order's notional is locked",
        format!("locked {usd_locked_live} against a notional of {NOTIONAL}"),
    );
    log.check(
        usd_total_live == usd_total_before,
        "placing an order does not change the total balance",
        format!(
            "total was {usd_total_before} before the order and {usd_total_live} while it rests, \
             an increase of {}. Locking collateral must move money from free to locked, not \
             add to the holding.",
            usd_total_live - usd_total_before
        ),
    );
    log.check(
        usd_free_live <= usd_before - dec(NOTIONAL),
        "the locked collateral has left the free balance",
        format!(
            "free was {usd_before} before the order and {usd_free_live} while it rests; \
             {NOTIONAL} of notional should no longer be spendable"
        ),
    );

    // ---- cancel, and see the funds unlock -------------------------------
    let cancelled = maker.delete_ok(&format!("/orders/{order_id}")).await;
    log.check(
        cancelled.status == 200,
        "cancelling the order succeeds",
        format!("HTTP {} {}", cancelled.status, cancelled.raw),
    );
    log.check(
        cancelled.body["status"].as_str() == Some("cancelled"),
        "the cancel reports the order as cancelled",
        format!("status was {}", cancelled.body["status"]),
    );
    await_book_level(&maker, "BTC-USD", "buy", PRICE, false).await;

    let after = maker
        .await_account("the cancelled order's collateral to unlock", |account| {
            cb_e2e::client::balance_entry(account, "USD", "locked") == dec("0")
        })
        .await;
    log.check_eq(
        cb_e2e::client::balance_entry(&after, "USD", "free"),
        usd_before,
        "cancelling returns every locked unit to the free balance",
    );
    log.check_eq(
        cb_e2e::client::balance_entry(&after, "USD", "total"),
        usd_total_before,
        "a place-then-cancel round trip leaves the holding exactly as it was",
    );

    // ---- trade with somebody -------------------------------------------
    //
    // The second account holds BTC from its own grant, so the two can cross
    // without any operator intervention — which is the point of the grant
    // containing both assets.
    let (_taker_session, taker) = funded_user(api, 1, "journey_taker").await;

    let maker_order = maker
        .post_ok(
            "/orders",
            json!({
                "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": PRICE, "qty": QTY, "tif": "gtc"
            }),
        )
        .await;
    let maker_order_id = maker_order.ok(201)["order_id"]
        .as_str()
        .expect("an accepted order has an id")
        .to_string();
    await_book_level(&maker, "BTC-USD", "buy", PRICE, true).await;

    let btc_before =
        cb_e2e::client::balance_entry(maker.get_ok("/account").await.ok(200), "BTC", "total");

    let taker_order = taker
        .post_ok(
            "/orders",
            json!({
                "symbol": "BTC-USD", "side": "sell", "type": "limit",
                "price": PRICE, "qty": QTY, "tif": "gtc"
            }),
        )
        .await;
    log.check(
        taker_order.status == 201,
        "the crossing order is accepted",
        format!("HTTP {} {}", taker_order.status, taker_order.raw),
    );
    let taker_order_id = taker_order.body["order_id"]
        .as_str()
        .expect("an accepted order has an id")
        .to_string();

    // The fill is real when the assets have moved. That is the assertion that
    // cannot be satisfied by bookkeeping alone.
    let settled = maker
        .await_account("the bought BTC to arrive", |account| {
            cb_e2e::client::balance_entry(account, "BTC", "total") >= btc_before + dec(QTY)
        })
        .await;
    log.check(
        cb_e2e::client::balance_entry(&settled, "BTC", "total") >= btc_before + dec(QTY),
        "the crossing order fills and the base asset arrives",
        format!(
            "BTC went from {btc_before} to {}",
            cb_e2e::client::balance_entry(&settled, "BTC", "total")
        ),
    );
    await_book_level(&maker, "BTC-USD", "buy", PRICE, false).await;

    // And a filled order has to stop claiming to be open. A terminal state is
    // not cosmetic: the order ticket offers a cancel button for anything the
    // API calls open, and cancelling an order that no longer exists is the
    // next bug report.
    for (who, id) in [("maker", &maker_order_id), ("taker", &taker_order_id)] {
        let client = maker_or_taker(&maker, &taker, who);
        // Polled rather than read once: the balance moves when the block is
        // applied and the order record is updated from the same block, but the
        // two views are not guaranteed to be refreshed in that order. Reading
        // immediately would make this a race, and a racy assertion about a real
        // defect is worse than none — it gets dismissed as flakiness.
        let found = cb_e2e::wait::poll_until_some(
            &format!("the {who}'s order {id} to reach a terminal status"),
            std::time::Duration::from_secs(20),
            || async {
                let reply = client
                    .get("/account/orders?status=all")
                    .await
                    .map_err(|err| err.to_string())?;
                let order = reply.body["orders"]
                    .as_array()
                    .ok_or_else(|| format!("no orders array in {}", reply.raw))?
                    .iter()
                    .find(|order| order["order_id"].as_str() == Some(id.as_str()))
                    .cloned()
                    .ok_or_else(|| format!("the {who}'s order is not listed at all"))?;
                match order["status"].as_str() {
                    Some("filled") => Ok(order),
                    other => Err(format!(
                        "status {other:?} with filled_qty {}",
                        order["filled_qty"]
                    )),
                }
            },
        )
        .await;

        match found {
            Ok(_) => log.passed(format!(
                "the {who}'s fully-filled order reaches a terminal `filled` status"
            )),
            Err(err) => log.failed(
                format!("the {who}'s fully-filled order reaches a terminal `filled` status"),
                format!(
                    "{err}. The assets moved, so the order did fill; the order record never \
                     caught up. An order the API still calls `open` gets a cancel button on \
                     a ticket for an order that no longer exists."
                ),
            ),
        }
    }

    log.finish();
}

/// Picks one of the two clients by name, for the loop above.
fn maker_or_taker<'a>(
    maker: &'a cb_e2e::Api,
    taker: &'a cb_e2e::Api,
    who: &str,
) -> &'a cb_e2e::Api {
    if who == "maker" {
        maker
    } else {
        taker
    }
}
