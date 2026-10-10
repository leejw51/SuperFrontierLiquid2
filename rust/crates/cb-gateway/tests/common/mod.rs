//! Shared plumbing for the integration suite.
//!
//! Every test here drives the **real** router over the real services, with
//! only the ports replaced: an in-memory database, an in-memory key-value
//! store and a scriptable sequencer. A suite that exercised a simplified
//! router would prove nothing about the one that ships.

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use cb_gateway::markets::SpotDefaults;
use cb_gateway::node::{
    BookLevel, BookSnapshot, FutureMarketSnapshot, MarkSnapshot, PerpMarketSnapshot,
};
use cb_gateway::testing::{Harness, Player};
use cb_types::{AssetId, DerivId};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// A response, decoded.
pub struct Reply {
    /// HTTP status.
    pub status: StatusCode,
    /// Parsed body, or `Null` when there was none.
    pub body: Value,
}

impl Reply {
    /// The contract's error code, when this is an error envelope.
    pub fn error_code(&self) -> Option<&str> {
        self.body.get("error")?.get("code")?.as_str()
    }

    /// Assert the status and the error code together, which is what the
    /// contract actually specifies.
    pub fn assert_error(&self, status: u16, code: &str) {
        assert_eq!(
            self.status.as_u16(),
            status,
            "expected {status} {code}, got {} with body {}",
            self.status,
            self.body
        );
        assert_eq!(
            self.error_code(),
            Some(code),
            "expected error code {code}, got body {}",
            self.body
        );
    }

    /// Assert a 2xx, with the body in the failure message.
    pub fn assert_ok(&self, status: u16) -> &Value {
        assert_eq!(
            self.status.as_u16(),
            status,
            "expected {status}, got {} with body {}",
            self.status,
            self.body
        );
        &self.body
    }
}

/// A harness plus the conveniences the suite needs.
pub struct Api {
    /// The gateway under test.
    pub harness: Harness,
}

impl Api {
    /// A gateway with no markets listed.
    pub fn new() -> Self {
        Self {
            harness: Harness::new(),
        }
    }

    /// A gateway from a configured harness.
    pub fn from(harness: Harness) -> Self {
        Self { harness }
    }

    /// A gateway with one perpetual and one spot market, a book on the perp,
    /// and a fresh mark price — enough to place an order against.
    pub async fn with_markets() -> Self {
        let api = Self::new();
        api.list_btc_perp().await;
        api
    }

    /// List `BTC-PERP` with a two-sided book and a fresh mark.
    pub async fn list_btc_perp(&self) {
        self.harness.node.edit(|state| {
            state.perp_markets = vec![PerpMarketSnapshot {
                id: DerivId::perp(0),
                underlying: AssetId::Btc,
                tick_size: 10_000_000, // 0.10
                lot_size: 10_000,      // 0.0001
                max_leverage_x100: 5_000,
                initial_margin_bps: 200,
                maintenance_margin_bps: 100,
                taker_fee_bps: 4,
                maker_fee_bps: -1,
                funding_interval_blocks: 36_000,
                max_position_notional: 0,
                paused: false,
                reduce_only_mode: false,
                last_trade_px_q8: 6_425_000_000_000,
                last_trade_height: 1,
            }];
            state.deriv_books.insert(
                DerivId::perp(0),
                BookSnapshot {
                    bids: vec![BookLevel {
                        price: 6_424_900_000_000,
                        qty: 1_000_000_000,
                    }],
                    asks: vec![BookLevel {
                        price: 6_425_100_000_000,
                        qty: 1_000_000_000,
                    }],
                    height: 10,
                },
            );
            state.marks.insert(
                DerivId::perp(0),
                MarkSnapshot {
                    px_q8: 6_425_000_000_000,
                    computed_at_height: 1,
                    premium_ema_q9: 0,
                },
            );
            state.health.last_height = 10;
        });
        self.harness.refresh_markets().await;
    }

