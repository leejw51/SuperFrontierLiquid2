//! The mock feed: a live exchange with no server behind it.
//!
//! This is the counterpart to [`crate::api::mock::MockApi`], and it shares that
//! type's state, so a limit order placed through the mock gateway really does
//! come back down this socket as an `order_update` a few hundred milliseconds
//! later. Blocks arrive every hundred milliseconds, prices walk, the tape fills
//! and the book is patched with genuine deltas — including removals at quantity
//! zero, which is the clause most easily got wrong and the one that has a test.
//!
//! It also implements failure. [`MockTransport::simulate_outage`] makes
//! `connect` refuse for a stretch of seconds, which drives the real reconnection
//! path in [`crate::ws`]: the indicator turns brass and counts attempts, prices
//! go stale because nothing is arriving, the frozen book stays on screen, and on
//! the way back a fresh snapshot replaces it wholesale and raises *Resynced*.
//! Nothing about that path is special-cased for the mock; it is the same code
//! the live gateway will exercise.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{select, FutureExt, SinkExt, StreamExt};
use gloo_timers::future::IntervalStream;
use rust_decimal::Decimal;
use wasm_bindgen_futures::spawn_local;

use crate::api::fixtures::{self, Rng};
use crate::api::mock::MockState;
use crate::api::types::{Level, OrderBookPayload, Side};
use crate::clock::{now_millis, now_seconds, rfc3339};

use super::msg::ClientMessage;
use super::{Socket, Transport};

/// The mock chain's block interval, which is also the generator's tick.
const TICK_MS: u32 = 100;

/// A transport that generates a plausible exchange instead of dialling one.
pub struct MockTransport {
    state: Arc<Mutex<MockState>>,
    unavailable_until_ms: Arc<Mutex<i64>>,
    stalled_until_ms: Arc<Mutex<i64>>,
}

impl MockTransport {
    /// Creates a transport over the same state the mock gateway serves from.
    #[must_use]
    pub fn new(state: Arc<Mutex<MockState>>) -> Self {
        Self {
            state,
            unavailable_until_ms: Arc::new(Mutex::new(0)),
            stalled_until_ms: Arc::new(Mutex::new(0)),
        }
    }

    /// Refuses connections for `duration_ms`, to exercise the reconnection path.
    ///
    /// This is the only piece of instrumentation in the application, it is
    /// reachable only while the mock backend is selected, and it does not fake
    /// the states it demonstrates: the socket really closes, the backoff really
    /// runs, and the book really is replaced by a fresh snapshot afterwards.
    pub fn simulate_outage(&self, duration_ms: i64) {
        if let Ok(mut until) = self.unavailable_until_ms.lock() {
            *until = now_millis() + duration_ms;
        }
    }

    /// True while a simulated outage is in force.
    #[must_use]
    pub fn is_out(&self) -> bool {
        self.unavailable_until_ms
            .lock()
            .map(|until| *until > now_millis())
            .unwrap_or(false)
    }

    /// Stops sending frames without closing the socket.
    ///
    /// The commoner real failure, and the one the mock could not produce: a
    /// gateway that stops answering rather than hanging up. A clean `close` is
    /// handled well and always was; this is the case that left the top bar
    /// reporting a live feed against a process that had stopped running, and
    /// it was invisible in development because nothing here could cause it.
    pub fn simulate_stall(&self, duration_ms: i64) {
        if let Ok(mut until) = self.stalled_until_ms.lock() {
            *until = now_millis() + duration_ms;
        }
    }
}

/// Wraps a payload in the contract's `{channel, type, data}` envelope.
fn frame(channel: &str, kind: &str, data: serde_json::Value) -> String {
    serde_json::json!({ "channel": channel, "type": kind, "data": data }).to_string()
}

impl Transport for MockTransport {
    fn connect(&self) -> LocalBoxFuture<'static, Result<Socket, String>> {
        if self.is_out() {
            return futures::future::ready(Err(
                "the simulated gateway is not answering".to_string()
            ))
            .boxed_local();
        }

        let state = Arc::clone(&self.state);
        let (incoming_tx, incoming_rx) = mpsc::unbounded::<String>();
        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded::<String>();
        let unavailable = Arc::clone(&self.unavailable_until_ms);
        let stalled = Arc::clone(&self.stalled_until_ms);

