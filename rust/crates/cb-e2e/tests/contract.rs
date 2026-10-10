//! Conformance to `docs/api-contract.md`, endpoint by endpoint.
//!
//! The contract is frozen and is the seam two teams code against independently.
//! That only works if somebody checks it against the running server, which is
//! what this file does: for each documented endpoint, the success shape it
//! promises, and for each documented error code, the condition that is supposed
//! to produce it.
//!
//! # Money
//!
//! Every response is walked for the contract's number conventions — money as a
//! decimal string, `height` and the `*_bps` parameters as JSON numbers. This is
//! the check that catches a serialiser change nothing else would: emitting
//! `64250.5` instead of `"64250.50"` breaks no Rust client and silently rounds
//! a balance in every browser. See `money.rs` for the reasoning and the field
//! lists.
//!
//! # Where this file is harsher than the implementation
//!
//! Several assertions below fail today. They are written to the contract rather
//! than to the server on purpose: a test relaxed to match a deviation stops
//! being a contract test and becomes a description of the bug.

use cb_e2e::flows::{funded_user, rest_limit, settle_through_a_block, sign_in};
use cb_e2e::money::{assert_number_conventions, dec};
use cb_e2e::wait::expect_within;
use cb_e2e::{Stack, StackConfig};
use serde_json::{json, Value};
use std::time::Duration;

/// Asserts that a body has every key the contract lists for it.
///
/// Every missing key is collected before failing. Reporting them one at a time
/// turns "this endpoint does not match the contract" into as many runs as there
/// are omissions.
fn has_keys(body: &Value, keys: &[&str], what: &str) {
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| body.get(*key).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "{what} is missing {} documented field(s): {missing:?}.\nBody: {body}",
        missing.len()
    );
}

