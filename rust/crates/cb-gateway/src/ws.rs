//! The WebSocket surface: one upstream subscription, many browsers.
//!
//! ## Fan-out, not fan-in
//!
//! The gateway holds exactly **one** block subscription against the node
//! however many browsers are connected. Everything a client sees is derived
//! from that single stream and pushed out through [`Hub`]. Opening a node
//! subscription per browser would turn a hundred idle tabs into a hundred
//! streams the sequencer has to serve.
//!
//! ## A slow client is disconnected, never buffered
//!
//! Each connection has a bounded queue. If a client cannot keep up — a
//! backgrounded tab, a dead network that has not timed out yet — its queue
//! fills, and the connection is closed. The alternative is an unbounded queue,
//! which means one stalled browser can grow the server's memory until the
//! process dies, taking every other user with it. Dropping one connection is
//! the cheap failure.
//!
//! ## The private channel
//!
//! `account` carries balances, orders, positions and fills. It is delivered by
//! **user id**, resolved from the session at subscribe time, and there is no
//! code path that broadcasts an account message to more than its owner. The
//! test [`tests::one_users_account_stream_never_reaches_another_user`] is the
//! guard on that.

use crate::auth::AuthService;
use crate::error::ErrorCode;
use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// A channel a client can subscribe to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// `orderbook:{symbol}` — snapshot then deltas.
    OrderBook(String),
    /// `trades:{symbol}` — individual trades.
    Trades(String),
    /// `ticker` — all-market ticker updates.
    Ticker,
    /// `candles:{symbol}:{interval}` — the in-progress bar.
    Candles(String, String),
    /// `chain` — new block headers.
    Chain,
    /// `account` — the subscriber's own balances, orders, positions and fills.
    Account,
}

/// Longest channel name the gateway will look at.
///
/// Every legal name is `orderbook:<symbol>`, `candles:<symbol>:<interval>` or
/// a bare word, and symbols are short. A name longer than this is not a
/// subscription, it is a way to store the sender's bytes in our memory
/// (finding 5: 20,000 names of 500 characters on one unauthenticated socket).
pub const MAX_CHANNEL_NAME: usize = 64;

/// Most channels one connection may hold at once.
///
/// The frontend subscribes to a handful per open market — ticker, chain, a
/// book, trades and a candle interval — so this is roughly an order of
/// magnitude more than a real client needs, chosen so that no honest user ever
/// meets it while a socket can no longer hold an unbounded set.
pub const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 128;

impl Channel {
    /// Parse the contract's channel name.
    ///
    /// The symbol is required to be *well formed* as well as non-empty, so a
    /// five-hundred-character string is refused before anything stores it. It
    /// is deliberately **not** checked against the live market catalogue here:
    /// the gateway serves `/ws` before its first catalogue refresh (it starts
    /// without the sequencer by design), and a client that subscribes in that
    /// window should not be told its market does not exist. The bound that
    /// matters for memory is the length and the per-connection cap.
    pub fn parse(name: &str) -> Option<Self> {
        if name.len() > MAX_CHANNEL_NAME {
            return None;
        }
        let mut parts = name.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("ticker"), None, _, _) => Some(Self::Ticker),
            (Some("chain"), None, _, _) => Some(Self::Chain),
            (Some("account"), None, _, _) => Some(Self::Account),
            (Some("orderbook"), Some(symbol), None, _) if is_symbol(symbol) => {
                Some(Self::OrderBook(symbol.to_ascii_uppercase()))
            }
            (Some("trades"), Some(symbol), None, _) if is_symbol(symbol) => {
                Some(Self::Trades(symbol.to_ascii_uppercase()))
            }
            (Some("candles"), Some(symbol), Some(interval), None)
                if is_symbol(symbol) && cb_ports::CandleInterval::parse(interval).is_some() =>
            {
                Some(Self::Candles(
                    symbol.to_ascii_uppercase(),
                    interval.to_string(),
                ))
            }
            _ => None,
        }
    }

    /// The wire name.
    pub fn name(&self) -> String {
        match self {
            Self::OrderBook(symbol) => format!("orderbook:{symbol}"),
            Self::Trades(symbol) => format!("trades:{symbol}"),
            Self::Ticker => "ticker".into(),
            Self::Candles(symbol, interval) => format!("candles:{symbol}:{interval}"),
            Self::Chain => "chain".into(),
            Self::Account => "account".into(),
        }
    }

    /// Whether a session is required to subscribe.
    pub fn requires_auth(&self) -> bool {
        matches!(self, Self::Account)
    }

    /// The market symbol this channel is about, if any.
    pub fn symbol(&self) -> Option<&str> {
        match self {
            Self::OrderBook(symbol) | Self::Trades(symbol) | Self::Candles(symbol, _) => {
                Some(symbol)
            }
            _ => None,
        }
    }
}

/// Whether a channel's symbol component could name a market.
///
/// Shape only, not existence: `crate::symbols::is_well_formed` is the same
/// test the REST layer applies before it looks a symbol up.
fn is_symbol(symbol: &str) -> bool {
    !symbol.is_empty() && crate::symbols::is_well_formed(&symbol.to_ascii_uppercase())
}

/// One server-to-client message.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    /// The channel it belongs to.
    pub channel: String,
    /// The message type within that channel.
    #[serde(rename = "type")]
    pub kind: String,
    /// The payload.
    pub data: Value,
}

