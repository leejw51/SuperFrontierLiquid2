//! The trading terminal.
//!
//! Reading order runs left to right from *what to trade*, through *what it has
//! done*, to *what it costs now*, to *what you are about to do*. The ticket sits
//! on the right because it is the end of that sentence and because it is where a
//! right hand rests.
//!
//! The layout collapses on **container queries** declared in the lifted CSS, not
//! on media queries, so the terminal behaves identically whether it fills a
//! monitor or sits in a split pane. The `container-type` lives on `.route-host`
//! in [`crate::App`].
//!
//! What never collapses, at any width: the live price with its direction, the
//! two-sided book, and the connection status.

use std::sync::Arc;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::api::Interval;
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::state::{use_stores, Quote, Toast, Tone};
use crate::ticket::{use_ticket, validate, Blocked};
use crate::ui::book::{not_trading_reason, status_badge_class, BookLadder};
use crate::ui::chart::CandleChart;
use crate::ui::{Banner, ChainRail, EmptyState, TopBar};

/// Which blotter tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blotter {
    /// Resting orders.
    Orders,
    /// Open positions.
    Positions,
    /// Executions.
    Fills,
    /// Every balance movement.
    Ledger,
}

/// The terminal screen.
#[component]
pub fn Terminal() -> impl IntoView {
    let stores = use_stores();
    let backend = expect_context::<Arc<Backend>>();

    // `/trade?side=buy` and `/trade?side=sell` are the menu's Buy and Sell
    // entries: the ticket opens on that side. The phone sheet is left to the
    // persistent Buy / Sell bar; opening it here drew a second ticket over
    // the desktop one and made the visible button unclickable.
    let sheet_open = RwSignal::new(false);
    match crate::config::query_param("side").as_deref() {
        Some("buy") => use_ticket().side.set(Side::Buy),
        Some("sell") => use_ticket().side.set(Side::Sell),
        _ => {}
    }

    let interval = RwSignal::new(Interval::M5);
    let candles = RwSignal::new(Vec::<Candle>::new());
    let candles_load = RwSignal::new(crate::state::Load::Loading);
    let hovered = RwSignal::new(None::<usize>);
    let show_tape_tab = RwSignal::new(false);

    {
        let backend = Arc::clone(&backend);
        Effect::new(move |_| {
            crate::load_candles(
                Arc::clone(&backend),
                stores.selected.get(),
                interval.get(),
                candles,
                candles_load,
            );
        });
    }

    let market = move || stores.market();
    let hazard = move || stores.hazard_position();

    view! {
        <div class="term">
            <TopBar />

            // The strip and the critical-risk banner share one grid row. `.term`
            // is a four-row grid — top bar, strip, body, rail — and an extra
            // child would push the rail out of its row and the body out of its
            // `1fr`.
            <div style="display:flex;flex-direction:column;min-height:0">
                <MarketStrip />
                // At `critical` the banner follows the user into the market
                // strip so it is visible from any screen. This is the loudest
                // the interface ever gets, and it is reserved for money about to
                // be lost.
                <Show when=move || hazard().is_some_and(|p| p.risk() == LiquidationRisk::Critical)>
                    {move || hazard().map(|position| view! {
                        <Banner
                            tone="hazard"
                            title=format!(
                                "{} is {}% from liquidation",
                                position.symbol,
                                format::fixed(
                                    ((position.mark_price - position.liquidation_price)
                                        / position.mark_price.max(Decimal::ONE)
                                        * Decimal::from(100)).abs(),
                                    2,
                                ),
                            )
                            body=format!(
                                "Mark {} against a liquidation price of {}.",
                                format::fixed(position.mark_price, 2),
                                format::fixed(position.liquidation_price, 2),
                            )
                        >
                            <a class="btn btn-secondary btn-sm" href="/portfolio">"Add margin"</a>
                            <a class="btn btn-sell btn-sm" href="/portfolio">"Reduce"</a>
                        </Banner>
                    })}
                </Show>
            </div>

            <div class="term-main">
                <MarketList />

                <div class="c-center">
                    <section class="panel spine-market">
                        <div class="panel-head">
                            <div class="seg" role="group" aria-label="Chart interval">
                                {Interval::ALL
                                    .into_iter()
                                    .map(|option| view! {
                                        <button
                                            aria-pressed=move || (interval.get() == option).to_string()
                                            on:click=move |_| interval.set(option)
                                        >
                                            {option.as_str()}
                                        </button>
                                    })
                                    .collect_view()}
                            </div>
                            <span class="chart-ohlc" style="font-size:var(--fs-xs);color:var(--text-muted)">
                                {move || {
                                    let series = candles.get();
                                    let candle = hovered
                                        .get()
                                        .and_then(|index| series.get(index).cloned())
                                        .or_else(|| series.last().cloned());
                                    let dp = market().map_or(2, |m| m.price_decimals);
                                    let sdp = market().map_or(4, |m| m.size_decimals);
                                    candle.map(|candle| view! {
                                        <>
                                            "O "<b class="num" style="color:var(--text-primary)">
                                                {format::price(candle.o, dp)}</b>"\u{00A0} "
                                            "H "<b class="num" style="color:var(--text-primary)">
                                                {format::price(candle.h, dp)}</b>"\u{00A0} "
                                            "L "<b class="num" style="color:var(--text-primary)">
                                                {format::price(candle.l, dp)}</b>"\u{00A0} "
                                            "C "<b class={if candle.c >= candle.o { "num up" } else { "num down" }}>
                                                {format::price(candle.c, dp)}</b>"\u{00A0} "
                                            "V "<b class="num" style="color:var(--text-primary)">
                                                {format::size(candle.v, sdp)}</b>
                                        </>
                                    })
                                }}
                            </span>
                            <span class="spacer"></span>
                            <span class="badge pending chart-opt">
                                {move || stores.quote().map_or_else(
                                    || "Mark \u{2014}".to_string(),
                                    |quote| format!(
                                        "Mark {}",
                                        format::price(quote.mark, market().map_or(2, |m| m.price_decimals)),
                                    ),
                                )}
                            </span>
                        </div>
                        <div class="cv-wrap" class:frozen=move || stores.is_frozen()>
                            // A 570px panel with the words "Waiting for candles"
                            // in its corner is a promise it cannot keep on a
                            // market that has never traded. This is the same
                            // empty state every other panel uses, and it says
                            // what would put a candle here.
                            <Show
                                when=move || !candles.read().is_empty()
                                fallback=move || if candles_load.get().is_loading() {
                                    view! { <crate::ui::SkeletonRows rows=6 /> }.into_any()
                                } else {
                                    view! {
                                        <EmptyState
                                            title="No candles yet"
                                            body="A candle is drawn for every interval in which \
                                                  something traded. Nothing has traded on this \
                                                  market, so there is nothing to plot."
                                        />
                                    }.into_any()
                                }
                            >
                            <CandleChart
                                candles=Signal::derive(move || candles.get())
                                market=Signal::derive(market)
                                mark=Signal::derive(move || stores.quote().map(|quote| quote.mark))
                                liquidation=Signal::derive(move || {
                                    let symbol = stores.selected.get();
                                    stores
                                        .positions
                                        .read()
                                        .iter()
                                        .find(|position| position.symbol == symbol)
                                        .map(|position| position.liquidation_price)
                                })
                                hovered=hovered
                            />
                            </Show>
                        </div>
                    </section>

                    <BlotterPanel />
                </div>

                <section class="panel spine-market c-book">
                    <div class="panel-head">
                        <h3>{move || if show_tape_tab.get() { "Recent trades" } else { "Order book" }}</h3>
                        // At 1440 and below the tape folds into this panel as a
                        // second tab: it is the panel you glance at least and the
                        // one that duplicates what the chart already says.
                        <button
                            class="tab"
                            data-tape-tab=""
                            aria-selected=move || show_tape_tab.get().to_string()
                            on:click=move |_| show_tape_tab.update(|on| *on = !*on)
                        >
                            {move || if show_tape_tab.get() { "Book" } else { "Trades" }}
                        </button>
                        <span class="spacer"></span>
                        <Show when=move || stores.is_resynced()>
                            <span class="badge live">"Resynced"</span>
                        </Show>
                        <Aggregation />
                    </div>
                    <Show
                        when=move || !show_tape_tab.get()
                        fallback=|| view! { <div class="panel-body"><TradeTape /></div> }
                    >
                        <BookLadder />
                    </Show>
                </section>

                <section class="panel spine-market c-tape">
                    <div class="panel-head"><h3>"Recent trades"</h3></div>
                    <div class="panel-body"><TradeTape /></div>
                </section>

                <section class="panel spine-bid c-entry">
                    <div class="panel-head">
                        <h3>{t("Order ticket")}</h3>
                        <span class="spacer"></span>
                        <span class="badge">{move || stores.selected.get()}</span>
                    </div>
                    <div class="panel-body"><OrderTicket /></div>
                </section>
            </div>

            <ChainRail />

            // The phone's persistent Buy / Sell bar, pinned above the rail. The
            // ticket leaves the flow entirely and becomes a sheet behind it.
            <div class="mobile-bar mobile-only">
                <button
                    class="btn btn-buy"
                    style="flex:1"
                    on:click=move |_| {
                        use_ticket().side.set(Side::Buy);
                        sheet_open.set(true);
                    }
                >
                    {t("Buy")}
                </button>
                <button
                    class="btn btn-sell"
                    style="flex:1"
                    on:click=move |_| {
                        use_ticket().side.set(Side::Sell);
                        sheet_open.set(true);
                    }
                >
                    {t("Sell")}
                </button>
            </div>
        </div>

        <Show when=move || sheet_open.get()>
            <div class="sheet" role="dialog" aria-modal="true" aria-label="Order ticket">
                <div class="panel-head">
                    <h3>"Order ticket"</h3>
                    <span class="spacer"></span>
                    <button class="btn btn-ghost btn-sm" on:click=move |_| sheet_open.set(false)>
                        {t("Close")}
                    </button>
                </div>
                <OrderTicket />
            </div>
        </Show>
    }
}