/// Asserts the documented shape of the first element of a documented list.
fn first_of(body: &Value, list: &str, what: &str) -> Option<Value> {
    let items = body[list]
        .as_array()
        .unwrap_or_else(|| panic!("{what} should carry a `{list}` array. Body: {body}"));
    items.first().cloned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn every_public_endpoint_returns_its_documented_shape() {
    let stack = Stack::start().await;
    let api = &stack.api;

    // Give the public endpoints something to describe: a resting order makes
    // the book non-empty, so the level shapes are actually exercised rather
    // than passing over an empty array.
    let (_, trader) = funded_user(api, 0, "public_shapes").await;
    rest_limit(&trader, "BTC-USD", "buy", "45000.00", "0.02000000").await;

    let markets = api.get_ok("/markets").await;
    let markets_body = markets.ok(200);
    assert_number_conventions(markets_body, "GET /markets");
    let market = first_of(markets_body, "markets", "GET /markets").expect("at least one market");
    has_keys(
        &market,
        &[
            "symbol",
            "kind",
            "asset_class",
            "base",
            "quote",
            "display_name",
            "status",
            "tick_size",
            "lot_size",
            "min_notional",
            "price_decimals",
            "size_decimals",
            "max_leverage",
            "initial_margin_bps",
            "maintenance_margin_bps",
            "funding_interval_seconds",
            "expiry",
            "maker_fee_bps",
            "taker_fee_bps",
        ],
        "a market in GET /markets",
    );
    assert!(
        ["spot", "perp", "future"].contains(&market["kind"].as_str().unwrap_or("")),
        "`kind` must be spot, perp or future; got {}",
        market["kind"]
    );

    let ticker = api.get_ok("/markets/BTC-USD/ticker").await;
    let ticker_body = ticker.ok(200);
    assert_number_conventions(ticker_body, "GET /markets/BTC-USD/ticker");
    has_keys(
        ticker_body,
        &[
            "symbol",
            "last_price",
            "mark_price",
            "index_price",
            "open_interest",
            "volume_24h",
            "change_24h_pct",
            "high_24h",
            "low_24h",
            "funding_rate",
            "next_funding_at",
        ],
        "GET /markets/{symbol}/ticker",
    );

    let book = api.get_ok("/markets/BTC-USD/orderbook?depth=50").await;
    let book_body = book.ok(200);
    assert_number_conventions(book_body, "GET /markets/BTC-USD/orderbook");
    has_keys(
        book_body,
        &["symbol", "height", "bids", "asks"],
        "the order book",
    );
    assert!(
        !book_body["bids"]
            .as_array()
            .expect("bids is a list")
            .is_empty(),
        "the book has no bids although an order was just placed and confirmed resting: {}",
        book.raw
    );

    let trades = api.get_ok("/markets/BTC-USD/trades?limit=10").await;
    assert_number_conventions(trades.ok(200), "GET /markets/BTC-USD/trades");

    let candles = api
        .get_ok("/markets/BTC-USD/candles?interval=1m&limit=10")
        .await;
    let candles_body = candles.ok(200);
    assert_number_conventions(candles_body, "GET /markets/BTC-USD/candles");
    has_keys(
        candles_body,
        &["symbol", "interval", "candles"],
        "the candle series",
    );

    let funding = api.get_ok("/markets/BTC-PERP/funding?limit=10").await;
    assert_number_conventions(funding.ok(200), "GET /markets/BTC-PERP/funding");

    let status = api.get_ok("/chain/status").await;
    let status_body = status.ok(200);
    assert_number_conventions(status_body, "GET /chain/status");
    has_keys(
        status_body,
        &[
            "chain_id",
            "height",
            "state_root",
            "block_time_ms",
            "tps_1m",
            "tx_count",
            "mempool_depth",
            "validators",
            "synced",
            "uptime_seconds",
        ],
        "GET /chain/status",
    );

    let blocks = api.get_ok("/chain/blocks?limit=5").await;
    let blocks_body = blocks.ok(200);
    assert_number_conventions(blocks_body, "GET /chain/blocks");
    if let Some(block) = first_of(blocks_body, "blocks", "GET /chain/blocks") {
        has_keys(
            &block,
            &["height", "hash", "state_root", "tx_count", "at", "proposer"],
            "a block in GET /chain/blocks",
        );
    }

    let leaderboard = api.get_ok("/leaderboard?window=24h&limit=10").await;
    let leaderboard_body = leaderboard.ok(200);
    assert_number_conventions(leaderboard_body, "GET /leaderboard");
    has_keys(leaderboard_body, &["window", "entries"], "GET /leaderboard");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn every_authenticated_endpoint_returns_its_documented_shape() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (session, user) = funded_user(api, 0, "private_shapes").await;
    rest_limit(&user, "BTC-USD", "buy", "45000.00", "0.02000000").await;

    let account = user.get_ok("/account").await;
    let account_body = account.ok(200);
    assert_number_conventions(account_body, "GET /account");
    let balance = first_of(account_body, "balances", "GET /account").expect("a funded balance");
    has_keys(
        &balance,
        &["asset", "free", "locked", "total"],
        "a balance entry",
    );
    has_keys(
        &account_body["portfolio"],
        &[
            "equity",
            "available_margin",
            "used_margin",
            "unrealized_pnl",
            "realized_pnl_24h",
            "margin_ratio",
            "liquidation_risk",
        ],
        "the portfolio block of GET /account",
    );

    // The two-address design is the contract's own headline: logging in proves
    // you control one address, and the money lives at another. The account view
    // is where a person checks that, and both names are documented here.
    has_keys(
        account_body,
        &[
            "login_address",
            "trading_address",
            "nonce",
            "balances",
            "portfolio",
        ],
        "GET /account",
    );

    let orders = user.get_ok("/account/orders?status=open").await;
    let orders_body = orders.ok(200);
    assert_number_conventions(orders_body, "GET /account/orders");
    let order = first_of(orders_body, "orders", "GET /account/orders").expect("a resting order");
    has_keys(
        &order,
        &[
            "order_id",
            "client_order_id",
            "symbol",
            "side",
            "type",
            "price",
            "qty",
            "filled_qty",
            "remaining_qty",
            "avg_fill_price",
            "status",
            "tif",
            "post_only",
            "reduce_only",
            "created_at",
            "updated_at",
        ],
        "an order in GET /account/orders",
    );

    for path in ["/account/positions", "/account/fills", "/account/transfers"] {
        let reply = user.get_ok(path).await;
        assert_number_conventions(reply.ok(200), &format!("GET {path}"));
    }

    let transfers = user.get_ok("/account/transfers?limit=100").await;
    let entries = transfers.ok(200)["entries"]
        .as_array()
        .expect("a ledger")
        .clone();
    let entry = entries.first().expect("the faucet grant is a ledger entry");
    has_keys(
        entry,
        &[
            "id",
            "kind",
            "asset",
            "amount",
            "balance_after",
            "memo",
            "at",
            "tx_hash",
        ],
        "a ledger entry",
    );

    // A ledger whose `balance_after` is always zero is not a ledger. It is the
    // column that makes the history reconcilable: a user checking how they got
    // to the balance they have reads down it, and an export that shows every
    // row ending at zero cannot be checked against anything.
    for entry in &entries {
        let asset = entry["asset"].as_str().unwrap_or("?");
        let after = cb_e2e::money::as_decimal(&entry["balance_after"], "balance_after");
        let amount = cb_e2e::money::as_decimal(&entry["amount"], "amount");
        assert!(
            after >= amount,
            "a {asset} faucet grant of {amount} left `balance_after` at {after}. The account \
             held nothing before the grant, so the balance after it cannot be smaller than \
             the grant itself. Entry: {entry}"
        );
    }

    // And the grant's own amount must be the amount granted. The BTC row of the
    // shipped grant is 1.00000000; a ledger that records 1.00 has lost six
    // decimal places of an eight-decimal asset.
    let claimed: Vec<(String, rust_decimal::Decimal)> = user.get_ok("/faucet").await.ok(200)
        ["grant"]
        .as_array()
        .expect("the advertised grant")
        .iter()
        .map(|grant| {
            (
                grant["asset"].as_str().unwrap_or("?").to_string(),
                cb_e2e::money::as_decimal(&grant["amount"], "grant.amount"),
            )
        })
        .collect();
    for (asset, amount) in &claimed {
        let row = entries
            .iter()
            .find(|entry| entry["asset"].as_str() == Some(asset.as_str()))
            .unwrap_or_else(|| panic!("no ledger row for the granted {asset}: {}", transfers.raw));
        assert_eq!(
            cb_e2e::money::as_decimal(&row["amount"], "amount"),
            *amount,
            "the faucet granted {amount} {asset} and the ledger records {}: {row}",
            row["amount"]
        );
    }

    let profile = user.get_ok("/user/profile").await;
    let profile_body = profile.ok(200);
    assert_number_conventions(profile_body, "GET /user/profile");
    has_keys(
        profile_body,
        &[
            "user_id",
            "handle",
            "address",
            "joined_at",
            "last_seen_at",
            "is_bot",
            "roles",
            "stats",
        ],
        "GET /user/profile",
    );
    has_keys(
        &profile_body["stats"],
        &[
            "trades",
            "volume_30d",
            "realized_pnl_30d",
            "markets_traded",
            "liquidations",
            "faucet_claims",
            "best_rank",
        ],
        "the stats block of GET /user/profile",
    );

    let sessions = user.get_ok("/user/sessions").await;
    let listed = first_of(sessions.ok(200), "sessions", "GET /user/sessions")
        .expect("the caller's own session is listed");
    has_keys(
        &listed,
        &[
            "session_id",
            "issued_at",
            "expires_at",
            "last_used_at",
            "user_agent",
            "ip_prefix",
            "current",
        ],
        "a session in GET /user/sessions",
    );

    let activity = user.get_ok("/user/activity?limit=10").await;
    let activity_body = activity.ok(200);
    assert_number_conventions(activity_body, "GET /user/activity");
    has_keys(
        activity_body,
        &["entries", "next_cursor"],
        "GET /user/activity",
    );
    let event = first_of(activity_body, "entries", "GET /user/activity")
        .expect("claiming the faucet is an activity");
    has_keys(
        &event,
        &[
            "id", "kind", "at", "summary", "symbol", "amount", "asset", "tx_hash", "height",
        ],
        "an activity entry",
    );

    // An activity entry that names a transaction must name the block it is in.
    // The feed is the chain-explorer link out of a person's own history; an
    // entry with a hash and no height cannot be located, and the frontend has
    // nothing to render in its height column.
    assert!(
        event["tx_hash"].as_str().is_some(),
        "an on-chain activity entry with no transaction hash: {event}"
    );
    assert!(
        event["height"].is_number(),
        "activity entry `{}` names transaction {} but reports height {}. The transaction is \
         in a block — the ledger and the block list both know which — so an entry that \
         cannot say where leaves the explorer link and the height column empty.",
        event["summary"].as_str().unwrap_or("?"),
        event["tx_hash"],
        event["height"],
    );

    let faucet = user.get_ok("/faucet").await;
    let faucet_body = faucet.ok(200);
    assert_number_conventions(faucet_body, "GET /faucet");
    has_keys(
        faucet_body,
        &[
            "available",
            "next_available_at",
            "cooldown_seconds",
            "grant",
            "claims_made",
        ],
        "GET /faucet",
    );

    let session_view = user.get_ok("/auth/session").await;
    has_keys(
        session_view.ok(200),
        &[
            "user_id",
            "handle",
            "login_address",
            "trading_address",
            "roles",
            "issued_at",
            "expires_at",
        ],
        "GET /auth/session",
    );
    assert_eq!(
        session_view.body["login_address"].as_str(),
        Some(session.address.as_str())
    );

    // PATCH /user/profile returns the updated profile, not an acknowledgement.
    let renamed = user
        .patch_ok("/user/profile", json!({ "handle": "private_shapes_2" }))
        .await;
    assert_eq!(
        renamed.ok(200)["handle"].as_str(),
        Some("private_shapes_2"),
        "PATCH /user/profile should return the updated profile: {}",
        renamed.raw
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn each_documented_error_code_is_returned_for_its_documented_condition() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, user) = funded_user(api, 0, "error_codes").await;

    // VALIDATION_FAILED — a malformed request.
    user.post_ok(
        "/orders",
        json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit", "qty": "0.02" }),
    )
    .await
    .error(400, "VALIDATION_FAILED");

    user.post_ok(
        "/orders",
        json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                "price": "1.00", "qty": "0.00000001" }),
    )
    .await
    .error(400, "VALIDATION_FAILED");

    api.get_ok("/markets/BTC-USD/candles?interval=7m")
        .await
        .error(400, "VALIDATION_FAILED");

    // UNAUTHORIZED — no session. Covered exhaustively in `auth.rs`; here for
    // completeness of the table.
    api.anonymous()
        .get_ok("/account")
        .await
        .error(401, "UNAUTHORIZED");

    // FORBIDDEN — a real session that is not permitted.
    user.get_ok("/admin/overview").await.error(403, "FORBIDDEN");

    // NOT_FOUND — an unknown symbol and an unknown order.
    api.get_ok("/markets/NOPE-USD/ticker")
        .await
        .error(404, "NOT_FOUND");
    user.delete_ok("/orders/0xdeadbeef")
        .await
        .error(404, "NOT_FOUND");

    // FAUCET_COOLDOWN — a second claim inside the cooldown. The stack uses the
    // shipped hour, so the second claim is always inside it.
    user.post_ok("/faucet/claim", json!({}))
        .await
        .error(429, "FAUCET_COOLDOWN");

    // MARKET_HALTED — covered against a real halt in `admin.rs`, which is the
    // only place a market can be halted from.
}