impl Envelope {
    /// Build an envelope.
    pub fn new(channel: impl Into<String>, kind: impl Into<String>, data: Value) -> Self {
        Self {
            channel: channel.into(),
            kind: kind.into(),
            data,
        }
    }

    /// A `system` error message, as the contract defines it.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(
            "system",
            "error",
            json!({ "code": code.as_str(), "message": message.into() }),
        )
    }

    /// The reply to a client `ping`.
    pub fn pong() -> Self {
        Self::new("system", "pong", Value::Null)
    }

    /// Render for the wire. Serialisation of these shapes cannot fail; a
    /// failure would mean a non-string map key, which none of them has.
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"channel":"system","type":"error","data":{"code":"INTERNAL","message":"encode failed"}}"#.into()
        })
    }
}

/// What a client can send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientOp {
    /// Present a session token.
    Auth(String),
    /// Subscribe to channels.
    Subscribe(Vec<String>),
    /// Unsubscribe from channels.
    Unsubscribe(Vec<String>),
    /// Liveness.
    Ping,
}

impl ClientOp {
    /// Parse a client frame. `Err` carries the message to send back.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("message is not JSON: {e}"))?;
        let op = value
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| "message needs an `op`".to_string())?;
        let channels = |value: &Value| -> Result<Vec<String>, String> {
            value
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(|| "`channels` must be an array".to_string())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        };
        match op {
            "auth" => value
                .get("token")
                .and_then(Value::as_str)
                .map(|t| Self::Auth(t.to_string()))
                .ok_or_else(|| "`auth` needs a `token`".to_string()),
            "subscribe" => Ok(Self::Subscribe(channels(&value)?)),
            "unsubscribe" => Ok(Self::Unsubscribe(channels(&value)?)),
            "ping" => Ok(Self::Ping),
            other => Err(format!("unknown op `{other}`")),
        }
    }
}

/// A connection's identifier within the hub.
pub type ConnectionId = u64;

struct Connection {
    sender: mpsc::Sender<String>,
    channels: Mutex<HashSet<Channel>>,
    /// Who this connection is, and which session says so.
    ///
    /// The session's *hash* is kept, never the token: it is what the store
    /// holds, it is enough to recheck the session, and a socket registry is not
    /// a place to keep credentials.
    session: Mutex<Option<Session>>,
}

/// The session a connection presented.
#[derive(Debug, Clone)]
struct Session {
    user_id: String,
    token_hash: String,
}

/// The fan-out registry.
pub struct Hub {
    connections: DashMap<ConnectionId, Arc<Connection>>,
    next_id: AtomicU64,
    buffer: usize,
    dropped_slow_clients: AtomicU64,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("connections", &self.connections.len())
            .field("buffer", &self.buffer)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub whose connections queue at most `buffer` messages.
    pub fn new(buffer: usize) -> Arc<Self> {
        Arc::new(Self {
            connections: DashMap::new(),
            next_id: AtomicU64::new(1),
            buffer: buffer.max(1),
            dropped_slow_clients: AtomicU64::new(0),
        })
    }

    /// Register a connection, returning its id and the queue to drain.
    pub fn register(&self) -> (ConnectionId, mpsc::Receiver<String>) {
        let (sender, receiver) = mpsc::channel(self.buffer);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.connections.insert(
            id,
            Arc::new(Connection {
                sender,
                channels: Mutex::new(HashSet::new()),
                session: Mutex::new(None),
            }),
        );
        (id, receiver)
    }

    /// Forget a connection.
    pub fn remove(&self, id: ConnectionId) {
        self.connections.remove(&id);
    }

    /// Attach an authenticated user, and the session that authenticated them,
    /// to a connection.
    pub fn authenticate(&self, id: ConnectionId, user_id: &str, token_hash: &str) {
        if let Some(connection) = self.connections.get(&id) {
            *connection.session.lock() = Some(Session {
                user_id: user_id.to_string(),
                token_hash: token_hash.to_string(),
            });
        }
    }

    /// Whether a connection has presented a session.
    pub fn user_of(&self, id: ConnectionId) -> Option<String> {
        self.connections
            .get(&id)
            .and_then(|c| c.session.lock().as_ref().map(|s| s.user_id.clone()))
    }

    /// The session hash a connection authenticated with, for rechecking it.
    pub fn session_hash_of(&self, id: ConnectionId) -> Option<String> {
        self.connections
            .get(&id)
            .and_then(|c| c.session.lock().as_ref().map(|s| s.token_hash.clone()))
    }

    /// Close every connection a session opened.
    ///
    /// Revoking a session that leaves its socket delivering balances, fills and
    /// positions is not revocation (§14 of the flow review). Logout, an
    /// individual revocation, "revoke all sessions" and an operator suspension
    /// all end here, so the channel the private data actually flows over stops
    /// with the credential that opened it.
    pub fn disconnect_session(&self, token_hash: &str) -> usize {
        self.disconnect(|session| session.token_hash == token_hash)
    }

    /// Close every connection belonging to a user, except one session.
    ///
    /// `keep` is the session making the request — "revoke all my other
    /// sessions" must not log the caller out of the tab they are sitting in.
    /// Pass `None` to close all of them, which is what a suspension does.
    pub fn disconnect_user(&self, user_id: &str, keep: Option<&str>) -> usize {
        self.disconnect(|session| {
            session.user_id == user_id && keep != Some(session.token_hash.as_str())
        })
    }

