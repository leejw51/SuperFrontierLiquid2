//! Shared chrome and small components.
//!
//! Everything here is a direct lift of a block from `docs/design/reference.html`
//! with its data replaced by a signal. Where a class name appears in this module
//! it is a class the reference defines; no component here invents a colour, a
//! spacing value or a radius, because a value that is not a token is a design
//! change and not a local decision.

pub mod book;
pub mod canvas;
pub mod chart;
pub mod table;

use leptos::prelude::*;
use leptos_router::components::A;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::state::Tone;

/// An icon from the sprite in `index.html`.
///
/// The sprite is lifted from the reference document, so a `<use href="#i-key">`
/// in lifted markup resolves without the component having to carry the path.
#[component]
pub fn Icon(
    /// The symbol id, such as `i-key`.
    #[prop(into)]
    name: String,
    /// Size in pixels.
    #[prop(default = 13)]
    size: u32,
    /// An inline style, usually just a colour.
    #[prop(into, default = String::new())]
    style: String,
    /// Extra classes.
    #[prop(into, default = String::new())]
    class: String,
) -> impl IntoView {
    view! {
        <svg width=size height=size style=style class=class aria-hidden="true" focusable="false">
            <use href=format!("#{name}")></use>
        </svg>
    }
}

/// A pixel-art sprite from `assets/`, drawn crisp at any scale.
///
/// The sprites came out of Grok at 1024 pixels and were requantised to 48
/// pixel art scaled to 96, so `image-rendering: pixelated` is what keeps them
/// looking like SNES sprites rather than blurry stickers.
#[component]
pub fn Sprite(
    /// The file stem under `assets/`, such as `coin_btc`.
    #[prop(into)]
    name: String,
    /// Rendered size in pixels.
    #[prop(default = 32)]
    size: u32,
    /// Alternative text; empty means decorative.
    #[prop(into, default = String::new())]
    alt: String,
    /// Extra classes.
    #[prop(into, default = String::new())]
    class: String,
) -> impl IntoView {
    let decorative = alt.is_empty();
    view! {
        <img
            class=format!("sprite {class}")
            src=format!("/assets/{name}.png")
            width=size
            height=size
            alt=alt
            aria-hidden=decorative.then_some("true")
            draggable="false"
        />
    }
}

/// One entry in the menu panel.
struct MenuItem {
    /// Route, possibly with a query.
    href: &'static str,
    /// The sprite beside it.
    sprite: &'static str,
    /// The label.
    label: &'static str,
    /// What it does, in a few words.
    hint: &'static str,
    /// Shown only to an operator.
    admin_only: bool,
}

const MENU: [MenuItem; 9] = [
    MenuItem {
        href: "/faucet",
        sprite: "faucet",
        label: "Faucet",
        hint: "Claim test money",
        admin_only: false,
    },
    MenuItem {
        href: "/trade?side=buy",
        sprite: "bull",
        label: "Buy",
        hint: "Open the ticket on the buy side",
        admin_only: false,
    },
    MenuItem {
        href: "/trade?side=sell",
        sprite: "bear",
        label: "Sell",
        hint: "Open the ticket on the sell side",
        admin_only: false,
    },
    MenuItem {
        href: "/trade",
        sprite: "coin_btc",
        label: "Trade",
        hint: "Spot, perpetuals and futures",
        admin_only: false,
    },
    MenuItem {
        href: "/portfolio",
        sprite: "stock_cert",
        label: "Portfolio",
        hint: "Positions, equity and risk",
        admin_only: false,
    },
    MenuItem {
        href: "/leaderboard",
        sprite: "trophy",
        label: "Leaderboard",
        hint: "Who is winning",
        admin_only: false,
    },
    MenuItem {
        href: "/chain",
        sprite: "block",
        label: "Dashboard",
        hint: "Chain, RPC and transactions",
        admin_only: false,
    },
    MenuItem {
        href: "/account",
        sprite: "mascot_trader",
        label: "Account",
        hint: "Your addresses and sessions",
        admin_only: false,
    },
    MenuItem {
        href: "/admin",
        sprite: "mascot_admin",
        label: "Admin",
        hint: "Simulation switch and operator controls",
        admin_only: true,
    },
];