/// An order for more than the account holds must be refused, with the code the
/// contract names, before it reaches the chain.
///
/// The contract lists `422 INSUFFICIENT_BALANCE` — "Not enough free balance".
/// Accepting the order instead is not a cosmetic difference: the client is told
/// `201` and given an order id, the chain rejects the transaction, and nothing
/// ever tells the user. The order sits in their list forever in a state the
/// contract's own status enum does not have a terminal value for.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn an_order_larger_than_the_free_balance_is_refused_with_insufficient_balance() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "too_poor").await;

    let free = user.free_balance("USD").await;
    let reply = user
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "50000.00", "qty": "1000.00000000" }),
        )
        .await;
    assert_eq!(
        reply.status, 422,
        "an order for 50,000,000 USD of notional against a free balance of {free} returned \
         {} {}.\nThe contract documents 422 INSUFFICIENT_BALANCE for this.",
        reply.status, reply.raw
    );
    assert_eq!(
        reply.error_code(),
        Some("INSUFFICIENT_BALANCE"),
        "{}",
        reply.raw
    );
}

/// A derivative order on a chain with no oracle price must be refused with
/// `422 ORACLE_STALE`, not a 500.
///
/// This is fault 3 in a second place. The ticker was fixed; order placement was
/// not, and the condition is the default one — every freshly created chain has
/// no marks. A 500 also leaks the node's internal error text to the browser,
/// which is how a user finds out about `perp:0`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_derivative_order_with_no_oracle_price_is_refused_as_oracle_stale() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "no_oracle").await;

    let reply = user
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-PERP", "side": "buy", "type": "limit",
                    "price": "50000.00", "qty": "0.01000", "leverage": "5",
                    "margin_mode": "cross" }),
        )
        .await;
    assert_ne!(
        reply.status, 500,
        "placing a perpetual order on a chain with no oracle price returned a 500: {}\n\
         'No mark price yet' is the normal state of a new chain, and the contract has a code \
         for it. A 500 is also an internal message on a user's screen.",
        reply.raw
    );
    assert_eq!(
        reply.status, 422,
        "expected 422 ORACLE_STALE, got {} {}",
        reply.status, reply.raw
    );
    assert_eq!(reply.error_code(), Some("ORACLE_STALE"), "{}", reply.raw);
}