/// The market strip: the live price, and everything that qualifies it.
#[component]
fn MarketStrip() -> impl IntoView {
    let stores = use_stores();
    let market = move || stores.market();
    let quote = move || stores.quote();
    // The quote, but only once the market has actually traded. A last price of
    // zero is the absence of a price, not a very low one, and every figure
    // derived from it — the direction, the percentage, the glyph — is likewise
    // absent rather than neutral.
    let traded = move || stores.quote().filter(|quote| quote.last > Decimal::ZERO);
    let dp = move || market().map_or(2, |market| market.price_decimals);

    let funding_in = move || {
        let now = stores.now_ms.get() / 1_000;
        let next = now - now.rem_euclid(3_600) + 3_600;
        format::duration(next - now)
    };

    view! {
        <div class="strip">
            <MarketPicker />
            <div class="sym-block">
                <div class="sym-row">
                    <span class="sym">{move || stores.selected.get()}</span>
                    <span class="badge">
                        {move || market().map_or_else(String::new, |m| m.kind.label().to_string())}
                    </span>
                    <span class="badge">
                        {move || market().map_or_else(String::new, |m| m.asset_class.label().to_string())}
                    </span>
                    <span class=move || market().map_or("badge", |m| status_badge_class(m.status))>
                        {move || market().map_or_else(String::new, |m| m.status.label().to_string())}
                    </span>
                    // Stale is a brass rule and an age counter beside the price.
                    // The price itself keeps its last value: never blanked.
                    <Show when=move || stores.is_stale()>
                        <span class="badge warn">
                            {move || quote().map_or_else(
                                String::new,
                                |quote| format!(
                                    "Price {} old",
                                    format::age(quote.age_ms(stores.now_ms.get())),
                                ),
                            )}
                        </span>
                    </Show>
                </div>
                <div class="sym-row">
                    <span
                        class=move || {
                            let base = "price";
                            let direction = match traded().map(|q| q.change_pct) {
                                Some(pct) if pct > Decimal::ZERO => " up",
                                Some(pct) if pct < Decimal::ZERO => " down",
                                _ => "",
                            };
                            let tick = quote().map_or("", |q| q.tick_class());
                            format!("{base}{direction} {tick}")
                        }
                        class:stale-rule=move || stores.is_stale()
                        class:frozen=move || stores.is_frozen()
                        aria-live="off"
                    >
                        {move || traded().map_or_else(
                            || "\u{2014}".to_string(),
                            |quote| format::price(quote.last, dp()),
                        )}
                    </span>
                    <span style="display:flex;flex-direction:column;line-height:1.1">
                        // No trade, no direction. The sign and the glyph carry
                        // direction redundantly with the colour, and a market
                        // that has never traded has none of the three.
                        <Show
                            when=move || traded().is_some()
                            fallback=|| view! {
                                <span class="t-meta">"no trades yet"</span>
                            }
                        >
                            <crate::ui::Signed
                                value=Signal::derive(move || quote()
                                    .map_or(Decimal::ZERO, |q| q.change_pct))
                                percent=true
                                glyph=true
                            />
                            <span
                                class="num"
                                style="font-size:var(--fs-micro);color:var(--text-muted)"
                            >"24h"</span>
                        </Show>
                    </span>
                </div>
            </div>
            // `optional` and `optional2` are the reference's own hooks for what
            // the strip sheds as it narrows. What never collapses at any width
            // is the live price with its direction, the two-sided book and the
            // connection status.
            <span class="stat optional">
                <span class="k">"Mark"</span>
                <span class="v sm">
                    {move || quote()
                        .filter(|quote| quote.mark > Decimal::ZERO)
                        .map_or_else(
                            || "\u{2014}".to_string(), |q| format::price(q.mark, dp()))}
                </span>
            </span>
            <span class="stat optional">
                <span class="k">"Spread"</span>
                <span class="v sm">
                    {move || stores.book.read().spread().map_or_else(
                        || "\u{2014}".to_string(),
                        |spread| format::fixed(spread, dp()),
                    )}
                </span>
            </span>
            <span class="stat optional2">
                <span class="k">"Best bid"</span>
                <span class="v sm up">
                    {move || stores.book.read().best_bid().map_or_else(
                        || "\u{2014}".to_string(), |price| format::price(price, dp()))}
                </span>
            </span>
            <span class="stat optional2">
                <span class="k">"Best ask"</span>
                <span class="v sm down">
                    {move || stores.book.read().best_ask().map_or_else(
                        || "\u{2014}".to_string(), |price| format::price(price, dp()))}
                </span>
            </span>
            <Show when=move || market().is_some_and(|market| market.kind.is_derivative())>
                <span class="stat optional2">
                    <span class="k">{move || format!("Funding in {}", funding_in())}</span>
                    <span class="v sm">
                        {move || quote()
                            .and_then(|quote| quote.funding_rate)
                            .map_or_else(|| "\u{2014}".to_string(), format::funding_pct)}
                    </span>
                </span>
            </Show>
        </div>
    }
}