/// Every screen in the product, as a menu.
///
/// The user page lists the whole product rather than trusting the top bar,
/// because on a phone the bar drops its navigation and a newcomer should be
/// able to find the faucet from the one page that is about them.
#[component]
pub fn MenuPanel() -> impl IntoView {
    let stores = crate::state::use_stores();
    let navigate = crate::nav::navigator();
    let is_admin = move || {
        stores
            .session
            .read()
            .as_ref()
            .is_some_and(|session| session.roles.iter().any(|role| role == "admin"))
    };
    view! {
        <section class="panel spine-account menu-panel" style="margin-top:var(--sp-16)" data-testid="menu-panel">
            <div class="panel-head">
                <h3>{t("Menu")}</h3>
                <span class="spacer"></span>
                <span class="t-meta">{t("Every screen in the exchange")}</span>
            </div>
            <div class="menu-grid">
                {MENU.iter().map(|item| {
                    let admin_only = item.admin_only;
                    let href = item.href;
                    view! {
                        <Show when=move || !admin_only || is_admin()>
                            <button
                                class="menu-item"
                                data-menu=item.label.to_lowercase()
                                on:click=move |_| navigate(href)
                            >
                                <Sprite name=item.sprite size=40 />
                                <span class="menu-text">
                                    <b>{t(item.label)}</b>
                                    <span>{t(item.hint)}</span>
                                </span>
                            </button>
                        </Show>
                    }
                }).collect_view()}
            </div>
        </section>
    }
}

/// One line of the RPC panel: a label, a value, and a copy button.
#[component]
fn RpcLine(
    /// The label.
    #[prop(into)]
    label: String,
    /// The value, or `None` for "not configured".
    #[prop(into)]
    value: Signal<Option<String>>,
) -> impl IntoView {
    let stores = crate::state::use_stores();
    let label_for_toast = label.clone();
    view! {
        <div class="rpc-line">
            <span class="k">{label}</span>
            <code class="v">
                {move || value.get().unwrap_or_else(|| t("not configured").to_string())}
            </code>
            <Show when=move || value.get().is_some()>
                <button
                    class="btn btn-ghost btn-sm"
                    aria-label=format!("Copy {}", label_for_toast.to_lowercase())
                    on:click={
                        let label = label_for_toast.clone();
                        move |_| {
                            if let Some(text) = value.get_untracked() {
                                crate::screens::landing::copy_to_clipboard(&text);
                                stores.notify(Tone::Good, format!("{label} copied"), text);
                            }
                        }
                    }
                ><Icon name="i-copy" size=12 /></button>
            </Show>
        </div>
    }
}

/// Where everything is: the gateway API, its WebSocket, the sequencer's RPC,
/// the oracle's control API, and the chain the lot of them agree on.
///
/// Shown on the user page and on the dashboard so that a person who wants to
/// drive the same stack from `cb-client` or `curl` can copy the addresses
/// instead of reading a Makefile.
#[component]
pub fn RpcPanel() -> impl IntoView {
    let stores = crate::state::use_stores();
    let selection = crate::config::selection();
    let api_base = selection.api_base.clone();
    let ws_url = crate::config::websocket_url(&api_base);
    let live = matches!(selection.kind, crate::config::BackendKind::Live);
    let chain = move || stores.chain.get();

    let some = |text: String| Some(text);
    view! {
        <section class="panel spine-chain rpc-panel" style="margin-top:var(--sp-16)" data-testid="rpc-panel">
            <div class="panel-head">
                <h3>{t("RPC")}</h3>
                <span class=if live { "badge live" } else { "badge warn" }>
                    {if live { t("Live gateway") } else { t("Mock data") }}
                </span>
                <span class="spacer"></span>
                <span class="t-meta">{t("Point any client at the same stack")}</span>
            </div>
            <div class="rpc-grid">
                <RpcLine label=t("Gateway API") value=Signal::derive({ let api_base = api_base.clone(); move || some(api_base.clone()) }) />
                <RpcLine label=t("Gateway WebSocket") value=Signal::derive({ let ws_url = ws_url.clone(); move || some(ws_url.clone()) }) />
                <RpcLine label=t("Gateway listens on") value=Signal::derive(move || chain().map(|c| c.gateway_listen).filter(|s| !s.is_empty())) />
                <RpcLine label=t("Node RPC (Cap'n Proto)") value=Signal::derive(move || chain().map(|c| c.node_rpc).filter(|s| !s.is_empty())) />
                <RpcLine label=t("Oracle control") value=Signal::derive(move || chain().and_then(|c| c.oracle_control)) />
                <RpcLine label=t("Chain id") value=Signal::derive(move || chain().map(|c| c.chain_id.to_string())) />
                <RpcLine label=t("Block time") value=Signal::derive(move || chain().map(|c| format!("{} ms", c.block_time_ms))) />
                <RpcLine label=t("Simulation switch") value=Signal::derive(move || chain().map(|c| t(c.sim_mode.label()).to_string())) />
            </div>
            <div class="addr-note">
                <Icon name="i-info" size=14 style="color:var(--text-muted);flex:0 0 auto;margin-top:2px" />
                <span>
                    "The browser talks to the gateway over the API and the WebSocket. The gateway                      talks to the sequencer over Cap'n Proto, which is not HTTP: use "
                    <code>"cb-client --rpc <node rpc>"</code>
                    " from a terminal. Numbers on the wire are decimal strings; the                      contract is "<code>"docs/api-contract.md"</code>"."
                </span>
            </div>
        </section>
    }
}

