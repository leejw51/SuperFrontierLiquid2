//! The two or three set-ups every suite needs, written once.
//!
//! Nothing here reaches past the API. "A funded user" means an account that
//! signed in and claimed the faucet and then waited until the money was
//! visible on `GET /account` — that is, until the chain had actually executed
//! the grant. A helper that credited an account by any other route would let a
//! test pass on a chain where the faucet is broken, which is fault 1 again.

use std::time::Duration;

use serde_json::{json, Value};

use crate::client::{Api, Session};
use crate::money::dec;
use crate::stack::AdminGrant;
use crate::wait::expect_within;

/// The canonical all-zero-entropy BIP-39 phrase.
///
/// The contract pins it: index 0 derives
/// `0x9858EfFD232B4033E47d90003D41EC34EcaEda94`, the same address `eth-account`
/// and MetaMask produce. Every account in this suite is an index of this one
/// phrase, so an address in a failure message can be re-derived by hand.
pub const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// The address `TEST_MNEMONIC` derives at index 0, as the contract states it.
pub const TEST_ADDRESS_INDEX_0: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

/// Signs in a new account at `index` and returns the session.
///
/// The operator's index is refused here rather than allowed to surprise
/// someone later: a test that gave an "ordinary" user the operator's address
/// would still pass its `403` assertions in the sense that they would not
/// panic — they would simply be asserting about an administrator, and saying
/// nothing.
pub async fn sign_in(api: &Api, index: u32, handle: &str) -> Session {
    assert_ne!(
        index,
        crate::stack::ADMIN_INDEX,
        "account index {index} is reserved for the operator (see          `cb_e2e::stack::ADMIN_INDEX`). Use `sign_in_admin` for the operator, and any other          index for an ordinary account."
    );
    api.dev_login(TEST_MNEMONIC, index, handle).await
}

/// Signs in the operator, bypassing the reservation check above.
async fn sign_in_reserved(api: &Api, handle: &str) -> Session {
    api.dev_login(TEST_MNEMONIC, crate::stack::ADMIN_INDEX, handle)
        .await
}

/// Signs in, claims the faucet, and waits until the money is on chain.
///
/// Returns the session and a client already carrying its token.
pub async fn funded_user(api: &Api, index: u32, handle: &str) -> (Session, Api) {
    let session = sign_in(api, index, handle).await;
    let user = api.as_user(&session);
    let reply = user.post_ok("/faucet/claim", json!({})).await;
    assert_eq!(
        reply.status, 200,
        "the faucet refused {handle}: {}. Without a working faucet nothing \
         downstream of it can be tested at all.",
        reply.raw
    );
    user.await_account("the faucet grant to arrive on chain", |account| {
        crate::client::balance_entry(account, "USD", "total") > dec("0")
    })
    .await;
    (session, user)
}

/// Places an order and returns the `POST /orders` body.
///
/// The body is returned rather than asserted on, because `status` here may
/// legitimately be `pending`: the gateway waits only `--order-confirm-ms` for
/// the block. What the order became is an observation for
/// [`crate::client::Api::await_order_status`].
pub async fn place(user: &Api, order: Value) -> Value {
    let reply = user.post_ok("/orders", order.clone()).await;
    assert_eq!(
        reply.status, 201,
        "POST /orders {order} was rejected: {}",
        reply.raw
    );
    reply.body
}

/// Places a limit order and waits until it is resting on the public book.
///
/// Returns the order id. Waiting on the *public* book rather than on the
/// caller's own order list is deliberate: a resting order that the owner can
/// see and the market cannot is not resting.
pub async fn rest_limit(user: &Api, symbol: &str, side: &str, price: &str, qty: &str) -> String {
    let body = place(
        user,
        json!({
            "symbol": symbol, "side": side, "type": "limit",
            "price": price, "qty": qty, "tif": "gtc"
        }),
    )
    .await;
    let order_id = body["order_id"]
        .as_str()
        .unwrap_or_else(|| panic!("POST /orders returned no order_id: {body}"))
        .to_string();
    await_book_level(user, symbol, side, price, true).await;
    order_id
}

/// Waits until a price level is present on (or absent from) the public book.
pub async fn await_book_level(api: &Api, symbol: &str, side: &str, price: &str, present: bool) {
    let want_side = if side == "buy" { "bids" } else { "asks" };
    let wanted = dec(price);
    let what = format!(
        "{price} to be {} the {want_side} of {symbol}",
        if present { "on" } else { "off" }
    );
    expect_within(&what, Duration::from_secs(30), || async {
        let reply = api
            .get(&format!("/markets/{symbol}/orderbook?depth=200"))
            .await
            .map_err(|err| err.to_string())?;
        if reply.status != 200 {
            return Err(format!("status {} body {}", reply.status, reply.raw));
        }
        let levels = reply.body[want_side]
            .as_array()
            .ok_or_else(|| format!("no {want_side} in {}", reply.raw))?;
        let found = levels.iter().any(|level| {
            level
                .get(0)
                .and_then(Value::as_str)
                .map(|text| dec(text) == wanted)
                .unwrap_or(false)
        });
        if found == present {
            Ok(())
        } else {
            Err(format!("{want_side} = {}", reply.body[want_side]))
        }
    })
    .await;
}

/// Signs in the operator account for a stack.
///
/// The operator is identified by the account index the stack was booted to
/// trust, never by the handle it chooses — the handle used here is deliberately
/// *not* "admin", so that a gateway which still derives the role from a handle
/// string fails this rather than passing by coincidence.
pub async fn sign_in_admin(stack: &crate::Stack) -> (Session, Api) {
    let api = &stack.api;
    let handle = match stack.admin_grant() {
        // Bound to an address: the handle is free, and choosing a boring one
        // proves the role did not come from it.
        AdminGrant::Address(_) => "operator",
        // Bound to a handle: the gateway has no other way to know who the
        // operator is, so the configured handle has to be claimed.
        AdminGrant::Handle(handle) => handle.as_str(),
    };
    let session = sign_in_reserved(api, handle).await;
    let admin = api.as_user(&session);
    let reply = admin.get_ok("/auth/session").await;
    let roles = reply.ok(200)["roles"].clone();
    assert!(
        has_role(&roles, "admin"),
        "the account this stack was booted to trust as operator ({:?}) did not get the admin \
         role; roles were {roles}. Every admin assertion below would be meaningless.",
        stack.admin_grant()
    );
    (session, admin)
}

/// Whether a `roles` array contains a role.
pub fn has_role(roles: &Value, role: &str) -> bool {
    roles
        .as_array()
        .map(|roles| roles.iter().any(|r| r.as_str() == Some(role)))
        .unwrap_or(false)
}

/// Pins "by now" on a chain that only produces blocks when something happens.
///
/// Absence is the hardest thing to assert. "The balance did not change" is only
/// meaningful relative to a moment, and a `sleep` picks that moment by guessing
/// how fast the machine is. This instead makes the *chain* say when: an
/// unrelated account rests an order and cancels it, and the call returns once
/// the public book has shown and then lost the level. Both of those
/// transactions were submitted after whatever the test is judging, and both
/// have been executed, so anything submitted earlier has been executed too.
///
/// `other` must be an account the assertion does not care about — it pays the
/// fees and moves its own balances.
pub async fn settle_through_a_block(other: &Api, symbol: &str, price: &str, qty: &str) {
    let order = rest_limit(other, symbol, "buy", price, qty).await;
    other.delete_ok(&format!("/orders/{order}")).await.ok(200);
    await_book_level(other, symbol, "buy", price, false).await;
}