    /// List a dated future, for the settlement tests.
    pub async fn list_btc_future(&self, expiry_height: u64) {
        self.harness.node.edit(|state| {
            state.future_markets = vec![FutureMarketSnapshot {
                id: DerivId::future(0),
                underlying: AssetId::Btc,
                tick_size: 10_000_000,
                lot_size: 10_000,
                max_leverage_x100: 1_000,
                initial_margin_bps: 1_000,
                maintenance_margin_bps: 500,
                taker_fee_bps: 5,
                maker_fee_bps: 0,
                expiry_height,
                paused: false,
                reduce_only_mode: false,
                final_settlement_px_q8: None,
                last_trade_px_q8: 0,
                last_trade_height: 0,
            }];
            state.marks.insert(
                DerivId::future(0),
                MarkSnapshot {
                    px_q8: 6_425_000_000_000,
                    computed_at_height: 1,
                    premium_ema_q9: 0,
                },
            );
        });
        self.harness.refresh_markets().await;
    }

    /// Sign a player in through the development login.
    pub async fn player(&self, handle: &str, index: u32) -> Player {
        self.harness.login(handle, index).await
    }

    /// Give a player money on chain.
    pub fn fund(&self, player: &Player, asset: AssetId, amount: u128) {
        self.harness
            .node
            .fund(player.trading_address, asset, amount);
    }

    /// Give a player margin collateral.
    pub fn collateralise(&self, player: &Player, micro_usd: u128) {
        self.harness.node.edit(|state| {
            state.margins.insert(
                player.trading_address,
                cb_gateway::node::MarginSnapshot {
                    collateral: micro_usd,
                    unrealized_pnl: 0,
                    bad_debt: 0,
                },
            );
        });
    }

    /// Issue a request with no body and no session.
    pub async fn get(&self, path: &str) -> Reply {
        self.send(Request::builder().method("GET").uri(path), None)
            .await
    }

    /// Issue a request with a session.
    pub async fn get_as(&self, path: &str, player: &Player) -> Reply {
        self.send(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("authorization", player.bearer()),
            None,
        )
        .await
    }

    /// Issue a request with an arbitrary `Authorization` header.
    pub async fn get_with_token(&self, path: &str, token: &str) -> Reply {
        self.send(
            Request::builder()
                .method("GET")
                .uri(path)
                .header("authorization", token),
            None,
        )
        .await
    }

    /// `POST` with a JSON body and no session.
    pub async fn post(&self, path: &str, body: Value) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json"),
            Some(body),
        )
        .await
    }

    /// `POST` with a JSON body, no session, and extra headers.
    ///
    /// Used to prove that a header a client chooses — `x-forwarded-for` — does
    /// not choose the rate-limit key.
    pub async fn post_with_headers(
        &self,
        path: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        self.send(builder, Some(body)).await
    }

    /// `POST` with a JSON body and a session.
    pub async fn post_as(&self, path: &str, player: &Player, body: Value) -> Reply {
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", player.bearer()),
            Some(body),
        )
        .await
    }

    /// `PATCH` with a JSON body and a session.
    pub async fn patch_as(&self, path: &str, player: &Player, body: Value) -> Reply {
        self.send(
            Request::builder()
                .method("PATCH")
                .uri(path)
                .header("content-type", "application/json")
                .header("authorization", player.bearer()),
            Some(body),
        )
        .await
    }

    /// `DELETE` with a session.
    pub async fn delete_as(&self, path: &str, player: &Player) -> Reply {
        self.send(
            Request::builder()
                .method("DELETE")
                .uri(path)
                .header("authorization", player.bearer()),
            None,
        )
        .await
    }

    /// A request with a raw, possibly malformed, body and an optional
    /// content type, as a session.
    pub async fn raw_as(
        &self,
        method: &str,
        path: &str,
        player: &Player,
        content_type: Option<&str>,
        raw: &str,
    ) -> Reply {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", player.bearer());
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder
            .body(Body::from(raw.to_string()))
            .expect("request builds");
        self.dispatch(request).await
    }

    /// Send a fully built request through the router.
    pub async fn dispatch(&self, request: Request<Body>) -> Reply {
        let response = self
            .harness
            .router()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body collects")
            .to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        Reply { status, body }
    }

    async fn send(&self, builder: axum::http::request::Builder, body: Option<Value>) -> Reply {
        let request = match body {
            Some(value) => builder
                .body(Body::from(value.to_string()))
                .expect("request builds"),
            None => builder.body(Body::empty()).expect("request builds"),
        };
        let response = self
            .harness
            .router()
            .oneshot(request)
            .await
            .expect("the router answers");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body collects")
            .to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Reply { status, body }
    }
}