/// Resting an order moves money between columns; it does not create any.
///
/// Split out of the journey so the defect has a test of its own with a name
/// that says what is wrong. The contract's own example — free 94,000 plus
/// locked 6,000 equals total 100,000, for an account that deposited 100,000 —
/// is the arithmetic being checked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn resting_an_order_moves_money_from_free_to_locked_and_invents_none() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "locked_money").await;

    let before = user.get_ok("/account").await;
    let free_before = cb_e2e::client::balance_entry(before.ok(200), "USD", "free");
    let total_before = cb_e2e::client::balance_entry(&before.body, "USD", "total");

    rest_limit(&user, "BTC-USD", "buy", "50000.00", "0.02000000").await;
    let live = user
        .await_account("the collateral to be locked", |account| {
            cb_e2e::client::balance_entry(account, "USD", "locked") > dec("0")
        })
        .await;

    let free = cb_e2e::client::balance_entry(&live, "USD", "free");
    let locked = cb_e2e::client::balance_entry(&live, "USD", "locked");
    let total = cb_e2e::client::balance_entry(&live, "USD", "total");

    assert_eq!(
        total,
        total_before,
        "the account's total USD went from {total_before} to {total} when an order was \
         placed — it gained {}, which is the amount locked ({locked}). Locking has to move \
         money out of `free`, not add it to the holding: equity, margin and the leaderboard \
         are all computed from this number.",
        total - total_before
    );
    assert_eq!(
        free,
        free_before - locked,
        "`free` was {free_before} before and {free} while {locked} is locked; it should be \
         {}. A `free` that still counts locked collateral is a spendable balance the account \
         does not have.",
        free_before - locked
    );
}

