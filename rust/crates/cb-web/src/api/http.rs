//! The live gateway client.
//!
//! A thin, literal transcription of `docs/api-contract.md` over `fetch`. It adds
//! nothing: no retries, no caching, no field massaging. Anything clever here
//! would be a second, undocumented contract.
//!
//! Two behaviours are worth calling out. First, a non-2xx response is read as the
//! contract's error envelope and surfaced as [`ApiError::Gateway`] with the
//! gateway's own `message` intact, because the interface shows that sentence
//! verbatim. Second, a body that does not parse becomes [`ApiError::Decode`]
//! rather than a default value — a zero price that came from a parse failure is
//! indistinguishable from a real zero price, and that is how people lose money.

use std::sync::Mutex;

use futures::FutureExt;
use gloo_net::http::{Request, RequestBuilder};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::error::{ApiError, ApiResult, ErrorEnvelope};
use super::types::*;
use super::{Api, Fut, Interval, LeaderboardWindow, OrderFilter};

/// A query string built only from the parameters that have something to say.
///
/// Every empty parameter this used to send came back a `400`, and the screens
/// that depended on them — the account activity feed and the admin user
/// table — rendered a column header over nothing at all.
#[derive(Default)]
struct Query {
    parts: Vec<String>,
}

impl Query {
    /// An empty query.
    const fn new() -> Self {
        Self { parts: Vec::new() }
    }

    /// Adds `name=value`, percent-encoding the value.
    fn push(&mut self, name: &str, value: &str) {
        self.parts.push(format!("{name}={}", path_segment(value)));
    }

    /// The `?a=1&b=2` string, or an empty string when nothing was added.
    fn finish(self) -> String {
        if self.parts.is_empty() {
            String::new()
        } else {
            format!("?{}", self.parts.join("&"))
        }
    }
}

/// A client for a running `cb-gateway`.
pub struct HttpApi {
    base: String,
    token: Mutex<Option<String>>,
}

impl HttpApi {
    /// Creates a client against a base URL such as `http://localhost:8080/api/v1`.
    ///
    /// A trailing slash is trimmed so that callers can pass either form.
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            token: Mutex::new(None),
        }
    }

    /// Builds an absolute URL for a contract path such as `/markets`.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Attaches the bearer token, when there is one.
    fn authed(&self, builder: RequestBuilder) -> RequestBuilder {
        match self.token.lock() {
            Ok(guard) => match guard.as_deref() {
                Some(token) => builder.header("Authorization", &format!("Bearer {token}")),
                None => builder,
            },
            // A poisoned lock in a single-threaded wasm build is not reachable,
            // but an unauthenticated request that gets a clean 401 is a far
            // better failure than a panic, which in wasm is a blank page.
            Err(_) => builder,
        }
    }

    /// Sends a request and decodes the body, mapping failures onto [`ApiError`].
    async fn run<T: DeserializeOwned>(request: Request) -> ApiResult<T> {
        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Transport(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ApiError::Transport(error.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(match serde_json::from_str::<ErrorEnvelope>(&body) {
                Ok(envelope) => ApiError::Gateway(envelope.error),
                Err(error) => ApiError::Decode(format!("HTTP {status}: {error}")),
            });
        }
        // `()` is the shape of a 204, whose body is empty and not valid JSON.
        if body.trim().is_empty() {
            return serde_json::from_str("null")
                .map_err(|error| ApiError::Decode(error.to_string()));
        }
        serde_json::from_str(&body).map_err(|error| ApiError::Decode(error.to_string()))
    }

    /// A `GET` against a contract path.
    fn get<T: DeserializeOwned>(&self, path: &str) -> Fut<'_, T> {
        let request = self.authed(Request::get(&self.url(path))).build();
        async move {
            let request = request.map_err(|error| ApiError::Transport(error.to_string()))?;
            Self::run(request).await
        }
        .boxed_local()
    }

    /// A `POST` carrying a JSON body.
    fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Fut<'_, T> {
        let request = self.authed(Request::post(&self.url(path))).json(body);
        async move {
            let request = request.map_err(|error| ApiError::Transport(error.to_string()))?;
            Self::run(request).await
        }
        .boxed_local()
    }

    /// A `PATCH` carrying a JSON body.
    fn patch<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Fut<'_, T> {
        let request = self.authed(Request::patch(&self.url(path))).json(body);
        async move {
            let request = request.map_err(|error| ApiError::Transport(error.to_string()))?;
            Self::run(request).await
        }
        .boxed_local()
    }

    /// A `DELETE`.
    fn delete<T: DeserializeOwned>(&self, path: &str) -> Fut<'_, T> {
        let request = self.authed(Request::delete(&self.url(path))).build();
        async move {
            let request = request.map_err(|error| ApiError::Transport(error.to_string()))?;
            Self::run(request).await
        }
        .boxed_local()
    }
}