/// The market picker shown once the market list column collapses.
#[component]
fn MarketPicker() -> impl IntoView {
    let stores = use_stores();
    view! {
        <div class="select" data-market-picker="" style="width:auto">
            <select
                aria-label="Market"
                on:change=move |event| {
                    use wasm_bindgen::JsCast;
                    if let Some(select) = event
                        .target()
                        .and_then(|target| target.dyn_into::<web_sys::HtmlSelectElement>().ok())
                    {
                        stores.select_market(&select.value());
                    }
                }
            >
                <For
                    each=move || stores.markets.get()
                    key=|market| market.symbol.clone()
                    let:market
                >
                    <option
                        value=market.symbol.clone()
                        selected=move || stores.selected.get() == market.symbol
                    >
                        {format!("{} \u{00B7} {}", market.symbol, market.display_name)}
                    </option>
                </For>
            </select>
        </div>
    }
}

/// The market list, filtered across spot/perp/future and crypto/equity.
#[component]
fn MarketList() -> impl IntoView {
    let stores = use_stores();
    let class_filter = RwSignal::new(None::<AssetClass>);
    let kind_filter = RwSignal::new(None::<MarketKind>);

    let visible = move || {
        stores
            .markets
            .get()
            .into_iter()
            .filter(|market| {
                class_filter
                    .get()
                    .is_none_or(|want| market.asset_class == want)
            })
            .filter(|market| kind_filter.get().is_none_or(|want| market.kind == want))
            .collect::<Vec<_>>()
    };

    view! {
        <section class="panel spine-market c-markets">
            <div class="panel-head">
                <h3>"Markets"</h3>
                <span class="spacer"></span>
                <span class="badge">{move || visible().len().to_string()}</span>
            </div>
            <div style="padding:var(--sp-8);display:flex;flex-direction:column;gap:var(--sp-6);border-bottom:1px solid var(--border-subtle)">
                <div class="seg seg-fill" role="group" aria-label="Asset class">
                    <button
                        aria-pressed=move || class_filter.get().is_none().to_string()
                        on:click=move |_| class_filter.set(None)
                    >"All"</button>
                    <button
                        aria-pressed=move || (class_filter.get() == Some(AssetClass::Crypto)).to_string()
                        on:click=move |_| class_filter.set(Some(AssetClass::Crypto))
                    >"Crypto"</button>
                    <button
                        aria-pressed=move || (class_filter.get() == Some(AssetClass::Equity)).to_string()
                        on:click=move |_| class_filter.set(Some(AssetClass::Equity))
                    >"Equities"</button>
                </div>
                <div class="seg seg-fill" role="group" aria-label="Instrument">
                    <button
                        aria-pressed=move || kind_filter.get().is_none().to_string()
                        on:click=move |_| kind_filter.set(None)
                    >"All"</button>
                    <button
                        aria-pressed=move || (kind_filter.get() == Some(MarketKind::Spot)).to_string()
                        on:click=move |_| kind_filter.set(Some(MarketKind::Spot))
                    >"Spot"</button>
                    <button
                        aria-pressed=move || (kind_filter.get() == Some(MarketKind::Perp)).to_string()
                        on:click=move |_| kind_filter.set(Some(MarketKind::Perp))
                    >"Perp"</button>
                    <button
                        aria-pressed=move || (kind_filter.get() == Some(MarketKind::Future)).to_string()
                        on:click=move |_| kind_filter.set(Some(MarketKind::Future))
                    >"Fut"</button>
                </div>
            </div>
            <div class="panel-body">
                <For each=visible key=|market| market.symbol.clone() let:market>
                    {
                        let symbol = market.symbol.clone();
                        let dp = market.price_decimals;
                        let status = market.status;
                        let for_click = symbol.clone();
                        view! {
                            <button
                                class="mkt"
                                class:on=move || stores.selected.get() == symbol
                                class:halted=move || !status.is_tradable()
                                style="width:100%;text-align:left"
                                on:click=move |_| stores.select_market(&for_click)
                            >
                                <span class="ms">{market.symbol.clone()}</span>
                                <span class="mp num">
                                    {
                                        let symbol = market.symbol.clone();
                                        move || stores.quotes.read().get(&symbol)
                                            .filter(|quote| quote.last > Decimal::ZERO)
                                            .map_or_else(
                                                || "\u{2014}".to_string(),
                                                |quote| format::price(quote.last, dp),
                                            )
                                    }
                                </span>
                                <span class="mc">
                                    {
                                        let symbol = market.symbol.clone();
                                        move || {
                                            if !status.is_tradable() {
                                                return view! {
                                                    <span class=status_badge_class(status)>
                                                        {status.label()}
                                                    </span>
                                                }.into_any();
                                            }
                                            // A market that has never traded has
                                            // no change. `+0.00` in jade across
                                            // sixteen untraded markets says every
                                            // one of them is up.
                                            let quote = stores.quotes.read().get(&symbol).cloned();
                                            let Some(quote) = quote
                                                .filter(|quote| quote.last > Decimal::ZERO)
                                            else {
                                                return view! {
                                                    <span class="num dim">"\u{2014}"</span>
                                                }.into_any();
                                            };
                                            let change = quote.change_pct;
                                            view! {
                                                <crate::ui::Signed
                                                    value=Signal::derive(move || change)
                                                />
                                            }.into_any()
                                        }
                                    }
                                </span>
                            </button>
                        }
                    }
                </For>
            </div>
        </section>
    }
}

/// The aggregation pill: 0.10 / 1 / 10 / 100, on the market's own tick.
#[component]
fn Aggregation() -> impl IntoView {
    let stores = use_stores();
    let step = RwSignal::new(None::<Decimal>);
    provide_context(AggregationStep(step));
    let tick = move || {
        stores
            .market()
            .map_or(Decimal::ONE, |market| market.tick_size)
    };

    view! {
        <div class="select" style="width:auto">
            <select
                aria-label="Price aggregation"
                style="height:22px;font-size:var(--fs-micro);padding-right:22px"
                on:change=move |event| {
                    use wasm_bindgen::JsCast;
                    let value = event
                        .target()
                        .and_then(|target| target.dyn_into::<web_sys::HtmlSelectElement>().ok())
                        .map(|select| select.value())
                        .unwrap_or_default();
                    step.set(value.parse::<Decimal>().ok().filter(|v| *v > Decimal::ZERO));
                }
            >
                <option value="0">{move || format!("{} (raw)", tick())}</option>
                <option value="1">"1"</option>
                <option value="10">"10"</option>
                <option value="100">"100"</option>
            </select>
        </div>
    }
}

/// The chosen aggregation step, shared with the ladder.
#[derive(Debug, Clone, Copy)]
pub struct AggregationStep(pub RwSignal<Option<Decimal>>);