/// A fully filled order has to reach a terminal status.
///
/// The assets move, so the trade happened; the order record does not follow.
/// The order ticket offers a cancel button for anything the API calls `open`,
/// so this is not cosmetic — it is a button that cancels an order which no
/// longer exists, on a screen that shows a resting order which is not on the
/// book.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_fully_filled_order_stops_being_reported_as_open() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, maker) = funded_user(api, 0, "fill_maker").await;
    let (_, taker) = funded_user(api, 1, "fill_taker").await;

    let maker_order = rest_limit(&maker, "BTC-USD", "buy", "50000.00", "0.02000000").await;
    let btc_before = maker.free_balance("BTC").await;

    taker
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "sell", "type": "limit",
                    "price": "50000.00", "qty": "0.02000000" }),
        )
        .await
        .ok(201);

    // Wait for proof the fill happened before judging the order record.
    maker
        .await_account("the bought BTC to arrive", |account| {
            cb_e2e::client::balance_entry(account, "BTC", "free") >= btc_before + dec("0.02")
        })
        .await;

    // Polled, not read once: the balance and the order record are updated from
    // the same block but not necessarily refreshed in that order, and a racy
    // assertion about a real defect gets dismissed as flakiness.
    let found = maker.await_order_status(&maker_order, &["filled"]).await;

    assert_eq!(
        found["status"].as_str(),
        Some("filled"),
        "the order filled — the BTC arrived — but its record still says `{}` with \
         filled_qty {} and remaining_qty {}. Full record: {found}",
        found["status"].as_str().unwrap_or("?"),
        found["filled_qty"],
        found["remaining_qty"],
    );
}

