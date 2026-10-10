//! SuperFrontierLiquid's web client.
//!
//! A Leptos client-side-rendered application compiled to WebAssembly. It is the
//! consumer half of the seam described in `docs/api-contract.md`, and it is
//! built to the letter of `docs/design/reference.html` and
//! `docs/design-system.md`.
//!
//! # How it is put together
//!
//! | Layer | Where | What it is responsible for |
//! |---|---|---|
//! | Wire types | [`api::types`] | One Rust type per documented shape, money as [`rust_decimal::Decimal`] |
//! | Gateway | [`api`] | The [`api::Api`] trait, with a live and a mock implementation |
//! | Identity | [`wallet`] | BIP-39 derivation, EIP-55 addresses, EIP-191 signing, all in the browser |
//! | Feed | [`ws`] | One socket, reconnected with backoff, resubscribed and resnapshotted |
//! | Book | [`ws::book`] | Snapshot and delta application, including removal at quantity zero |
//! | State | [`state`] | The signals every component reads instead of fetching |
//! | Formatting | [`format`] | The one implementation of every numeric rule in the design system |
//! | Views | [`ui`], [`screens`] | The lifted design system, driven by the stores |
//!
//! # The two rules this crate is strictest about
//!
//! **No money value is ever an `f64`.** Prices, sizes, balances and P&L are
//! `Decimal` from the moment they are parsed to the moment they are formatted.
//! The single conversion to `f64` is [`ui::canvas::px`], which produces a canvas
//! coordinate and is documented at the call site.
//!
//! **No `unwrap` outside tests.** In WebAssembly a panic is a blank page and a
//! line in a console nobody has open, which is the worst failure mode available.
//! Every fallible path either renders a state that says what happened or falls
//! back to something visibly incomplete rather than absent.

#![warn(missing_docs)]
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::must_use_candidate)]

pub mod api;
pub mod clock;
pub mod config;
pub mod format;
pub mod i18n;
pub mod nav;
pub mod prefs;
pub mod screens;
pub mod state;
pub mod ticket;
pub mod ui;
pub mod wallet;
pub mod ws;

use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;

use gloo_storage::{LocalStorage, Storage};

use crate::api::{Interval, OrderFilter};
use crate::config::{Backend, SESSION_TOKEN_KEY};
use crate::state::{Stores, Tone};
use crate::ws::msg::{orderbook_channel, trades_channel};
use crate::ws::{FeedHandle, ServerMessage};

/// How many book levels to ask for. The design's book shows nine a side; the
/// rest feed the depth curve and the cumulative totals.
const BOOK_DEPTH: u32 = 50;

/// How many candles the chart loads.
const CANDLE_COUNT: u32 = 120;

/// How many trades the tape loads before the socket takes over.
const TAPE_COUNT: u32 = 40;

/// Boots the application.
///
/// Called from `main`. Applies the stored theme and density **before** mounting,
/// so the first paint is already in the user's chosen theme rather than flashing
/// through the default.
pub fn run() {
    console_error_panic_hook::set_once();
    prefs::apply_stored();
    i18n::apply_stored();

    let backend = Arc::new(config::backend());
    let stores = Stores::new();

    // Mounted into `#cb-root` rather than appended to `<body>`, so the app sits
    // inside the element the stylesheet gives a full-height box to. Appending to
    // the body would leave it below an empty 100%-tall placeholder.
    let root = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("cb-root"))
        .and_then(|element| {
            use wasm_bindgen::JsCast;
            element.dyn_into::<web_sys::HtmlElement>().ok()
        });

    let view = move || {
        provide_context(Arc::clone(&backend));
        state::provide_stores(stores);
        ticket::provide_ticket(ticket::Ticket::new());

        // One timer for the whole application. Every age counter, countdown and
        // staleness check reads `now_ms`, so nothing else needs an interval.
        let tick = gloo_timers::callback::Interval::new(1_000, move || {
            stores.now_ms.set(clock::now_millis());
            stores.expire_toasts();
        });
        tick.forget();

        let handle = start_feed(Arc::clone(&backend), stores);
        provide_context(handle.clone());
        bootstrap(Arc::clone(&backend), stores, handle);

        view! { <App /> }
    };

    match root {
        Some(root) => leptos::mount::mount_to(root, view).forget(),
        // Reachable only if index.html has been edited. Appending to the body
        // still renders the application, which beats a blank page.
        None => leptos::mount::mount_to_body(view),
    }
}