/// A modal over the scrim, with the design system's own dialog semantics.
///
/// Every dialog that asks for an irreversible decision carries
/// `data-density-lock="comfortable"`. Compact density exists to fit more rows on
/// a screen; a confirmation wants fewer things on a screen. This is the only
/// scope in the system allowed to override the user's density preference, and
/// it may not be used on a table.
#[component]
pub fn Modal(
    /// The dialog's accessible name, and its heading.
    #[prop(into)]
    title: String,
    /// The icon beside the heading.
    #[prop(into, default = "i-info".to_string())]
    icon: String,
    /// The icon's colour, as a token reference.
    #[prop(into, default = "var(--text-muted)".to_string())]
    icon_colour: String,
    /// `w-mid` or `w-wide`.
    #[prop(into, default = "w-mid".to_string())]
    width: String,
    /// Anything between the head and the body, such as the join flow's steps.
    #[prop(optional, into)]
    steps: Option<ViewFn>,
    /// The footer's buttons.
    #[prop(optional, into)]
    foot: Option<ViewFn>,
    /// The body.
    children: Children,
) -> impl IntoView {
    let heading_id = format!(
        "modal-{}",
        title.replace(char::is_whitespace, "-").to_lowercase()
    );
    let labelled_by = heading_id.clone();
    view! {
        <div class="scrim-wrap scrim-layer" role="dialog" aria-modal="true" aria-labelledby=labelled_by>
            <div class=format!("modal {width}") data-density-lock="comfortable">
                <div class="modal-head">
                    <Icon name=icon size=16 style=format!("color:{icon_colour}") />
                    <h3 id=heading_id>{title}</h3>
                </div>
                {steps.map(|steps| steps.run())}
                <div class="modal-body">{children()}</div>
                {foot.map(|foot| view! { <div class="modal-foot">{foot.run()}</div> })}
            </div>
        </div>
    }
}

/// The wordmark: three bars — an ask, a spread and a bid — and the name.
///
/// It is the one place the market's own colours appear outside market data, and
/// it earns the exception by being a picture of a book rather than a decoration.
#[component]
pub fn Wordmark(
    /// Glyph size in pixels.
    #[prop(default = 15)]
    size: u32,
) -> impl IntoView {
    view! {
        <span class="wordmark">
            <svg width=size height=size viewBox="0 0 14 14" aria-hidden="true">
                <rect x="0" y="1.4" width="14" height="3.1" rx=".5" fill="var(--ask)"></rect>
                <rect x="0" y="5.6" width="8.6" height="2.8" rx=".5" fill="var(--text-faint)"></rect>
                <rect x="0" y="9.5" width="14" height="3.1" rx=".5" fill="var(--bid)"></rect>
            </svg>
            "SuperFrontier"<span class="cb">"Liquid"</span>
        </span>
    }
}

/// The connection indicator: a dot, a word **and** a figure, always.
///
/// The bare dot is never the whole message. A user glancing at a green dot
/// learns nothing about whether the last price is two seconds or two minutes
/// old, which is the only thing they actually need to know.
#[component]
pub fn ConnIndicator() -> impl IntoView {
    let stores = crate::state::use_stores();
    // Derived from the age of the last frame, not only from what the socket
    // says about itself. A gateway frozen with `kill -STOP` never closes
    // anything, so the socket stays `Live` while nothing at all arrives — and
    // the top bar, which the design system puts the connection status in
    // "always" for exactly this reason, read `Live` for two minutes.
    let report = move || stores.feed_report();

    view! {
        <span
            class="conn"
            data-state=move || report().attr
            role="status"
            aria-live="polite"
        >
            <span class="dot"></span>
            {move || report().word}
            <span class="num dim">{move || report().figure}</span>
        </span>
    }
}