/// The trade tape, newest first.
#[component]
fn TradeTape() -> impl IntoView {
    let stores = use_stores();
    let dp = move || stores.market().map_or(2, |market| market.price_decimals);
    let sdp = move || stores.market().map_or(4, |market| market.size_decimals);

    view! {
        <div class="book-head" style="grid-template-columns:1fr 1fr auto">
            <span>"Price"</span>
            <span>"Size"</span>
            <span>"Time"</span>
        </div>
        <div class:frozen=move || stores.is_frozen()>
            <crate::ui::Region
                load=Signal::derive(move || stores.tape_load.get())
                empty=Signal::derive(move || stores.tape.read().is_empty())
                rows=10
                title="No trades yet"
                body="Every match on this market prints here the moment its block lands, newest \
                      first, with the side it traded on. Nothing has traded here so far."
            >
                {move || stores
                    .tape
                    .get()
                    .into_iter()
                    .take(40)
                    .map(|trade| {
                        let side = if trade.side == Side::Buy { "tape-row buy" } else { "tape-row sell" };
                        let at = format::parse_rfc3339(&trade.at)
                            .map_or_else(|| trade.at.clone(), format::clock);
                        view! {
                            <div class=side>
                                <span class="px">{format::price(trade.price, dp())}</span>
                                <span class="sz">{format::size(trade.qty, sdp())}</span>
                                <span class="at">{at}</span>
                            </div>
                        }
                    })
                    .collect_view()}
            </crate::ui::Region>
        </div>
    }
}

/// The blotter: open orders, positions, fills and the ledger.
#[component]
fn BlotterPanel() -> impl IntoView {
    let stores = use_stores();
    let backend = expect_context::<Arc<Backend>>();
    let tab = RwSignal::new(Blotter::Orders);
    let fills = RwSignal::new(Vec::<Fill>::new());
    let ledger = RwSignal::new(Vec::<LedgerEntry>::new());
    // A table that has not been fetched yet is not an empty table. Without
    // these the tab would flash "Nothing has moved yet" on every open, which is
    // the same lie as an endless skeleton, told faster.
    let fills_load = RwSignal::new(crate::state::Load::Loading);
    let ledger_load = RwSignal::new(crate::state::Load::Loading);

    {
        let backend = Arc::clone(&backend);
        Effect::new(move |_| {
            // Nobody is signed in, so there is nothing to fetch and nothing to
            // wait for: the tables are legitimately empty rather than pending.
            if stores.session.read().is_none() {
                fills_load.set(crate::state::Load::Ready);
                ledger_load.set(crate::state::Load::Ready);
                return;
            }
            let backend = Arc::clone(&backend);
            match tab.get() {
                Blotter::Fills => leptos::task::spawn_local(async move {
                    let response = backend.api.fills(240).await;
                    fills_load.set(crate::state::Load::from_result(&response));
                    if let Ok(response) = response {
                        fills.set(response.fills);
                    }
                }),
                Blotter::Ledger => leptos::task::spawn_local(async move {
                    let response = backend.api.ledger(260).await;
                    ledger_load.set(crate::state::Load::from_result(&response));
                    if let Ok(response) = response {
                        ledger.set(response.entries);
                    }
                }),
                _ => {}
            }
        });
    }

    let open_orders = move || {
        stores
            .orders
            .get()
            .into_iter()
            .filter(|order| order.status.is_live())
            .collect::<Vec<_>>()
    };

    let cancel_all = {
        let backend = Arc::clone(&backend);
        move |_| {
            let backend = Arc::clone(&backend);
            leptos::task::spawn_local(async move {
                match backend.api.cancel_all(None).await {
                    Ok(result) => {
                        stores.notify(
                            Tone::Good,
                            "Orders cancelled",
                            format!("{} resting orders withdrawn.", result.cancelled),
                        );
                        crate::refresh_account(Arc::clone(&backend), stores);
                    }
                    Err(error) => crate::report(&backend, stores, &error),
                }
            });
        }
    };

    view! {
        <section class="panel spine-account">
            <div class="tabbar" role="tablist">
                <TabButton tab=tab this=Blotter::Orders label="Open orders"
                    count=Signal::derive(move || Some(open_orders().len())) />
                <TabButton tab=tab this=Blotter::Positions label="Positions"
                    count=Signal::derive(move || Some(stores.positions.read().len())) />
                <TabButton tab=tab this=Blotter::Fills label="Fills"
                    count=Signal::derive(move || (!fills.read().is_empty()).then(|| fills.read().len())) />
                <TabButton tab=tab this=Blotter::Ledger label="Ledger"
                    count=Signal::derive(|| None) />
                <span style="margin-left:auto;display:flex;align-items:center;padding-right:var(--sp-10)">
                    <button
                        class="btn btn-ghost btn-sm"
                        disabled=move || open_orders().is_empty()
                        on:click=cancel_all
                    >
                        "Cancel all"
                    </button>
                </span>
            </div>
            {move || match tab.get() {
                Blotter::Orders => view! { <OrdersTable /> }.into_any(),
                Blotter::Positions => view! { <PositionsTable /> }.into_any(),
                Blotter::Fills => view! {
                    <FillsTable fills=fills load=Signal::derive(move || fills_load.get()) />
                }.into_any(),
                Blotter::Ledger => view! {
                    <LedgerTable entries=ledger load=Signal::derive(move || ledger_load.get()) />
                }.into_any(),
            }}
        </section>
    }
}

/// The identity of an order row, including the fields that change under it.
///
/// `For` rebuilds a row when its key changes and reuses it otherwise, so a key
/// of the order id alone would leave a `pending` row reading `pending` forever
/// after the socket said otherwise.
fn row_key(order: &Order) -> String {
    format!(
        "{}:{:?}:{}:{}",
        order.order_id, order.status, order.filled_qty, order.updated_at
    )
}

/// The identity of a position row, including the mark and the P&L that move.
fn position_key(position: &Position) -> String {
    format!(
        "{}:{}:{}:{}",
        position.symbol, position.qty, position.mark_price, position.unrealized_pnl
    )
}

/// One tab, with its live count in a pill so the label never reflows.
#[component]
fn TabButton(
    /// The signal holding the selection.
    tab: RwSignal<Blotter>,
    /// Which tab this button is.
    this: Blotter,
    /// The label.
    label: &'static str,
    /// The count, when there is one.
    #[prop(into)]
    count: Signal<Option<usize>>,
) -> impl IntoView {
    view! {
        <button
            class="tab"
            role="tab"
            aria-selected=move || (tab.get() == this).to_string()
            on:click=move |_| tab.set(this)
        >
            {label}
            {move || count.get().map(|count| view! { <span class="count">{count.to_string()}</span> })}
        </button>
    }
}

