//! The market-data feed: one socket, kept up.
//!
//! The design system treats the connection as a first-class piece of
//! information, not an implementation detail. Four of its nine states live here:
//! *disconnected*, *reconnecting*, the *Resynced* badge that follows a
//! reconnection, and — indirectly — *stale price*, which is simply what happens
//! to a price when this module stops delivering updates.
//!
//! The rules the driver below implements, in the order they matter:
//!
//! 1. **On every connect and every reconnect, mark every book as awaiting a
//!    snapshot, then resubscribe.** The gateway answers a subscription with a
//!    fresh snapshot, so the book is replaced wholesale rather than patched. Any
//!    delta that arrives in the window before that snapshot is dropped by
//!    [`book::OrderBook`], which is the one rule that keeps a reconnected book
//!    from quietly containing levels that never existed.
//! 2. **Never hide the failure.** A dropped socket raises an event; it does not
//!    retry silently and it does not clear the screen. The last known book stays
//!    up, visibly frozen, because a blank panel and a stale panel look identical
//!    to someone glancing at it and only one of them is honest.
//! 3. **Back off, with jitter.** See [`backoff`].
//!
//! The socket itself is behind the [`Transport`] trait for the same reason the
//! gateway is behind [`crate::api::Api`]: the mock feed drives the entire
//! application, including the reconnection path, without a server.

pub mod backoff;
pub mod book;
pub mod mockfeed;
pub mod msg;
pub mod transport;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::stream::LocalBoxStream;
use futures::{select, SinkExt, StreamExt};
use gloo_timers::future::{IntervalStream, TimeoutFuture};
use wasm_bindgen_futures::spawn_local;

use crate::clock::now_millis;

pub use backoff::Backoff;
pub use book::{depth_pct, BookRow, DeltaOutcome, OrderBook};
pub use msg::{ClientMessage, ServerMessage};

/// How often to ping. The gateway closes a connection silent for 90 seconds.
///
/// Public because the connection indicator's staleness threshold is derived
/// from it rather than picked: on an idle chain the pong is the only inbound
/// frame there is, so "how long is too long" is a fact about this number.
pub const PING_INTERVAL_MS: u32 = 30_000;

/// How long without an inbound frame before the connection is treated as dead.
const SILENCE_TIMEOUT_MS: i64 = 90_000;

/// How often the silence above is actually checked.
///
/// It used to be checked only on the ping tick, so a gateway that stopped
/// answering was not noticed until as late as a hundred and twenty seconds —
/// and a reviewer watching the top bar at a hundred and ten saw it still
/// reporting a live feed.
const WATCHDOG_INTERVAL_MS: u32 = 5_000;

/// How long a connection must survive before its backoff schedule is cleared.
///
/// Without this, a gateway that accepts a socket and immediately closes it would
/// be hammered at the base delay forever, because every attempt would look like
/// a success.
const STABLE_AFTER_MS: i64 = 3_000;

/// One open connection: a stream of text frames in, a sink of text frames out.
pub struct Socket {
    /// Frames from the gateway. The stream ending means the socket closed.
    pub incoming: LocalBoxStream<'static, String>,
    /// Frames to the gateway.
    pub outgoing: mpsc::UnboundedSender<String>,
}

/// Something that can open a connection.
pub trait Transport: Send + Sync {
    /// Opens one connection, or explains why it could not.
    fn connect(&self) -> LocalBoxFuture<'static, Result<Socket, String>>;
}

/// Where the connection is, as the top-bar indicator reports it.
///
/// Every variant carries a figure as well as a word, because the design system
/// forbids the bare dot: a status with no number tells the user nothing about
/// whether to wait or to stop trading.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// The first connection has not completed yet.
    #[default]
    Connecting,
    /// Connected and receiving. Carries the moment it came up.
    Live {
        /// Wall-clock milliseconds at which the connection was established.
        since_ms: i64,
    },
    /// Dropped, and retrying. Carries the attempt number the label shows.
    Reconnecting {
        /// Which attempt is in flight, counting from one.
        attempt: u32,
        /// Wall-clock milliseconds at which the connection dropped.
        dropped_ms: i64,
    },
    /// Given up for now, or never connected.
    Down {
        /// Wall-clock milliseconds at which the connection dropped.
        dropped_ms: i64,
    },
}