/// A status badge with the design system's own modifier classes.
#[component]
pub fn Badge(
    /// The text.
    #[prop(into)]
    label: String,
    /// The full class string, such as `badge live`.
    #[prop(into, default = "badge".to_string())]
    class: String,
) -> impl IntoView {
    view! { <span class=class>{label}</span> }
}

/// A labelled figure, in the reference's `.stat` shape.
#[component]
pub fn Stat(
    /// The label.
    #[prop(into)]
    k: String,
    /// The value, already formatted.
    #[prop(into)]
    v: Signal<String>,
    /// Extra classes on the value, such as `lg` or `up`.
    #[prop(into, default = String::new())]
    value_class: String,
) -> impl IntoView {
    let class = format!("v {value_class}");
    view! {
        <span class="stat">
            <span class="k">{k}</span>
            <span class=class>{move || v.get()}</span>
        </span>
    }
}

/// A signed, coloured figure with its direction glyph.
///
/// Sign, colour and glyph together: colour is never the only carrier of
/// direction, so this renders all three or none.
#[component]
pub fn Signed(
    /// The value.
    #[prop(into)]
    value: Signal<Decimal>,
    /// Decimal places.
    #[prop(default = 2)]
    dp: u32,
    /// Append a percent sign.
    #[prop(default = false)]
    percent: bool,
    /// Draw the ▲ / ▼ glyph before the figure.
    #[prop(default = false)]
    glyph: bool,
) -> impl IntoView {
    let class = move || {
        let v = value.get();
        if v > Decimal::ZERO {
            "num up"
        } else if v < Decimal::ZERO {
            "num down"
        } else {
            "num dim"
        }
    };
    let text = move || {
        let v = value.get();
        let body = format::signed(v, dp);
        let body = if percent { format!("{body}%") } else { body };
        match (glyph, format::direction_glyph(v)) {
            (true, Some(g)) => format!("{g} {body}"),
            _ => body,
        }
    };
    view! { <span class=class>{text}</span> }
}

/// The chain rail: forty pixels along the foot of every screen.
///
/// Giving the chain a rail rather than a page is the decision that makes the
/// blockchain a fact about the product instead of a claim on a marketing page,
/// so it is a component every route mounts rather than something a route opts
/// into.
#[component]
pub fn ChainRail() -> impl IntoView {
    let stores = crate::state::use_stores();
    let chain = move || stores.chain.get();
    let synced = move || chain().is_none_or(|chain| chain.synced);

    // The chips are a twelve-block histogram of load: each chip's fill is its
    // transaction count against the busiest block on the rail.
    let chips = move || {
        let blocks = stores.blocks.read();
        let window: Vec<Block> = blocks
            .iter()
            .take(crate::state::RAIL_BLOCKS)
            .cloned()
            .collect();
        let busiest = window.iter().map(|b| b.tx_count).max().unwrap_or(1).max(1);
        window
            .into_iter()
            .rev()
            .map(|block| (block.height, (block.tx_count * 100 / busiest).max(6)))
            .collect::<Vec<_>>()
    };

    view! {
        <div class="rail" class:warn=move || !synced()>
            <span class="badge pending" style="gap:5px">
                {move || chain().map_or("Chain".to_string(), |c| format!("Chain {}", c.chain_id))}
            </span>
            <span class="lbl">{t("Height")}</span>
            <span class="val" class:tick-up=move || stores.block_tick.get().is_multiple_of(2)>
                {move || chain().map_or_else(|| "—".into(), |c| format::count(c.height))}
            </span>
            <span class="lbl">{t("Throughput")}</span>
            <span class="val">
                {move || chain().map_or_else(|| "—".into(), |c| format!("{} tx/s", format::fixed(c.tps_1m, 1)))}
            </span>
            <span class="lbl opt">{t("Block time")}</span>
            <span class="val opt">
                {move || chain().map_or_else(|| "—".into(), |c| format!("{}ms", c.block_time_ms))}
            </span>
            // Mempool depth is shown only when there is something in it: a zero
            // is noise on a rail that is always on screen.
            <Show when=move || chain().is_some_and(|c| c.mempool_depth > 0)>
                <span class="lbl opt">{t("Mempool")}</span>
                <span class="val opt">
                    {move || chain().map_or_else(String::new, |c| c.mempool_depth.to_string())}
                </span>
            </Show>
            <span class="lbl opt">{t("Validators")}</span>
            <span class="val opt">
                {move || chain().map_or_else(|| "—".into(), |c| c.validators.to_string())}
            </span>
            <span class="lbl opt">{t("State root")}</span>
            <span class="val opt">
                {move || chain().map_or_else(|| "—".into(), |c| format::hash(&c.state_root))}
            </span>
            <span class="blocks">
                <For each=chips key=|(height, _)| *height let:chip>
                    <span class="blk" style=format!("--fill:{}", chip.1)>
                        <i></i>
                    </span>
                </For>
            </span>
        </div>
    }
}