    fn disconnect(&self, mut wanted: impl FnMut(&Session) -> bool) -> usize {
        let targets: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|entry| entry.session.lock().as_ref().is_some_and(&mut wanted))
            .map(|entry| *entry.key())
            .collect();
        let goodbye = Envelope::error(
            ErrorCode::Unauthorized,
            "this session has ended; sign in again to resume the account channel",
        )
        .encode();
        for id in &targets {
            if let Some(connection) = self.connections.get(id) {
                // Best effort: a queue that is already full is about to be
                // dropped anyway, and the removal below is what matters.
                let _ = connection.sender.try_send(goodbye.clone());
            }
            // Dropping the connection drops its queue, which ends the writer
            // task, which closes the socket.
            self.connections.remove(id);
        }
        targets.len()
    }

    /// Add a subscription.
    ///
    /// Fails when the channel needs a session and the connection has not
    /// presented one, and when the connection is already holding
    /// [`MAX_SUBSCRIPTIONS_PER_CONNECTION`] channels. The cap is the memory
    /// bound: WebSocket reads are deliberately not rate limited, so without it
    /// one session-less socket can hold as many channels as it cares to name.
    pub fn subscribe(&self, id: ConnectionId, channel: Channel) -> Result<(), ErrorCode> {
        let Some(connection) = self.connections.get(&id) else {
            return Err(ErrorCode::NotFound);
        };
        if channel.requires_auth() && connection.session.lock().is_none() {
            return Err(ErrorCode::Unauthorized);
        }
        let mut channels = connection.channels.lock();
        if !channels.contains(&channel) && channels.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
            return Err(ErrorCode::RateLimited);
        }
        channels.insert(channel);
        Ok(())
    }

    /// Remove a subscription.
    pub fn unsubscribe(&self, id: ConnectionId, channel: &Channel) {
        if let Some(connection) = self.connections.get(&id) {
            connection.channels.lock().remove(channel);
        }
    }

    /// A connection's current subscriptions.
    pub fn subscriptions(&self, id: ConnectionId) -> Vec<Channel> {
        self.connections
            .get(&id)
            .map(|c| {
                let mut channels: Vec<Channel> = c.channels.lock().iter().cloned().collect();
                channels.sort();
                channels
            })
            .unwrap_or_default()
    }

    /// Send one message to one connection, dropping it if it cannot keep up.
    pub fn send(&self, id: ConnectionId, envelope: &Envelope) -> bool {
        let Some(connection) = self.connections.get(&id).map(|c| Arc::clone(c.value())) else {
            return false;
        };
        self.deliver(id, &connection, envelope.encode())
    }

    /// Publish to every connection subscribed to `channel`.
    ///
    /// Returns how many connections received it. The message is encoded once
    /// and the string cloned per connection, so a thousand subscribers cost
    /// one serialisation.
    pub fn publish(&self, channel: &Channel, envelope: &Envelope) -> usize {
        let encoded = envelope.encode();
        let targets: Vec<(ConnectionId, Arc<Connection>)> = self
            .connections
            .iter()
            .filter(|entry| entry.channels.lock().contains(channel))
            .map(|entry| (*entry.key(), Arc::clone(entry.value())))
            .collect();
        let mut delivered = 0;
        for (id, connection) in targets {
            if self.deliver(id, &connection, encoded.clone()) {
                delivered += 1;
            }
        }
        delivered
    }

    /// Publish to one user's `account` subscriptions, and nowhere else.
    ///
    /// The only path by which an account message reaches a socket. It matches
    /// on the connection's authenticated user id, so a connection that has not
    /// authenticated — or has authenticated as somebody else — is not a target
    /// and cannot be made one by subscribing.
    pub fn publish_to_user(&self, user_id: &str, envelope: &Envelope) -> usize {
        let encoded = envelope.encode();
        let targets: Vec<(ConnectionId, Arc<Connection>)> = self
            .connections
            .iter()
            .filter(|entry| {
                entry
                    .session
                    .lock()
                    .as_ref()
                    .is_some_and(|session| session.user_id == user_id)
            })
            .filter(|entry| entry.channels.lock().contains(&Channel::Account))
            .map(|entry| (*entry.key(), Arc::clone(entry.value())))
            .collect();
        let mut delivered = 0;
        for (id, connection) in targets {
            if self.deliver(id, &connection, encoded.clone()) {
                delivered += 1;
            }
        }
        delivered
    }

    /// Whether any connection is listening to a channel, so the gateway can
    /// skip work nobody has asked for.
    pub fn has_subscribers(&self, channel: &Channel) -> bool {
        self.connections
            .iter()
            .any(|entry| entry.channels.lock().contains(channel))
    }

    /// Every market symbol anybody is watching.
    pub fn watched_symbols(&self) -> Vec<String> {
        let mut symbols: Vec<String> = self
            .connections
            .iter()
            .flat_map(|entry| {
                entry
                    .channels
                    .lock()
                    .iter()
                    .filter_map(|c| c.symbol().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .collect();
        symbols.sort();
        symbols.dedup();
        symbols
    }

    /// Close every connection, for a graceful shutdown.
    pub fn close_all(&self) {
        let goodbye = Envelope::new(
            "system",
            "closing",
            json!({ "reason": "server shutting down" }),
        );
        let encoded = goodbye.encode();
        for entry in self.connections.iter() {
            let _ = entry.sender.try_send(encoded.clone());
        }
        self.connections.clear();
    }

    /// How many connections are open.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Whether anybody is connected.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// How many connections have been dropped for falling behind.
    pub fn dropped_slow_clients(&self) -> u64 {
        self.dropped_slow_clients.load(Ordering::Relaxed)
    }

    fn deliver(&self, id: ConnectionId, connection: &Connection, encoded: String) -> bool {
        match connection.sender.try_send(encoded) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // The client is not draining. Dropping it is the only bounded
                // option; the alternative grows this server's memory until it
                // takes every other user down with it.
                tracing::warn!(
                    target: "cb_gateway::ws",
                    connection = id,
                    "dropping a websocket client that fell behind"
                );
                self.dropped_slow_clients.fetch_add(1, Ordering::Relaxed);
                self.connections.remove(&id);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.connections.remove(&id);
                false
            }
        }
    }
}