        spawn_local(async move {
            let mut books: Vec<(String, OrderBookPayload)> = Vec::new();
            let mut subscribed: BTreeSet<String> = BTreeSet::new();
            let mut rng = Rng::new(0xFEED_5EED);
            let mut ticks = IntervalStream::new(TICK_MS).fuse();
            let mut sink = incoming_tx;
            let mut counter: u64 = 0;

            /// True while a stall has been asked for.
            macro_rules! is_stalled {
                () => {
                    stalled
                        .lock()
                        .map(|until| *until > now_millis())
                        .unwrap_or(false)
                };
            }

            loop {
                select! {
                    command = outgoing_rx.next() => {
                        let Some(text) = command else { break };
                        // A stalled gateway answers nothing at all, the ping
                        // included. Silencing only the tick stream would leave
                        // the pong arriving every thirty seconds, which is
                        // exactly the frame the staleness threshold is measured
                        // against — the simulation would then be unreachable.
                        if is_stalled!() {
                            continue;
                        }
                        let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
                            continue;
                        };
                        match message {
                            ClientMessage::Ping => {
                                if sink.send(frame("system", "pong", serde_json::Value::Null)).await.is_err() {
                                    break;
                                }
                            }
                            ClientMessage::Subscribe { channels } => {
                                for channel in channels {
                                    subscribed.insert(channel.clone());
                                    // The contract promises a fresh snapshot on
                                    // every subscribe. That promise is what makes
                                    // a reconnection safe, so the mock keeps it.
                                    if let Some(symbol) = channel.strip_prefix("orderbook:") {
                                        if let Some(payload) = fresh_book(&state, symbol) {
                                            books.retain(|(held, _)| held != symbol);
                                            books.push((symbol.to_string(), payload.clone()));
                                            let data = serde_json::to_value(&payload)
                                                .unwrap_or(serde_json::Value::Null);
                                            if sink.send(frame(&channel, "snapshot", data)).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            ClientMessage::Unsubscribe { channels } => {
                                for channel in channels {
                                    subscribed.remove(&channel);
                                    if let Some(symbol) = channel.strip_prefix("orderbook:") {
                                        books.retain(|(held, _)| held != symbol);
                                    }
                                }
                            }
                            ClientMessage::Auth { .. } => {}
                        }
                    },
                    _ = ticks.next() => {
                        // An outage declared while this socket is open closes it,
                        // which is what a gateway falling over actually looks like.
                        let out = unavailable.lock().map(|until| *until > now_millis()).unwrap_or(false);
                        if out {
                            break;
                        }
                        // A stall sends nothing and closes nothing: the socket
                        // stays open and goes quiet, exactly as a process
                        // stopped with `kill -STOP` does.
                        if is_stalled!() {
                            continue;
                        }
                        counter += 1;
                        if emit_tick(&state, &mut sink, &mut books, &subscribed, &mut rng, counter)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        futures::future::ready(Ok(Socket {
            incoming: incoming_rx.boxed_local(),
            outgoing: outgoing_tx,
        }))
        .boxed_local()
    }
}

/// Builds a fresh book for a market from the current state.
fn fresh_book(state: &Arc<Mutex<MockState>>, symbol: &str) -> Option<OrderBookPayload> {
    let guard = state.lock().ok()?;
    let market = guard.market(symbol)?.clone();
    let last = guard.last_price(symbol);
    let height = guard.height();
    Some(fixtures::book(&market, last, height, 40, 0xB00C_5EED))
}

/// Emits one tick's worth of frames.
///
/// Every frame is built while the state is locked and sent after the lock is
/// released. Holding a `MutexGuard` across an `await` in a single-threaded
/// executor is how a mock deadlocks against the very gateway calls it is
/// answering, and it is not a hazard worth keeping for the convenience.
async fn emit_tick(
    state: &Arc<Mutex<MockState>>,
    sink: &mut mpsc::UnboundedSender<String>,
    books: &mut [(String, OrderBookPayload)],
    subscribed: &BTreeSet<String>,
    rng: &mut Rng,
    counter: u64,
) -> Result<(), ()> {
    let frames = build_frames(state, books, subscribed, rng, counter)?;
    for text in frames {
        send(sink, text).await?;
    }
    Ok(())
}

/// Builds this tick's frames while holding the lock. No `await` inside.
fn build_frames(
    state: &Arc<Mutex<MockState>>,
    books: &mut [(String, OrderBookPayload)],
    subscribed: &BTreeSet<String>,
    rng: &mut Rng,
    counter: u64,
) -> Result<Vec<String>, ()> {
    let Ok(mut guard) = state.lock() else {
        return Err(());
    };
    let height = guard.height();
    let now = now_seconds();
    let mut frames = Vec::new();

    // ---- every tick: a block, because the chain rail is never idle.
    if subscribed.contains("chain") {
        let tx_count =
            fixtures::BLOCK_TX_COUNTS[(height as usize) % fixtures::BLOCK_TX_COUNTS.len()];
        let data = serde_json::json!({
            "height": height,
            "hash": format!("0x{:012x}", 0x4c1e_9ba2_0000_u64 ^ height.wrapping_mul(0x9E37_79B9)),
            "tx_count": tx_count,
            "at": rfc3339(now),
        });
        frames.push(frame("chain", "block", data));
    }

    // ---- every ten ticks: prices walk, and the ticker channel says so.
    //
    // A halted or settled market is deliberately left out. Its price then ages
    // past five seconds and the interface shows it as stale, which is exactly
    // what a market with no oracle behind it should look like.
    if counter.is_multiple_of(10) && subscribed.contains("ticker") {
        let symbols: Vec<String> = guard.markets.iter().map(|m| m.symbol.clone()).collect();
        for symbol in symbols {
            let Some(market) = guard.market(&symbol).cloned() else {
                continue;
            };
            if !market.status.is_tradable() {
                continue;
            }
            let last = guard.last_price(&symbol);
            // The walk is pulled back toward the figure the reference prints
            // whenever it drifts more than a few ticks away, so the strip and
            // the static book never disagree about where the market is.
            let anchor = fixtures::quotes()
                .into_iter()
                .find(|(quoted, _, _)| *quoted == symbol)
                .map_or(last, |(_, price, _)| fixtures::dec(price));
            let drift = last - anchor;
            let pull = if drift.abs() > market.tick_size * Decimal::from(8) {
                -drift / Decimal::from(4)
            } else {
                Decimal::ZERO
            };
            let moved = (last + pull + rng.jitter(3, market.tick_size)).max(market.tick_size);
            let moved = moved.round_dp(market.price_decimals);
            guard.last.insert(symbol.clone(), moved);
            let change = guard.change.get(&symbol).copied().unwrap_or_default();
            let data = serde_json::json!({
                "symbol": symbol,
                "last_price": moved.to_string(),
                "mark_price": (moved - market.tick_size / Decimal::TWO)
                    .round_dp(market.price_decimals)
                    .to_string(),
                "change_24h_pct": change.to_string(),
                "funding_rate": market.kind.is_derivative().then(|| "0.000125".to_string()),
            });
            frames.push(frame("ticker", "update", data));
        }
    }

    // ---- every four ticks: a trade on each subscribed tape.
    if counter.is_multiple_of(4) {
        for channel in subscribed.iter().filter(|c| c.starts_with("trades:")) {
            let Some(symbol) = channel.strip_prefix("trades:") else {
                continue;
            };
            let Some(market) = guard.market(symbol).cloned() else {
                continue;
            };
            let last = guard.last_price(symbol);
            let price = (last + rng.jitter(2, market.tick_size)).round_dp(market.price_decimals);
            let qty = (market.lot_size * Decimal::from(1 + rng.below(3_000)))
                .round_dp(market.size_decimals);
            let side = if rng.below(2) == 0 {
                Side::Buy
            } else {
                Side::Sell
            };
            let data = serde_json::json!({
                "price": price.to_string(),
                "qty": qty.to_string(),
                "side": if side == Side::Buy { "buy" } else { "sell" },
                "at": rfc3339(now),
            });
            frames.push(frame(channel, "trade", data));
        }
    }

    // ---- every five ticks: a book delta, including removals at zero.
    if counter.is_multiple_of(5) {
        for (symbol, book) in books.iter_mut() {
            if !subscribed.contains(&format!("orderbook:{symbol}")) {
                continue;
            }
            let Some(market) = guard.market(symbol).cloned() else {
                continue;
            };
            let mut bid_changes = Vec::new();
            let mut ask_changes = Vec::new();

            for _ in 0..2 {
                let touch_bids = rng.below(2) == 0;
                let side = if touch_bids {
                    &mut book.bids
                } else {
                    &mut book.asks
                };
                if side.is_empty() {
                    continue;
                }
                let index = (rng.below(side.len() as u64)) as usize;
                let price = side[index].price();
                // A third of the time the level is taken out entirely, which the
                // contract expresses as a quantity of zero.
                if rng.below(3) == 0 && side.len() > 12 {
                    side.remove(index);
                    let change = Level(price, Decimal::ZERO);
                    if touch_bids {
                        bid_changes.push(change);
                    } else {
                        ask_changes.push(change);
                    }
                } else {
                    let qty = (market.lot_size * Decimal::from(1 + rng.below(30_000)))
                        .round_dp(market.size_decimals);
                    side[index] = Level(price, qty);
                    let change = Level(price, qty);
                    if touch_bids {
                        bid_changes.push(change);
                    } else {
                        ask_changes.push(change);
                    }
                }
            }

            book.height = height;
            let data = serde_json::json!({
                "height": height,
                "bids": bid_changes,
                "asks": ask_changes,
            });
            frames.push(frame(&format!("orderbook:{symbol}"), "delta", data));
        }
    }

    // ---- the private channel: orders reaching their acceptance block.
    if subscribed.contains("account") {
        guard.settle_pending();
        for order in guard.take_announcements() {
            let data = serde_json::to_value(&order).unwrap_or(serde_json::Value::Null);
            frames.push(frame("account", "order_update", data));
        }
    }

    Ok(frames)
}

/// Sends a frame, reporting a closed socket as an error so the task can stop.
async fn send(sink: &mut mpsc::UnboundedSender<String>, text: String) -> Result<(), ()> {
    sink.send(text).await.map_err(|_| ())
}