/// The top bar: identity, navigation, connection and the two preferences.
#[component]
pub fn TopBar() -> impl IntoView {
    let stores = crate::state::use_stores();
    let backend = expect_context::<std::sync::Arc<Backend>>();
    let is_mock = backend.is_mock();
    let sim = backend.sim.clone();
    let is_admin = move || {
        stores
            .session
            .read()
            .as_ref()
            .is_some_and(|session| session.roles.iter().any(|role| role == "admin"))
    };

    // The phone menu. Below 560px the bar drops its navigation to make room for
    // the price, and below 700px it drops both preference switches — and for a
    // while nothing replaced either, so on a phone every route but the one you
    // were on stopped existing. That includes the faucet, which is the only way
    // to get money, which is the only way to trade.
    let menu_open = RwSignal::new(false);
    // Escape closes it, like every other layer in the product.
    let _escape = window_event_listener(leptos::ev::keydown, move |event| {
        if event.key() == "Escape" {
            menu_open.set(false);
        }
    });

    view! {
        <div class="topbar">
            <Wordmark />
            <button
                class="btn btn-ghost btn-sm navmenu-btn"
                aria-expanded=move || menu_open.get().to_string()
                aria-controls="nav-menu"
                aria-label="Sections and preferences"
                on:click=move |_| menu_open.update(|open| *open = !*open)
            >
                <svg width="13" height="13" viewBox="0 0 14 14" aria-hidden="true">
                    <path d="M1.6 3.4h10.8M1.6 7h10.8M1.6 10.6h10.8" fill="none"
                          stroke="currentColor" stroke-width="1.5" stroke-linecap="round"></path>
                </svg>
                {t("Menu")}
            </button>
            <nav class="nav" aria-label="Sections">
                <A href="/trade">{t("Trade")}</A>
                <A href="/portfolio">{t("Portfolio")}</A>
                <A href="/faucet">{t("Faucet")}</A>
                <A href="/leaderboard">{t("Leaderboard")}</A>
                <A href="/chain">{t("Dashboard")}</A>
                <A href="/account">{t("Account")}</A>
                // Presentation only. The gateway enforces the role on every
                // admin endpoint regardless of what this client believes.
                <Show when=is_admin>
                    <A href="/admin">{t("Admin")}</A>
                </Show>
            </nav>
            <Show when=move || menu_open.get()>
                <div
                    class="navmenu-scrim"
                    on:click=move |_| menu_open.set(false)
                    aria-hidden="true"
                ></div>
                <nav id="nav-menu" class="navmenu" aria-label="Sections">
                    <A href="/trade" on:click=move |_| menu_open.set(false)>{t("Trade")}</A>
                    <A href="/portfolio" on:click=move |_| menu_open.set(false)>{t("Portfolio")}</A>
                    <A href="/faucet" on:click=move |_| menu_open.set(false)>{t("Faucet")}</A>
                    <A href="/leaderboard" on:click=move |_| menu_open.set(false)>{t("Leaderboard")}</A>
                    <A href="/chain" on:click=move |_| menu_open.set(false)>{t("Dashboard")}</A>
                    <A href="/account" on:click=move |_| menu_open.set(false)>{t("Account")}</A>
                    <Show when=is_admin>
                        <A href="/admin" on:click=move |_| menu_open.set(false)>{t("Admin")}</A>
                    </Show>
                    // The two preferences come with it rather than being
                    // deleted: the brief asks for both themes and both
                    // densities to work, and on a phone this is where they live.
                    <div class="navmenu-prefs">
                        <PreferenceToggles />
                    </div>
                </nav>
            </Show>
            <div class="right">
                <Show when=move || is_mock>
                    <span
                        class="badge warn dev-only"
                        title="Fixture data, not a running exchange"
                    >{t("Mock data")}</span>
                </Show>
                {sim.clone().map(|sim| {
                    let stalling = sim.clone();
                    view! {
                        <>
                            <button
                                class="btn btn-ghost btn-sm dev-only"
                                on:click=move |_| {
                                    sim.simulate_outage(6_000);
                                    stores.notify(
                                        Tone::Warn,
                                        "Dropping the socket",
                                        "The feed will reconnect on its own. Prices hold their \
                                         last value and stop flashing until it does.",
                                    );
                                }
                            >
                                "Drop the socket"
                            </button>
                            // The other kind of failure, and the commoner one: a
                            // gateway that stops answering without hanging up.
                            <button
                                class="btn btn-ghost btn-sm dev-only"
                                on:click=move |_| {
                                    stalling.simulate_stall(120_000);
                                    stores.notify(
                                        Tone::Warn,
                                        "Stalling the socket",
                                        "The connection stays open and nothing arrives, which is \
                                         what a frozen gateway looks like. Watch the indicator \
                                         go brass, then vermilion.",
                                    );
                                }
                            >
                                "Stall the socket"
                            </button>
                        </>
                    }
                })}
                <ConnIndicator />
                <span class="stat" style="align-items:flex-end">
                    <span class="k">{t("Equity")}</span>
                    <span class="v sm">
                        {move || stores.account.read().as_ref().map_or_else(
                            || "—".to_string(),
                            |account| format::fixed(account.portfolio.equity, 2),
                        )}
                    </span>
                </span>
                <PreferenceToggles />
                <Show
                    when=move || stores.session.read().is_some()
                    fallback=|| view! { <A href="/" attr:class="btn btn-secondary btn-sm">{t("Sign in")}</A> }
                >
                    <A href="/account" attr:class="pill" attr:style="height:26px;text-decoration:none">
                        <span style="width:16px;height:16px;border-radius:var(--r-full);background:var(--brass);color:var(--surface-canvas);display:grid;place-items:center;font-size:9px;font-weight:700">
                            {move || stores.session.read().as_ref()
                                .and_then(|s| s.handle.chars().next())
                                .map_or_else(String::new, |c| c.to_string())}
                        </span>
                        {move || stores.session.read().as_ref()
                            .map_or_else(String::new, |s| s.handle.clone())}
                    </A>
                </Show>
            </div>
        </div>
    }
}