/// A fill has to appear in the account's own fill history.
///
/// `GET /account/fills` is what the portfolio's trade blotter renders and what
/// any tax or PnL export reads. `docs/PLAN.md` §8 records the cause — the node
/// exposes no trade stream, so the gateway derives trades per block and "spot
/// trades and candles are unavailable" — which explains the gap without closing
/// it. The endpoint is documented without that caveat, so the test asserts the
/// documented behaviour and this stays visible.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_fill_is_reported_in_the_account_fill_history_and_the_public_trade_feed() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (_, maker) = funded_user(api, 0, "blotter_maker").await;
    let (_, taker) = funded_user(api, 1, "blotter_taker").await;

    rest_limit(&maker, "BTC-USD", "buy", "50000.00", "0.02000000").await;
    let btc_before = maker.free_balance("BTC").await;
    taker
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "sell", "type": "limit",
                    "price": "50000.00", "qty": "0.02000000" }),
        )
        .await
        .ok(201);
    maker
        .await_account("the trade to settle", |account| {
            cb_e2e::client::balance_entry(account, "BTC", "free") >= btc_before + dec("0.02")
        })
        .await;

    // The chain has settled it. The balance above is read from the chain
    // directly, while the fill history is derived by the gateway's block
    // watcher, which can trail it by a block. Reading the history once, at
    // once, raced that watcher and failed about two runs in five before any
    // gateway change; the contract is that the fill appears, so the test
    // waits a bounded time for it rather than for one lucky read.
    let fills = expect_within(
        "the maker's fill in GET /account/fills",
        Duration::from_secs(10),
        || async {
            let reply = maker.get_ok("/account/fills?limit=100").await;
            let empty = reply.ok(200)["fills"]
                .as_array()
                .expect("a fills list")
                .is_empty();
            if empty {
                Err("the maker bought 0.02 BTC — the balance moved — and GET \
                     /account/fills is empty. This is the blotter the portfolio screen \
                     renders and the history any PnL export reads."
                    .to_string())
            } else {
                Ok(reply)
            }
        },
    )
    .await;
    let listed = fills.ok(200)["fills"].as_array().expect("a fills list");
    let fill = &listed[0];
    has_keys(
        fill,
        &[
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
        ],
        "a fill in GET /account/fills",
    );
    assert_number_conventions(&fills.body, "GET /account/fills");

    let trades = api.get_ok("/markets/BTC-USD/trades?limit=100").await;
    let public = trades.ok(200)["trades"].as_array().expect("a trades list");
    assert!(
        !public.is_empty(),
        "a trade printed on BTC-USD and the public trade feed is empty. The tape is what \
         tells every other participant the market is alive."
    );
}