/// The open-orders table.
#[component]
fn OrdersTable() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let orders = move || stores.orders.get();

    view! {
        <div class="tbl-wrap">
            <Show
                when=move || !orders().is_empty()
                fallback=|| view! {
                    <EmptyState
                        title="No orders yet"
                        body="Orders you place that do not fill immediately will rest here until they do."
                    >
                        <span class="t-meta">"Place your first order from the ticket on the right."</span>
                    </EmptyState>
                }
            >
                <table class="tbl">
                    <thead>
                        <tr>
                            <th>"Market"</th><th>"Side"</th><th>"Type"</th>
                            <th class="n">"Price"</th><th class="n">"Size"</th><th class="n">"Filled"</th>
                            <th class="n">"Value"</th><th>"Time in force"</th><th>"Status"</th>
                            <th class="n">"Placed"</th><th></th>
                        </tr>
                    </thead>
                    <tbody>
                        // Keyed on what is drawn, not only on identity: a `For`
                        // re-renders a row when its key changes, and an order's
                        // status and filled quantity change under a stable id.
                        <For each=orders key=row_key let:order>
                            {
                                let id = order.order_id.clone();
                                let dp = stores.market_named(&order.symbol).map_or(2, |m| m.price_decimals);
                                let sdp = stores.market_named(&order.symbol).map_or(4, |m| m.size_decimals);
                                let filled = order.filled_pct();
                                // A market order has no price, so it has no
                                // value: `0.00` is not what a 120-lot sale was
                                // worth, and the Price cell beside it already
                                // says the honest thing with an em dash.
                                let value = order.price.map(|price| price * order.qty);
                                let placed = format::parse_rfc3339(&order.created_at)
                                    .map_or_else(|| order.created_at.clone(), format::clock);
                                let pending = order.status == OrderStatus::Pending;
                                let reject = order.reject_reason.clone();
                                let cancellable = order.status.is_live() && !pending;
                                view! {
                                    // A pending order appears immediately at 60%
                                    // opacity: it is real, it is just not in a
                                    // block yet. Cancel is disabled because there
                                    // is nothing yet to cancel.
                                    <tr style=if pending { "opacity:.62" } else { "" }>
                                        <td class="strong">{order.symbol.clone()}</td>
                                        <td>
                                            <span class=if order.side == Side::Buy {
                                                "side-badge long"
                                            } else {
                                                "side-badge short"
                                            }>{order.side.label()}</span>
                                        </td>
                                        <td>{order.order_type.label()}</td>
                                        <td class="n">
                                            {order.price.map_or_else(
                                                || "\u{2014}".to_string(),
                                                |price| format::price(price, dp),
                                            )}
                                        </td>
                                        <td class="n">{format::size(order.qty, sdp)}</td>
                                        <td class="n">
                                            <span class="fillbar" style=format!("--f:{filled}")></span>
                                            {format!("{filled}%")}
                                        </td>
                                        <td class="n">
                                            {value.map_or_else(
                                                || "\u{2014}".to_string(),
                                                |value| format::fixed(value, 2))}
                                        </td>
                                        <td>
                                            <span class="tag-caps">{order.tif.tag()}</span>
                                            {order.post_only.then_some(" \u{00B7} post only")}
                                            {order.reduce_only.then_some(" \u{00B7} reduce only")}
                                        </td>
                                        <td><span class=order.status.badge_class()>{order.status.label()}</span></td>
                                        <td class="n">{placed}</td>
                                        <td style="width:1%">
                                            {if cancellable {
                                                view! {
                                                    <button
                                                        class="btn btn-ghost btn-sm rowact"
                                                        on:click=move |_| {
                                                            let backend = backend.get_value();
                                                            let id = id.clone();
                                                            leptos::task::spawn_local(async move {
                                                                match backend.api.cancel_order(&id).await {
                                                                    Ok(_) => crate::refresh_account(
                                                                        Arc::clone(&backend), stores),
                                                                    Err(error) => crate::report(
                                                                        &backend, stores, &error),
                                                                }
                                                            });
                                                        }
                                                    >"Cancel"</button>
                                                }.into_any()
                                            } else if pending {
                                                view! {
                                                    <button class="btn btn-ghost btn-sm" disabled=true>
                                                        "Cancel"
                                                    </button>
                                                }.into_any()
                                            } else {
                                                view! {
                                                    <span class="t-meta" style="color:var(--ask)">
                                                        {reject.clone().unwrap_or_default()}
                                                    </span>
                                                }.into_any()
                                            }}
                                        </td>
                                    </tr>
                                }
                            }
                        </For>
                    </tbody>
                </table>
            </Show>
        </div>
    }
}

/// Open positions, with a proximity meter where the risk warrants one.
#[component]
pub fn PositionsTable() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let positions = move || stores.positions.get();

    view! {
        <div class="tbl" style="overflow:auto">
            <div class="pos-main pos-head">
                <span>"Market"</span><span>"Side"</span><span class="n">"Size"</span>
                <span class="n">"Entry"</span><span class="n">"Mark"</span>
                <span class="n">"Liq. price"</span><span class="n">"Unrealised"</span><span></span>
            </div>
            <Show
                when=move || !positions().is_empty()
                fallback=|| view! {
                    <EmptyState
                        title="No open positions"
                        body="A filled order on a perpetual or a future opens a position, and it appears here with its liquidation price."
                    />
                }
            >
                <For each=positions key=position_key let:position>
                    {
                        let symbol = position.symbol.clone();
                        let dp = stores.market_named(&position.symbol).map_or(2, |m| m.price_decimals);
                        let sdp = stores.market_named(&position.symbol).map_or(4, |m| m.size_decimals);
                        let risk = position.risk();
                        let proximity = position.liquidation_proximity();
                        // `settling` and `expired` both mean the market has
                        // stopped publishing a mark.
                        let settled = stores
                            .market_named(&position.symbol)
                            .is_some_and(|market| {
                                matches!(
                                    market.status,
                                    MarketStatus::Settling | MarketStatus::Expired
                                )
                            });
                        let account_risk = stores.risk();
                        // The meter is drawn only once the account itself is off
                        // its rest state. At `low` it is not drawn at all.
                        let show_meter = account_risk != LiquidationRisk::Low
                            && risk != LiquidationRisk::Low
                            && !settled;
                        let danger = matches!(risk, LiquidationRisk::High | LiquidationRisk::Critical);
                        view! {
                            <div class="pos">
                                <div class="pos-main">
                                    <span class="sym">
                                        {position.symbol.clone()}
                                        <span class="badge">
                                            {format!(
                                                "{}\u{00D7} {}",
                                                format::fixed(position.leverage, 0),
                                                position.margin_mode.label().to_lowercase(),
                                            )}
                                        </span>
                                    </span>
                                    <span>
                                        <span class=if position.side == PositionSide::Long {
                                            "side-badge long"
                                        } else {
                                            "side-badge short"
                                        }>
                                            <span class="dir">
                                                {if position.side == PositionSide::Long { "\u{25B2}" } else { "\u{25BC}" }}
                                            </span>
                                            {position.side.label()}
                                        </span>
                                    </span>
                                    <span class="n">{format::size(position.qty, sdp)}</span>
                                    <span class="n">{format::price(position.entry_price, dp)}</span>
                                    <span class="n">{format::price(position.mark_price, dp)}</span>
                                    <span class="n" style=if danger { "color:var(--ask)" } else { "" }>
                                        {format::price(position.liquidation_price, dp)}
                                    </span>
                                    // A settling or settled market has no mark
                                    // price left to compute an unrealised figure
                                    // from, so the cell says what is actually
                                    // true rather than showing a number that has
                                    // stopped meaning anything.
                                    {if settled {
                                        view! {
                                            <span class="n dim">"Settlement pending"</span>
                                        }
                                        .into_any()
                                    } else {
                                        view! {
                                            <span
                                                class="n"
                                                style={if position.unrealized_pnl >= Decimal::ZERO {
                                                    "color:var(--bid)"
                                                } else {
                                                    "color:var(--ask)"
                                                }}
                                            >
                                                {format::pnl(position.unrealized_pnl)}
                                                <span style="font-size:var(--fs-micro)">
                                                    {format!(
                                                        " ({})",
                                                        format::pct(position.unrealized_pnl_pct),
                                                    )}
                                                </span>
                                            </span>
                                        }
                                        .into_any()
                                    }}
                                    <span>
                                        <button
                                            class=if danger { "btn btn-danger btn-sm" } else { "btn btn-ghost btn-sm" }
                                            on:click=move |_| {
                                                let backend = backend.get_value();
                                                let symbol = symbol.clone();
                                                leptos::task::spawn_local(async move {
                                                    match backend.api.close_position(&symbol, None).await {
                                                        Ok(result) => {
                                                            stores.toast(Toast {
                                                                id: 0,
                                                                tone: Tone::Good,
                                                                title: format!("{symbol} closed"),
                                                                body: format!(
                                                                    "{} at {} for {}",
                                                                    format::fixed(result.closed_qty, 4),
                                                                    format::fixed(result.avg_price, 2),
                                                                    format::pnl(result.realized_pnl),
                                                                ),
                                                                height: None,
                                                                action: None,
                                                                expires_ms: Some(
                                                                    crate::clock::now_millis() + 5_000),
                                                            });
                                                            crate::refresh_account(
                                                                Arc::clone(&backend), stores);
                                                        }
                                                        Err(error) => crate::report(&backend, stores, &error),
                                                    }
                                                });
                                            }
                                        >
                                            "Close"
                                        </button>
                                    </span>
                                </div>
                                <Show when=move || show_meter>
                                    <div
                                        class="liqmeter"
                                        data-risk=risk.attr()
                                        style=format!("--risk:{proximity}")
                                        role="meter"
                                        aria-valuenow=proximity.to_string()
                                        aria-valuemin="0"
                                        aria-valuemax="100"
                                        aria-label=format!("{} distance to liquidation", position.symbol)
                                    >
                                        <i></i>
                                    </div>
                                </Show>
                            </div>
                        }
                    }
                </For>
            </Show>
        </div>
    }
}

