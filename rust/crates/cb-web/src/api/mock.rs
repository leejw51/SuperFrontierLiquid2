//! The mock gateway.
//!
//! The real gateway is being written in parallel against the same frozen
//! contract. Rather than block on it — or, worse, build against a guess and
//! discover the difference at integration — the whole application runs on this,
//! which answers every endpoint with the design reference's own figures.
//!
//! It is not a stub. It keeps state: orders placed here appear in the blotter,
//! move from `pending` to `open` a few hundred milliseconds later the way a real
//! block would take them, and partially fill. It rejects the way the contract
//! says to reject, with real error codes and real `details`, so the rejection
//! treatment and its one-tap remedies are exercised by ordinary use rather than
//! by a special demo mode. Eight of the nine states the design system requires
//! arise from data this file produces; the two socket states come from the feed
//! driver in [`crate::ws`].
//!
//! Every response is delayed by a plausible round trip, because a UI that has
//! never seen a loading state has not been built for one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use gloo_timers::future::TimeoutFuture;
use rust_decimal::Decimal;

use crate::clock::{now_millis, now_seconds, rfc3339};

use super::error::{ApiError, ApiResult, ErrorCode, GatewayError};
use super::fixtures::{self, dec};
use super::types::*;
use super::{Api, Fut, Interval, LeaderboardWindow, OrderFilter};

/// The genesis height the mock chain counts up from.
const GENESIS_HEIGHT: u64 = 184_203;

/// The block interval the mock chain runs at, matching `block_time_ms`.
const BLOCK_MS: i64 = 100;

/// A plausible gateway round trip, so loading states are real.
const LATENCY_MS: u32 = 120;

/// The address the contract's canonical recovery phrase derives.
///
/// Seeding the mock with it means the published phrase signs in as an existing
/// account, which is the only way to demonstrate the returning-user path
/// without inventing a phrase that exists nowhere else.
pub const CANONICAL_ADDRESS: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

/// How long an order sits `pending` before the next block accepts it.
const ACCEPT_MS: i64 = 400;

/// Everything the mock remembers between calls.
///
/// Shared with the mock websocket transport, which reads the same order book and
/// pushes the same order updates, so the two never disagree about what happened.
#[derive(Debug)]
pub struct MockState {
    /// The signed-in session, if any.
    pub session: Option<Session>,
    /// Outstanding login challenges, keyed by lower-case address.
    pub challenges: BTreeMap<String, Challenge>,
    /// Addresses that already have an account, to their handle.
    ///
    /// Seeded with the contract's own canonical address so that signing in with
    /// the published phrase exercises the returning-user path.
    pub known: BTreeMap<String, String>,
    /// Operator actions, newest first.
    pub audit: Vec<AuditRecord>,
    /// Suspended accounts.
    pub suspended: BTreeSet<String>,
    /// Whether the faucet is paying out at all.
    pub faucet_enabled: bool,
    /// The cooldown an operator has configured.
    pub faucet_cooldown: i64,
    /// What a claim pays.
    pub faucet_payout: Vec<Grant>,
    /// The simulator's state.
    pub sim: SimStatus,
    /// Every market.
    pub markets: Vec<Market>,
    /// Last traded price per market.
    pub last: BTreeMap<String, Decimal>,
    /// 24-hour change per market.
    pub change: BTreeMap<String, Decimal>,
    /// The account's orders, newest first.
    pub orders: Vec<Order>,
    /// Open positions.
    pub positions: Vec<Position>,
    /// Fill history.
    pub fills: Vec<Fill>,
    /// The unified ledger.
    pub ledger: Vec<LedgerEntry>,
    /// How many faucet claims have been made.
    pub claims_made: u32,
    /// When the faucet next unlocks, in epoch seconds.
    pub faucet_next_at: Option<i64>,
    /// Faucet claims as ledger entries, newest first.
    pub faucet_history: Vec<LedgerEntry>,
    /// Wall-clock milliseconds at which the mock chain started.
    pub started_ms: i64,
    /// Order ids waiting to be promoted from `pending` to `open`, with the
    /// wall-clock millisecond at which that happens.
    pub accepting: Vec<(String, i64)>,
    /// Orders that have changed and have not yet been announced on the socket.
    ///
    /// Separate from [`Self::accepting`] because both a REST read and the feed's
    /// tick advance the clock, and whichever runs first must not swallow the
    /// change: the reader needs the new status, and the socket still needs to
    /// say so.
    pub announce: Vec<Order>,
}

impl MockState {
    /// Builds the initial state from the fixtures.
    fn new() -> Self {
        let now = now_seconds();
        let markets = fixtures::markets();
        let mut last = BTreeMap::new();
        let mut change = BTreeMap::new();
        for (symbol, price, pct) in fixtures::quotes() {
            last.insert(symbol.to_string(), dec(price));
            change.insert(symbol.to_string(), dec(pct));
        }
        let height = GENESIS_HEIGHT;
        Self {
            session: None,
            challenges: BTreeMap::new(),
            known: [(
                CANONICAL_ADDRESS.to_ascii_lowercase(),
                "satoshi".to_string(),
            )]
            .into_iter()
            .collect(),
            audit: fixtures::audit(now),
            suspended: BTreeSet::new(),
            faucet_enabled: true,
            faucet_cooldown: 3_600,
            faucet_payout: fixtures::faucet_grant(),
            sim: fixtures::sim_status(),
            markets,
            last,
            change,
            orders: fixtures::orders(now),
            positions: fixtures::positions(now),
            fills: fixtures::fills(240, height, now),
            ledger: fixtures::ledger(260, height, now),
            claims_made: 3,
            faucet_next_at: None,
            faucet_history: fixtures::faucet_history(height, now),
            started_ms: now_millis(),
            accepting: Vec::new(),
            announce: Vec::new(),
        }
    }