/// The theme and density switches, both remembered per user.
#[component]
pub fn PreferenceToggles() -> impl IntoView {
    let (light, set_light) = signal(crate::prefs::theme() == "light");
    let (compact, set_compact) = signal(crate::prefs::density() == "compact");

    view! {
        <label
            class="switch pref-only"
            title="Fit about half again as many rows on screen"
        >
            <input
                type="checkbox"
                prop:checked=move || compact.get()
                on:change=move |event| {
                    let on = event_target_checked(&event);
                    set_compact.set(on);
                    crate::prefs::set_density(if on { "compact" } else { "comfortable" });
                }
            />
            <span class="track"></span>
            {t("Compact")}
        </label>
        <label class="switch pref-only" title="Warm paper instead of black lacquer">
            <input
                type="checkbox"
                prop:checked=move || light.get()
                on:change=move |event| {
                    let on = event_target_checked(&event);
                    set_light.set(on);
                    crate::prefs::set_theme(if on { "light" } else { "dark" });
                }
            />
            <span class="track"></span>
            {t("Light")}
        </label>
        // The language. Two positions today; the switch is a segmented
        // control rather than a checkbox so a third language is one more
        // button and not a redesign.
        <div class="seg lang-switch pref-only" role="group" aria-label=t("Language") data-testid="lang-switch">
            {crate::i18n::Lang::ALL.into_iter().map(|lang| view! {
                <button
                    lang=lang.tag()
                    data-lang=lang.tag()
                    aria-pressed=(crate::i18n::current() == lang).to_string()
                    on:click=move |_| crate::i18n::choose(lang)
                >{lang.native_name()}</button>
            }).collect_view()}
        </div>
    }
}

/// Reads `checked` off a change event without unwrapping.
fn event_target_checked(event: &leptos::ev::Event) -> bool {
    use wasm_bindgen::JsCast;
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::HtmlInputElement>().ok())
        .is_some_and(|input| input.checked())
}