/// Percent-encodes the handful of characters that can appear in a symbol or an
/// order id and would otherwise change the meaning of a path.
fn path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '/' => "%2F".to_string(),
            '?' => "%3F".to_string(),
            '#' => "%23".to_string(),
            ' ' => "%20".to_string(),
            other => other.to_string(),
        })
        .collect()
}

impl Api for HttpApi {
    fn set_token(&self, token: Option<String>) {
        if let Ok(mut guard) = self.token.lock() {
            *guard = token;
        }
    }

    fn token(&self) -> Option<String> {
        self.token.lock().ok().and_then(|guard| guard.clone())
    }

    fn challenge<'a>(&'a self, address: &'a str) -> Fut<'a, Challenge> {
        self.post(
            "/auth/challenge",
            &ChallengeRequest {
                address: address.to_string(),
            },
        )
    }

    fn login<'a>(
        &'a self,
        address: &'a str,
        signature: &'a str,
        handle: Option<&'a str>,
    ) -> Fut<'a, Session> {
        self.post(
            "/auth/login",
            &LoginRequest {
                address: address.to_string(),
                signature: signature.to_string(),
                handle: handle.map(str::to_string),
            },
        )
    }

    fn session(&self) -> Fut<'_, SessionInfo> {
        self.get("/auth/session")
    }

    fn logout(&self) -> Fut<'_, ()> {
        self.post("/auth/logout", &serde_json::Value::Null)
    }

    fn markets(&self) -> Fut<'_, MarketsResponse> {
        self.get("/markets")
    }

    fn ticker<'a>(&'a self, symbol: &'a str) -> Fut<'a, Ticker> {
        self.get(&format!("/markets/{}/ticker", path_segment(symbol)))
    }

    fn orderbook<'a>(&'a self, symbol: &'a str, depth: u32) -> Fut<'a, OrderBookPayload> {
        self.get(&format!(
            "/markets/{}/orderbook?depth={depth}",
            path_segment(symbol)
        ))
    }

    fn trades<'a>(&'a self, symbol: &'a str, limit: u32) -> Fut<'a, TradesResponse> {
        self.get(&format!(
            "/markets/{}/trades?limit={limit}",
            path_segment(symbol)
        ))
    }

    fn candles<'a>(
        &'a self,
        symbol: &'a str,
        interval: Interval,
        limit: u32,
    ) -> Fut<'a, CandlesResponse> {
        self.get(&format!(
            "/markets/{}/candles?interval={}&limit={limit}",
            path_segment(symbol),
            interval.as_str()
        ))
    }

    fn account(&self) -> Fut<'_, Account> {
        self.get("/account")
    }

    fn orders(&self, filter: OrderFilter) -> Fut<'_, OrdersResponse> {
        self.get(&format!("/account/orders?status={}", filter.as_str()))
    }

    fn positions(&self) -> Fut<'_, PositionsResponse> {
        self.get("/account/positions")
    }

    fn fills(&self, limit: u32) -> Fut<'_, FillsResponse> {
        self.get(&format!("/account/fills?limit={limit}"))
    }

    fn ledger(&self, limit: u32) -> Fut<'_, LedgerResponse> {
        self.get(&format!("/account/transfers?limit={limit}"))
    }

    fn place_order<'a>(&'a self, request: &'a PlaceOrderRequest) -> Fut<'a, PlaceOrderResponse> {
        self.post("/orders", request)
    }

    fn cancel_order<'a>(&'a self, order_id: &'a str) -> Fut<'a, CancelOrderResponse> {
        self.delete(&format!("/orders/{}", path_segment(order_id)))
    }

    fn cancel_all<'a>(&'a self, symbol: Option<&'a str>) -> Fut<'a, CancelAllResponse> {
        let body = match symbol {
            Some(symbol) => serde_json::json!({ "symbol": symbol }),
            None => serde_json::json!({}),
        };
        self.post("/orders/cancel-all", &body)
    }

    fn close_position<'a>(
        &'a self,
        symbol: &'a str,
        qty: Option<Decimal>,
    ) -> Fut<'a, ClosePositionResponse> {
        let body = match qty {
            Some(qty) => serde_json::json!({ "symbol": symbol, "qty": qty.to_string() }),
            None => serde_json::json!({ "symbol": symbol }),
        };
        self.post("/positions/close", &body)
    }

    fn faucet(&self) -> Fut<'_, FaucetStatus> {
        self.get("/faucet")
    }

    fn faucet_claim(&self) -> Fut<'_, FaucetClaim> {
        self.post("/faucet/claim", &serde_json::json!({}))
    }

    fn chain_status(&self) -> Fut<'_, ChainStatus> {
        self.get("/chain/status")
    }

    fn blocks(&self, limit: u32) -> Fut<'_, BlocksResponse> {
        self.get(&format!("/chain/blocks?limit={limit}"))
    }

    fn chain_txs(&self, limit: u32) -> Fut<'_, TxsResponse> {
        self.get(&format!("/chain/txs?limit={limit}"))
    }

    fn leaderboard(&self, window: LeaderboardWindow, limit: u32) -> Fut<'_, LeaderboardResponse> {
        self.get(&format!(
            "/leaderboard?window={}&limit={limit}",
            window.as_str()
        ))
    }

    fn profile(&self) -> Fut<'_, Profile> {
        self.get("/user/profile")
    }

    fn update_handle<'a>(&'a self, handle: &'a str) -> Fut<'a, Profile> {
        self.patch(
            "/user/profile",
            &UpdateProfileRequest {
                handle: handle.to_string(),
            },
        )
    }

    fn sessions(&self) -> Fut<'_, SessionsResponse> {
        self.get("/user/sessions")
    }

    fn revoke_session<'a>(&'a self, session_id: &'a str) -> Fut<'a, ()> {
        self.delete(&format!("/user/sessions/{}", path_segment(session_id)))
    }

    fn revoke_other_sessions(&self) -> Fut<'_, RevokedResponse> {
        self.post("/user/sessions/revoke-all", &serde_json::json!({}))
    }

    fn activity<'a>(&'a self, limit: u32, cursor: Option<&'a str>) -> Fut<'a, ActivityResponse> {
        // An empty `cursor=` is not "no cursor", it is a cursor that does not
        // parse, and the gateway is right to answer `400 VALIDATION_FAILED`.
        // A parameter with nothing to say is left out entirely.
        let mut query = Query::new();
        query.push("limit", &limit.to_string());
        if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
            query.push("cursor", cursor);
        }
        self.get(&format!("/user/activity{}", query.finish()))
    }

    fn admin_overview(&self) -> Fut<'_, AdminOverview> {
        self.get("/admin/overview")
    }

    fn admin_users<'a>(
        &'a self,
        query: &'a str,
        is_bot: Option<bool>,
        limit: u32,
    ) -> Fut<'a, AdminUsersResponse> {
        let mut url = Query::new();
        if !query.is_empty() {
            url.push("query", query);
        }
        if let Some(is_bot) = is_bot {
            url.push("is_bot", &is_bot.to_string());
        }
        url.push("limit", &limit.to_string());
        self.get(&format!("/admin/users{}", url.finish()))
    }

    fn admin_suspend<'a>(&'a self, user_id: &'a str, reason: &'a str) -> Fut<'a, ()> {
        self.post(
            &format!("/admin/users/{}/suspend", path_segment(user_id)),
            &serde_json::json!({ "reason": reason }),
        )
    }

    fn admin_unsuspend<'a>(&'a self, user_id: &'a str) -> Fut<'a, ()> {
        self.post(
            &format!("/admin/users/{}/unsuspend", path_segment(user_id)),
            &serde_json::json!({}),
        )
    }

    fn admin_markets(&self) -> Fut<'_, AdminMarketsResponse> {
        self.get("/admin/markets")
    }

    fn admin_halt<'a>(&'a self, symbol: &'a str, reason: &'a str) -> Fut<'a, ()> {
        self.post(
            &format!("/admin/markets/{}/halt", path_segment(symbol)),
            &serde_json::json!({ "reason": reason }),
        )
    }

    fn admin_resume<'a>(&'a self, symbol: &'a str) -> Fut<'a, ()> {
        self.post(
            &format!("/admin/markets/{}/resume", path_segment(symbol)),
            &serde_json::json!({}),
        )
    }

    fn admin_settle<'a>(
        &'a self,
        symbol: &'a str,
        settlement_price: Decimal,
        force: bool,
    ) -> Fut<'a, ()> {
        self.post(
            &format!("/admin/markets/{}/settle", path_segment(symbol)),
            &serde_json::json!({
                "settlement_price": settlement_price.to_string(),
                "force": force,
            }),
        )
    }

    fn admin_set_risk<'a>(
        &'a self,
        symbol: &'a str,
        max_leverage: Decimal,
        initial_margin_bps: u32,
        maintenance_margin_bps: u32,
        dry_run: bool,
    ) -> Fut<'a, RiskApplied> {
        let query = if dry_run { "?dry_run=true" } else { "" };
        self.patch(
            &format!("/admin/markets/{}/risk{query}", path_segment(symbol)),
            &serde_json::json!({
                "max_leverage": max_leverage.to_string(),
                "initial_margin_bps": initial_margin_bps,
                "maintenance_margin_bps": maintenance_margin_bps,
            }),
        )
    }

    fn admin_faucet(&self) -> Fut<'_, AdminFaucet> {
        self.get("/admin/faucet")
    }

    fn admin_set_faucet<'a>(
        &'a self,
        enabled: bool,
        cooldown_seconds: i64,
        grant: &'a [Grant],
    ) -> Fut<'a, AdminFaucet> {
        self.patch(
            "/admin/faucet",
            &serde_json::json!({
                "enabled": enabled,
                "cooldown_seconds": cooldown_seconds,
                "grant": grant,
            }),
        )
    }

    fn admin_grant<'a>(
        &'a self,
        user_id: &'a str,
        grants: &'a [Grant],
        reason: &'a str,
    ) -> Fut<'a, FaucetClaim> {
        self.post(
            "/admin/faucet/grant",
            &serde_json::json!({ "user_id": user_id, "grants": grants, "reason": reason }),
        )
    }

    fn admin_audit<'a>(&'a self, action: &'a str, limit: u32) -> Fut<'a, AuditResponse> {
        self.get(&format!(
            "/admin/audit?action={}&limit={limit}",
            path_segment(action)
        ))
    }

    fn admin_sim(&self) -> Fut<'_, SimStatus> {
        self.get("/admin/sim")
    }

    fn admin_set_sim<'a>(&'a self, request: &'a SimRequest) -> Fut<'a, SimStatus> {
        self.post("/admin/sim", request)
    }
}