    /// The mock chain's current height, which advances with the wall clock.
    #[must_use]
    pub fn height(&self) -> u64 {
        let elapsed = (now_millis() - self.started_ms).max(0);
        GENESIS_HEIGHT + (elapsed / BLOCK_MS) as u64
    }

    /// Looks a market up by symbol.
    #[must_use]
    pub fn market(&self, symbol: &str) -> Option<&Market> {
        self.markets.iter().find(|market| market.symbol == symbol)
    }

    /// The last traded price for a market.
    #[must_use]
    pub fn last_price(&self, symbol: &str) -> Decimal {
        self.last.get(symbol).copied().unwrap_or_default()
    }

    /// Promotes any order whose acceptance block has arrived.
    ///
    /// This is what produces the `pending` → `open` transition the design system
    /// calls out: the row appears immediately at 60% opacity, then fills in.
    /// Promoted orders are queued on [`Self::announce`] rather than returned,
    /// so a REST read that happens to advance the clock cannot swallow the
    /// update the socket still owes the client.
    pub fn settle_pending(&mut self) {
        let now = now_millis();
        let due: Vec<String> = self
            .accepting
            .iter()
            .filter(|(_, at)| *at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        self.accepting.retain(|(_, at)| *at > now);
        for id in due {
            if let Some(order) = self.orders.iter_mut().find(|order| order.order_id == id) {
                if order.status == OrderStatus::Pending {
                    order.status = OrderStatus::Open;
                    order.updated_at = rfc3339(now_seconds());
                    let changed = order.clone();
                    self.announce.push(changed);
                }
            }
        }
    }

    /// Takes everything waiting to be announced on the socket.
    pub fn take_announcements(&mut self) -> Vec<Order> {
        std::mem::take(&mut self.announce)
    }
}

/// A gateway that answers from the design reference instead of a network.
pub struct MockApi {
    state: Arc<Mutex<MockState>>,
}

impl Default for MockApi {
    fn default() -> Self {
        Self::new()
    }
}

impl MockApi {
    /// Creates a mock gateway with the fixtures loaded.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState::new())),
        }
    }

    /// The shared state, so the mock websocket transport can read the same book
    /// and announce the same order updates.
    #[must_use]
    pub fn state(&self) -> Arc<Mutex<MockState>> {
        Arc::clone(&self.state)
    }

    /// Runs `body` against the locked state after a plausible round trip.
    fn call<'a, T: 'a>(
        &'a self,
        body: impl FnOnce(&mut MockState) -> ApiResult<T> + 'a,
    ) -> Fut<'a, T> {
        let state = Arc::clone(&self.state);
        async move {
            TimeoutFuture::new(LATENCY_MS).await;
            let mut guard = state
                .lock()
                .map_err(|_| ApiError::Transport("mock state is unavailable".into()))?;
            guard.settle_pending();
            body(&mut guard)
        }
        .boxed_local()
    }
}

/// Builds a gateway error with the contract's shape.
fn reject(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::Gateway(GatewayError {
        code,
        message: message.into(),
        details: BTreeMap::new(),
    })
}

/// Builds a gateway error carrying a `details` object.
fn reject_with(
    code: ErrorCode,
    message: impl Into<String>,
    details: &[(&str, String)],
) -> ApiError {
    ApiError::Gateway(GatewayError {
        code,
        message: message.into(),
        details: details
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect(),
    })
}

/// The on-chain account a login address maps to.
///
/// The gateway does this with `HKDF-SHA256(server_secret, login_address)`. The
/// mock has no server secret, so it uses a keyed hash of the address instead —
/// the property that matters here is the one the contract names: one login
/// address always maps to the same trading account.
#[must_use]
pub fn trading_address_for(login_address: &str) -> String {
    let digest = crate::wallet::keccak256(
        format!("cbl-trading-account:{}", login_address.to_ascii_lowercase()).as_bytes(),
    );
    let mut address = [0_u8; 20];
    address.copy_from_slice(&digest[12..]);
    crate::wallet::to_eip55(&address)
}

/// The session a mock login hands back.
fn issue_session(address: &str, handle: &str, is_new: bool) -> Session {
    let now = now_seconds();
    Session {
        user_id: format!(
            "usr_{:06x}",
            crate::wallet::keccak256(address.as_bytes())[0] as u32 * 977
        ),
        handle: handle.to_string(),
        address: address.to_string(),
        session_token: format!("sess_{now}"),
        expires_at: rfc3339(now + 86_400),
        is_new,
    }
}