/// The toast stack: bottom right, newest at the bottom, at most four.
#[component]
pub fn Toaster() -> impl IntoView {
    let stores = crate::state::use_stores();
    let price_dp = move || stores.market().map_or(2, |market| market.price_decimals);
    // Read in the component body: `use_navigate` reads router context, which
    // exists while a component renders and not inside the click handler's task.
    let navigate = crate::nav::navigator();

    view! {
        <div class="toaster" role="log" aria-live="polite" aria-relevant="additions">
            <For each=move || stores.toasts.get() key=|toast| toast.id let:toast>
                {
                    let id = toast.id;
                    let action = toast.action.clone();
                    let height = toast.height;
                    view! {
                        <div class=toast.tone.class()>
                            <div class="body">
                                <div class="title">{toast.title.clone()}</div>
                                <div class="desc">{toast.body.clone()}</div>
                                // Every trade toast names the block. On this
                                // product that is the receipt.
                                {height.map(|height| view! {
                                    <div class="desc">
                                        "Block "
                                        <span class="t-mono">{format::count(height)}</span>
                                    </div>
                                })}
                                {action.map(|action| {
                                    let label = action.label(price_dp());
                                    view! {
                                        <div style="margin-top:var(--sp-8)">
                                            <button
                                                class="btn btn-secondary btn-sm"
                                                on:click=move |_| {
                                                    if let Some(route) =
                                                        crate::ticket::apply_remedy(&action)
                                                    {
                                                        navigate(route);
                                                    }
                                                    stores.dismiss(id);
                                                }
                                            >
                                                {label}
                                            </button>
                                        </div>
                                    }
                                })}
                            </div>
                            <button
                                class="x"
                                aria-label="Dismiss"
                                on:click=move |_| stores.dismiss(id)
                            >
                                "\u{2715}"
                            </button>
                        </div>
                    }
                }
            </For>
        </div>
    }
}

/// An empty state: a glyph, a heading, one line, and an invitation.
#[component]
pub fn EmptyState(
    /// The heading.
    #[prop(into)]
    title: String,
    /// One line of explanation, which says what would appear here and why.
    #[prop(into)]
    body: String,
    /// Optional action.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="state">
            <svg class="glyph" width="28" height="28" viewBox="0 0 28 28" aria-hidden="true">
                <rect x="2.5" y="6.5" width="23" height="15" rx="1.5" fill="none"
                      stroke="currentColor" stroke-width="1.3"></rect>
                <path d="M2.5 12h23M9.5 12v9.5" fill="none" stroke="currentColor"
                      stroke-width="1.1" opacity=".6"></path>
            </svg>
            <h4>{title}</h4>
            <p>{body}</p>
            {children.map(|children| children())}
        </div>
    }
}

/// An error state: the same anatomy, a vermilion glyph, and a way forward.
///
/// It says what happened and what to do. It does not apologise and it is never
/// vague.
#[component]
pub fn ErrorState(
    /// What happened.
    #[prop(into)]
    title: String,
    /// The detail, which is the gateway's own sentence wherever there is one.
    #[prop(into)]
    body: String,
    /// Optional action.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="state error">
            <svg class="glyph" width="28" height="28" viewBox="0 0 28 28" aria-hidden="true">
                <path d="M14 3.2 26 24.8H2L14 3.2Z" fill="none" stroke="currentColor"
                      stroke-width="1.4" stroke-linejoin="round"></path>
                <path d="M14 11.2v6.2" stroke="currentColor" stroke-width="1.6"
                      stroke-linecap="round"></path>
                <circle cx="14" cy="21.4" r="1.1" fill="currentColor"></circle>
            </svg>
            <h4>{title}</h4>
            <p>{body}</p>
            {children.map(|children| children())}
        </div>
    }
}

/// What a plain `<table>` shows underneath its header row when there are no
/// rows: a skeleton, a failure, an empty state — or nothing, when there are.
///
/// Five tables in this product rendered a column header over blank space and
/// said nothing at all. §8 specifies the anatomy and the product already
/// implements it well elsewhere; these were simply missed.
#[component]
pub fn TableState(
    /// Whether the rows have been answered for.
    #[prop(into)]
    load: Signal<crate::state::Load>,
    /// Whether the answer was nothing.
    #[prop(into)]
    empty: Signal<bool>,
    /// The empty state's heading.
    #[prop(into)]
    title: String,
    /// The empty state's one line.
    #[prop(into)]
    body: String,
) -> impl IntoView {
    let title = StoredValue::new(title);
    let body = StoredValue::new(body);
    view! {
        {move || {
            let load = load.get();
            if load.is_loading() {
                return Some(view! { <SkeletonRows rows=4 /> }.into_any());
            }
            if let Some(message) = load.failure() {
                return Some(
                    view! {
                        <ErrorState title="That did not load" body=message.to_string() />
                    }
                    .into_any(),
                );
            }
            if !empty.get() {
                return None;
            }
            Some(
                view! { <EmptyState title=title.get_value() body=body.get_value() /> }.into_any(),
            )
        }}
    }
}

