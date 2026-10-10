//! Reactive stores: one place where the world is known.
//!
//! Components in this application do not fetch. They read signals, and this
//! module is what fills them — from the initial REST calls and then from the
//! websocket. The reason is not tidiness. Several panels show the same number
//! (the order book's touch, the market strip's last price, the ticket's
//! reference price), and if each fetched for itself they would disagree by a
//! round trip, which is exactly the class of disagreement that makes a trading
//! interface untrustworthy.
//!
//! The other thing that lives here is **freshness**. Every ticker carries the
//! moment it arrived, and [`now_ms`](Stores::now_ms) ticks once a second, so
//! "this price is eight seconds old" is a derived fact rather than a flag
//! somebody has to remember to set. That is what the stale-price state in the
//! design system is built on.

use std::collections::BTreeMap;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::api::{ApiError, Remedy};
use crate::clock::now_millis;
use crate::ws::msg::{BlockUpdate, TickerUpdate};
use crate::ws::{ConnectionState, DeltaOutcome, FeedEvent, OrderBook, ServerMessage};

/// After this long without a ticker, a price is shown as stale.
pub const STALE_AFTER_MS: i64 = 5_000;

/// After this long with no frame of any kind, the **connection** is stale.
///
/// Derived from the ping schedule rather than picked. On an idle chain the pong
/// is the only inbound frame there is — measured against a live gateway with no
/// trading on it, the age of the last frame sawtooths from zero to about
/// twenty-eight seconds and resets — so one and a half ping intervals is the
/// shortest threshold that cannot fire on a healthy socket. Anything past it
/// means even the answer to the ping is not coming back.
pub const FEED_STALE_AFTER_MS: i64 = crate::ws::PING_INTERVAL_MS as i64 * 3 / 2;

/// After this long with no frame of any kind, the connection is treated as down.
///
/// §9 of the design system: no `pong` within ninety seconds turns the indicator
/// vermilion and makes it read *Offline*. That rule existed and was never
/// reached, because the indicator only ever reported what the socket said about
/// itself — and a gateway that has stopped answering does not say anything,
/// including goodbye.
pub const FEED_DEAD_AFTER_MS: i64 = 90_000;

/// How long the *Resynced* badge stays up after a reconnection.
pub const RESYNCED_FOR_MS: i64 = 2_000;

/// How many blocks the chain rail holds.
pub const RAIL_BLOCKS: usize = 12;

/// How many blocks the chain screen's histogram holds.
pub const CHAIN_BLOCKS: usize = 24;

/// How many trades the tape keeps.
pub const TAPE_LENGTH: usize = 60;

/// Whether a region of the screen has an answer yet.
///
/// The single most consequential distinction in this client on a chain with no
/// data in it. *Loading* and *empty* were one picture — a shimmering
/// skeleton — so on a fresh chain the order book and the tape shimmered
/// forever, promising data that was never going to arrive. A skeleton is a
/// promise; an empty state is a fact, and on day one the fact is what every
/// visitor is actually looking at.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load {
    /// The request has not answered yet. This, and only this, draws a skeleton.
    #[default]
    Loading,
    /// The gateway answered. What is on screen is the truth, including nothing.
    Ready,
    /// The gateway did not answer, or not in a shape this build understands.
    ///
    /// Carries its own sentence, because a region that silently renders a
    /// column header over blank space is indistinguishable from an empty one
    /// and only one of them is honest.
    Failed(String),
}

impl Load {
    /// True while a skeleton is the right thing to draw.
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    /// The failure's own sentence, if it failed.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }

    /// `Ready` on success, `Failed` with the gateway's own words otherwise.
    #[must_use]
    pub fn from_result<T, E: std::fmt::Display>(result: &Result<T, E>) -> Self {
        match result {
            Ok(_) => Self::Ready,
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

/// A price with the moment it arrived and the direction it moved.
///
/// The previous value is kept so the tick flash knows which way to flash; the
/// arrival time is kept so staleness is a derived fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    /// Last traded price.
    pub last: Decimal,
    /// Mark price, which margin is computed against.
    pub mark: Decimal,
    /// 24-hour change, already a percentage.
    pub change_pct: Decimal,
    /// Funding rate per interval, as a fraction.
    pub funding_rate: Option<Decimal>,
    /// The previous last price, or `None` on the first quote.
    pub previous: Option<Decimal>,
    /// Wall-clock milliseconds at which this quote arrived.
    pub at_ms: i64,
}

impl Quote {
    /// How far the last price moved on this update.
    #[must_use]
    pub fn tick(&self) -> Option<Decimal> {
        self.previous.map(|previous| self.last - previous)
    }

    /// The CSS class for the tick flash, if the price moved.
    #[must_use]
    pub fn tick_class(&self) -> &'static str {
        match self.tick() {
            Some(delta) if delta > Decimal::ZERO => "tick-up",
            Some(delta) if delta < Decimal::ZERO => "tick-down",
            _ => "",
        }
    }

    /// How old this quote is now.
    #[must_use]
    pub const fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms - self.at_ms
    }

    /// True when the price should be shown as stale.
    ///
    /// Stale does **not** mean hidden. The design system is explicit: the price
    /// keeps its last value, gains a brass rule and an age counter, and order
    /// entry stays enabled, because the node may still accept.
    #[must_use]
    pub const fn is_stale(&self, now_ms: i64) -> bool {
        self.age_ms(now_ms) > STALE_AFTER_MS
    }
}

/// What the connection indicator says: a dot, a word **and** a figure.
///
/// Derived from the age of the last frame as well as from the socket's own
/// state, because those are different facts. A socket that is open to a process
/// which has stopped running is `Live` by every measure the socket has and
/// dead by the only measure a trader cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedReport {
    /// The `data-state` attribute: `live`, `stale`, `reconnecting` or `down`.
    pub attr: &'static str,
    /// The word.
    pub word: String,
    /// The figure beside it. Never empty except while connecting.
    pub figure: String,
    /// True when prices must stop looking live.
    pub frozen: bool,
}

/// Works out what the indicator should say.
///
/// A pure function of three numbers, so the rule can be asserted rather than
/// eyeballed against a stopped server.
#[must_use]
pub fn feed_report(state: &ConnectionState, last_frame_ms: i64, now_ms: i64) -> FeedReport {
    match state {
        ConnectionState::Connecting => FeedReport {
            attr: "reconnecting",
            word: "Connecting".to_string(),
            figure: String::new(),
            frozen: true,
        },
        ConnectionState::Reconnecting {
            attempt,
            dropped_ms,
        } => FeedReport {
            attr: "reconnecting",
            word: format!("Reconnecting, attempt {attempt}"),
            figure: format!(
                "down {}",
                crate::format::duration((now_ms - dropped_ms) / 1_000)
            ),
            frozen: true,
        },
        ConnectionState::Down { dropped_ms } => FeedReport {
            attr: "down",
            word: "Offline".to_string(),
            figure: format!("since {}", crate::format::clock(dropped_ms / 1_000)),
            frozen: true,
        },
        ConnectionState::Live { since_ms } => {
            // A browser's `WebSocket` is handed back the moment it is asked
            // for, not when the handshake completes, so "the socket is open" is
            // not evidence that anything is on the other end of it. Against a
            // gateway stopped with `kill -STOP` the connection is accepted by
            // the kernel and then says nothing at all — so until a frame has
            // actually arrived on **this** connection, the age is measured from
            // when it came up and the word is *Connecting*, never *Live*.
            // The clock runs on the last frame and is **never** restarted by a
            // reconnection. A driver that reopens the socket every ninety
            // seconds against a server that is not answering would otherwise
            // reset its own evidence each time, and the age a user is reading
            // would count how long this socket has been open rather than how
            // old the last thing the gateway said is.
            let fresh = last_frame_ms < *since_ms;
            let reference = last_frame_ms;
            let age = (now_ms - reference).max(0);
            if age > FEED_DEAD_AFTER_MS {
                FeedReport {
                    attr: "down",
                    word: "Offline".to_string(),
                    figure: format!("silent since {}", crate::format::clock(reference / 1_000)),
                    frozen: true,
                }
            } else if age > FEED_STALE_AFTER_MS {
                FeedReport {
                    attr: "stale",
                    word: "Stale".to_string(),
                    figure: format!("nothing for {}", crate::format::age(age)),
                    frozen: true,
                }
            } else if fresh {
                FeedReport {
                    attr: "reconnecting",
                    word: "Connecting".to_string(),
                    figure: crate::format::age(age),
                    frozen: true,
                }
            } else {
                FeedReport {
                    attr: "live",
                    word: "Live".to_string(),
                    figure: crate::format::age(age),
                    frozen: false,
                }
            }
        }
    }
}