impl ConnectionState {
    /// The `data-state` attribute the lifted CSS keys the indicator off.
    #[must_use]
    pub const fn attr(&self) -> &'static str {
        match self {
            Self::Live { .. } => "live",
            Self::Connecting | Self::Reconnecting { .. } => "reconnecting",
            Self::Down { .. } => "down",
        }
    }

    /// True when market data is arriving and a price may be trusted.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// Something the application asks of the feed.
#[derive(Debug, Clone)]
pub enum FeedCommand {
    /// Add channels to the subscription set and subscribe to them now.
    Subscribe(Vec<String>),
    /// Drop channels from the subscription set.
    Unsubscribe(Vec<String>),
    /// Install or clear the session token used for the private `account` channel.
    Auth(Option<String>),
    /// Ask for fresh snapshots on everything currently subscribed.
    Resubscribe,
    /// Close the socket now. Used to demonstrate the reconnection path.
    Drop,
}

/// Something the feed reports.
pub enum FeedEvent {
    /// A connection came up. `resynced` is true for a reconnection, which is
    /// what raises the *Resynced* badge.
    Connected {
        /// True when this is a reconnection rather than the first connection.
        resynced: bool,
    },
    /// The connection went away.
    Disconnected {
        /// What happened, in the transport's words.
        reason: String,
    },
    /// A retry is scheduled.
    Reconnecting {
        /// Which attempt is in flight, counting from one.
        attempt: u32,
        /// How long until it is made.
        delay_ms: u32,
    },
    /// A frame arrived.
    Message(ServerMessage),
}

/// A handle on a running feed.
#[derive(Clone)]
pub struct FeedHandle {
    commands: mpsc::UnboundedSender<FeedCommand>,
}

impl FeedHandle {
    /// Sends a command, ignoring the case where the driver has already stopped.
    pub fn send(&self, command: FeedCommand) {
        let mut sender = self.commands.clone();
        spawn_local(async move {
            let _ = sender.send(command).await;
        });
    }

    /// Subscribes to channels, which also asks for their snapshots.
    pub fn subscribe(&self, channels: Vec<String>) {
        self.send(FeedCommand::Subscribe(channels));
    }

    /// Stops receiving channels.
    pub fn unsubscribe(&self, channels: Vec<String>) {
        self.send(FeedCommand::Unsubscribe(channels));
    }

    /// Installs or clears the session token.
    pub fn authenticate(&self, token: Option<String>) {
        self.send(FeedCommand::Auth(token));
    }

    /// Closes the socket, exercising the real reconnection path.
    pub fn drop_connection(&self) {
        self.send(FeedCommand::Drop);
    }
}

/// A caller-supplied source of jitter, in permille.
///
/// Passed in rather than drawn here so the backoff schedule stays a pure
/// function that a test can assert against.
fn jitter_permille() -> u32 {
    (now_millis().unsigned_abs() % 1_000) as u32
}