/// The fills blotter, virtualised.
#[component]
fn FillsTable(
    /// The fills, newest first.
    fills: RwSignal<Vec<Fill>>,
    /// Whether they have been fetched yet.
    #[prop(into)]
    load: Signal<crate::state::Load>,
) -> impl IntoView {
    let rows = std::sync::Arc::new(move |index: usize| {
        let Some(fill) = fills.read().get(index).cloned() else {
            return ().into_any();
        };
        let at = format::parse_rfc3339(&fill.at).map_or_else(|| fill.at.clone(), format::clock);
        view! {
            <>
                <span class="strong">{fill.symbol.clone()}</span>
                <span>
                    <span class=if fill.side == Side::Buy { "side-badge long" } else { "side-badge short" }>
                        {fill.side.label()}
                    </span>
                </span>
                <span class="n">{format::fixed(fill.price, 2)}</span>
                <span class="n">{format::fixed(fill.qty, 4)}</span>
                <span class="n">{format::fixed(fill.fee, 2)}</span>
                <span>{fill.role.label()}</span>
                <span class="n">{format::pnl(fill.realized_pnl)}</span>
                <span class="n">{format::count(fill.height)}</span>
                <span class="n">{at}</span>
            </>
        }
        .into_any()
    });

    view! {
        <crate::ui::table::VirtualTable
            count=Signal::derive(move || fills.read().len())
            columns="1.2fr 70px 1fr 1fr .8fr .8fr 1fr 1fr .9fr"
            label="Fills"
            load=load
            empty_title="No fills yet"
            empty_body="Every execution lands here with its price, its fee and the block it \
                        settled in \u{2014} the receipt for each of your orders."
            row=rows
            header=std::sync::Arc::new(move || view! {
                <>
                    <span>"Market"</span><span>"Side"</span><span class="n">"Price"</span>
                    <span class="n">"Size"</span><span class="n">"Fee"</span><span>"Role"</span>
                    <span class="n">"Realised"</span><span class="n">"Block"</span>
                    <span class="n">"Time"</span>
                </>
            }.into_any())
        />
    }
}

/// The unified ledger, virtualised.
#[component]
pub fn LedgerTable(
    /// The entries, newest first.
    entries: RwSignal<Vec<LedgerEntry>>,
    /// Whether they have been fetched yet.
    #[prop(into)]
    load: Signal<crate::state::Load>,
) -> impl IntoView {
    let rows = std::sync::Arc::new(move |index: usize| {
        let Some(entry) = entries.read().get(index).cloned() else {
            return ().into_any();
        };
        let at = format::parse_rfc3339(&entry.at).map_or_else(|| entry.at.clone(), format::clock);
        let colour = if entry.amount.is_zero() {
            ""
        } else if entry.amount > Decimal::ZERO {
            "color:var(--bid)"
        } else {
            "color:var(--ask)"
        };
        view! {
            <>
                <span class="strong">{entry.kind.label()}</span>
                <span>{entry.memo.clone()}</span>
                <span>{entry.asset.clone()}</span>
                <span class="n" style=colour>{format::pnl(entry.amount)}</span>
                <span class="n">{format::fixed(entry.balance_after, 2)}</span>
                // The contract's ledger entry carries no height, so anything but
                // an em dash here is invented — and a zero under a caption
                // promising every movement is traceable to a block is the
                // caption falsifying itself. The transaction hash beside it is
                // real and does the traceability on its own.
                <span class="n">
                    {if entry.height > 0 {
                        format::count(entry.height)
                    } else {
                        "\u{2014}".to_string()
                    }}
                </span>
                <span class="n">{at}</span>
                <span class="n dim">{format::hash(&entry.tx_hash)}</span>
            </>
        }
        .into_any()
    });

    view! {
        <crate::ui::table::VirtualTable
            count=Signal::derive(move || entries.read().len())
            columns="1.1fr 2fr .6fr 1fr 1fr .9fr .9fr 1.1fr"
            label="Ledger"
            load=load
            empty_title="Nothing has moved yet"
            empty_body="Faucet grants, fills, funding and liquidations all move a balance, and \
                        each one lands here with the transaction that carried it. Claim from the \
                        faucet and the first line appears."
            row=rows
            header=std::sync::Arc::new(move || view! {
                <>
                    <span>"Kind"</span><span>"Market or memo"</span><span>"Asset"</span>
                    <span class="n">"Amount"</span><span class="n">"Balance after"</span>
                    <span class="n">"Block"</span><span class="n">"Time"</span>
                    <span>"Transaction"</span>
                </>
            }.into_any())
        />
    }
}