/// What kind of thing a toast is telling the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// It worked.
    Good,
    /// It did not work.
    Bad,
    /// It worked, with a caveat, or something needs attention.
    Warn,
    /// Something happened that is neither good nor bad.
    Info,
}

impl Tone {
    /// The `.toast` modifier class.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Good => "toast good",
            Self::Bad => "toast bad",
            Self::Warn => "toast warn",
            Self::Info => "toast info",
        }
    }
}

/// A one-tap remedy attached to a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    /// Move a post-only price to a price that will rest.
    Reprice(Decimal),
    /// Go to the faucet.
    OpenFaucet,
    /// Halve the order size.
    ReduceSize,
}

impl ToastAction {
    /// The one the gateway's error code allows, with the numbers it supplied.
    #[must_use]
    pub fn from_error(error: &ApiError) -> Option<Self> {
        match error.remedy() {
            Remedy::Reprice => error
                .detail("reprice_to")
                .and_then(|value| value.parse().ok())
                .map(Self::Reprice),
            Remedy::OpenFaucet => Some(Self::OpenFaucet),
            Remedy::ReduceSize => Some(Self::ReduceSize),
            Remedy::None => None,
        }
    }

    /// The button label, which names the number where there is one.
    #[must_use]
    pub fn label(&self, price_decimals: u32) -> String {
        match self {
            Self::Reprice(price) => {
                format!(
                    "Reprice to {}",
                    crate::format::price(*price, price_decimals)
                )
            }
            Self::OpenFaucet => "Open the faucet".to_string(),
            Self::ReduceSize => "Halve the size".to_string(),
        }
    }
}

/// A message in the bottom-right stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Identity, so the list can be keyed and dismissed.
    pub id: u64,
    /// Which kind of thing this is.
    pub tone: Tone,
    /// The plain-language consequence.
    pub title: String,
    /// The detail, which for a rejection is the gateway's own sentence.
    pub body: String,
    /// The block this is a receipt for, where there is one. On this product a
    /// trade toast that does not name a block is not a receipt.
    pub height: Option<u64>,
    /// The one-tap remedy, where the error code allows one.
    pub action: Option<ToastAction>,
    /// When it should disappear; `None` stays until dismissed.
    pub expires_ms: Option<i64>,
}

/// Every signal the application reads.
///
/// `Copy`, because every field is a `Copy` signal handle: passing this around is
/// passing a handful of arena indices, not cloning the world.
#[derive(Debug, Clone, Copy)]
pub struct Stores {
    /// Where the socket is.
    pub connection: RwSignal<ConnectionState>,
    /// Wall-clock millisecond until which the *Resynced* badge shows.
    pub resynced_until_ms: RwSignal<i64>,
    /// Why the socket last dropped, shown beside the indicator.
    pub disconnect_reason: RwSignal<Option<String>>,
    /// Wall-clock millisecond at which the last frame arrived.
    ///
    /// This is the figure the live indicator shows. Uptime is not the useful
    /// number: what a trader needs to know at a glance is how old the last thing
    /// the gateway said is.
    pub last_frame_ms: RwSignal<i64>,

