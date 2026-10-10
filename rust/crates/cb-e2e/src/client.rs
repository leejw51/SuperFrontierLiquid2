//! An HTTP client that behaves like the browser, and asserts like the contract.
//!
//! Everything a test can observe goes through here: a status code, a JSON body,
//! and an error envelope. There is deliberately no access to the gateway's
//! database or the node's state from this crate. A test that reached into a
//! table to confirm a row would pass while the API that exposes that row was
//! broken, which is the exact failure mode this suite exists to end.
//!
//! Requests carry a hard timeout. Without one, "the gateway reports 503 rather
//! than hanging" cannot be tested at all: a hang and a slow answer are the same
//! observation to a client that waits forever.

use std::time::Duration;

use serde_json::{json, Value};

use crate::wait::expect_within;

/// How long any one request may take before the test calls it a hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One response, decoded.
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// Parsed JSON body, or `Null` when the body was empty or not JSON.
    pub body: Value,
    /// The raw body, kept so a failure can show what actually arrived when the
    /// response was not the JSON the contract promises.
    pub raw: String,
}

impl Reply {
    /// The contract's stable error code, when this is an error envelope.
    pub fn error_code(&self) -> Option<&str> {
        self.body.get("error")?.get("code")?.as_str()
    }

    /// Asserts a success status and hands back the body.
    pub fn ok(&self, status: u16) -> &Value {
        assert_eq!(
            self.status, status,
            "expected {status}, got {} with body {}",
            self.status, self.raw
        );
        &self.body
    }

    /// Asserts the status *and* the error code, which is what the contract
    /// specifies. A 422 with the wrong code is a client-visible break: the
    /// frontend branches on the code, not on the number.
    pub fn error(&self, status: u16, code: &str) {
        assert_eq!(
            self.status, status,
            "expected {status} {code}, got {} with body {}",
            self.status, self.raw
        );
        assert_eq!(
            self.error_code(),
            Some(code),
            "expected error code {code}, got body {}",
            self.raw
        );
    }

    /// Asserts a status without pinning the code.
    pub fn status_is(&self, status: u16) -> &Value {
        assert_eq!(
            self.status, status,
            "expected {status}, got {} with body {}",
            self.status, self.raw
        );
        &self.body
    }

    /// A field, or a panic naming the field and showing the body.
    pub fn field(&self, name: &str) -> &Value {
        self.body
            .get(name)
            .unwrap_or_else(|| panic!("no `{name}` in response body {}", self.raw))
    }

    /// A field that the contract says is a decimal string.
    pub fn decimal(&self, name: &str) -> rust_decimal::Decimal {
        crate::money::as_decimal(self.field(name), name)
    }
}

/// A signed-in user: the token and what the gateway said about them.
#[derive(Debug, Clone)]
pub struct Session {
    /// Bearer token.
    pub token: String,
    /// Gateway-assigned identifier.
    pub user_id: String,
    /// The handle the account registered.
    pub handle: String,
    /// The EIP-55 login address.
    pub address: String,
    /// Whether this login created the account.
    pub is_new: bool,
}