/// Re-sending a `client_order_id` returns the original order rather than a
/// second one. The contract's idempotency rule exists because a browser that
/// retries a timed-out request must not double a position.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn resubmitting_a_client_order_id_returns_the_original_order() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "idempotent").await;

    let first = user
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "40000.00", "qty": "0.02000000",
                    "client_order_id": "cli_only_once" }),
        )
        .await;
    let first_id = first.ok(201)["order_id"]
        .as_str()
        .expect("an order id")
        .to_string();

    // A different price and size, the same id: the retry of a request whose
    // answer was lost, or a client bug. Either way it must not become a second
    // order.
    let second = user
        .post_ok(
            "/orders",
            json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                    "price": "41000.00", "qty": "0.03000000",
                    "client_order_id": "cli_only_once" }),
        )
        .await;
    assert_eq!(
        second.ok(201)["order_id"].as_str(),
        Some(first_id.as_str()),
        "the same client_order_id produced a second order: {}",
        second.raw
    );

    let orders = user.get_ok("/account/orders?status=all").await;
    let count = orders.ok(200)["orders"]
        .as_array()
        .expect("an orders list")
        .iter()
        .filter(|order| order["client_order_id"].as_str() == Some("cli_only_once"))
        .count();
    assert_eq!(
        count, 1,
        "the order list has {count} orders with that client id"
    );
}

/// Too many mutating requests in a minute is `429 RATE_LIMITED`.
///
/// This stack is booted with a deliberately tiny per-session limit. The rest of
/// the suite raises the limits, because every test arrives from `127.0.0.1` and
/// would otherwise be rate-limited by its neighbours' traffic — a suite that
/// fails depending on what else is running is worse than no suite.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn too_many_requests_in_a_minute_are_rate_limited() {
    let stack = Stack::start_with(StackConfig {
        rate_limit_per_session: 5,
        rate_limit_per_ip: 1_000_000,
        ..StackConfig::default()
    })
    .await;
    let session = sign_in(&stack.api, 0, "rate_limited").await;
    let user = stack.api.as_user(&session);

    let mut seen = Vec::new();
    for attempt in 0..40 {
        let reply = user
            .post_ok(
                "/orders",
                json!({ "symbol": "BTC-USD", "side": "buy", "type": "limit",
                        "price": "40000.00", "qty": "0.02000000",
                        "client_order_id": format!("cli_rate_{attempt}") }),
            )
            .await;
        if reply.status == 429 {
            assert_eq!(
                reply.error_code(),
                Some("RATE_LIMITED"),
                "a 429 arrived with the wrong code: {}",
                reply.raw
            );
            return;
        }
        seen.push(reply.status);
    }
    panic!(
        "40 mutating requests against a limit of 5 per minute were never rate limited; \
         statuses seen were {seen:?}. Without the limit one client can drive the sequencer \
         as hard as it likes."
    );
}

/// The cooldown the faucet advertises is the cooldown it enforces.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_second_faucet_claim_inside_the_cooldown_is_refused_and_grants_nothing() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "cooldown").await;

    // A second account, so the chain can be made to move without touching the
    // balance this test is about.
    let (_, bystander) = funded_user(&stack.api, 1, "cooldown_bystander").await;

    let before = user.free_balance("USD").await;
    let second = user.post_ok("/faucet/claim", json!({})).await;
    second.error(429, "FAUCET_COOLDOWN");

    let status = user.get_ok("/faucet").await;
    assert_eq!(
        status.ok(200)["claims_made"].as_u64(),
        Some(1),
        "a refused claim was still counted: {}",
        status.raw
    );

    // And nothing was minted. A refusal that pays out anyway is fault 2's
    // shape — the money and the error — so "nothing arrived" has to be judged
    // at a moment the chain defines rather than at one the test guessed.
    settle_through_a_block(&bystander, "BTC-USD", "37000.00", "0.02000000").await;
    assert_eq!(
        user.free_balance("USD").await,
        before,
        "a faucet claim that was refused with FAUCET_COOLDOWN still changed the balance, \
         from {before}. The mint is submitted before the claim is recorded, which is exactly \
         how a user once got an error and the money at the same time."
    );
}