    /// The chain's headline numbers.
    pub chain: RwSignal<Option<ChainStatus>>,
    /// The most recent blocks, newest first.
    pub blocks: RwSignal<Vec<Block>>,
    /// Bumped on every new block, to drive the height flash.
    pub block_tick: RwSignal<u64>,

    /// Every listed market.
    pub markets: RwSignal<Vec<Market>>,
    /// The latest quote per market, with its arrival time.
    pub quotes: RwSignal<BTreeMap<String, Quote>>,
    /// The market the terminal is showing.
    pub selected: RwSignal<String>,

    /// The selected market's book.
    pub book: RwSignal<OrderBook>,
    /// The selected market's tape, newest first.
    pub tape: RwSignal<Vec<Trade>>,
    /// Whether the tape has been answered for the selected market.
    pub tape_load: RwSignal<Load>,
    /// Whether the market list has been answered.
    pub markets_load: RwSignal<Load>,
    /// Whether the chain status has been answered.
    pub chain_load: RwSignal<Load>,
    /// Whether the block list has been answered. Separate from the status:
    /// `/chain/status` can answer while `/chain/blocks` does not, and "no
    /// blocks yet" would then be a claim about a call that failed.
    pub blocks_load: RwSignal<Load>,

    /// Who is signed in, if anyone.
    pub session: RwSignal<Option<SessionInfo>>,
    /// Balances and portfolio.
    pub account: RwSignal<Option<Account>>,
    /// The account's orders, newest first.
    pub orders: RwSignal<Vec<Order>>,
    /// The account's open positions.
    pub positions: RwSignal<Vec<Position>>,

    /// The toast stack.
    pub toasts: RwSignal<Vec<Toast>>,
    /// The next toast identity.
    next_toast_id: RwSignal<u64>,

    /// Wall-clock milliseconds, republished once a second.
    ///
    /// Every age counter, countdown and staleness check reads this, so there is
    /// exactly one timer in the application rather than one per component.
    pub now_ms: RwSignal<i64>,
}

impl Default for Stores {
    fn default() -> Self {
        Self::new()
    }
}