/// How the connection loop is configured.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionConfig {
    /// How long a connection may stay silent before it is closed.
    pub idle_timeout: Duration,
    /// How often an authenticated connection rechecks its session.
    pub session_recheck: Duration,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            // The contract's ninety seconds.
            idle_timeout: Duration::from_secs(90),
            // A session is checked once at `auth` and then never again by the
            // socket itself, which is how an expiry mid-connection used to go
            // unnoticed for as long as the client kept pinging. Revocation is
            // pushed (see `Hub::disconnect_session`); this is the backstop that
            // catches expiry and anything that revoked a session without going
            // through the gateway.
            session_recheck: Duration::from_secs(30),
        }
    }
}

/// What a client operation asked the caller to do next.
///
/// The connection loop is split from the socket so the decision logic can be
/// tested without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    /// Nothing further.
    Handled,
    /// Send a fresh order-book snapshot for these symbols.
    SendSnapshots(Vec<String>),
    /// Close the connection.
    Close,
}

/// Apply one client operation to the hub.
///
/// Returns the envelopes to send back and what the caller should do next.
pub async fn apply_op(
    hub: &Hub,
    auth: &AuthService,
    id: ConnectionId,
    op: ClientOp,
) -> (Vec<Envelope>, OpOutcome) {
    match op {
        ClientOp::Ping => (vec![Envelope::pong()], OpOutcome::Handled),
        ClientOp::Auth(token) => match auth.authenticate(&token).await {
            Ok(user) => {
                hub.authenticate(id, &user.user_id, &crate::auth::session_token_hash(&token));
                (
                    vec![Envelope::new(
                        "system",
                        "authenticated",
                        json!({ "user_id": user.user_id, "handle": user.handle }),
                    )],
                    OpOutcome::Handled,
                )
            }
            Err(failure) => (
                vec![Envelope::error(ErrorCode::Unauthorized, failure.message())],
                OpOutcome::Handled,
            ),
        },
        ClientOp::Subscribe(names) => {
            let mut replies = Vec::new();
            let mut snapshots = Vec::new();
            for name in names {
                let Some(channel) = Channel::parse(&name) else {
                    replies.push(Envelope::error(
                        ErrorCode::ValidationFailed,
                        format!("unknown channel `{name}`"),
                    ));
                    continue;
                };
                match hub.subscribe(id, channel.clone()) {
                    Ok(()) => {
                        replies.push(Envelope::new(
                            channel.name(),
                            "subscribed",
                            json!({ "channel": channel.name() }),
                        ));
                        if let Channel::OrderBook(symbol) = &channel {
                            snapshots.push(symbol.clone());
                        }
                    }
                    Err(ErrorCode::Unauthorized) => replies.push(Envelope::error(
                        ErrorCode::Unauthorized,
                        format!("`{}` requires a session", channel.name()),
                    )),
                    Err(ErrorCode::RateLimited) => replies.push(Envelope::error(
                        ErrorCode::RateLimited,
                        format!(
                            "this connection already holds {MAX_SUBSCRIPTIONS_PER_CONNECTION} \
                             subscriptions; unsubscribe from something first"
                        ),
                    )),
                    Err(code) => replies.push(Envelope::error(code, "cannot subscribe")),
                }
            }
            let outcome = if snapshots.is_empty() {
                OpOutcome::Handled
            } else {
                OpOutcome::SendSnapshots(snapshots)
            };
            (replies, outcome)
        }
        ClientOp::Unsubscribe(names) => {
            let mut replies = Vec::new();
            for name in names {
                if let Some(channel) = Channel::parse(&name) {
                    hub.unsubscribe(id, &channel);
                    replies.push(Envelope::new(
                        channel.name(),
                        "unsubscribed",
                        json!({ "channel": channel.name() }),
                    ));
                }
            }
            (replies, OpOutcome::Handled)
        }
    }
}