/// Starts the feed and wires its events into the stores.
fn start_feed(backend: Arc<Backend>, stores: Stores) -> FeedHandle {
    let channels = vec![
        "ticker".to_string(),
        "chain".to_string(),
        orderbook_channel(&stores.selected.get_untracked()),
        trades_channel(&stores.selected.get_untracked()),
    ];
    let token = LocalStorage::get::<String>(SESSION_TOKEN_KEY).ok();

    ws::start(
        Arc::clone(&backend.transport),
        token,
        channels,
        move |event| {
            // A socket-level `UNAUTHORIZED` means the stored token is dead.
            // Clearing it here keeps the one rule the session has: a 401 signs
            // the user out rather than leaving a broken session on screen.
            if let ws::FeedEvent::Message(ServerMessage::Error(error)) = &event {
                if error.code == api::ErrorCode::Unauthorized {
                    sign_out(&backend, stores);
                }
            }
            stores.apply_feed(&event);
            // A balance update carries balances and nothing else; the equity
            // in the top bar, the margin figures and the ledger are all
            // derived by the gateway. Re-read the account so a faucet grant
            // or a fill shows up everywhere it should, not only in the one
            // list the frame happened to carry.
            if let ws::FeedEvent::Message(ServerMessage::BalanceUpdate(_)) = &event {
                refresh_account(Arc::clone(&backend), stores);
            }
        },
    )
}

/// Clears the session: storage, the gateway's token, and the stores.
pub fn sign_out(backend: &Backend, stores: Stores) {
    LocalStorage::delete(SESSION_TOKEN_KEY);
    backend.api.set_token(None);
    stores.session.set(None);
    stores.account.set(None);
    stores.orders.set(Vec::new());
    stores.positions.set(Vec::new());
}

/// Handles an API error uniformly: a `401` signs out, everything else toasts.
pub fn report(backend: &Backend, stores: Stores, error: &api::ApiError) {
    if error.is_unauthorized() {
        sign_out(backend, stores);
        stores.notify(
            Tone::Warn,
            "Signed out",
            "That session is no longer valid. Sign in again to keep trading.",
        );
        return;
    }
    stores.reject(error);
}

/// A failure as a mutating control shows it: a consequence, and the gateway's
/// own sentence.
///
/// Used by every dialog that changes something, so that a request which does
/// not land says so in the dialog rather than nowhere.
#[must_use]
pub fn failure_text(error: &api::ApiError) -> (String, String) {
    (error.title().to_string(), error.message())
}