/// Cancelling everything cancels everything, and says how much it cancelled.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn cancel_all_clears_every_resting_order_and_reports_the_count() {
    let stack = Stack::start().await;
    let (_, user) = funded_user(&stack.api, 0, "cancel_all").await;

    for price in ["39000.00", "38000.00", "37000.00"] {
        rest_limit(&user, "BTC-USD", "buy", price, "0.02000000").await;
    }

    let cancelled = user
        .post_ok("/orders/cancel-all", json!({ "symbol": "BTC-USD" }))
        .await;
    let body = cancelled.ok(200);
    assert_eq!(
        body["cancelled"].as_u64(),
        Some(3),
        "three orders were resting and cancel-all reported {}: {}",
        body["cancelled"],
        cancelled.raw
    );

    for price in ["39000.00", "38000.00", "37000.00"] {
        cb_e2e::flows::await_book_level(&user, "BTC-USD", "buy", price, false).await;
    }
    let open = user.get_ok("/account/orders?status=open").await;
    assert_eq!(
        open.ok(200)["orders"].as_array().map(Vec::len),
        Some(0),
        "orders are still open after cancel-all: {}",
        open.raw
    );
}

/// A transfer moves money between two accounts and both ledgers say so.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_transfer_moves_money_and_appears_in_both_ledgers() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let (sender_session, sender) = funded_user(api, 0, "transfer_from").await;
    let (recipient_session, recipient) = funded_user(api, 1, "transfer_to").await;

    let recipient_address = api
        .as_user(&recipient_session)
        .get_ok("/auth/session")
        .await
        .ok(200)["trading_address"]
        .as_str()
        .expect("a trading address")
        .to_string();

    let before = recipient.free_balance("USD").await;
    let sent = sender
        .post_ok(
            "/transfers",
            json!({ "to": recipient_address, "asset": "USD",
                    "amount": "100.00", "memo": "gg" }),
        )
        .await;
    let body = sent.ok(200);
    assert!(
        body.get("tx_hash").is_some() && body.get("at").is_some(),
        "a transfer should report its transaction and when: {}",
        sent.raw
    );

    recipient
        .await_account("the transfer to arrive", |account| {
            cb_e2e::client::balance_entry(account, "USD", "free") >= before + dec("100")
        })
        .await;

    // Both sides have to be able to account for it. A transfer that shows up
    // only as a balance change is a movement nobody can explain later.
    let incoming = recipient.get_ok("/account/transfers?limit=100").await;
    assert!(
        incoming.ok(200)["entries"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["kind"].as_str() == Some("transfer_in"))),
        "the recipient's balance rose and their ledger has no `transfer_in`: {}",
        incoming.raw
    );

    let outgoing = sender.get_ok("/account/transfers?limit=100").await;
    assert!(
        outgoing.ok(200)["entries"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["kind"].as_str() == Some("transfer_out"))),
        "the sender's ledger has no `transfer_out` for a transfer they made: {}",
        outgoing.raw
    );
    let _ = sender_session;
}

/// A named session can be revoked on its own, and the caller's survives.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn revoking_one_session_leaves_the_others_alone() {
    let stack = Stack::start().await;
    let api = &stack.api;
    let first = sign_in(api, 0, "two_devices").await;
    let second = api
        .post_ok(
            "/auth/dev-login",
            json!({ "mnemonic": cb_e2e::flows::TEST_MNEMONIC, "index": 0 }),
        )
        .await;
    let second_token = second.ok(200)["session_token"]
        .as_str()
        .expect("a second session")
        .to_string();
    let keeper = api.with_token(second_token);

    // Find the *other* session in the list — the one that is not this caller's.
    let listed = keeper.get_ok("/user/sessions").await;
    let other = listed.ok(200)["sessions"]
        .as_array()
        .expect("a session list")
        .iter()
        .find(|session| session["current"].as_bool() != Some(true))
        .cloned()
        .unwrap_or_else(|| panic!("two sessions exist and only one is listed: {}", listed.raw));
    let other_id = other["session_id"].as_str().expect("a session id");

    let revoked = keeper
        .delete_ok(&format!("/user/sessions/{other_id}"))
        .await;
    assert_eq!(
        revoked.status, 204,
        "the contract documents DELETE /user/sessions/{{id}} as 204; got {} {}",
        revoked.status, revoked.raw
    );

    api.with_token(first.token)
        .get_ok("/account")
        .await
        .error(401, "UNAUTHORIZED");
    keeper.get_ok("/account").await.ok(200);
}