/// An HTTP client for one gateway, optionally carrying a session.
#[derive(Debug, Clone)]
pub struct Api {
    base: String,
    origin: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl Api {
    /// A client against `http://host:port`, which serves the API under
    /// `/api/v1` and the websocket at the root.
    pub fn new(origin: impl Into<String>) -> Self {
        let origin = origin.into();
        Self {
            base: format!("{origin}/api/v1"),
            origin,
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("a reqwest client with a timeout is always constructible"),
            token: None,
        }
    }

    /// The same gateway, seen as the holder of `token`.
    pub fn with_token(&self, token: impl Into<String>) -> Self {
        Self {
            token: Some(token.into()),
            ..self.clone()
        }
    }

    /// The same gateway, seen as a signed-in user.
    pub fn as_user(&self, session: &Session) -> Self {
        self.with_token(session.token.clone())
    }

    /// The same gateway, with no credentials at all.
    pub fn anonymous(&self) -> Self {
        Self {
            token: None,
            ..self.clone()
        }
    }

    /// `http://host:port` — the server root.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `http://host:port/api/v1` — what the frontend is configured with.
    pub fn api_base(&self) -> &str {
        &self.base
    }

    /// The token this client presents, if any.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> reqwest::Result<Reply> {
        let request = match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {token}")),
            None => request,
        };
        let response = request.send().await?;
        let status = response.status().as_u16();
        let raw = response.text().await.unwrap_or_default();
        let body = serde_json::from_str(&raw).unwrap_or(Value::Null);
        Ok(Reply { status, body, raw })
    }

    /// `GET {base}{path}`. `path` starts with a slash.
    pub async fn get(&self, path: &str) -> reqwest::Result<Reply> {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    /// `POST {base}{path}` with a JSON body.
    pub async fn post(&self, path: &str, body: Value) -> reqwest::Result<Reply> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(&body))
            .await
    }

    /// `PATCH {base}{path}` with a JSON body.
    pub async fn patch(&self, path: &str, body: Value) -> reqwest::Result<Reply> {
        self.send(self.http.patch(format!("{}{path}", self.base)).json(&body))
            .await
    }

    /// `DELETE {base}{path}`.
    pub async fn delete(&self, path: &str) -> reqwest::Result<Reply> {
        self.send(self.http.delete(format!("{}{path}", self.base)))
            .await
    }

    /// A raw request against the server root rather than the API base, for the
    /// handful of things that do not live under `/api/v1`.
    pub async fn get_absolute(&self, url: &str) -> reqwest::Result<Reply> {
        self.send(self.http.get(url)).await
    }

    /// The same calls, panicking on a transport error rather than returning it.
    ///
    /// A connection refused in the middle of a test is a failure of the test,
    /// not a result the test should be branching on — except in the node-outage
    /// suite, which uses the `Result`-returning forms above deliberately.
    pub async fn get_ok(&self, path: &str) -> Reply {
        self.get(path)
            .await
            .unwrap_or_else(|err| panic!("GET {path} failed at the transport: {err}"))
    }

    /// See [`Api::get_ok`].
    pub async fn post_ok(&self, path: &str, body: Value) -> Reply {
        self.post(path, body)
            .await
            .unwrap_or_else(|err| panic!("POST {path} failed at the transport: {err}"))
    }

    /// See [`Api::get_ok`].
    pub async fn patch_ok(&self, path: &str, body: Value) -> Reply {
        self.patch(path, body)
            .await
            .unwrap_or_else(|err| panic!("PATCH {path} failed at the transport: {err}"))
    }

    /// See [`Api::get_ok`].
    pub async fn delete_ok(&self, path: &str) -> Reply {
        self.delete(path)
            .await
            .unwrap_or_else(|err| panic!("DELETE {path} failed at the transport: {err}"))
    }

    // ---------------------------------------------------------------- flows

    /// Signs in with a mnemonic through the development login.
    ///
    /// This is the affordance the contract documents for exactly this purpose:
    /// "it lets the bot simulator, the integration suite and a developer poking
    /// at curl log in without implementing client-side signing". The real
    /// challenge/signature path is exercised separately, in the auth suite,
    /// because a suite that only ever used the shortcut would never test it.
    pub async fn dev_login(&self, mnemonic: &str, index: u32, handle: &str) -> Session {
        let reply = self
            .post_ok(
                "/auth/dev-login",
                json!({ "mnemonic": mnemonic, "index": index, "handle": handle }),
            )
            .await;
        let body = reply.ok(200);
        Session {
            token: body["session_token"]
                .as_str()
                .unwrap_or_else(|| panic!("dev-login returned no session_token: {}", reply.raw))
                .to_string(),
            user_id: body["user_id"].as_str().unwrap_or_default().to_string(),
            handle: body["handle"].as_str().unwrap_or_default().to_string(),
            address: body["address"].as_str().unwrap_or_default().to_string(),
            is_new: body["is_new"].as_bool().unwrap_or(false),
        }
    }

    /// The free balance of one asset, as a decimal.
    pub async fn free_balance(&self, asset: &str) -> rust_decimal::Decimal {
        let reply = self.get_ok("/account").await;
        let body = reply.ok(200);
        balance_entry(body, asset, "free")
    }

    /// The locked balance of one asset, as a decimal.
    pub async fn locked_balance(&self, asset: &str) -> rust_decimal::Decimal {
        let reply = self.get_ok("/account").await;
        let body = reply.ok(200);
        balance_entry(body, asset, "locked")
    }

    /// Waits until an order reaches one of `wanted`, and returns it.
    ///
    /// `POST /orders` may legitimately answer `pending`: the gateway waits only
    /// `--order-confirm-ms` for the block carrying the order. Asserting on the
    /// POST body would therefore be asserting on a race. The order list is the
    /// observable the frontend polls, so that is what this waits on.
    pub async fn await_order_status(&self, order_id: &str, wanted: &[&str]) -> Value {
        let what = format!("order {order_id} to reach one of {wanted:?}");
        expect_within(&what, Duration::from_secs(30), || async {
            let reply = self
                .get("/account/orders?status=all")
                .await
                .map_err(|err| err.to_string())?;
            if reply.status != 200 {
                return Err(format!("status {} body {}", reply.status, reply.raw));
            }
            let orders = reply.body["orders"]
                .as_array()
                .ok_or_else(|| format!("no orders array in {}", reply.raw))?;
            let found = orders
                .iter()
                .find(|order| order["order_id"].as_str() == Some(order_id))
                .ok_or_else(|| format!("order not listed yet; {} orders", orders.len()))?;
            let status = found["status"].as_str().unwrap_or("");
            if wanted.contains(&status) {
                Ok(found.clone())
            } else {
                Err(format!("status {status}"))
            }
        })
        .await
    }

    /// Waits until a predicate over `GET /account` holds, returning the body.
    ///
    /// Written as an explicit loop rather than through [`expect_within`]
    /// because the predicate has to be called between awaits, which a borrowed
    /// `FnMut` inside an async block cannot be.
    pub async fn await_account<F>(&self, what: &str, mut predicate: F) -> Value
    where
        F: FnMut(&Value) -> bool,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut last: String;
        loop {
            match self.get("/account").await {
                Ok(reply) if reply.status == 200 => {
                    if predicate(&reply.body) {
                        return reply.body;
                    }
                    last = format!("balances {}", reply.body["balances"]);
                }
                Ok(reply) => last = format!("status {} body {}", reply.status, reply.raw),
                Err(err) => last = err.to_string(),
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out after 30s waiting for {what}; last seen: {last}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// One field of one balance entry, or zero when the asset is not held.
///
/// A missing asset really is zero to a client: the contract lists balances, and
/// an account that has never held BTC has no BTC row.
pub fn balance_entry(account: &Value, asset: &str, field: &str) -> rust_decimal::Decimal {
    let entry = account["balances"]
        .as_array()
        .unwrap_or_else(|| panic!("no balances array in account body {account}"))
        .iter()
        .find(|entry| entry["asset"].as_str() == Some(asset));
    match entry {
        Some(entry) => crate::money::as_decimal(&entry[field], &format!("{asset}.{field}")),
        None => rust_decimal::Decimal::ZERO,
    }
}