impl Stores {
    /// Creates the stores, empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            connection: RwSignal::new(ConnectionState::Connecting),
            resynced_until_ms: RwSignal::new(0),
            disconnect_reason: RwSignal::new(None),
            // Seeded with the moment the application started, not zero, so that
            // "how old is the last thing the gateway said" has an answer before
            // the gateway has said anything — and so a gateway that is dead
            // from the first paint reaches *Offline* rather than resetting its
            // own clock on every reconnection.
            last_frame_ms: RwSignal::new(now_millis()),
            chain: RwSignal::new(None),
            blocks: RwSignal::new(Vec::new()),
            block_tick: RwSignal::new(0),
            markets: RwSignal::new(Vec::new()),
            quotes: RwSignal::new(BTreeMap::new()),
            selected: RwSignal::new("BTC-USD".to_string()),
            book: RwSignal::new(OrderBook::new()),
            tape: RwSignal::new(Vec::new()),
            tape_load: RwSignal::new(Load::Loading),
            markets_load: RwSignal::new(Load::Loading),
            chain_load: RwSignal::new(Load::Loading),
            blocks_load: RwSignal::new(Load::Loading),
            session: RwSignal::new(None),
            account: RwSignal::new(None),
            orders: RwSignal::new(Vec::new()),
            positions: RwSignal::new(Vec::new()),
            toasts: RwSignal::new(Vec::new()),
            next_toast_id: RwSignal::new(1),
            now_ms: RwSignal::new(now_millis()),
        }
    }

    /// The market definition currently selected, if it has loaded.
    #[must_use]
    pub fn market(&self) -> Option<Market> {
        let symbol = self.selected.get();
        self.markets
            .read()
            .iter()
            .find(|market| market.symbol == symbol)
            .cloned()
    }

    /// A market definition by symbol.
    #[must_use]
    pub fn market_named(&self, symbol: &str) -> Option<Market> {
        self.markets
            .read()
            .iter()
            .find(|market| market.symbol == symbol)
            .cloned()
    }

    /// The quote for the selected market.
    #[must_use]
    pub fn quote(&self) -> Option<Quote> {
        self.quotes.read().get(&self.selected.get()).cloned()
    }

    /// True when the selected market's price is older than the stale threshold.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        let now = self.now_ms.get();
        self.quote().is_some_and(|quote| quote.is_stale(now))
    }

    /// True when prices should stop looking live: the socket is not up.
    ///
    /// Nothing is hidden in this state. The last known book stays on screen,
    /// visibly frozen, because a blank panel and a stale panel look identical at
    /// a glance and only one of them is honest.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        self.feed_report().frozen
    }

    /// What the connection indicator says right now.
    #[must_use]
    pub fn feed_report(&self) -> FeedReport {
        feed_report(
            &self.connection.get(),
            self.last_frame_ms.get(),
            self.now_ms.get(),
        )
    }

    /// True while the *Resynced* badge should show.
    #[must_use]
    pub fn is_resynced(&self) -> bool {
        self.now_ms.get() < self.resynced_until_ms.get()
    }

    /// The account-level liquidation risk, or `Low` when nothing is loaded.
    #[must_use]
    pub fn risk(&self) -> LiquidationRisk {
        self.account
            .read()
            .as_ref()
            .map_or(LiquidationRisk::Low, |account| {
                account.portfolio.liquidation_risk
            })
    }

    /// The position the hazard banner is about, if any warrants one.
    ///
    /// Only `high` and `critical` raise a banner: the design system reserves the
    /// loudest treatment for money about to be lost, and a banner that appears
    /// for an ordinary drawdown teaches people to ignore banners.
    #[must_use]
    pub fn hazard_position(&self) -> Option<Position> {
        self.positions
            .read()
            .iter()
            .filter(|position| {
                matches!(
                    position.risk(),
                    LiquidationRisk::High | LiquidationRisk::Critical
                )
            })
            .max_by_key(|position| position.liquidation_proximity())
            .cloned()
    }

    /// Pushes a toast onto the stack, keeping at most four.
    pub fn toast(&self, toast: Toast) {
        let id = self.next_toast_id.get_untracked();
        self.next_toast_id.set(id + 1);
        self.toasts.update(|list| {
            list.push(Toast { id, ..toast });
            while list.len() > 4 {
                list.remove(0);
            }
        });
    }

    /// A toast that goes away on its own after five seconds.
    pub fn notify(&self, tone: Tone, title: impl Into<String>, body: impl Into<String>) {
        self.toast(Toast {
            id: 0,
            tone,
            title: title.into(),
            body: body.into(),
            height: None,
            action: None,
            expires_ms: Some(now_millis() + 5_000),
        });
    }

    /// A rejection toast: the consequence as a title, the gateway's own sentence
    /// as the body, and the one-tap remedy the code allows.
    pub fn reject(&self, error: &ApiError) {
        self.toast(Toast {
            id: 0,
            tone: Tone::Bad,
            title: error.title().to_string(),
            body: error.message(),
            height: None,
            action: ToastAction::from_error(error),
            // A rejection stays until it is read. Anything the user must act on
            // does not get to scroll past on a five-second timer.
            expires_ms: None,
        });
    }

    /// Removes a toast.
    pub fn dismiss(&self, id: u64) {
        self.toasts
            .update(|list| list.retain(|toast| toast.id != id));
    }

    /// Drops toasts whose dwell time has run out.
    pub fn expire_toasts(&self) {
        let now = self.now_ms.get_untracked();
        let any_expired = self
            .toasts
            .read_untracked()
            .iter()
            .any(|toast| toast.expires_ms.is_some_and(|at| at <= now));
        if any_expired {
            self.toasts
                .update(|list| list.retain(|toast| toast.expires_ms.is_none_or(|at| at > now)));
        }
    }

    /// Replaces the selected market and resets the per-market stores.
    ///
    /// The book is marked as awaiting a snapshot rather than emptied, so the
    /// panel shows a skeleton at the right row height instead of jumping.
    pub fn select_market(&self, symbol: &str) {
        if self.selected.get_untracked() == symbol {
            return;
        }
        self.selected.set(symbol.to_string());
        self.book.update(|book| *book = OrderBook::new());
        self.tape.set(Vec::new());
        // A new market's tape has not been asked for yet, so it is loading
        // again rather than empty.
        self.tape_load.set(Load::Loading);
    }

    /// Records a ticker update, keeping the previous price for the flash.
    pub fn apply_ticker(&self, update: &TickerUpdate) {
        let at_ms = now_millis();
        self.quotes.update(|quotes| {
            let previous = quotes.get(&update.symbol).map(|quote| quote.last);
            quotes.insert(
                update.symbol.clone(),
                Quote {
                    last: update.last_price,
                    mark: update.mark_price,
                    change_pct: update.change_24h_pct,
                    funding_rate: update.funding_rate,
                    previous,
                    at_ms,
                },
            );
        });
    }

    /// Seeds a quote from a REST ticker, before the socket has said anything.
    pub fn seed_quote(&self, ticker: &Ticker) {
        self.apply_ticker(&TickerUpdate {
            symbol: ticker.symbol.clone(),
            last_price: ticker.last_price,
            mark_price: ticker.mark_price,
            change_24h_pct: ticker.change_24h_pct,
            funding_rate: ticker.funding_rate,
        });
    }

    /// Records a new block on the rail.
    pub fn apply_block(&self, update: &BlockUpdate) {
        self.blocks.update(|blocks| {
            if blocks
                .first()
                .is_some_and(|first| first.height >= update.height)
            {
                return;
            }
            blocks.insert(
                0,
                Block {
                    height: update.height,
                    hash: update.hash.clone(),
                    state_root: String::new(),
                    tx_count: update.tx_count,
                    at: update.at.clone(),
                    proposer: String::new(),
                },
            );
            blocks.truncate(CHAIN_BLOCKS);
        });
        self.chain.update(|chain| {
            if let Some(chain) = chain.as_mut() {
                chain.height = update.height;
            }
        });
        self.block_tick.update(|tick| *tick = tick.wrapping_add(1));
    }

    /// Applies one feed event to the stores.
    ///
    /// The ordering here is the contract: a `Connected` event marks the book as
    /// awaiting a snapshot **before** any frame from the new connection can
    /// arrive, so a delta that races the snapshot is dropped rather than applied
    /// to a book the snapshot is about to replace.
    pub fn apply_feed(&self, event: &FeedEvent) {
        match event {
            FeedEvent::Connected { resynced } => {
                self.book.update(OrderBook::expect_snapshot);
                // `last_frame_ms` is deliberately **not** touched here. A socket
                // that has opened has not said anything yet, and pretending it
                // has is how a frozen gateway kept reporting a live feed: the
                // driver reconnects every ninety seconds, and each reconnection
                // would have restarted the clock on a connection carrying
                // nothing. [`feed_report`] compares it against `since_ms`
                // instead.
                self.connection.set(ConnectionState::Live {
                    since_ms: now_millis(),
                });
                self.disconnect_reason.set(None);
                if *resynced {
                    self.resynced_until_ms.set(now_millis() + RESYNCED_FOR_MS);
                }
            }
            FeedEvent::Disconnected { reason } => {
                self.disconnect_reason.set(Some(reason.clone()));
                self.connection.set(ConnectionState::Down {
                    dropped_ms: now_millis(),
                });
            }
            FeedEvent::Reconnecting { attempt, .. } => {
                self.connection.set(ConnectionState::Reconnecting {
                    attempt: *attempt,
                    dropped_ms: now_millis(),
                });
            }
            FeedEvent::Message(message) => {
                self.last_frame_ms.set(now_millis());
                self.apply_message(message);
            }
        }
    }

    /// Applies one websocket frame.
    fn apply_message(&self, message: &ServerMessage) {
        let selected = self.selected.get_untracked();
        match message {
            ServerMessage::BookSnapshot(symbol, payload) if *symbol == selected => {
                self.book.update(|book| book.apply_snapshot(payload));
            }
            ServerMessage::BookDelta(symbol, payload) if *symbol == selected => {
                self.book.update(|book| {
                    // A `Gap` outcome is advisory and nothing acts on it. The
                    // contract's sequence number is a block height, and a block
                    // in which the book did not change produces no delta, so a
                    // jump in height is ordinary rather than a lost message. The
                    // real resynchronisation is the snapshot on reconnect.
                    let _: DeltaOutcome = book.apply_delta(payload);
                });
            }
            ServerMessage::Trade(symbol, trade) if *symbol == selected => {
                self.tape.update(|tape| {
                    tape.insert(
                        0,
                        Trade {
                            id: String::new(),
                            price: trade.price,
                            qty: trade.qty,
                            side: trade.side,
                            at: trade.at.clone(),
                            height: 0,
                        },
                    );
                    tape.truncate(TAPE_LENGTH);
                });
            }
            ServerMessage::Ticker(update) => self.apply_ticker(update),
            ServerMessage::Block(update) => self.apply_block(update),
            ServerMessage::OrderUpdate(order) => {
                let order = order.as_ref().clone();
                self.orders.update(|orders| {
                    match orders
                        .iter_mut()
                        .find(|held| held.order_id == order.order_id)
                    {
                        Some(held) => *held = order,
                        None => orders.insert(0, order),
                    }
                });
            }
            ServerMessage::PositionUpdate(position) => {
                let position = position.as_ref().clone();
                self.positions.update(|positions| {
                    match positions
                        .iter_mut()
                        .find(|held| held.symbol == position.symbol)
                    {
                        Some(held) => *held = position,
                        None => positions.push(position),
                    }
                });
            }
            ServerMessage::BalanceUpdate(balances) => {
                let balances = balances.clone();
                self.account.update(|account| {
                    if let Some(account) = account.as_mut() {
                        account.balances = balances;
                    }
                });
            }
            ServerMessage::Fill(fill) => {
                self.notify(
                    Tone::Good,
                    format!("{} filled", fill.symbol),
                    format!(
                        "{} {} at {} in block {}",
                        fill.side.label(),
                        crate::format::fixed(fill.qty, 4),
                        crate::format::fixed(fill.price, 2),
                        crate::format::count(fill.height),
                    ),
                );
            }
            ServerMessage::Liquidation(event) => {
                // The game's most consequential event. It stays until dismissed.
                self.toast(Toast {
                    id: 0,
                    tone: Tone::Bad,
                    title: format!("{} was liquidated", event.symbol),
                    body: format!(
                        "{} closed at {} for {}",
                        crate::format::fixed(event.qty, 4),
                        crate::format::fixed(event.price, 2),
                        crate::format::pnl(event.loss),
                    ),
                    height: (event.height > 0).then_some(event.height),
                    action: None,
                    expires_ms: None,
                });
            }
            ServerMessage::Error(error) => {
                self.notify(Tone::Warn, error.code.consequence(), error.message.clone());
            }
            _ => {}
        }
    }
}

/// Puts the stores in context for the whole application.
pub fn provide_stores(stores: Stores) {
    provide_context(stores);
}

/// Reads the stores from context.
///
/// # Panics
///
/// Only if called outside the application root, which is a programming error
/// rather than a runtime condition.
#[must_use]
pub fn use_stores() -> Stores {
    use_context::<Stores>().unwrap_or_else(|| {
        // Reachable only if a component is mounted outside the app root. A fresh
        // set of stores renders an empty screen instead of a wasm panic, which
        // in a browser is a blank page and a console message.
        Stores::new()
    })
}