impl Default for Api {
    fn default() -> Self {
        Self::new()
    }
}

/// A harness with tighter spot defaults, for the spot tests.
pub fn spot_defaults() -> SpotDefaults {
    SpotDefaults::default()
}

/// A subscriber on the private `account` channel, attached straight to the
/// hub as a browser's socket would be once authenticated.
///
/// The socket itself is covered by `ws_live`; the lifecycle suites care about
/// which frames the gateway produces, for whom, and what is in them.
pub struct AccountSocket {
    /// The hub's id for this subscriber.
    pub id: u64,
    frames: tokio::sync::mpsc::Receiver<String>,
    seen: Vec<Value>,
}

impl AccountSocket {
    /// Attach to the hub as `player`, authenticated by their real session.
    pub fn attach(api: &Api, player: &Player) -> Self {
        let hub = &api.harness.state.hub;
        let (id, frames) = hub.register();
        hub.authenticate(
            id,
            &player.user_id,
            &cb_gateway::auth::session_token_hash(&player.token),
        );
        hub.subscribe(id, cb_gateway::ws::Channel::Account)
            .expect("an authenticated connection may take the account channel");
        Self {
            id,
            frames,
            seen: Vec::new(),
        }
    }

    /// Everything delivered so far, decoded.
    pub fn drain(&mut self) -> &[Value] {
        while let Ok(text) = self.frames.try_recv() {
            self.seen
                .push(serde_json::from_str(&text).expect("the gateway sends JSON"));
        }
        &self.seen
    }

    /// Forget what has been seen, so a later assertion is about later frames.
    pub fn forget(&mut self) {
        self.drain();
        self.seen.clear();
    }

    /// Every frame of one type seen so far.
    pub fn of_type(&mut self, kind: &str) -> Vec<Value> {
        self.drain()
            .iter()
            .filter(|frame| frame["type"] == kind)
            .cloned()
            .collect()
    }

    /// Every `order_update` payload seen since the last [`Self::forget`].
    pub fn order_updates(&mut self) -> Vec<Value> {
        self.of_type("order_update")
            .into_iter()
            .map(|frame| frame["data"].clone())
            .collect()
    }
}

/// Mine everything submitted since the last block and drive it through the
/// real watcher, exactly as the block subscription would.
pub async fn settle(api: &Api, height: u64) {
    let block = api.harness.node.mine(height);
    cb_gateway::watcher::on_block(&api.harness.state, &block)
        .await
        .expect("the block is processed");
}

/// The hash of the most recent transaction the gateway submitted.
pub fn last_submitted(api: &Api) -> cb_types::H256 {
    api.harness
        .node
        .submitted()
        .last()
        .expect("something was submitted")
        .hash()
}

/// A `0x`-prefixed hex rendering of a hash, as the contract publishes one.
pub fn hex_hash(hash: &cb_types::H256) -> String {
    format!("0x{}", hex::encode(hash.0))
}

/// A resting spot limit order on `BTC-USD` at `price`.
pub fn spot_buy(price: &str) -> Value {
    serde_json::json!({
        "symbol": "BTC-USD", "side": "buy", "type": "limit",
        "price": price, "qty": "0.50000000"
    })
}

impl Api {
    /// Every order `player` has, in any state, as `GET /account/orders`
    /// renders them.
    pub async fn all_orders(&self, player: &Player) -> Vec<Value> {
        self.get_as("/api/v1/account/orders?status=all", player)
            .await
            .assert_ok(200)["orders"]
            .as_array()
            .expect("orders is an array")
            .clone()
    }

    /// One order by the id `POST /orders` or an `order_update` named it by.
    ///
    /// The id changes once, from the transaction hash to the on-chain id, so
    /// the lookup accepts either by matching on the tracker first.
    pub async fn order(&self, player: &Player, tx_hash: &str) -> Value {
        let record = self
            .harness
            .state
            .tracker
            .find(tx_hash)
            .expect("the gateway tracks the order");
        let public = record.public_id();
        self.all_orders(player)
            .await
            .into_iter()
            .find(|o| o["order_id"] == public.as_str())
            .unwrap_or_else(|| panic!("order {public} is not in the REST listing"))
    }
}