/// Drive one WebSocket connection until it closes.
///
/// The outgoing queue is drained by a separate task, so a client that stops
/// reading blocks only its own queue — which then fills and gets it dropped —
/// rather than blocking the fan-out.
pub async fn run_connection(
    socket: WebSocket,
    hub: Arc<Hub>,
    auth: Arc<AuthService>,
    config: ConnectionConfig,
    on_snapshot: Arc<dyn Fn(ConnectionId, String) + Send + Sync>,
) {
    use futures::{SinkExt, StreamExt};

    let (id, mut outgoing) = hub.register();
    let span = tracing::info_span!("ws", connection = id);
    let _entered = span.enter();
    tracing::debug!(target: "cb_gateway::ws", "websocket connection opened");

    let (mut sink, mut stream) = socket.split();

    let writer = tokio::spawn(async move {
        while let Some(text) = outgoing.recv().await {
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut recheck = tokio::time::interval(config.session_recheck);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_activity = tokio::time::Instant::now();

    loop {
        // The idle deadline is measured from the last frame the client sent,
        // not from the last time this loop went round, so the session recheck
        // below cannot keep a silent connection alive past it.
        let idle_deadline = last_activity + config.idle_timeout;
        let message = tokio::select! {
            _ = recheck.tick() => {
                if let Some(hash) = hub.session_hash_of(id) {
                    if !auth.session_is_live(&hash).await {
                        tracing::info!(
                            target: "cb_gateway::ws",
                            connection = id,
                            "the session behind this connection is no longer valid; closing it"
                        );
                        hub.disconnect_session(&hash);
                        break;
                    }
                }
                continue;
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                // The contract: a connection that has not pinged for ninety
                // seconds is closed.
                tracing::debug!(target: "cb_gateway::ws", "closing an idle websocket");
                break;
            }
            next = stream.next() => match next {
                None => break,
                Some(Err(_)) => break,
                Some(Ok(message)) => message,
            },
        };
        last_activity = tokio::time::Instant::now();

        match message {
            Message::Text(text) => match ClientOp::parse(&text) {
                Ok(op) => {
                    let (replies, outcome) = apply_op(&hub, &auth, id, op).await;
                    for reply in replies {
                        if !hub.send(id, &reply) {
                            return finish(writer, hub, id).await;
                        }
                    }
                    match outcome {
                        OpOutcome::Close => break,
                        OpOutcome::SendSnapshots(symbols) => {
                            for symbol in symbols {
                                on_snapshot(id, symbol);
                            }
                        }
                        OpOutcome::Handled => {}
                    }
                }
                Err(message) => {
                    hub.send(id, &Envelope::error(ErrorCode::ValidationFailed, message));
                }
            },
            // A protocol-level ping counts as liveness, and axum answers it.
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) => {
                hub.send(
                    id,
                    &Envelope::error(
                        ErrorCode::ValidationFailed,
                        "binary frames are not accepted",
                    ),
                );
            }
            Message::Close(_) => break,
        }
    }

    finish(writer, hub, id).await;
}

/// How long the writer is given to flush what is already queued before the
/// connection is torn down.
///
/// The last thing in that queue is usually the frame that explains the close —
/// the `UNAUTHORIZED` a revoked session is sent, or the shutdown notice — and a
/// client that is closed on without being told is a client that reports the
/// server as flaky. Bounded, because a client that is not reading is exactly
/// the client this must not wait for.
const FLUSH_GRACE: Duration = Duration::from_millis(250);

async fn finish(writer: tokio::task::JoinHandle<()>, hub: Arc<Hub>, id: ConnectionId) {
    hub.remove(id);
    // Dropping the connection closed the queue, so the writer finishes on its
    // own once it has sent what was in it.
    let mut writer = writer;
    if tokio::time::timeout(FLUSH_GRACE, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
    tracing::debug!(target: "cb_gateway::ws", connection = id, "websocket connection closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Directory;
    use crate::memory::{InMemoryDatabase, InMemoryKv};
    use cb_ports::SystemClock;

    fn auth_service() -> (Arc<InMemoryDatabase>, Arc<AuthService>) {
        let db = InMemoryDatabase::shared();
        let directory = Arc::new(Directory::new(Arc::new(InMemoryKv::new())));
        let auth = Arc::new(
            AuthService::new(
                db.clone(),
                directory,
                Arc::new(SystemClock),
                chrono::Duration::hours(24),
                "ws-test-server-secret-77e1b3c40a92fd58",
            )
            .expect("the test secret clears the floor"),
        );
        (db, auth)
    }

    fn envelope() -> Envelope {
        Envelope::new("chain", "block", json!({ "height": 1 }))
    }

    #[test]
    fn channel_names_round_trip() {
        for name in [
            "ticker",
            "chain",
            "account",
            "orderbook:BTC-PERP",
            "trades:BTC-PERP",
            "candles:BTC-PERP:1m",
        ] {
            let channel = Channel::parse(name).unwrap_or_else(|| panic!("`{name}` should parse"));
            assert_eq!(channel.name(), name);
        }
    }

    #[test]
    fn symbols_are_upper_cased_so_one_market_is_one_channel() {
        assert_eq!(
            Channel::parse("orderbook:btc-perp"),
            Some(Channel::OrderBook("BTC-PERP".into()))
        );
    }

    #[test]
    fn nonsense_channels_are_rejected() {
        for name in [
            "",
            "orderbook",
            "orderbook:",
            "candles:BTC-PERP",
            "candles:BTC-PERP:3m",
            "candles:BTC-PERP:1m:extra",
            "ticker:BTC-PERP",
            "account:someone_else",
        ] {
            assert!(Channel::parse(name).is_none(), "`{name}` should not parse");
        }
    }

    #[test]
    fn a_junk_channel_name_is_refused_before_anything_stores_it() {
        // Finding 5: 20,000 names of 500 characters were accepted on a
        // session-less socket. A name that long is not a subscription.
        let long_symbol = "Z".repeat(500);
        for name in [
            format!("trades:{long_symbol}"),
            format!("orderbook:{long_symbol}"),
            format!("candles:{long_symbol}:1m"),
            // Well under the length cap, but not shaped like a market.
            "trades:ZZZZ".to_string(),
            "orderbook:not a symbol".to_string(),
            "trades:btc perp".to_string(),
        ] {
            assert!(
                Channel::parse(&name).is_none(),
                "`{}…` should not parse",
                &name[..name.len().min(24)]
            );
        }
        // The boundary itself, so the length cap is the thing under test and
        // not the shape check.
        let base = "A".repeat(MAX_CHANNEL_NAME - "trades:".len() - "-PERP".len());
        let at_cap = format!("trades:{base}-PERP");
        assert_eq!(at_cap.len(), MAX_CHANNEL_NAME);
        assert!(Channel::parse(&at_cap).is_some(), "the cap itself is legal");
        let over_cap = format!("trades:A{base}-PERP");
        assert_eq!(over_cap.len(), MAX_CHANNEL_NAME + 1);
        assert!(Channel::parse(&over_cap).is_none(), "one byte over is not");
    }

    #[tokio::test]
    async fn one_connection_cannot_hold_unbounded_subscriptions() {
        let hub = Hub::new(8);
        let (id, _rx) = hub.register();
        // Distinct, well-formed, unauthenticated channels.
        let mut accepted = 0;
        let mut refused = None;
        for i in 0..(MAX_SUBSCRIPTIONS_PER_CONNECTION + 50) {
            let symbol = format!("A{i}-PERP");
            match hub.subscribe(id, Channel::Trades(symbol)) {
                Ok(()) => accepted += 1,
                Err(code) => {
                    refused = Some(code);
                    break;
                }
            }
        }
        assert_eq!(accepted, MAX_SUBSCRIPTIONS_PER_CONNECTION);
        assert_eq!(refused, Some(ErrorCode::RateLimited));
        assert_eq!(
            hub.subscriptions(id).len(),
            MAX_SUBSCRIPTIONS_PER_CONNECTION
        );

        // Re-subscribing to something already held is not a new subscription,
        // so a client that repeats itself is not punished for it.
        assert!(hub.subscribe(id, Channel::Trades("A0-PERP".into())).is_ok());

        // And a slot freed by unsubscribing is usable again.
        hub.unsubscribe(id, &Channel::Trades("A0-PERP".into()));
        assert!(hub.subscribe(id, Channel::Ticker).is_ok());
    }

    #[test]
    fn only_the_account_channel_needs_a_session() {
        assert!(Channel::Account.requires_auth());
        assert!(!Channel::Ticker.requires_auth());
        assert!(!Channel::OrderBook("BTC-PERP".into()).requires_auth());
    }

    #[test]
    fn client_frames_parse_to_operations() {
        assert_eq!(
            ClientOp::parse(r#"{"op":"auth","token":"sess_x"}"#).expect("auth"),
            ClientOp::Auth("sess_x".into())
        );
        assert_eq!(
            ClientOp::parse(r#"{"op":"subscribe","channels":["ticker","chain"]}"#).expect("sub"),
            ClientOp::Subscribe(vec!["ticker".into(), "chain".into()])
        );
        assert_eq!(
            ClientOp::parse(r#"{"op":"ping"}"#).expect("ping"),
            ClientOp::Ping
        );
        assert!(ClientOp::parse("not json").is_err());
        assert!(ClientOp::parse(r#"{"op":"fly"}"#).is_err());
        assert!(ClientOp::parse(r#"{"op":"subscribe"}"#).is_err());
        assert!(ClientOp::parse(r#"{"op":"auth"}"#).is_err());
    }

    #[test]
    fn the_envelope_carries_channel_and_type() {
        let encoded = envelope().encode();
        let value: Value = serde_json::from_str(&encoded).expect("valid JSON");
        assert_eq!(value["channel"], "chain");
        assert_eq!(value["type"], "block");
        assert_eq!(value["data"]["height"], 1);
    }

    #[test]
    fn an_error_uses_the_system_channel_and_a_contract_code() {
        let value: Value =
            serde_json::from_str(&Envelope::error(ErrorCode::Unauthorized, "no").encode())
                .expect("valid JSON");
        assert_eq!(value["channel"], "system");
        assert_eq!(value["type"], "error");
        assert_eq!(value["data"]["code"], "UNAUTHORIZED");
    }

    #[test]
    fn a_pong_answers_a_ping() {
        let value: Value = serde_json::from_str(&Envelope::pong().encode()).expect("valid JSON");
        assert_eq!(value["channel"], "system");
        assert_eq!(value["type"], "pong");
    }

    #[tokio::test]
    async fn a_message_reaches_only_the_connections_that_subscribed() {
        let hub = Hub::new(16);
        let (subscriber, mut subscriber_rx) = hub.register();
        let (bystander, mut bystander_rx) = hub.register();
        hub.subscribe(subscriber, Channel::Chain)
            .expect("subscribe");

        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 1);
        assert!(subscriber_rx.try_recv().is_ok());
        assert!(bystander_rx.try_recv().is_err());
        let _ = bystander;
    }

    #[tokio::test]
    async fn unsubscribing_stops_delivery() {
        let hub = Hub::new(16);
        let (id, mut rx) = hub.register();
        hub.subscribe(id, Channel::Chain).expect("subscribe");
        hub.publish(&Channel::Chain, &envelope());
        assert!(rx.try_recv().is_ok());

        hub.unsubscribe(id, &Channel::Chain);
        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_account_channel_cannot_be_subscribed_without_a_session() {
        let hub = Hub::new(16);
        let (id, _rx) = hub.register();
        assert_eq!(
            hub.subscribe(id, Channel::Account),
            Err(ErrorCode::Unauthorized)
        );
        hub.authenticate(id, "usr_a", "hash_a");
        assert!(hub.subscribe(id, Channel::Account).is_ok());
    }

    #[tokio::test]
    async fn one_users_account_stream_never_reaches_another_user() {
        // The guard on the private channel. Both connections are
        // authenticated and both subscribed to `account`; a fill for one must
        // not appear on the other.
        let hub = Hub::new(16);
        let (alice, mut alice_rx) = hub.register();
        let (bob, mut bob_rx) = hub.register();
        hub.authenticate(alice, "usr_alice", "hash_alice");
        hub.authenticate(bob, "usr_bob", "hash_bob");
        hub.subscribe(alice, Channel::Account).expect("alice");
        hub.subscribe(bob, Channel::Account).expect("bob");

        let fill = Envelope::new("account", "fill", json!({ "qty": "1", "who": "alice" }));
        assert_eq!(hub.publish_to_user("usr_alice", &fill), 1);

        let delivered = alice_rx.try_recv().expect("alice receives her own fill");
        assert!(delivered.contains("alice"));
        assert!(
            bob_rx.try_recv().is_err(),
            "bob must never see alice's fill"
        );
    }

    #[tokio::test]
    async fn ending_a_session_ends_the_socket_it_opened() {
        // §14 of the flow review. `user_id` was written into the connection at
        // `auth` and never cleared: logout, an individual revocation, "revoke
        // all sessions" and an operator suspension all left the socket in
        // place, and it went on delivering balances, fills and positions for as
        // long as the client kept pinging. Revocation that does not close the
        // channel the private data flows over is not revocation.
        let hub = Hub::new(16);
        let (tab_a, mut a_rx) = hub.register();
        let (tab_b, mut b_rx) = hub.register();
        hub.authenticate(tab_a, "usr_alice", "hash_one");
        hub.authenticate(tab_b, "usr_alice", "hash_two");
        hub.subscribe(tab_a, Channel::Account).expect("tab a");
        hub.subscribe(tab_b, Channel::Account).expect("tab b");
        assert_eq!(hub.publish_to_user("usr_alice", &envelope()), 2);
        while a_rx.try_recv().is_ok() {}
        while b_rx.try_recv().is_ok() {}

        // The first tab's session is revoked.
        assert_eq!(hub.disconnect_session("hash_one"), 1);
        let goodbye = a_rx.try_recv().expect("the tab is told why");
        assert!(goodbye.contains("UNAUTHORIZED"), "got {goodbye}");
        assert_eq!(
            hub.publish_to_user("usr_alice", &envelope()),
            1,
            "only the session that is still valid receives anything"
        );
        assert!(hub.user_of(tab_a).is_none(), "the connection is gone");

        // "Revoke all my other sessions" keeps the one making the request.
        let (tab_c, _c_rx) = hub.register();
        hub.authenticate(tab_c, "usr_alice", "hash_three");
        hub.subscribe(tab_c, Channel::Account).expect("tab c");
        assert_eq!(hub.disconnect_user("usr_alice", Some("hash_two")), 1);
        assert_eq!(hub.publish_to_user("usr_alice", &envelope()), 1);
        assert_eq!(hub.user_of(tab_b).as_deref(), Some("usr_alice"));
    }

    #[tokio::test]
    async fn an_unauthenticated_connection_receives_no_account_messages() {
        let hub = Hub::new(16);
        let (id, mut rx) = hub.register();
        // Not authenticated, so the subscription is refused...
        assert!(hub.subscribe(id, Channel::Account).is_err());
        // ...and a publish addressed at a user finds no target here.
        assert_eq!(hub.publish_to_user("usr_alice", &envelope()), 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_disconnected_rather_than_buffered() {
        let hub = Hub::new(2);
        let (id, _rx) = hub.register();
        hub.subscribe(id, Channel::Chain).expect("subscribe");

        // Fill the queue — the receiver is never drained.
        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 1);
        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 1);
        // The third message has nowhere to go, so the connection goes.
        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 0);
        assert_eq!(hub.len(), 0, "the slow client was dropped");
        assert_eq!(hub.dropped_slow_clients(), 1);
    }

    #[tokio::test]
    async fn dropping_a_slow_client_does_not_affect_the_others() {
        let hub = Hub::new(2);
        let (slow, _slow_rx) = hub.register();
        let (fast, mut fast_rx) = hub.register();
        hub.subscribe(slow, Channel::Chain).expect("slow");
        hub.subscribe(fast, Channel::Chain).expect("fast");

        for _ in 0..5 {
            hub.publish(&Channel::Chain, &envelope());
            while fast_rx.try_recv().is_ok() {}
        }
        assert_eq!(hub.len(), 1, "only the slow client was dropped");
        assert!(hub.subscriptions(fast).contains(&Channel::Chain));
    }

    #[tokio::test]
    async fn a_closed_connection_is_reaped_on_the_next_publish() {
        let hub = Hub::new(4);
        let (id, rx) = hub.register();
        hub.subscribe(id, Channel::Chain).expect("subscribe");
        drop(rx);
        assert_eq!(hub.publish(&Channel::Chain, &envelope()), 0);
        assert!(hub.is_empty());
    }

    #[tokio::test]
    async fn subscribing_to_a_book_asks_for_a_snapshot() {
        let hub = Hub::new(16);
        let (_db, auth) = auth_service();
        let (id, mut rx) = hub.register();
        let (replies, outcome) = apply_op(
            &hub,
            &auth,
            id,
            ClientOp::Subscribe(vec!["orderbook:BTC-PERP".into()]),
        )
        .await;
        assert_eq!(replies.len(), 1);
        assert_eq!(
            outcome,
            OpOutcome::SendSnapshots(vec!["BTC-PERP".into()]),
            "every subscribe gets a fresh snapshot before any delta"
        );
        for reply in &replies {
            hub.send(id, reply);
        }
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn subscribing_to_account_without_a_session_returns_an_error_frame() {
        let hub = Hub::new(16);
        let (_db, auth) = auth_service();
        let (id, _rx) = hub.register();
        let (replies, outcome) =
            apply_op(&hub, &auth, id, ClientOp::Subscribe(vec!["account".into()])).await;
        assert_eq!(outcome, OpOutcome::Handled);
        let value: Value = serde_json::from_str(&replies[0].encode()).expect("JSON");
        assert_eq!(value["data"]["code"], "UNAUTHORIZED");
        assert!(hub.subscriptions(id).is_empty());
    }

    #[tokio::test]
    async fn authenticating_over_the_socket_unlocks_the_private_channel() {
        let hub = Hub::new(16);
        let (_db, auth) = auth_service();
        let registered = auth
            .dev_login(
                "abandon abandon abandon abandon abandon abandon abandon abandon \
                 abandon abandon abandon about",
                0,
                Some("satoshi"),
            )
            .await
            .expect("login");
        let (id, _rx) = hub.register();

        let (replies, _) = apply_op(
            &hub,
            &auth,
            id,
            ClientOp::Auth(registered.session_token.clone()),
        )
        .await;
        let value: Value = serde_json::from_str(&replies[0].encode()).expect("JSON");
        assert_eq!(value["type"], "authenticated");
        assert_eq!(
            hub.user_of(id).as_deref(),
            Some(registered.user.user_id.as_str())
        );

        let (replies, _) =
            apply_op(&hub, &auth, id, ClientOp::Subscribe(vec!["account".into()])).await;
        let value: Value = serde_json::from_str(&replies[0].encode()).expect("JSON");
        assert_eq!(value["type"], "subscribed");
    }

    #[tokio::test]
    async fn a_bad_token_over_the_socket_is_refused_without_closing() {
        let hub = Hub::new(16);
        let (_db, auth) = auth_service();
        let (id, _rx) = hub.register();
        let (replies, outcome) =
            apply_op(&hub, &auth, id, ClientOp::Auth("sess_nope".into())).await;
        assert_eq!(outcome, OpOutcome::Handled);
        let value: Value = serde_json::from_str(&replies[0].encode()).expect("JSON");
        assert_eq!(value["data"]["code"], "UNAUTHORIZED");
        assert!(hub.user_of(id).is_none());
    }

    #[tokio::test]
    async fn an_unknown_channel_is_reported_and_the_rest_still_subscribe() {
        let hub = Hub::new(16);
        let (_db, auth) = auth_service();
        let (id, _rx) = hub.register();
        let (replies, _) = apply_op(
            &hub,
            &auth,
            id,
            ClientOp::Subscribe(vec!["nonsense".into(), "chain".into()]),
        )
        .await;
        assert_eq!(replies.len(), 2);
        assert!(hub.subscriptions(id).contains(&Channel::Chain));
    }

    #[tokio::test]
    async fn the_hub_reports_which_symbols_anyone_is_watching() {
        let hub = Hub::new(16);
        let (a, _a_rx) = hub.register();
        let (b, _b_rx) = hub.register();
        hub.subscribe(a, Channel::OrderBook("BTC-PERP".into()))
            .expect("a");
        hub.subscribe(b, Channel::Trades("BTC-PERP".into()))
            .expect("b");
        hub.subscribe(b, Channel::Candles("ETH-PERP".into(), "1m".into()))
            .expect("b");
        assert_eq!(hub.watched_symbols(), vec!["BTC-PERP", "ETH-PERP"]);
        assert!(hub.has_subscribers(&Channel::OrderBook("BTC-PERP".into())));
        assert!(!hub.has_subscribers(&Channel::Ticker));
    }

    #[tokio::test]
    async fn closing_the_hub_drains_every_connection() {
        let hub = Hub::new(4);
        let (_id, mut rx) = hub.register();
        hub.close_all();
        let message = rx.try_recv().expect("a goodbye was queued");
        assert!(message.contains("closing"));
        assert!(hub.is_empty());
    }
}
