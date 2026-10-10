//! The order-book ladder.
//!
//! Shared by the terminal and the landing page, because the landing page's hero
//! is not a picture of a market: it is this component, at this density, on the
//! same data shape. What a stranger sees first is the thing they are about to
//! use.
//!
//! Three rules from the design system are load-bearing here:
//!
//! * **Asks above, bids below, at every breakpoint.** Position carries side
//!   before colour does, so the layout never flips and never stacks.
//! * **The depth bar is cumulative**, scaled against the largest total on either
//!   side, and grows from the outer edge inward toward the spread — so the two
//!   sides converge and the book reads as one silhouette.
//! * **Each level is a `<button>`, not a div**, so the book is tabbable and the
//!   focus ring lands on the level. Clicking the price fills the ticket's price;
//!   clicking the size fills price and size together.

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::{Market, MarketStatus};
use crate::format;
use crate::state::use_stores;
use crate::ticket::use_ticket;
use crate::ws::book::{depth_pct, BookRow};
use crate::ws::OrderBook;

/// How many levels a side the ladder shows.
pub const LADDER_DEPTH: usize = 9;

/// One side of the ladder.
#[component]
fn Levels(
    /// The rows, best first.
    rows: Vec<BookRow>,
    /// The scale every depth bar on both sides is measured against.
    scale: Decimal,
    /// `ask` or `bid`.
    side: &'static str,
    /// Where prices this market's prices are rounded.
    price_decimals: u32,
    /// Where sizes are rounded.
    size_decimals: u32,
    /// Prices at which this account has a resting order.
    mine: Vec<Decimal>,
    /// False when the market is not accepting orders, which makes the ladder
    /// inert rather than hiding it.
    interactive: bool,
) -> impl IntoView {
    let ticket = use_ticket();
    view! {
        {rows
            .into_iter()
            .map(|row| {
                let is_mine = mine.contains(&row.price);
                let depth = depth_pct(row.cumulative, scale);
                let (price, qty) = (row.price, row.qty);
                let label = format!(
                    "{} {} at {}",
                    if side == "bid" { "Bid" } else { "Ask" },
                    format::fixed(qty, size_decimals),
                    format::fixed(price, price_decimals),
                );
                view! {
                    <button
                        class=format!("ob-row {side}")
                        class:mine=is_mine
                        style=format!("--depth:{depth}")
                        disabled=!interactive
                        aria-label=label
                        on:click=move |event| {
                            ticket.price.set(format::fixed(price, price_decimals));
                            // Shift-click, or a click on the size column, takes
                            // the size too. The plain click takes only the price,
                            // so a mis-aimed click cannot change the size.
                            if event.shift_key() {
                                ticket.qty.set(format::fixed(qty, size_decimals));
                            }
                        }
                    >
                        <span class="px">{format::fixed(price, price_decimals)}</span>
                        <span class="sz">{format::fixed(qty, size_decimals)}</span>
                        <span class="cum">{format::fixed(row.cumulative, 3)}</span>
                    </button>
                }
            })
            .collect_view()}
    }
}