/// Loads everything the first paint needs, then keeps the per-market data in
/// step with the selected market.
fn bootstrap(backend: Arc<Backend>, stores: Stores, handle: FeedHandle) {
    // --- public data, which every visitor sees whether signed in or not.
    {
        let backend = Arc::clone(&backend);
        leptos::task::spawn_local(async move {
            // Every one of these records whether it answered. A request that
            // fails and leaves its panel shimmering is the same defect as
            // "empty looks like loading" wearing a different hat: the screen
            // promises data that is never coming.
            let markets = backend.api.markets().await;
            stores.markets_load.set(state::Load::from_result(&markets));
            match markets {
                Ok(response) => stores.markets.set(response.markets),
                Err(error) => stores.reject(&error),
            }
            let status = backend.api.chain_status().await;
            stores.chain_load.set(state::Load::from_result(&status));
            if let Ok(status) = status {
                stores.chain.set(Some(status));
            }
            let blocks = backend.api.blocks(state::CHAIN_BLOCKS as u32).await;
            stores.blocks_load.set(state::Load::from_result(&blocks));
            if let Ok(response) = blocks {
                stores.blocks.set(response.blocks);
            }
        });
    }

    // --- a restored session. Only the token is persisted; the recovery phrase
    // and the key derived from it never touch storage.
    {
        let backend = Arc::clone(&backend);
        let handle = handle.clone();
        leptos::task::spawn_local(async move {
            let Ok(token) = LocalStorage::get::<String>(SESSION_TOKEN_KEY) else {
                return;
            };
            backend.api.set_token(Some(token.clone()));
            match backend.api.session().await {
                Ok(info) => {
                    stores.session.set(Some(info));
                    handle.authenticate(Some(token));
                    handle.subscribe(vec!["account".to_string()]);
                    refresh_account(Arc::clone(&backend), stores);
                }
                Err(error) if error.is_unauthorized() => sign_out(&backend, stores),
                Err(_) => {}
            }
        });
    }

    // --- per-market data, refetched whenever the selection changes, and
    // resubscribed on the socket at the same time.
    let previous = RwSignal::new(stores.selected.get_untracked());
    Effect::new(move |_| {
        let symbol = stores.selected.get();
        let was = previous.get_untracked();
        if was != symbol {
            handle.unsubscribe(vec![orderbook_channel(&was), trades_channel(&was)]);
            previous.set(symbol.clone());
        }
        handle.subscribe(vec![orderbook_channel(&symbol), trades_channel(&symbol)]);

        let backend = Arc::clone(&backend);
        leptos::task::spawn_local(async move {
            if let Ok(ticker) = backend.api.ticker(&symbol).await {
                stores.seed_quote(&ticker);
            }
            if let Ok(book) = backend.api.orderbook(&symbol, BOOK_DEPTH).await {
                // Seeding from REST counts as a snapshot: it is a whole book,
                // not a patch, which is the only thing allowed to install one.
                stores.book.update(|held| held.apply_snapshot(&book));
            }
            let trades = backend.api.trades(&symbol, TAPE_COUNT).await;
            stores.tape_load.set(state::Load::from_result(&trades));
            if let Ok(trades) = trades {
                stores.tape.set(trades.trades);
            }
        });
    });
}

/// Reloads the account, its orders and its positions.
pub fn refresh_account(backend: Arc<Backend>, stores: Stores) {
    leptos::task::spawn_local(async move {
        if let Ok(account) = backend.api.account().await {
            stores.account.set(Some(account));
        }
        if let Ok(orders) = backend.api.orders(OrderFilter::All).await {
            stores.orders.set(orders.orders);
        }
        if let Ok(positions) = backend.api.positions().await {
            stores.positions.set(positions.positions);
        }
    });
}

/// Reloads the selected market's candles at an interval.
pub fn load_candles(
    backend: Arc<Backend>,
    symbol: String,
    interval: Interval,
    into: RwSignal<Vec<api::types::Candle>>,
    load: RwSignal<state::Load>,
) {
    load.set(state::Load::Loading);
    leptos::task::spawn_local(async move {
        let response = backend.api.candles(&symbol, interval, CANDLE_COUNT).await;
        load.set(state::Load::from_result(&response));
        if let Ok(response) = response {
            into.set(response.candles);
        }
    });
}

/// The routed application.
#[component]
fn App() -> impl IntoView {
    view! {
        <Router>
            // `container-type: inline-size` lives here, on the element wrapping
            // every route. The terminal collapses on container queries, not
            // media queries, so a wrapper without a container type produces a
            // terminal that never responds to its width.
            <div class="route-host">
                <Routes fallback=screens::NotFound>
                    <Route path=path!("/") view=screens::landing::Landing />
                    <Route path=path!("/trade") view=screens::terminal::Terminal />
                    <Route path=path!("/portfolio") view=screens::portfolio::Portfolio />
                    <Route path=path!("/faucet") view=screens::faucet::Faucet />
                    <Route path=path!("/leaderboard") view=screens::leaderboard::Leaderboard />
                    <Route path=path!("/chain") view=screens::chain::ChainExplorer />
                    <Route path=path!("/account") view=screens::account::AccountPage />
                    <Route path=path!("/admin") view=screens::admin::Admin />
                </Routes>
            </div>
            <ui::Toaster />
        </Router>
    }
}