/// The order ticket.
#[component]
fn OrderTicket() -> impl IntoView {
    let stores = use_stores();
    let ticket = use_ticket();
    // Held in a `StoredValue` rather than moved: the ticket lives inside a
    // `Show`, whose children must be `Fn`, so nothing in here may consume a
    // captured value.
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());

    let market = move || stores.market();
    let dp = move || market().map_or(2, |market| market.price_decimals);
    let sdp = move || market().map_or(4, |market| market.size_decimals);
    let is_derivative = move || market().is_some_and(|market| market.kind.is_derivative());
    let max_leverage = move || {
        market()
            .and_then(|market| market.max_leverage)
            .and_then(|value| u32::try_from(value.trunc()).ok())
            .unwrap_or(1)
            .max(1)
    };
    // What the order can spend: free margin on a derivative, the free balance
    // of the asset the order pays with on spot. See `ticket::validate`.
    let available = move || {
        let account = stores.account.read();
        let Some(account) = account.as_ref() else {
            return Decimal::ZERO;
        };
        match market() {
            Some(market) if !market.kind.is_derivative() => {
                crate::ticket::spendable(account, &market, ticket.side.get())
            }
            _ => account.portfolio.available_margin,
        }
    };
    let reference = move || {
        stores
            .quote()
            .map_or(Decimal::ZERO, |quote: Quote| quote.last)
    };

    let check = move || {
        let Some(market) = market() else {
            return Err(Blocked::Empty);
        };
        validate(
            &market,
            ticket.side.get(),
            ticket.order_type.get(),
            ticket.price_value(),
            ticket.qty_value(),
            Decimal::from(ticket.leverage.get()),
            available(),
            reference(),
        )
    };
    // What the order is worth, computed from what is typed and from nothing
    // else. It used to be `check().unwrap_or(ZERO)`, which meant that an order
    // the account could not afford — the commonest state on a fresh chain —
    // reported its own value as `0.00 USD`, four lines above a hint correctly
    // saying it needed 600.00 of margin for it.
    //
    // `None` is "nothing has been typed yet", and renders as an em dash. A
    // zero here would be a number, and there is no number yet.
    let notional = move || {
        let qty = ticket.qty_value().filter(|qty| *qty > Decimal::ZERO)?;
        let price = match ticket.order_type.get() {
            OrderType::Market => reference(),
            OrderType::Limit => ticket.price_value().unwrap_or_else(reference),
        };
        (price > Decimal::ZERO).then(|| price * qty)
    };
    let initial_margin = move || {
        let notional = notional()?;
        if is_derivative() {
            Some(notional / Decimal::from(ticket.leverage.get().max(1)))
        } else {
            Some(notional)
        }
    };
    let signed_in = move || stores.session.read().is_some();
    let frozen = move || stores.is_frozen();
    let can_send = move || check().is_ok() && signed_in() && !frozen() && !ticket.sending.get();

    // The hint under a blocked button is drawn from the same fields the
    // gateway's `details` object carries, so the guard and the rejection say the
    // identical thing.
    let hint = move || {
        if !signed_in() {
            return "Sign in to place an order".to_string();
        }
        if frozen() {
            return "The feed is down, so the price on screen may not be the price you get"
                .to_string();
        }
        match check() {
            Ok(_) => String::new(),
            Err(blocked) => blocked.hint(sdp(), dp()),
        }
    };

    let submit = move |_| {
        if !can_send() {
            return;
        }
        let Some(market) = market() else { return };
        let Some(qty) = ticket.qty_value() else {
            return;
        };
        let request = PlaceOrderRequest {
            symbol: market.symbol.clone(),
            side: ticket.side.get_untracked(),
            order_type: ticket.order_type.get_untracked(),
            price: (ticket.order_type.get_untracked() == OrderType::Limit)
                .then(|| ticket.price_value())
                .flatten(),
            qty,
            tif: ticket.tif.get_untracked(),
            post_only: ticket.post_only.get_untracked(),
            reduce_only: ticket.reduce_only.get_untracked(),
            leverage: market
                .kind
                .is_derivative()
                .then(|| Decimal::from(ticket.leverage.get_untracked())),
            margin_mode: market
                .kind
                .is_derivative()
                .then(|| ticket.margin_mode.get_untracked()),
            client_order_id: format!("cli_{:x}", crate::clock::now_millis()),
        };
        let backend = backend.get_value();
        ticket.sending.set(true);
        leptos::task::spawn_local(async move {
            let result = backend.api.place_order(&request).await;
            ticket.sending.set(false);
            match result {
                Ok(response) => {
                    stores.toast(Toast {
                        id: 0,
                        tone: Tone::Info,
                        title: "Order placed".to_string(),
                        body: format!(
                            "Submitted as {}. It appears in the book at the next block.",
                            format::hash(&response.tx_hash),
                        ),
                        height: None,
                        action: None,
                        expires_ms: Some(crate::clock::now_millis() + 5_000),
                    });
                    crate::refresh_account(Arc::clone(&backend), stores);
                }
                Err(error) => crate::report(&backend, stores, &error),
            }
        });
    };

    view! {
        {move || {
            let Some(market) = market().filter(|market| !market.status.is_tradable()) else {
                return ().into_any();
            };
            let (title, body) = not_trading_reason(&market);
            view! {
                <div style="padding:var(--sp-12)">
                    <Banner tone="warn" title=title body=body />
                </div>
            }
            .into_any()
        }}

        <Show when=move || market().is_some_and(|market| market.status.is_tradable())>
            <div class="ticket">
                <div class="seg seg-side seg-fill" role="group" aria-label="Side">
                    <button
                        data-side="buy"
                        aria-pressed=move || (ticket.side.get() == Side::Buy).to_string()
                        on:click=move |_| ticket.side.set(Side::Buy)
                    >{t("Buy / Long")}</button>
                    <button
                        data-side="sell"
                        aria-pressed=move || (ticket.side.get() == Side::Sell).to_string()
                        on:click=move |_| ticket.side.set(Side::Sell)
                    >{t("Sell / Short")}</button>
                </div>
                <div class="seg seg-fill" role="group" aria-label="Order type">
                    <button
                        aria-pressed=move || (ticket.order_type.get() == OrderType::Limit).to_string()
                        on:click=move |_| ticket.order_type.set(OrderType::Limit)
                    >{t("Limit")}</button>
                    <button
                        aria-pressed=move || (ticket.order_type.get() == OrderType::Market).to_string()
                        on:click=move |_| ticket.order_type.set(OrderType::Market)
                    >{t("Market")}</button>
                </div>

                <Show when=is_derivative>
                    <div style="display:flex;gap:var(--sp-8)">
                        <div class="seg" style="flex:1" role="group" aria-label="Margin mode">
                            <button
                                style="flex:1"
                                aria-pressed=move || (ticket.margin_mode.get() == MarginMode::Cross).to_string()
                                on:click=move |_| ticket.margin_mode.set(MarginMode::Cross)
                            >"Cross"</button>
                            <button
                                style="flex:1"
                                aria-pressed=move || (ticket.margin_mode.get() == MarginMode::Isolated).to_string()
                                on:click=move |_| ticket.margin_mode.set(MarginMode::Isolated)
                            >"Isolated"</button>
                        </div>
                    </div>
                    <div class="field">
                        <label for="tk-lev">
                            "Leverage "
                            <span class="num" style="color:var(--text-primary);font-weight:600">
                                {move || format!("{}\u{00D7}", ticket.leverage.get())}
                            </span>
                        </label>
                        <input
                            class="slider"
                            id="tk-lev"
                            type="range"
                            min="1"
                            max=move || max_leverage().to_string()
                            prop:value=move || ticket.leverage.get().to_string()
                            style=move || format!(
                                "--pct:{}%",
                                ticket.leverage.get() * 100 / max_leverage().max(1),
                            )
                            on:input=move |event| {
                                if let Ok(value) = number_value(&event).parse::<u32>() {
                                    ticket.leverage.set(value.max(1));
                                }
                            }
                        />
                        <div class="slider-ticks">
                            <span>"1\u{00D7}"</span>
                            <span>{move || format!("{}\u{00D7}", max_leverage() / 2)}</span>
                            <span>{move || format!("{}\u{00D7}", max_leverage())}</span>
                        </div>
                    </div>
                </Show>

                <Show when=move || ticket.order_type.get() == OrderType::Limit>
                    <div class="field">
                        <label for="tk-price">{t("Limit price")}</label>
                        <div class="numfield">
                            <input
                                id="tk-price"
                                inputmode="decimal"
                                prop:value=move || ticket.price.get()
                                on:input=move |event| ticket.price.set(number_value(&event))
                            />
                            <span class="unit">
                                {move || market().map_or_else(String::new, |market| market.quote)}
                            </span>
                            <span class="step">
                                <button
                                    aria-label="Increase price by one tick"
                                    on:click=move |_| step_price(ticket, market(), true)
                                >"\u{25B2}"</button>
                                <button
                                    aria-label="Decrease price by one tick"
                                    on:click=move |_| step_price(ticket, market(), false)
                                >"\u{25BC}"</button>
                            </span>
                        </div>
                        <span class="hint">
                            {move || market().map_or_else(String::new, |market| format!(
                                "Tick {} \u{00B7} minimum {} {}",
                                format::fixed(market.tick_size, market.price_decimals),
                                format::fixed(market.min_notional, 2),
                                market.quote,
                            ))}
                        </span>
                    </div>
                </Show>

                <div class="field">
                    <label for="tk-qty">{t("Quantity")}</label>
                    <div class="numfield">
                        <input
                            id="tk-qty"
                            inputmode="decimal"
                            prop:value=move || ticket.qty.get()
                            on:input=move |event| ticket.qty.set(number_value(&event))
                        />
                        <span class="unit">
                            {move || market().map_or_else(String::new, |market| market.base)}
                        </span>
                        <span class="step">
                            <button
                                aria-label="Increase size by one lot"
                                on:click=move |_| step_qty(ticket, market(), true)
                            >"\u{25B2}"</button>
                            <button
                                aria-label="Decrease size by one lot"
                                on:click=move |_| step_qty(ticket, market(), false)
                            >"\u{25BC}"</button>
                        </span>
                    </div>
                </div>

                <div class="seg seg-fill" role="group" aria-label="Size as a share of available margin">
                    {[25_u32, 50, 75, 100]
                        .into_iter()
                        .map(|percent| view! {
                            <button
                                on:click=move |_| {
                                    let price = if ticket.order_type.get() == OrderType::Market {
                                        reference()
                                    } else {
                                        ticket.price_value().unwrap_or_else(reference)
                                    };
                                    if price <= Decimal::ZERO { return; }
                                    let budget = available() * Decimal::from(percent)
                                        / Decimal::from(100)
                                        * Decimal::from(ticket.leverage.get().max(1));
                                    let Some(market) = market() else { return };
                                    let raw = budget / price;
                                    let lots = (raw / market.lot_size).floor();
                                    ticket.qty.set(format::fixed(
                                        lots * market.lot_size,
                                        market.size_decimals,
                                    ));
                                }
                            >
                                {if percent == 100 { "Max".to_string() } else { format!("{percent}%") }}
                            </button>
                        })
                        .collect_view()}
                </div>

                <div style="display:flex;gap:var(--sp-16)">
                    // Post only on a market order is a contradiction: it says
                    // refuse to take liquidity, and taking liquidity is the
                    // whole of what a market order does. The gateway rejects
                    // it, so the control does not offer it.
                    <label
                        class="switch"
                        title=move || if ticket.order_type.get() == OrderType::Market {
                            "A market order takes liquidity by definition, so it cannot be \
                             post only"
                        } else {
                            "Refuse the order rather than take liquidity"
                        }
                    >
                        <input
                            type="checkbox"
                            disabled=move || ticket.order_type.get() == OrderType::Market
                            prop:checked=move || ticket.post_only.get()
                                && ticket.order_type.get() != OrderType::Market
                            on:change=move |event| ticket.post_only.set(checkbox(&event))
                        />
                        <span class="track"></span>
                        "Post only"
                    </label>
                    <label class="switch">
                        <input
                            type="checkbox"
                            prop:checked=move || ticket.reduce_only.get()
                            on:change=move |event| ticket.reduce_only.set(checkbox(&event))
                        />
                        <span class="track"></span>
                        "Reduce only"
                    </label>
                </div>

                <hr />
                <div class="row">
                    <span class="k">"Order value"</span>
                    <span class="v">
                        {move || match notional() {
                            Some(value) => format!(
                                "{} {}",
                                format::fixed(value, 2),
                                market().map_or_else(String::new, |m| m.quote),
                            ),
                            None => "\u{2014}".to_string(),
                        }}
                    </span>
                </div>
                <Show when=is_derivative>
                    <div class="row">
                        <span class="k">"Initial margin"</span>
                        <span class="v">
                            {move || initial_margin().map_or_else(
                                || "\u{2014}".to_string(), |value| format::fixed(value, 2))}
                        </span>
                    </div>
                </Show>
                <div class="row">
                    <span class="k">
                        {move || market().map_or_else(String::new, |market| format!(
                            "{} fee {}",
                            if ticket.post_only.get() { "Maker" } else { "Taker" },
                            format::pct(Decimal::from(if ticket.post_only.get() {
                                market.maker_fee_bps
                            } else {
                                market.taker_fee_bps
                            }) / Decimal::from(100)),
                        ))}
                    </span>
                    <span class="v">
                        {move || market().map_or_else(String::new, |market| {
                            let bps = if ticket.post_only.get() {
                                market.maker_fee_bps
                            } else {
                                market.taker_fee_bps
                            };
                            let Some(notional) = notional() else {
                                return "\u{2014}".to_string();
                            };
                            let fee =
                                (notional * Decimal::from(bps) / Decimal::from(10_000)).round_dp(2);
                            // A fee that rounds to nothing is nothing, and
                            // `-0.00` is not a smaller number than `0.00`.
                            if fee.is_zero() {
                                format::fixed(Decimal::ZERO, 2)
                            } else {
                                format::pnl(-fee)
                            }
                        })}
                    </span>
                </div>

                <button
                    class=move || if ticket.side.get() == Side::Buy {
                        "btn btn-buy btn-block"
                    } else {
                        "btn btn-sell btn-block"
                    }
                    style="height:42px;margin-top:var(--sp-4)"
                    disabled=move || !can_send()
                    data-loading=move || ticket.sending.get().then_some("")
                    aria-describedby="tk-hint"
                    on:click=submit
                >
                    {move || {
                        let verb = if ticket.side.get() == Side::Buy { t("Buy") } else { t("Sell") };
                        match ticket.qty_value() {
                            Some(qty) => format!(
                                "{verb} {} {}",
                                format::size(qty, sdp()),
                                market().map_or_else(String::new, |m| m.base),
                            ),
                            None => verb.to_string(),
                        }
                    }}
                </button>
                // A disabled trading button never hides its reason.
                <span class="hint" id="tk-hint" style="text-align:center">{hint}</span>

                <div class="row">
                    <span class="k">"Available margin"</span>
                    <span class="v">
                        {move || if stores.account.read().is_some() {
                            format::fixed(available(), 2)
                        } else {
                            // Nobody is signed in, so nothing is known. A zero
                            // here reads as an account with no money in it.
                            "\u{2014}".to_string()
                        }}
                    </span>
                </div>
                <div class="row">
                    <span class="k">"Account equity"</span>
                    <span class="v">
                        {move || stores.account.read().as_ref().map_or_else(
                            || "\u{2014}".to_string(),
                            |account| format::fixed(account.portfolio.equity, 2),
                        )}
                    </span>
                </div>
            </div>
        </Show>
    }
}

/// Steps the price by the market's own tick, never by one.
fn step_price(ticket: crate::ticket::Ticket, market: Option<Market>, up: bool) {
    let Some(market) = market else { return };
    let current = ticket.price_value().unwrap_or_default();
    let next = if up {
        current + market.tick_size
    } else {
        (current - market.tick_size).max(Decimal::ZERO)
    };
    ticket.price.set(format::fixed(next, market.price_decimals));
}

/// Steps the size by the market's own lot, never by one.
fn step_qty(ticket: crate::ticket::Ticket, market: Option<Market>, up: bool) {
    let Some(market) = market else { return };
    let current = ticket.qty_value().unwrap_or_default();
    let next = if up {
        current + market.lot_size
    } else {
        (current - market.lot_size).max(Decimal::ZERO)
    };
    ticket.qty.set(format::fixed(next, market.size_decimals));
}

/// Reads an input's value from an event.
fn number_value(event: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}

/// Reads a checkbox from an event.
fn checkbox(event: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
        .is_some_and(|input| input.checked())
}