/// One panel region, drawn as whichever of its four states it is actually in.
///
/// The rule this enforces, which the product previously broke in six places:
/// **a skeleton means the data is coming.** It is not a picture of nothing. A
/// region that has been answered and has nothing in it says so in words, and a
/// region whose request failed says what happened — neither of them shimmers.
#[component]
pub fn Region(
    /// Whether this region has an answer yet.
    #[prop(into)]
    load: Signal<crate::state::Load>,
    /// Whether the answer was nothing.
    #[prop(into)]
    empty: Signal<bool>,
    /// How many skeleton rows to draw while it loads.
    #[prop(default = 6)]
    rows: usize,
    /// The empty state's heading.
    #[prop(into)]
    title: String,
    /// The empty state's one line: what would appear here, and how to make it.
    #[prop(into)]
    body: String,
    /// The region's content, once there is some.
    children: ChildrenFn,
) -> impl IntoView {
    let title = StoredValue::new(title);
    let body = StoredValue::new(body);
    view! {
        {move || {
            let load = load.get();
            if load.is_loading() {
                return view! { <SkeletonRows rows=rows /> }.into_any();
            }
            if let Some(message) = load.failure() {
                return view! {
                    <ErrorState
                        title="That did not load"
                        body=message.to_string()
                    />
                }
                .into_any();
            }
            if empty.get() {
                return view! {
                    <EmptyState title=title.get_value() body=body.get_value() />
                }
                .into_any();
            }
            children().into_any()
        }}
    }
}

/// A skeleton row at the current density's row height, so nothing jumps when
/// the data lands.
#[component]
pub fn SkeletonRows(
    /// How many rows to draw.
    #[prop(default = 6)]
    rows: usize,
) -> impl IntoView {
    view! {
        <div aria-hidden="true">
            {(0..rows)
                .map(|index| {
                    let width = 40 + (index * 17) % 45;
                    view! {
                        <div class="skel-row">
                            <span class="skel" style=format!("width:{width}%")></span>
                            <span class="skel" style="width:62%;justify-self:end"></span>
                            <span class="skel" style="width:48%;justify-self:end"></span>
                        </div>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// An inline banner, in the reference's `.banner` shape.
#[component]
pub fn Banner(
    /// A modifier: `warn`, `danger`, `info`, `good` or `hazard`.
    #[prop(into, default = String::new())]
    tone: String,
    /// The bold first line: the consequence.
    #[prop(into)]
    title: String,
    /// The explanation, and the remedy where there is one.
    #[prop(into)]
    body: String,
    /// Optional actions, which sit at the trailing edge.
    #[prop(optional)]
    children: Option<Children>,
) -> impl IntoView {
    let glyph_colour = match tone.as_str() {
        "warn" => "var(--brass)",
        "danger" | "hazard" => "var(--ask)",
        "good" => "var(--bid)",
        "info" => "var(--signal)",
        _ => "var(--text-muted)",
    };
    view! {
        <div class=format!("banner {tone}") role="status">
            <svg class="ico" width="14" height="14" viewBox="0 0 14 14"
                 style=format!("color:{glyph_colour}") aria-hidden="true">
                <path d="M7 1.6 13 12.4H1L7 1.6Z" fill="none" stroke="currentColor"
                      stroke-width="1.4" stroke-linejoin="round"></path>
                <path d="M7 5.6v3.1" stroke="currentColor" stroke-width="1.5"
                      stroke-linecap="round"></path>
                <circle cx="7" cy="10.7" r=".85" fill="currentColor"></circle>
            </svg>
            <div>
                <strong>{title}</strong>
                {body}
            </div>
            {children.map(|children| view! { <span class="acts">{children()}</span> })}
        </div>
    }
}

/// A failure a mutating control has to report where the control is.
///
/// A toast cannot do this job inside a dialog: the toast stack lives at
/// `--z-toast` and the modal scrim at `--z-modal`, so a dialog that reports its
/// failure by toasting reports it to nobody. More importantly, the design
/// system's rule is that the reason belongs beside the control that failed —
/// an operator who pressed Apply needs to know, at the button, that nothing was
/// applied.
#[component]
pub fn MutationFailure(
    /// The last failure: a consequence and the gateway's own sentence.
    failure: RwSignal<Option<(String, String)>>,
) -> impl IntoView {
    view! {
        {move || failure.get().map(|(title, body)| view! {
            <div style="margin-top:var(--sp-12)">
                <Banner tone="danger" title=title body=body />
            </div>
        })}
    }
}

/// The shell every signed-in screen sits in: top bar, page, chain rail.
#[component]
pub fn AppShell(
    /// The page.
    children: Children,
) -> impl IntoView {
    view! {
        <div class="app">
            <TopBar />
            <div class="page">{children()}</div>
            <ChainRail />
        </div>
    }
}