/// Starts the feed driver and returns a handle on it.
///
/// `on_event` runs on the browser's main task for every lifecycle change and
/// every frame; it is where the reactive stores are updated.
pub fn start(
    transport: Arc<dyn Transport>,
    token: Option<String>,
    initial_channels: Vec<String>,
    on_event: impl Fn(FeedEvent) + 'static,
) -> FeedHandle {
    let (command_tx, mut command_rx) = mpsc::unbounded::<FeedCommand>();
    let handle = FeedHandle {
        commands: command_tx,
    };

    let channels: Rc<RefCell<BTreeSet<String>>> =
        Rc::new(RefCell::new(initial_channels.into_iter().collect()));
    let token = Rc::new(RefCell::new(token));
    let on_event = Rc::new(on_event);

    spawn_local(async move {
        let mut backoff = Backoff::new();
        let mut first_connection = true;

        loop {
            let socket = match transport.connect().await {
                Ok(socket) => socket,
                Err(reason) => {
                    on_event(FeedEvent::Disconnected {
                        reason: reason.clone(),
                    });
                    let delay = backoff.next_delay_ms(jitter_permille());
                    on_event(FeedEvent::Reconnecting {
                        attempt: backoff.attempt(),
                        delay_ms: delay,
                    });
                    TimeoutFuture::new(delay).await;
                    continue;
                }
            };

            let opened_ms = now_millis();
            let mut last_inbound_ms = opened_ms;
            let mut stabilised = false;

            // Rule 1. The consumer is told the connection is up *before* any
            // frame arrives, so it can mark its books as awaiting a snapshot.
            // Then, and only then, the subscriptions are sent.
            on_event(FeedEvent::Connected {
                resynced: !first_connection,
            });
            first_connection = false;

            let mut outgoing = socket.outgoing.clone();
            // Both borrows end before the `await`: a `RefCell` held across a
            // suspension point is a panic waiting for the day two tasks
            // interleave.
            let stored_token = token.borrow().clone();
            if let Some(token) = stored_token {
                send(&mut outgoing, &ClientMessage::Auth { token }).await;
            }
            let wanted: Vec<String> = {
                let held = channels.borrow();
                held.iter().cloned().collect()
            };
            if !wanted.is_empty() {
                send(
                    &mut outgoing,
                    &ClientMessage::Subscribe { channels: wanted },
                )
                .await;
            }

            let mut incoming = socket.incoming.fuse();
            let mut pings = IntervalStream::new(PING_INTERVAL_MS).fuse();
            let mut watchdog = IntervalStream::new(WATCHDOG_INTERVAL_MS).fuse();
            let reason = loop {
                select! {
                    frame = incoming.next() => match frame {
                        Some(text) => {
                            last_inbound_ms = now_millis();
                            if !stabilised && last_inbound_ms - opened_ms > STABLE_AFTER_MS {
                                stabilised = true;
                                backoff.reset();
                            }
                            on_event(FeedEvent::Message(ServerMessage::parse(&text)));
                        }
                        None => break "the gateway closed the connection".to_string(),
                    },
                    command = command_rx.next() => match command {
                        Some(FeedCommand::Subscribe(new)) => {
                            {
                                let mut set = channels.borrow_mut();
                                for channel in &new { set.insert(channel.clone()); }
                            }
                            send(&mut outgoing, &ClientMessage::Subscribe { channels: new }).await;
                        }
                        Some(FeedCommand::Unsubscribe(gone)) => {
                            {
                                let mut set = channels.borrow_mut();
                                for channel in &gone { set.remove(channel); }
                            }
                            send(&mut outgoing, &ClientMessage::Unsubscribe { channels: gone }).await;
                        }
                        Some(FeedCommand::Auth(new_token)) => {
                            token.replace(new_token.clone());
                            if let Some(new_token) = new_token {
                                send(&mut outgoing, &ClientMessage::Auth { token: new_token }).await;
                            }
                        }
                        Some(FeedCommand::Resubscribe) => {
                            let wanted: Vec<String> = {
                                let held = channels.borrow();
                                held.iter().cloned().collect()
                            };
                            send(&mut outgoing, &ClientMessage::Subscribe { channels: wanted }).await;
                        }
                        Some(FeedCommand::Drop) => break "the connection was dropped".to_string(),
                        None => return,
                    },
                    _ = pings.next() => {
                        send(&mut outgoing, &ClientMessage::Ping).await;
                    },
                    _ = watchdog.next() => {
                        if now_millis() - last_inbound_ms > SILENCE_TIMEOUT_MS {
                            break "the gateway stopped answering".to_string();
                        }
                    }
                }
            };

            on_event(FeedEvent::Disconnected {
                reason: reason.clone(),
            });
            if !stabilised && now_millis() - opened_ms > STABLE_AFTER_MS {
                backoff.reset();
            }
            let delay = backoff.next_delay_ms(jitter_permille());
            on_event(FeedEvent::Reconnecting {
                attempt: backoff.attempt(),
                delay_ms: delay,
            });
            TimeoutFuture::new(delay).await;
        }
    });

    handle
}

/// Serialises and sends a command, ignoring a closed socket.
///
/// A send that fails means the socket is already gone; the incoming stream is
/// about to end and the driver will reconnect. Reporting it twice would show the
/// user two failures where there was one.
async fn send(sink: &mut mpsc::UnboundedSender<String>, message: &ClientMessage) {
    if let Ok(text) = serde_json::to_string(message) {
        let _ = sink.send(text).await;
    }
}