/// The two-sided ladder with the spread between them.
#[component]
pub fn BookLadder(
    /// How many levels a side to draw.
    #[prop(default = LADDER_DEPTH)]
    depth: usize,
    /// Aggregation tick, or `None` for the raw book.
    #[prop(into, default = Signal::derive(|| None))]
    aggregate: Signal<Option<Decimal>>,
) -> impl IntoView {
    let stores = use_stores();
    let market = move || stores.market();
    let price_dp = move || market().map_or(2, |market| market.price_decimals);
    let size_dp = move || market().map_or(4, |market| market.size_decimals);
    let base = move || market().map_or_else(|| "—".to_string(), |market| market.base);

    let bids = move || stores.book.read().bids(depth, aggregate.get());
    let asks = move || stores.book.read().asks(depth, aggregate.get());
    let scale = move || OrderBook::scale(&bids(), &asks());
    let mine = move || {
        let symbol = stores.selected.get();
        stores
            .orders
            .read()
            .iter()
            .filter(|order| order.symbol == symbol && order.status.is_live())
            .filter_map(|order| order.price)
            .collect::<Vec<_>>()
    };
    let interactive =
        move || market().is_some_and(|market| market.status.is_tradable()) && !stores.is_frozen();
    // Three separate facts, which used to be one. `is_ready()` was false both
    // while a snapshot was outstanding and after an empty snapshot had landed,
    // so a market with no orders in it shimmered forever.
    let awaiting = move || stores.book.read().is_awaiting_snapshot();
    let nothing = move || stores.book.read().is_empty();

    view! {
        <div
            class="book"
            style="flex:1 1 auto;overflow:hidden"
            class:book-inert=move || !interactive()
            class:frozen=move || stores.is_frozen()
        >
            <div class="book-head">
                <span>"Price"</span>
                <span>{move || format!("Size ({})", base())}</span>
                <span>"Total"</span>
            </div>
            <Show
                when=move || !awaiting() && !nothing()
                fallback=move || if awaiting() {
                    view! { <crate::ui::SkeletonRows rows=9 /> }.into_any()
                } else {
                    view! {
                        <crate::ui::EmptyState
                            title="No orders resting yet"
                            body="Every limit order that does not fill immediately appears here, \
                                  best price nearest the middle. Place one from the ticket and \
                                  it will be the top of the book."
                        />
                    }.into_any()
                }
            >
                // Asks sit above the spread and are drawn worst-first, so the
                // best ask is the row immediately above it.
                {move || {
                    let rows: Vec<BookRow> = asks().into_iter().rev().collect();
                    view! {
                        <Levels
                            rows=rows
                            scale=scale()
                            side="ask"
                            price_decimals=price_dp()
                            size_decimals=size_dp()
                            mine=mine()
                            interactive=interactive()
                        />
                    }
                }}
                <Spread />
                {move || view! {
                    <Levels
                        rows=bids()
                        scale=scale()
                        side="bid"
                        price_decimals=price_dp()
                        size_decimals=size_dp()
                        mine=mine()
                        interactive=interactive()
                    />
                }}
            </Show>
        </div>
    }
}

/// The band between the sides: the last price with its direction, and the gap.
#[component]
fn Spread() -> impl IntoView {
    let stores = use_stores();
    let price_dp = move || stores.market().map_or(2, |market| market.price_decimals);
    let quote = move || stores.quote();

    view! {
        <div class="ob-spread">
            <span
                class="last num"
                class:up=move || quote().is_some_and(|q| q.change_pct >= Decimal::ZERO)
                class:down=move || quote().is_some_and(|q| q.change_pct < Decimal::ZERO)
                class:stale-rule=move || stores.is_stale()
            >
                {move || quote().map_or_else(
                    || "—".to_string(),
                    |quote| format::price(quote.last, price_dp()),
                )}
                <span class="dir">
                    {move || quote()
                        .and_then(|quote| format::direction_glyph(quote.change_pct))
                        .map_or_else(String::new, |glyph| glyph.to_string())}
                </span>
            </span>
            <span class="gap">
                {move || {
                    let book = stores.book.read();
                    match (book.spread(), book.spread_bps()) {
                        (Some(spread), Some(bps)) => view! {
                            <>
                                "Spread "
                                <b class="num" style="color:var(--text-primary)">
                                    {format::fixed(spread, price_dp())}
                                </b>
                                " \u{00B7} "
                                {format::fixed(bps, 2)}
                                "\u{00A0}bp"
                            </>
                        }.into_any(),
                        _ => view! { <>"Spread \u{2014}"</> }.into_any(),
                    }
                }}
            </span>
        </div>
    }
}

/// The badge a market's status earns in the strip and the market list.
#[must_use]
pub fn status_badge_class(status: MarketStatus) -> &'static str {
    match status {
        MarketStatus::Trading => "badge live",
        MarketStatus::Halted => "badge warn",
        MarketStatus::Settling => "badge pending",
        MarketStatus::Expired => "badge",
    }
}

/// The explanation that replaces the order ticket when a market is not trading.
#[must_use]
pub fn not_trading_reason(market: &Market) -> (&'static str, String) {
    match market.status {
        MarketStatus::Halted => (
            "This market is halted",
            "New orders are not accepted. Resting orders are untouched and can still be \
             cancelled, and positions still accrue funding."
                .to_string(),
        ),
        MarketStatus::Settling => (
            "This market is settling",
            "It expired and the settlement transaction has not landed yet. Positions are \
             frozen at the settlement price and the realised amount appears in the ledger \
             within a block."
                .to_string(),
        ),
        MarketStatus::Expired => (
            "This market has settled",
            "It expired and every position was closed at the settlement price. The realised \
             amount is in your ledger."
                .to_string(),
        ),
        MarketStatus::Trading => ("", String::new()),
    }
}