impl Api for MockApi {
    fn set_token(&self, token: Option<String>) {
        // A token restored from storage has to reconstitute a session, or a page
        // reload would sign the user out and "restored on load" would be a claim
        // the demo cannot keep. The mock trusts any token it minted; real
        // authorisation lives in HttpApi.
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match token {
            Some(token) if token.starts_with("sess_") && state.session.is_none() => {
                let handle = state
                    .known
                    .get(&CANONICAL_ADDRESS.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| "satoshi".to_string());
                let mut session = issue_session(CANONICAL_ADDRESS, &handle, false);
                session.session_token = token;
                state.session = Some(session);
            }
            None => state.session = None,
            Some(_) => {}
        }
    }

    fn token(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.session.as_ref().map(|s| s.session_token.clone()))
    }

    fn challenge<'a>(&'a self, address: &'a str) -> Fut<'a, Challenge> {
        self.call(move |state| {
            if !crate::wallet::is_valid_address(address) {
                return Err(reject(
                    ErrorCode::ValidationFailed,
                    "that is not a well-formed address",
                ));
            }
            let now = now_seconds();
            let nonce = format!("cbl-login-{:012x}", now_millis());
            let challenge = Challenge {
                message: format!(
                    "SuperFrontierLiquid wants you to sign in.\n\nAddress: {address}\nNonce: {nonce}\nIssued: {}",
                    rfc3339(now)
                ),
                nonce,
                expires_at: rfc3339(now + 300),
            };
            state
                .challenges
                .insert(address.to_ascii_lowercase(), challenge.clone());
            Ok(challenge)
        })
    }

    fn login<'a>(
        &'a self,
        address: &'a str,
        signature: &'a str,
        handle: Option<&'a str>,
    ) -> Fut<'a, Session> {
        self.call(move |state| {
            let key = address.to_ascii_lowercase();
            let challenge = state
                .challenges
                .remove(&key)
                .ok_or_else(|| reject(ErrorCode::ChallengeExpired, "that sign-in has expired"))?;
            if crate::format::parse_rfc3339(&challenge.expires_at).unwrap_or_default()
                < now_seconds()
            {
                return Err(reject(
                    ErrorCode::ChallengeExpired,
                    "that sign-in has expired",
                ));
            }

            // The real check, not a rubber stamp: the mock recovers the signer
            // exactly as the gateway will and compares it to the claimed address.
            let recovered = crate::wallet::recover_address(&challenge.message, signature)
                .ok_or_else(|| {
                    reject(ErrorCode::BadSignature, "that signature could not be read")
                })?;
            if !recovered.eq_ignore_ascii_case(address) {
                return Err(reject(
                    ErrorCode::BadSignature,
                    "that signature was made by a different address",
                ));
            }

            let existing = state.known.get(&key).cloned();
            let (handle, is_new) = match (existing, handle) {
                (Some(known), _) => (known, false),
                (None, Some(wanted)) => {
                    if state.known.values().any(|held| held == wanted) {
                        return Err(reject(
                            ErrorCode::HandleTaken,
                            format!("someone is already trading as @{wanted}"),
                        ));
                    }
                    state.known.insert(key, wanted.to_string());
                    (wanted.to_string(), true)
                }
                (None, None) => {
                    return Err(reject(
                        ErrorCode::HandleRequired,
                        "this address has not traded here before, so it needs a handle",
                    ))
                }
            };

            let session = issue_session(address, &handle, is_new);
            state.session = Some(session.clone());
            Ok(session)
        })
    }

    fn session(&self) -> Fut<'_, SessionInfo> {
        self.call(|state| {
            let session = state
                .session
                .clone()
                .ok_or_else(|| reject(ErrorCode::Unauthorized, "this session has expired"))?;
            Ok(SessionInfo {
                user_id: session.user_id,
                handle: session.handle,
                login_address: session.address.clone(),
                trading_address: trading_address_for(&session.address),
                roles: vec!["user".into(), "admin".into()],
                issued_at: rfc3339(now_seconds() - 600),
                expires_at: session.expires_at,
            })
        })
    }

    fn logout(&self) -> Fut<'_, ()> {
        self.call(|state| {
            state.session = None;
            Ok(())
        })
    }

    fn markets(&self) -> Fut<'_, MarketsResponse> {
        self.call(|state| {
            Ok(MarketsResponse {
                markets: state.markets.clone(),
            })
        })
    }

    fn ticker<'a>(&'a self, symbol: &'a str) -> Fut<'a, Ticker> {
        self.call(move |state| {
            let market = state
                .market(symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, format!("no market called {symbol}")))?;
            let last = state.last_price(symbol);
            let change = state.change.get(symbol).copied().unwrap_or_default();
            Ok(fixtures::ticker(&market, last, change, now_seconds()))
        })
    }

    fn orderbook<'a>(&'a self, symbol: &'a str, depth: u32) -> Fut<'a, OrderBookPayload> {
        self.call(move |state| {
            let market = state
                .market(symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, format!("no market called {symbol}")))?;
            let last = state.last_price(symbol);
            let height = state.height();
            Ok(fixtures::book(&market, last, height, depth, 0xB00C_5EED))
        })
    }

    fn trades<'a>(&'a self, symbol: &'a str, limit: u32) -> Fut<'a, TradesResponse> {
        self.call(move |state| {
            let market = state
                .market(symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, format!("no market called {symbol}")))?;
            let last = state.last_price(symbol);
            let height = state.height();
            Ok(TradesResponse {
                trades: fixtures::trades(&market, last, height, limit, now_seconds()),
            })
        })
    }

    fn candles<'a>(
        &'a self,
        symbol: &'a str,
        interval: Interval,
        limit: u32,
    ) -> Fut<'a, CandlesResponse> {
        self.call(move |state| {
            let market = state
                .market(symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, format!("no market called {symbol}")))?;
            let last = state.last_price(symbol);
            Ok(CandlesResponse {
                symbol: symbol.to_string(),
                interval: interval.as_str().to_string(),
                candles: fixtures::candles(&market, last, interval.seconds(), limit, now_seconds()),
            })
        })
    }

    fn account(&self) -> Fut<'_, Account> {
        self.call(|state| {
            let login = state
                .session
                .as_ref()
                .map_or(CANONICAL_ADDRESS, |session| session.address.as_str())
                .to_string();
            let trading = trading_address_for(&login);
            Ok(fixtures::account(&login, &trading))
        })
    }

    fn orders(&self, filter: OrderFilter) -> Fut<'_, OrdersResponse> {
        self.call(move |state| {
            let orders = state
                .orders
                .iter()
                .filter(|order| match filter {
                    OrderFilter::All => true,
                    OrderFilter::Open => order.status.is_live(),
                    OrderFilter::Filled => order.status == OrderStatus::Filled,
                    OrderFilter::Cancelled => order.status == OrderStatus::Cancelled,
                })
                .cloned()
                .collect();
            Ok(OrdersResponse { orders })
        })
    }

    fn positions(&self) -> Fut<'_, PositionsResponse> {
        self.call(|state| {
            Ok(PositionsResponse {
                positions: state.positions.clone(),
            })
        })
    }

    fn fills(&self, limit: u32) -> Fut<'_, FillsResponse> {
        self.call(move |state| {
            Ok(FillsResponse {
                fills: state.fills.iter().take(limit as usize).cloned().collect(),
            })
        })
    }

    fn ledger(&self, limit: u32) -> Fut<'_, LedgerResponse> {
        self.call(move |state| {
            // Faucet grants are ledger entries like any other movement, so they
            // are merged in here rather than living in a parallel list: the
            // faucet screen's history is this ledger filtered to `kind: faucet`.
            let mut entries: Vec<LedgerEntry> = state
                .ledger
                .iter()
                .chain(state.faucet_history.iter())
                .cloned()
                .collect();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.height));
            entries.truncate(limit as usize);
            Ok(LedgerResponse { entries })
        })
    }

    fn place_order<'a>(&'a self, request: &'a PlaceOrderRequest) -> Fut<'a, PlaceOrderResponse> {
        self.call(move |state| {
            let market = state
                .market(&request.symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, "no such market"))?;

            if !market.status.is_tradable() {
                return Err(reject(
                    ErrorCode::MarketHalted,
                    format!(
                        "{} is {} and is not accepting orders",
                        market.symbol,
                        market.status.label().to_lowercase()
                    ),
                ));
            }

            // The same idempotency rule the contract describes: a repeated
            // client_order_id returns the original order rather than a duplicate.
            if let Some(existing) = state
                .orders
                .iter()
                .find(|order| order.client_order_id.as_deref() == Some(&request.client_order_id))
            {
                return Ok(PlaceOrderResponse {
                    order_id: existing.order_id.clone(),
                    client_order_id: request.client_order_id.clone(),
                    status: existing.status,
                    tx_hash: existing.tx_hash.clone().unwrap_or_default(),
                    accepted_at: existing.updated_at.clone(),
                    immediate_fills: Vec::new(),
                });
            }

            let last = state.last_price(&request.symbol);
            let book = fixtures::book(&market, last, state.height(), 10, 0xB00C_5EED);
            let best_ask = book.asks.first().map_or(last, Level::price);
            let best_bid = book.bids.first().map_or(last, Level::price);
            let reference = request.price.unwrap_or(last);
            let notional = reference * request.qty;

            if notional < market.min_notional {
                return Err(reject_with(
                    ErrorCode::ValidationFailed,
                    format!(
                        "{} needs an order worth at least {} USD",
                        market.symbol, market.min_notional
                    ),
                    &[("min_notional", market.min_notional.to_string())],
                ));
            }

            if request.post_only {
                let crosses = match request.side {
                    Side::Buy => reference >= best_ask,
                    Side::Sell => reference <= best_bid,
                };
                if crosses {
                    let (touch, away) = match request.side {
                        Side::Buy => (best_ask, best_ask - market.tick_size),
                        Side::Sell => (best_bid, best_bid + market.tick_size),
                    };
                    return Err(reject_with(
                        ErrorCode::PostOnlyWouldCross,
                        format!(
                            "your {} at {reference} would have taken liquidity. Best {} is {touch}.",
                            request.side.label().to_lowercase(),
                            if request.side == Side::Buy { "ask" } else { "bid" }
                        ),
                        &[("reprice_to", away.to_string())],
                    ));
                }
            }

            let available = dec("56086.00");
            let leverage = request.leverage.unwrap_or(Decimal::ONE).max(Decimal::ONE);
            let margin_required = (notional / leverage).round_dp(2);
            if market.kind.is_derivative() && margin_required > available {
                return Err(reject_with(
                    ErrorCode::InsufficientMargin,
                    format!("need {margin_required} USD of margin, have {available}"),
                    &[
                        ("required", margin_required.to_string()),
                        ("available", available.to_string()),
                    ],
                ));
            }
            if !market.kind.is_derivative() && notional > dec("94000.00") {
                return Err(reject_with(
                    ErrorCode::InsufficientBalance,
                    format!("need {notional} USD, have 94000.00"),
                    &[
                        ("required", notional.to_string()),
                        ("available", "94000.00".to_string()),
                    ],
                ));
            }

            let now = now_seconds();
            let height = state.height();
            let order_id = format!("0x{:012x}", 0xA1_B2C3_D400_u64 + height);
            let tx_hash = format!("0x{:014x}", 0x007f_2ad2_b1c4_9c04_u64 ^ height);

            // Anything that crosses the touch trades immediately; the rest rests.
            let takes = match request.side {
                Side::Buy => request.order_type == OrderType::Market || reference >= best_ask,
                Side::Sell => request.order_type == OrderType::Market || reference <= best_bid,
            };
            let touch = if request.side == Side::Buy { best_ask } else { best_bid };
            let available_at_touch = if request.side == Side::Buy {
                book.asks.first().map_or(Decimal::ZERO, Level::qty)
            } else {
                book.bids.first().map_or(Decimal::ZERO, Level::qty)
            };
            let filled = if takes {
                request.qty.min(available_at_touch)
            } else {
                Decimal::ZERO
            };
            let immediate_fills = if filled > Decimal::ZERO {
                vec![ImmediateFill {
                    price: touch,
                    qty: filled,
                    fee: (touch * filled * dec("0.0004")).round_dp(2),
                }]
            } else {
                Vec::new()
            };

            let order = Order {
                order_id: order_id.clone(),
                client_order_id: Some(request.client_order_id.clone()),
                symbol: request.symbol.clone(),
                side: request.side,
                order_type: request.order_type,
                price: request.price,
                qty: request.qty,
                filled_qty: filled,
                remaining_qty: request.qty - filled,
                avg_fill_price: (filled > Decimal::ZERO).then_some(touch),
                status: OrderStatus::Pending,
                tif: request.tif,
                post_only: request.post_only,
                reduce_only: request.reduce_only,
                created_at: rfc3339(now),
                updated_at: rfc3339(now),
                tx_hash: Some(tx_hash.clone()),
                reject_reason: None,
            };
            state.orders.insert(0, order);
            state
                .accepting
                .push((order_id.clone(), now_millis() + ACCEPT_MS));

            Ok(PlaceOrderResponse {
                order_id,
                client_order_id: request.client_order_id.clone(),
                status: OrderStatus::Pending,
                tx_hash,
                accepted_at: rfc3339(now),
                immediate_fills,
            })
        })
    }

    fn cancel_order<'a>(&'a self, order_id: &'a str) -> Fut<'a, CancelOrderResponse> {
        self.call(move |state| {
            let height = state.height();
            let order = state
                .orders
                .iter_mut()
                .find(|order| order.order_id == order_id)
                .ok_or_else(|| reject(ErrorCode::NotFound, "that order is not resting"))?;
            if !order.status.is_live() {
                return Err(reject(
                    ErrorCode::ValidationFailed,
                    "that order is no longer resting",
                ));
            }
            order.status = OrderStatus::Cancelled;
            order.updated_at = rfc3339(now_seconds());
            Ok(CancelOrderResponse {
                order_id: order_id.to_string(),
                status: OrderStatus::Cancelled,
                tx_hash: format!("0x{:014x}", 0x0055_b120_e700_0000_u64 ^ height),
            })
        })
    }

    fn cancel_all<'a>(&'a self, symbol: Option<&'a str>) -> Fut<'a, CancelAllResponse> {
        self.call(move |state| {
            let height = state.height();
            let mut cancelled = 0;
            for order in &mut state.orders {
                let matches = symbol.is_none_or(|symbol| order.symbol == symbol);
                if matches && order.status.is_live() {
                    order.status = OrderStatus::Cancelled;
                    order.updated_at = rfc3339(now_seconds());
                    cancelled += 1;
                }
            }
            Ok(CancelAllResponse {
                cancelled,
                tx_hash: format!("0x{:014x}", 0x0055_b120_e700_0001_u64 ^ height),
            })
        })
    }

    fn close_position<'a>(
        &'a self,
        symbol: &'a str,
        qty: Option<Decimal>,
    ) -> Fut<'a, ClosePositionResponse> {
        self.call(move |state| {
            let height = state.height();
            let index = state
                .positions
                .iter()
                .position(|position| position.symbol == symbol)
                .ok_or_else(|| reject(ErrorCode::NotFound, "no position in that market"))?;
            let position = state.positions[index].clone();
            let closed = qty.unwrap_or(position.qty).min(position.qty);
            let fraction = if position.qty.is_zero() {
                Decimal::ZERO
            } else {
                closed / position.qty
            };
            let realized = (position.unrealized_pnl * fraction).round_dp(2);
            if closed >= position.qty {
                state.positions.remove(index);
            } else if let Some(existing) = state.positions.get_mut(index) {
                existing.qty -= closed;
                existing.notional = (existing.qty * existing.mark_price).round_dp(2);
                existing.unrealized_pnl -= realized;
            }
            Ok(ClosePositionResponse {
                tx_hash: format!("0x{:014x}", 0x007f_2ad2_b1c4_9c04_u64 ^ height),
                closed_qty: closed,
                avg_price: position.mark_price,
                realized_pnl: realized,
            })
        })
    }

    fn faucet(&self) -> Fut<'_, FaucetStatus> {
        self.call(|state| {
            let now = now_seconds();
            let available = state.faucet_next_at.is_none_or(|at| at <= now);
            Ok(FaucetStatus {
                available,
                next_available_at: if available {
                    None
                } else {
                    state.faucet_next_at.map(rfc3339)
                },
                cooldown_seconds: 3_600,
                grant: fixtures::faucet_grant(),
                claims_made: state.claims_made,
            })
        })
    }

    fn faucet_claim(&self) -> Fut<'_, FaucetClaim> {
        self.call(|state| {
            let now = now_seconds();
            if let Some(next) = state.faucet_next_at {
                if next > now {
                    return Err(reject_with(
                        ErrorCode::FaucetCooldown,
                        format!("the next grant unlocks at {}", crate::format::clock(next)),
                        &[("next_available_at", rfc3339(next))],
                    ));
                }
            }
            let height = state.height();
            state.claims_made += 1;
            state.faucet_next_at = Some(now + 3_600);
            let claim_id = format!("clm_{:04x}", height % 0xFFFF);
            let tx_hash = format!("0x{:014x}", 0x001b_8e77_2c99_44af_u64 ^ height);
            state.faucet_history.insert(
                0,
                LedgerEntry {
                    id: claim_id.clone(),
                    kind: LedgerKind::Faucet,
                    asset: "USD".into(),
                    amount: dec("100000.00"),
                    balance_after: dec("194000.00"),
                    memo: "faucet claim".into(),
                    at: rfc3339(now),
                    tx_hash: tx_hash.clone(),
                    height,
                },
            );
            Ok(FaucetClaim {
                claim_id,
                tx_hash,
                granted: fixtures::faucet_grant(),
                next_available_at: rfc3339(now + 3_600),
            })
        })
    }

    fn chain_status(&self) -> Fut<'_, ChainStatus> {
        self.call(|state| Ok(fixtures::chain_status(state.height(), now_seconds())))
    }

    fn blocks(&self, limit: u32) -> Fut<'_, BlocksResponse> {
        self.call(move |state| {
            Ok(BlocksResponse {
                blocks: fixtures::blocks(state.height(), limit, now_millis()),
            })
        })
    }

    fn chain_txs(&self, limit: u32) -> Fut<'_, TxsResponse> {
        self.call(move |state| {
            Ok(TxsResponse {
                txs: fixtures::txs(state.height(), limit, now_millis()),
            })
        })
    }

    fn leaderboard(&self, window: LeaderboardWindow, limit: u32) -> Fut<'_, LeaderboardResponse> {
        self.call(move |state| {
            let handle = state
                .session
                .as_ref()
                .map_or("satoshi", |session| session.handle.as_str())
                .to_string();
            Ok(LeaderboardResponse {
                window: window.as_str().to_string(),
                entries: fixtures::leaderboard(&handle, limit),
            })
        })
    }
    // ------------------------------------------------------------ user page

    fn profile(&self) -> Fut<'_, Profile> {
        self.call(|state| {
            let session = state
                .session
                .clone()
                .ok_or_else(|| reject(ErrorCode::Unauthorized, "this session has expired"))?;
            Ok(fixtures::profile(
                &session.user_id,
                &session.handle,
                &session.address,
                &["user".to_string(), "admin".to_string()],
                now_seconds(),
            ))
        })
    }

    fn update_handle<'a>(&'a self, handle: &'a str) -> Fut<'a, Profile> {
        self.call(move |state| {
            let session = state
                .session
                .clone()
                .ok_or_else(|| reject(ErrorCode::Unauthorized, "this session has expired"))?;
            if state.known.iter().any(|(address, held)| {
                held == handle && !address.eq_ignore_ascii_case(&session.address)
            }) {
                return Err(reject(
                    ErrorCode::HandleTaken,
                    format!("someone is already trading as @{handle}"),
                ));
            }
            state
                .known
                .insert(session.address.to_ascii_lowercase(), handle.to_string());
            if let Some(current) = state.session.as_mut() {
                current.handle = handle.to_string();
            }
            Ok(fixtures::profile(
                &session.user_id,
                handle,
                &session.address,
                &["user".to_string(), "admin".to_string()],
                now_seconds(),
            ))
        })
    }

    fn sessions(&self) -> Fut<'_, SessionsResponse> {
        self.call(|state| {
            let token = state
                .session
                .as_ref()
                .map_or("sess_current", |session| session.session_token.as_str())
                .to_string();
            Ok(SessionsResponse {
                sessions: fixtures::sessions(&token, now_seconds()),
            })
        })
    }

    fn revoke_session<'a>(&'a self, _session_id: &'a str) -> Fut<'a, ()> {
        self.call(|_| Ok(()))
    }

    fn revoke_other_sessions(&self) -> Fut<'_, RevokedResponse> {
        self.call(|_| Ok(RevokedResponse { revoked: 2 }))
    }

    fn activity<'a>(&'a self, limit: u32, cursor: Option<&'a str>) -> Fut<'a, ActivityResponse> {
        self.call(move |state| {
            let offset: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let all = fixtures::activity(240, state.height(), now_seconds());
            let entries: Vec<ActivityEntry> =
                all.into_iter().skip(offset).take(limit as usize).collect();
            let next = offset + entries.len();
            Ok(ActivityResponse {
                next_cursor: (next < 240).then(|| next.to_string()),
                entries,
            })
        })
    }

    // ---------------------------------------------------------------- admin

    fn admin_overview(&self) -> Fut<'_, AdminOverview> {
        self.call(|state| Ok(fixtures::admin_overview(state.height(), &state.markets)))
    }

    fn admin_users<'a>(
        &'a self,
        query: &'a str,
        is_bot: Option<bool>,
        limit: u32,
    ) -> Fut<'a, AdminUsersResponse> {
        self.call(move |state| {
            let needle = query.trim().to_ascii_lowercase();
            let users: Vec<AdminUser> = fixtures::admin_users(120, now_seconds())
                .into_iter()
                .map(|mut user| {
                    if state.suspended.contains(&user.user_id) {
                        user.status = AccountStatus::Suspended;
                    }
                    user
                })
                .filter(|user| {
                    is_bot.is_none_or(|wanted| user.is_bot == wanted)
                        && (needle.is_empty()
                            || user.handle.to_ascii_lowercase().contains(&needle)
                            || user.address.to_ascii_lowercase().contains(&needle))
                })
                .take(limit as usize)
                .collect();
            Ok(AdminUsersResponse {
                users,
                next_cursor: None,
            })
        })
    }

    fn admin_suspend<'a>(&'a self, user_id: &'a str, reason: &'a str) -> Fut<'a, ()> {
        self.call(move |state| {
            state.suspended.insert(user_id.to_string());
            record(state, "user.suspend", user_id, "reason", reason);
            Ok(())
        })
    }

    fn admin_unsuspend<'a>(&'a self, user_id: &'a str) -> Fut<'a, ()> {
        self.call(move |state| {
            state.suspended.remove(user_id);
            record(state, "user.unsuspend", user_id, "reason", "");
            Ok(())
        })
    }

    fn admin_markets(&self) -> Fut<'_, AdminMarketsResponse> {
        self.call(|state| {
            let last = state.last.clone();
            Ok(AdminMarketsResponse {
                markets: fixtures::admin_markets(&state.markets, &|symbol| {
                    last.get(symbol).copied().unwrap_or_default()
                }),
            })
        })
    }

    fn admin_halt<'a>(&'a self, symbol: &'a str, reason: &'a str) -> Fut<'a, ()> {
        self.call(move |state| {
            set_status(state, symbol, MarketStatus::Halted)?;
            record(state, "market.halt", symbol, "reason", reason);
            Ok(())
        })
    }

    fn admin_resume<'a>(&'a self, symbol: &'a str) -> Fut<'a, ()> {
        self.call(move |state| {
            set_status(state, symbol, MarketStatus::Trading)?;
            record(state, "market.resume", symbol, "reason", "");
            Ok(())
        })
    }

    fn admin_settle<'a>(
        &'a self,
        symbol: &'a str,
        settlement_price: Decimal,
        force: bool,
    ) -> Fut<'a, ()> {
        self.call(move |state| {
            let market = state
                .market(symbol)
                .cloned()
                .ok_or_else(|| reject(ErrorCode::NotFound, "no such market"))?;
            // The contract's two guards, both enforced here so the confirmation
            // dialog and the gateway cannot disagree about what is allowed.
            if market.status.is_tradable() {
                return Err(reject(
                    ErrorCode::ValidationFailed,
                    "a market must be expired or halted before it can be settled",
                ));
            }
            let mark = state.last_price(symbol);
            let drift = if mark.is_zero() {
                Decimal::ZERO
            } else {
                ((settlement_price - mark) / mark).abs()
            };
            if drift > dec("0.10") && !force {
                return Err(reject_with(
                    ErrorCode::ValidationFailed,
                    format!("that is more than 10% from the last oracle mark of {mark}"),
                    &[("mark_price", mark.to_string())],
                ));
            }
            set_status(state, symbol, MarketStatus::Expired)?;
            state.last.insert(symbol.to_string(), settlement_price);
            record(
                state,
                "market.settle",
                symbol,
                "settlement_price",
                &settlement_price.to_string(),
            );
            Ok(())
        })
    }

    fn admin_set_risk<'a>(
        &'a self,
        symbol: &'a str,
        max_leverage: Decimal,
        initial_margin_bps: u32,
        maintenance_margin_bps: u32,
        dry_run: bool,
    ) -> Fut<'a, RiskApplied> {
        self.call(move |state| {
            let market = state
                .markets
                .iter_mut()
                .find(|market| market.symbol == symbol)
                .ok_or_else(|| reject(ErrorCode::NotFound, "no such market"))?;
            let was = market.maintenance_margin_bps.unwrap_or(0);
            // A dry run computes the consequence and changes nothing. The mock
            // has to honour that or the dialog's preview would silently apply.
            if !dry_run {
                market.max_leverage = Some(max_leverage);
                market.initial_margin_bps = Some(initial_margin_bps);
                market.maintenance_margin_bps = Some(maintenance_margin_bps);
                record(
                    state,
                    "market.risk",
                    symbol,
                    "maintenance_margin_bps",
                    &maintenance_margin_bps.to_string(),
                );
            }
            // Raising maintenance margin can put open accounts under water
            // immediately; the operator finds out here rather than from the
            // liquidation engine.
            let count = if maintenance_margin_bps > was {
                3 + (maintenance_margin_bps - was) / 50
            } else {
                0
            };
            Ok(RiskApplied {
                applied: !dry_run,
                accounts_now_below_maintenance: count,
                affected: (0..count)
                    .map(|index| AffectedAccount {
                        user_id: format!("usr_{index:04}"),
                        handle: format!("trader_{index}"),
                        margin_ratio_before: Decimal::new(40 + i64::from(index), 2),
                        margin_ratio_after: Decimal::new(82 + i64::from(index), 2),
                    })
                    .collect(),
            })
        })
    }

    fn admin_faucet(&self) -> Fut<'_, AdminFaucet> {
        self.call(|state| {
            Ok(fixtures::admin_faucet(
                state.faucet_enabled,
                state.faucet_cooldown,
                &state.faucet_payout,
            ))
        })
    }

    fn admin_set_faucet<'a>(
        &'a self,
        enabled: bool,
        cooldown_seconds: i64,
        grant: &'a [Grant],
    ) -> Fut<'a, AdminFaucet> {
        self.call(move |state| {
            state.faucet_enabled = enabled;
            state.faucet_cooldown = cooldown_seconds.max(0);
            state.faucet_payout = grant.to_vec();
            record(
                state,
                "faucet.update",
                "faucet",
                "cooldown_seconds",
                &cooldown_seconds.to_string(),
            );
            Ok(fixtures::admin_faucet(enabled, cooldown_seconds, grant))
        })
    }

    fn admin_grant<'a>(
        &'a self,
        user_id: &'a str,
        grants: &'a [Grant],
        reason: &'a str,
    ) -> Fut<'a, FaucetClaim> {
        self.call(move |state| {
            let height = state.height();
            record(state, "faucet.grant", user_id, "reason", reason);
            Ok(FaucetClaim {
                claim_id: format!("clm_{:04x}", height % 0xFFFF),
                tx_hash: format!("0x{:014x}", 0x008f_104b_6300_0000_u64 ^ height),
                granted: grants.to_vec(),
                next_available_at: rfc3339(now_seconds()),
            })
        })
    }

    fn admin_audit<'a>(&'a self, action: &'a str, limit: u32) -> Fut<'a, AuditResponse> {
        self.call(move |state| {
            let needle = action.trim().to_ascii_lowercase();
            Ok(AuditResponse {
                records: state
                    .audit
                    .iter()
                    .filter(|record| needle.is_empty() || record.action.contains(&needle))
                    .take(limit as usize)
                    .cloned()
                    .collect(),
                next_cursor: None,
            })
        })
    }

    fn admin_sim(&self) -> Fut<'_, SimStatus> {
        self.call(|state| Ok(state.sim.clone()))
    }

    fn admin_set_sim<'a>(&'a self, request: &'a SimRequest) -> Fut<'a, SimStatus> {
        self.call(move |state| {
            match request.action.as_str() {
                "start" => {
                    state.sim.mode = request.mode.unwrap_or(SimMode::On);
                    state.sim.running = true;
                }
                "stop" => {
                    state.sim.mode = SimMode::Off;
                    state.sim.running = false;
                }
                "mode" => {
                    let mode = request.mode.unwrap_or(SimMode::Off);
                    state.sim.mode = mode;
                    state.sim.running = mode != SimMode::Off;
                    record(state, "sim.control", "simulation", "mode", mode.as_str());
                }
                _ => {}
            }
            if let Some(scenario) = request.scenario {
                state.sim.scenario = scenario;
                record(state, "sim.scenario", scenario.as_str(), "seed", "42");
            }
            if let Some(seed) = request.seed {
                state.sim.seed = seed;
            }
            Ok(state.sim.clone())
        })
    }
}

/// Writes an audit record, as every mutating admin action must.
fn record(state: &mut MockState, action: &str, subject: &str, key: &str, value: &str) {
    let actor = state
        .session
        .as_ref()
        .map_or(CANONICAL_ADDRESS, |session| session.address.as_str())
        .to_string();
    state.audit.insert(
        0,
        AuditRecord {
            record_id: format!("aud_{:06x}", now_millis() as u64 % 0xFF_FFFF),
            actor,
            action: action.to_string(),
            subject: Some(subject.to_string()),
            detail: if value.is_empty() {
                std::collections::BTreeMap::new()
            } else {
                [(key.to_string(), value.to_string())].into_iter().collect()
            },
            at: rfc3339(now_seconds()),
        },
    );
}

/// Moves a market to a status, or explains why it does not exist.
fn set_status(state: &mut MockState, symbol: &str, status: MarketStatus) -> ApiResult<()> {
    let market = state
        .markets
        .iter_mut()
        .find(|market| market.symbol == symbol)
        .ok_or_else(|| reject(ErrorCode::NotFound, "no such market"))?;
    market.status = status;
    Ok(())
}
