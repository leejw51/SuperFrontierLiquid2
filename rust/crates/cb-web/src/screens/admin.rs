//! The operator dashboard.
//!
//! This is the one place in the product where **legibility beats density** — the
//! exact opposite of the terminal. An admin screen is where somebody acts under
//! pressure on incomplete information, so there are fewer figures, larger,
//! further apart, and every destructive action is confirmed in a dialog that
//! states what it is about to do before it offers a button that does it.
//!
//! Four rules from the design reference are load-bearing:
//!
//! * **One tile is allowed to shout.** `accounts_below_maintenance` is the only
//!   figure on the overview that is a decision rather than a fact, so it takes a
//!   brass wash. The others stay flat however large they get, because a number
//!   that is large is not the same as a number that is wrong.
//! * **Health is a shape and a word.** A row of green ticks would break the
//!   first rule in this system: green means buy and nothing else. So `ok` is a
//!   muted tick, `degraded` a brass triangle and `down` a vermilion cross, each
//!   beside the word. It reads identically in greyscale.
//! * **Destructive controls stay visible and disabled**, carrying their reason.
//!   A control that disappears sends the operator hunting, and hunting under
//!   pressure is how the wrong button gets pressed.
//! * **Confirmations ignore the density switch**, via `data-density-lock`.
//!
//! The force-settle dialog is the sharp end: the price is the whole risk, so it
//! is defended by the magnitude in words, the deviation from the oracle mark, an
//! explicit override for the 10% band, and a second independent entry of the
//! same price.

use std::sync::Arc;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::screens::landing::{input_value, select_value};
use crate::state::{use_stores, Load, Tone};
use crate::ticket::parse_typed;
use crate::ui::{AppShell, Banner, Icon, Modal, Sprite, TableState};

/// The band outside which the gateway refuses a settlement price without force.
const ORACLE_BAND_PCT: i64 = 10;

/// Which dialog is open.
#[derive(Debug, Clone, PartialEq)]
enum Dialog {
    /// Nothing.
    None,
    /// Stop a user trading, with a reason for the audit trail.
    Suspend(String, String),
    /// Halt a market, with a reason.
    Halt(String),
    /// Force-settle a market. Irreversible.
    Settle(Box<AdminMarket>),
    /// Change a market's margin requirements.
    Risk(Box<AdminMarket>),
    /// What a risk change just did.
    RiskApplied(String, RiskApplied),
}

/// The admin screen.
#[component]
pub fn Admin() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());

    let overview = RwSignal::new(None::<AdminOverview>);
    let users = RwSignal::new(Vec::<AdminUser>::new());
    let markets = RwSignal::new(Vec::<AdminMarket>::new());
    let faucet = RwSignal::new(None::<AdminFaucet>);
    let sim = RwSignal::new(None::<SimStatus>);
    let audit = RwSignal::new(Vec::<AuditRecord>::new());
    let dialog = RwSignal::new(Dialog::None);
    // Three tables that used to render a column header over blank space
    // whichever of loading, failed and empty they were actually in.
    let users_load = RwSignal::new(Load::Loading);
    let markets_load = RwSignal::new(Load::Loading);
    let audit_load = RwSignal::new(Load::Loading);

    let query = RwSignal::new(String::new());
    let hide_bots = RwSignal::new(false);
    let action_filter = RwSignal::new(String::new());

    let reload = move || {
        let backend = backend.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(found) = backend.api.admin_overview().await {
                overview.set(Some(found));
            }
            let found = backend.api.admin_markets().await;
            markets_load.set(Load::from_result(&found));
            if let Ok(found) = found {
                markets.set(found.markets);
            }
            if let Ok(found) = backend.api.admin_faucet().await {
                faucet.set(Some(found));
            }
            if let Ok(found) = backend.api.admin_sim().await {
                sim.set(Some(found));
            }
        });
    };

    Effect::new(move |_| {
        if stores.session.read().is_some() {
            reload();
        }
    });

    // The user list and the audit trail refetch on their own filters, because
    // both are server-side queries rather than client-side filtering.
    Effect::new(move |_| {
        // Nothing to ask without a session: every admin route answers 401 to
        // an anonymous caller, and asking anyway filled the console with
        // errors the screen already explains.
        if stores.session.read().is_none() {
            return;
        }
        let backend = backend.get_value();
        let needle = query.get();
        let bots = hide_bots.get().then_some(false);
        users_load.set(Load::Loading);
        leptos::task::spawn_local(async move {
            let found = backend.api.admin_users(&needle, bots, 60).await;
            users_load.set(Load::from_result(&found));
            if let Ok(found) = found {
                users.set(found.users);
            }
        });
    });
    Effect::new(move |_| {
        if stores.session.read().is_none() {
            return;
        }
        let backend = backend.get_value();
        let action = action_filter.get();
        audit_load.set(Load::Loading);
        leptos::task::spawn_local(async move {
            let found = backend.api.admin_audit(&action, 60).await;
            audit_load.set(Load::from_result(&found));
            if let Ok(found) = found {
                audit.set(found.records);
            }
        });
    });

    let is_admin = move || {
        stores
            .session
            .read()
            .as_ref()
            .is_some_and(|session| session.roles.iter().any(|role| role == "admin"))
    };
    let unhealthy = move || {
        overview.read().as_ref().and_then(|overview| {
            overview
                .health
                .iter()
                .find(|component| component.status != HealthStatus::Ok)
                .cloned()
        })
    };

    view! {
        <AppShell>
            <Show
                when=is_admin
                fallback=|| view! {
                    <crate::ui::ErrorState
                        title=t("This screen needs an operator session")
                        body="The gateway enforces the role on every admin endpoint regardless of \
                              what this client believes, so there is nothing to see here without \
                              one."
                    >
                        <a class="btn btn-primary" href="/trade">{t("Go to the terminal")}</a>
                    </crate::ui::ErrorState>
                }
            >
                // A status list is where you go to check; a banner is what finds
                // you. Everything ok and this is not drawn at all.
                {move || unhealthy().map(|component| view! {
                    <div style="margin-bottom:var(--sp-16)">
                        <Banner
                            tone="warn"
                            title=format!(
                                "The {} is {}",
                                component.component,
                                component.status.label().to_lowercase(),
                            )
                            body=component.detail.clone().unwrap_or_else(|| {
                                "The gateway did not say what is wrong.".to_string()
                            })
                        >
                            <button
                                class="btn btn-ghost btn-sm"
                                on:click=move |_| reload()
                            >"Refresh"</button>
                        </Banner>
                    </div>
                })}

                <div style="display:flex;align-items:baseline;gap:var(--sp-12);margin-bottom:var(--sp-8)">
                    <h2 class="t-h2">{t("Overview")}</h2>
                    <span class="t-meta">
                        {move || overview.read().as_ref().map_or_else(
                            || "Loading".to_string(),
                            |overview| format!(
                                "One request, one paint \u{00B7} node up {}",
                                format::duration(
                                    i64::try_from(overview.chain.uptime_seconds).unwrap_or(0)),
                            ))}
                    </span>
                </div>

                <div class="adm-tiles">
                    {move || overview.read().as_ref().map(|overview| {
                        let chain = &overview.chain;
                        let users = &overview.users;
                        let market = &overview.markets;
                        let volume = &overview.volume;
                        let f = &overview.faucet;
                        let risk = &overview.risk;
                        view! {
                            <>
                                <div class="tile">
                                    <span class="t"><Icon name="i-chain" size=11 />"Chain height"</span>
                                    <span class="big">{format::count(chain.height)}</span>
                                    <span class="rows">
                                        <span>"Block time "<b>{format!("{}ms", chain.block_time_ms)}</b></span>
                                        <span>"Throughput "<b>{format!("{} tx/s", format::fixed(chain.tps_1m, 1))}</b></span>
                                        <span>"Mempool "<b>{chain.mempool_depth.to_string()}</b></span>
                                        <span>"Validators "<b>{chain.validators.to_string()}</b></span>
                                    </span>
                                </div>
                                <div class="tile">
                                    <span class="t">"Users"</span>
                                    <span class="big">{format::count(users.total)}</span>
                                    <span class="rows">
                                        <span>"Humans "<b>{format::count(users.humans)}</b></span>
                                        <span>"Bots "<b>{format::count(users.bots)}</b></span>
                                        <span>"Active 24h "<b>{format::count(users.active_24h)}</b></span>
                                        <span>"New 24h "<b>{format::count(users.new_24h)}</b></span>
                                    </span>
                                </div>
                                <div class="tile">
                                    <span class="t">"Markets"</span>
                                    <span class="big">{market.total.to_string()}</span>
                                    <span class="rows">
                                        <span>"Trading "<b>{market.trading.to_string()}</b></span>
                                        <span>"Halted "<b>{market.halted.to_string()}</b></span>
                                        <span>"Expired "<b>{market.expired.to_string()}</b></span>
                                    </span>
                                </div>
                                <div class="tile">
                                    <span class="t">"Volume \u{00B7} 24h"</span>
                                    <span class="big">{format::notional(volume.volume_24h)}</span>
                                    <span class="rows">
                                        <span>"Trades "<b>{format::count(volume.trades_24h)}</b></span>
                                        <span>"Fees "<b>{format::fixed(volume.fees_24h, 2)}</b></span>
                                        <span>"Open interest "<b>{format::notional(risk.open_interest)}</b></span>
                                    </span>
                                </div>
                                <div class="tile">
                                    <span class="t">"Faucet claims \u{00B7} 24h"</span>
                                    <span class="big">{format::count(f.claims_24h)}</span>
                                    <span class="rows">
                                        <span>"Granted 24h "<b>{
                                            f.granted_24h.first()
                                                .map_or_else(|| "\u{2014}".to_string(),
                                                    |g| format::notional(g.amount))
                                        }</b></span>
                                        <span>"Granted total "<b>{
                                            f.total_granted.first()
                                                .map_or_else(|| "\u{2014}".to_string(),
                                                    |g| format::notional(g.amount))
                                        }</b></span>
                                    </span>
                                </div>
                                // The only figure here that is a decision rather
                                // than a fact, so the only one allowed to shout
                                // — and only when there is something to shout
                                // about. A tile that is permanently alarmed at
                                // zero teaches an operator to ignore it, which
                                // costs exactly as much as not having it.
                                <div class=if risk.accounts_below_maintenance == 0 {
                                    "tile"
                                } else {
                                    "tile alert"
                                }>
                                    <span class="t">
                                        {(risk.accounts_below_maintenance != 0).then(|| view! {
                                            <Icon name="i-warn" size=11 />
                                        })}
                                        "Accounts below maintenance"
                                    </span>
                                    <span class="big">
                                        {risk.accounts_below_maintenance.to_string()}
                                    </span>
                                    <span class="rows">
                                        <span>"Liquidations 24h "<b>{format::count(risk.liquidations_24h)}</b></span>
                                        <span>"Insurance fund "<b>{format::notional(risk.insurance_fund)}</b></span>
                                    </span>
                                </div>
                            </>
                        }
                    })}
                </div>

                <section class="panel spine-chain" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>"Component health"</h3>
                        <span class="spacer"></span>
                        <span class="t-meta">
                            {move || overview.read().as_ref().map_or_else(String::new, |overview| {
                                let degraded = overview.health.iter()
                                    .filter(|component| component.status != HealthStatus::Ok)
                                    .count();
                                format!(
                                    "{} component{}, {}",
                                    overview.health.len(),
                                    if overview.health.len() == 1 { "" } else { "s" },
                                    if degraded == 0 {
                                        "all well".to_string()
                                    } else {
                                        format!("{degraded} not well")
                                    },
                                )
                            })}
                        </span>
                    </div>
                    <div class="health">
                        {move || overview.read().as_ref().map(|overview| {
                            overview.health.iter().map(|component| {
                                let (glyph, status) = match component.status {
                                    HealthStatus::Ok => ("i-check", "ok"),
                                    HealthStatus::Degraded => ("i-warn", "degraded"),
                                    HealthStatus::Down => ("i-x", "down"),
                                };
                                view! {
                                    <span class="hrow" data-status=status>
                                        <span class="st">
                                            <Icon name=glyph size=11 />
                                            {component.status.label()}
                                        </span>
                                        <span class="nm">{component.component.clone()}</span>
                                        <span class="dt">
                                            {component.detail.clone()
                                                .unwrap_or_else(|| "\u{2014}".to_string())}
                                        </span>
                                    </span>
                                }
                            }).collect_view()
                        })}
                    </div>
                </section>

                <section class="panel spine-account" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>"Users"</h3>
                        <span class="badge">
                            {move || overview.read().as_ref()
                                .map_or_else(String::new, |o| format::count(o.users.total))}
                        </span>
                    </div>
                    <div class="adm-search">
                        <div class="grow" style="position:relative;display:flex;align-items:center">
                            <Icon
                                name="i-search"
                                size=13
                                style="position:absolute;left:10px;color:var(--text-muted)"
                            />
                            <input
                                class="input"
                                style="padding-left:30px"
                                placeholder="Search a handle or an address"
                                aria-label="Search users"
                                prop:value=move || query.get()
                                on:input=move |event| query.set(input_value(&event))
                            />
                        </div>
                        <label class="switch">
                            <input
                                type="checkbox"
                                prop:checked=move || hide_bots.get()
                                on:change=move |_| hide_bots.update(|on| *on = !*on)
                            />
                            <span class="track"></span>
                            "Hide bots"
                        </label>
                    </div>
                    <div class="tbl-wrap" style="max-height:340px">
                        <table class="tbl">
                            <thead>
                                <tr>
                                    <th>"Handle"</th><th>"Address"</th><th>"Type"</th>
                                    <th>"Joined"</th><th>"Last seen"</th>
                                    <th class="n">"Equity"</th><th class="n">"Positions"</th>
                                    <th class="n">"Trades 24h"</th><th class="n">"Claims"</th>
                                    <th>"Status"</th><th style="width:150px"></th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || users.get()
                                    key=|user| format!("{}:{:?}", user.user_id, user.status)
                                    let:user
                                >
                                    {
                                        let id = user.user_id.clone();
                                        let handle = user.handle.clone();
                                        let suspended = user.status == AccountStatus::Suspended;
                                        view! {
                                            <tr>
                                                <td class="strong">{format!("@{}", user.handle)}</td>
                                                <td class="num dim">{format::hash(&user.address)}</td>
                                                <td>
                                                    {if user.is_bot {
                                                        view! { <span class="badge bot">"Bot"</span> }
                                                            .into_any()
                                                    } else {
                                                        view! { "Human" }.into_any()
                                                    }}
                                                </td>
                                                <td class="num">
                                                    {format::parse_rfc3339(&user.joined_at)
                                                        .map_or_else(|| user.joined_at.clone(),
                                                            format::calendar)}
                                                </td>
                                                <td>
                                                    {format::parse_rfc3339(&user.last_seen_at)
                                                        .map_or_else(
                                                            || user.last_seen_at.clone(),
                                                            |at| format::age(
                                                                crate::clock::now_millis()
                                                                    - at * 1_000))}
                                                </td>
                                                <td class="n">{format::fixed(user.equity, 2)}</td>
                                                <td class="n">{user.open_positions.to_string()}</td>
                                                <td class="n">{format::count(user.trades_24h)}</td>
                                                <td class="n">{user.faucet_claims.to_string()}</td>
                                                <td>
                                                    <span class=if suspended {
                                                        "badge stop"
                                                    } else {
                                                        "badge live"
                                                    }>{user.status.label()}</span>
                                                </td>
                                                <td>
                                                    <span style="display:flex;gap:var(--sp-6)">
                                                        {if suspended {
                                                            view! {
                                                                <button
                                                                    class="btn btn-secondary btn-sm"
                                                                    on:click=move |_| {
                                                                        let backend = backend.get_value();
                                                                        let id = id.clone();
                                                                        leptos::task::spawn_local(async move {
                                                                            let _ = backend.api
                                                                                .admin_unsuspend(&id).await;
                                                                            if let Ok(found) = backend.api
                                                                                .admin_users("", None, 60).await
                                                                            {
                                                                                users.set(found.users);
                                                                            }
                                                                        });
                                                                    }
                                                                >"Unsuspend"</button>
                                                            }.into_any()
                                                        } else {
                                                            view! {
                                                                <button
                                                                    class="btn btn-danger btn-sm rowact"
                                                                    on:click=move |_| dialog.set(
                                                                        Dialog::Suspend(
                                                                            id.clone(), handle.clone()))
                                                                >"Suspend"</button>
                                                            }.into_any()
                                                        }}
                                                    </span>
                                                </td>
                                            </tr>
                                        }
                                    }
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <TableState
                        load=Signal::derive(move || users_load.get())
                        empty=Signal::derive(move || users.read().is_empty())
                        title="No accounts match"
                        body="Every address that has signed in appears here, bots included. \
                              Clear the search, or start the bots from the simulation panel to \
                              give the exchange some counterparties."
                    />
                    <div class="addr-note">
                        <Icon name="i-info" size=14 style="color:var(--text-muted);flex:0 0 auto;margin-top:2px" />
                        <span>
                            "Suspending stops orders and faucet claims. It does "<b>"not"</b>
                            " close anything: existing positions stay open and keep accruing \
                             funding, because force-closing somebody's position is a different \
                             and much larger decision than stopping them trading. The suspend \
                             dialog asks for a reason, and the reason lands in the audit trail \
                             beside your address."
                        </span>
                    </div>
                </section>

                <section class="panel spine-market" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>"Markets"</h3>
                        <span class="badge">{move || markets.read().len().to_string()}</span>
                        {move || {
                            let halted = markets.read().iter()
                                .filter(|m| m.status == MarketStatus::Halted).count();
                            (halted > 0).then(|| view! {
                                <span class="badge warn">{format!("{halted} halted")}</span>
                            })
                        }}
                    </div>
                    <div class="tbl-wrap" style="max-height:360px">
                        <table class="tbl">
                            <thead>
                                <tr>
                                    <th>"Market"</th><th>"Status"</th><th class="n">"Mark"</th>
                                    <th class="n">"Oracle age"</th><th class="n">"Open interest"</th>
                                    <th class="n">"Resting levels"</th><th style="width:210px"></th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || markets.get()
                                    key=|market| format!("{}:{:?}", market.symbol, market.status)
                                    let:market
                                >
                                    {
                                        let halted = market.status == MarketStatus::Halted;
                                        let trading = market.status.is_tradable();
                                        let expired = matches!(
                                            market.status,
                                            MarketStatus::Expired | MarketStatus::Settling);
                                        // A null age means the oracle has never
                                        // published. Rendering that as zero made
                                        // the worst value in this column read as
                                        // the best one, on the one screen where
                                        // an operator checks whether prices can
                                        // be trusted.
                                        let oracle_age = market.oracle_age_blocks;
                                        let stale = !market.oracle_fresh
                                            || oracle_age.is_none_or(|age| age > 5);
                                        let symbol = market.symbol.clone();
                                        let for_risk = market.clone();
                                        let for_settle = market.clone();
                                        let for_halt = market.symbol.clone();
                                        view! {
                                            <tr class:halted=!trading>
                                                <td class="strong">{market.symbol.clone()}</td>
                                                <td>
                                                    <span class=crate::ui::book::status_badge_class(
                                                        market.status)>
                                                        {market.status.label()}
                                                    </span>
                                                </td>
                                                <td class="n">
                                                    {market.mark().map_or_else(
                                                        || "\u{2014}".to_string(),
                                                        |mark| format::fixed(mark, 2))}
                                                </td>
                                                <td class="n">
                                                    {match oracle_age {
                                                        None => view! {
                                                            <span class="badge warn">
                                                                "No oracle price"
                                                            </span>
                                                        }.into_any(),
                                                        Some(age) => view! {
                                                            <span
                                                                class:badge=stale
                                                                class:warn=stale
                                                            >
                                                                {format!(
                                                                    "{} blocks",
                                                                    format::count(age))}
                                                            </span>
                                                        }.into_any(),
                                                    }}
                                                </td>
                                                <td class="n">
                                                    {format::fixed(market.open_interest, 4)}
                                                </td>
                                                <td class="n">
                                                    {format::count(market.resting_levels)}
                                                </td>
                                                <td>
                                                    <span style="display:flex;gap:var(--sp-6);justify-content:flex-end">
                                                        <button
                                                            class="btn btn-secondary btn-sm"
                                                            on:click=move |_| dialog.set(
                                                                Dialog::Risk(Box::new(for_risk.clone())))
                                                        >"Risk"</button>
                                                        {if halted {
                                                            view! {
                                                                <button
                                                                    class="btn btn-secondary btn-sm"
                                                                    on:click=move |_| {
                                                                        let backend = backend.get_value();
                                                                        let symbol = symbol.clone();
                                                                        leptos::task::spawn_local(async move {
                                                                            let _ = backend.api
                                                                                .admin_resume(&symbol).await;
                                                                            if let Ok(found) =
                                                                                backend.api.admin_markets().await
                                                                            {
                                                                                markets.set(found.markets);
                                                                            }
                                                                        });
                                                                    }
                                                                >"Resume"</button>
                                                            }.into_any()
                                                        } else if expired {
                                                            view! {
                                                                <button
                                                                    class="btn btn-ghost btn-sm"
                                                                    disabled=true
                                                                    title="A market that has expired cannot resume"
                                                                >"Resume"</button>
                                                            }.into_any()
                                                        } else {
                                                            view! {
                                                                <button
                                                                    class="btn btn-ghost btn-sm"
                                                                    on:click=move |_| dialog.set(
                                                                        Dialog::Halt(for_halt.clone()))
                                                                >"Halt"</button>
                                                            }.into_any()
                                                        }}
                                                        // Settle stays visible and
                                                        // disabled on a trading
                                                        // market, with its reason
                                                        // on the control, rather
                                                        // than disappearing.
                                                        {if trading {
                                                            view! {
                                                                <button
                                                                    class="btn btn-ghost btn-sm"
                                                                    disabled=true
                                                                    title="Force-settle needs the market to be expired or halted"
                                                                >"Settle"</button>
                                                            }.into_any()
                                                        } else {
                                                            view! {
                                                                <button
                                                                    class="btn btn-danger btn-sm"
                                                                    on:click=move |_| dialog.set(
                                                                        Dialog::Settle(
                                                                            Box::new(for_settle.clone())))
                                                                >"Settle"</button>
                                                            }.into_any()
                                                        }}
                                                    </span>
                                                </td>
                                            </tr>
                                        }
                                    }
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <TableState
                        load=Signal::derive(move || markets_load.get())
                        empty=Signal::derive(move || markets.read().is_empty())
                        title="No markets listed"
                        body="The gateway reports the markets in the chain's genesis. An empty \
                              list means it is not reading the node."
                    />
                    <div class="addr-note">
                        <Icon name="i-info" size=14 style="color:var(--text-muted);flex:0 0 auto;margin-top:2px" />
                        <span>
                            "Halting stops new orders and leaves the resting book alone; \
                             cancelling stays allowed, because trapping somebody's order with no \
                             way out would be worse than whatever caused the halt. "
                            <b>"Settle stays visible and disabled on a trading market"</b>
                            ", with the reason on the control, rather than disappearing \u{2014} \
                             an operator hunting for a missing button under pressure is how the \
                             wrong button gets pressed."
                        </span>
                    </div>
                </section>

                <FaucetControls faucet=faucet reload=Callback::new(move |()| reload()) />
                <SimControls sim=sim />

                <section class="panel spine-account" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>{t("Audit trail")}</h3>
                        <span class="spacer"></span>
                        <span class="t-meta">
                            "Every mutating action, with the address that made it"
                        </span>
                    </div>
                    <div class="adm-search">
                        <div class="select" style="width:200px">
                            <select
                                aria-label="Filter by action"
                                on:change=move |event| action_filter.set(select_value(&event))
                            >
                                <option value="">"All actions"</option>
                                <option value="market.halt">"market.halt"</option>
                                <option value="market.settle">"market.settle"</option>
                                <option value="market.risk">"market.risk"</option>
                                <option value="user.suspend">"user.suspend"</option>
                                <option value="faucet">"faucet.*"</option>
                                <option value="sim">"sim.*"</option>
                            </select>
                        </div>
                    </div>
                    <div class="tbl-wrap" style="max-height:300px">
                        <table class="tbl">
                            <thead>
                                <tr>
                                    <th class="n" style="width:92px">"At"</th>
                                    <th style="width:150px">"Actor"</th>
                                    <th style="width:150px">"Action"</th>
                                    <th style="width:180px">"Subject"</th>
                                    <th>"Detail"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || audit.get()
                                    key=|record| record.record_id.clone()
                                    let:record
                                >
                                    <tr>
                                        <td class="n">
                                            {format::parse_rfc3339(&record.at)
                                                .map_or_else(|| record.at.clone(), format::clock)}
                                        </td>
                                        // An address, not a handle: a handle can
                                        // be changed with one PATCH and an
                                        // address cannot change at all.
                                        <td class="num dim">{format::hash(&record.actor)}</td>
                                        <td class="strong">{record.action.clone()}</td>
                                        <td class="num">
                                            {record.subject.clone()
                                                .unwrap_or_else(|| "\u{2014}".to_string())}
                                        </td>
                                        <td>
                                            {record.detail.iter()
                                                .map(|(key, value)| format!("{key}: {value}"))
                                                .collect::<Vec<_>>()
                                                .join(" \u{00B7} ")}
                                        </td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <TableState
                        load=Signal::derive(move || audit_load.get())
                        empty=Signal::derive(move || audit.read().is_empty())
                        title="No operator actions yet"
                        body="Halts, settlements, risk changes, suspensions and faucet changes \
                              all land here with the address that made them. Nothing has been \
                              done to this exchange so far."
                    />
                    <div style="display:flex;align-items:center;gap:var(--sp-12);padding:var(--sp-12) var(--sp-16);border-top:var(--bw-hair) solid var(--border-subtle)">
                        <span class="t-meta">
                            "The audit trail is the answer to "<i>"who halted this market"</i>
                            ", so it is append-only and the actor column is an address rather \
                             than a handle."
                        </span>
                    </div>
                </section>
            </Show>
        </AppShell>

        <AdminDialogs dialog=dialog on_done=Callback::new(move |()| reload()) />
    }
}

/// The faucet's operator controls.
#[component]
fn FaucetControls(
    /// The faucet configuration.
    faucet: RwSignal<Option<AdminFaucet>>,
    /// Called after a change lands.
    reload: Callback<()>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let cooldown = RwSignal::new(String::new());
    let sending = RwSignal::new(false);
    let failure = RwSignal::new(None::<(String, String)>);
    let confirming_stop = RwSignal::new(false);

    // Turning the faucet off stops every new player getting money, which is the
    // single most consequential switch on this console. It goes behind the same
    // dialog as halting one market.
    let set_enabled = move |enabled: bool| {
        let backend = backend.get_value();
        let Some(state) = faucet.read_untracked().clone() else {
            return;
        };
        sending.set(true);
        failure.set(None);
        leptos::task::spawn_local(async move {
            let result = backend
                .api
                .admin_set_faucet(enabled, state.cooldown_seconds, &state.grant)
                .await;
            sending.set(false);
            match result {
                Ok(updated) => {
                    faucet.set(Some(updated));
                    confirming_stop.set(false);
                    stores.notify(
                        if enabled { Tone::Good } else { Tone::Warn },
                        if enabled {
                            "Faucet started"
                        } else {
                            "Faucet stopped"
                        },
                        if enabled {
                            "New players can claim again."
                        } else {
                            "No new player can get money until it is started again."
                        },
                    );
                    reload.run(());
                }
                Err(error) => failure.set(Some(crate::failure_text(&error))),
            }
        });
    };

    view! {
        <Show when=move || confirming_stop.get()>
            <Modal
                title="Stop the faucet"
                icon="i-warn"
                icon_colour="var(--ask)"
                foot=move || view! {
                    <>
                        <button
                            class="btn btn-ghost"
                            on:click=move |_| confirming_stop.set(false)
                        >"Cancel"</button>
                        <button
                            class="btn btn-danger"
                            disabled=move || sending.get()
                            data-loading=move || sending.get().then_some("")
                            on:click=move |_| set_enabled(false)
                        >"Stop the faucet"</button>
                    </>
                }.into_any()
            >
                <p style="font-size:var(--fs-sm)">
                    "Stopping the faucet refuses "<b style="color:var(--text-primary)">
                    "every new claim"</b>", which is the only way anybody on this exchange gets \
                     money. Nobody who has already claimed loses anything, trading and \
                     cancelling carry on, and starting it again takes one press \u{2014} but \
                     until then a new player lands on a screen they cannot do anything with."
                </p>
                <crate::ui::MutationFailure failure=failure />
            </Modal>
        </Show>
        <section class="panel spine-account" style="margin-top:var(--sp-16)">
            <div class="panel-head">
                <h3>"Faucet"</h3>
                <span class="spacer"></span>
                {move || faucet.read().as_ref().map(|faucet| view! {
                    <span class=if faucet.enabled { "badge live" } else { "badge stop" }>
                        {if faucet.enabled { "Paying out" } else { "Stopped" }}
                    </span>
                })}
            </div>
            <div style="padding:var(--sp-16);display:flex;gap:var(--sp-24);flex-wrap:wrap;align-items:flex-end">
                {move || faucet.read().as_ref().map(|state| {
                    let enabled = state.enabled;
                    let seconds = state.cooldown_seconds;
                    view! {
                        <>
                            <span class="stat">
                                <span class="k">"Claims 24h"</span>
                                <span class="v">{format::count(state.claims_24h)}</span>
                            </span>
                            // Both of these are arrays across every asset the
                            // faucet pays. Rendering element zero unlabelled
                            // showed "1,000.00" beside a grant of 100,000 USD,
                            // so an operator would read a faucet with a thousand
                            // grants left as one about to fail.
                            <span class="stat">
                                <span class="k">"Granted total"</span>
                                <span class="v" style="font-size:var(--fs-base)">
                                    {assets(&state.granted_total)}
                                </span>
                            </span>
                            // A faucet that has run dry looks exactly like a
                            // broken faucet to a user, so the balance is shown —
                            // and the useful figure is not what it holds but how
                            // many more grants that is.
                            <span class="stat">
                                <span class="k">"Authority left"</span>
                                <span class="v" style="font-size:var(--fs-base)">
                                    {assets(&state.authority_balance)}
                                </span>
                            </span>
                            <span class="stat">
                                <span class="k">"Grants left"</span>
                                <span class="v">
                                    {grants_remaining(
                                        &state.authority_balance, &state.grant)
                                        .map_or_else(
                                            || "\u{2014}".to_string(),
                                            |count| format!(
                                                "about {}", format::count(count)))}
                                </span>
                            </span>
                            <span class="stat">
                                <span class="k">"Grant"</span>
                                <span class="v" style="font-size:var(--fs-base)">
                                    {state.grant.iter()
                                        .map(|grant| format!(
                                            "{} {}", format::fixed(grant.amount, 2), grant.asset))
                                        .collect::<Vec<_>>()
                                        .join(" \u{00B7} ")}
                                </span>
                            </span>
                            <div class="field" style="width:160px">
                                <label for="fa-cd">"Cooldown, seconds"</label>
                                <div class="numfield">
                                    <input
                                        id="fa-cd"
                                        inputmode="numeric"
                                        prop:value=move || {
                                            let typed = cooldown.get();
                                            if typed.is_empty() { seconds.to_string() } else { typed }
                                        }
                                        on:input=move |event| cooldown.set(input_value(&event))
                                    />
                                </div>
                            </div>
                            <button
                                class="btn btn-secondary"
                                disabled=move || sending.get()
                                data-loading=move || sending.get().then_some("")
                                on:click=move |_| {
                                    let backend = backend.get_value();
                                    let seconds = cooldown.get_untracked()
                                        .parse::<i64>().unwrap_or(seconds);
                                    let grant = faucet.read_untracked().as_ref()
                                        .map(|state| state.grant.clone()).unwrap_or_default();
                                    sending.set(true);
                                    failure.set(None);
                                    leptos::task::spawn_local(async move {
                                        let result = backend.api
                                            .admin_set_faucet(enabled, seconds, &grant).await;
                                        sending.set(false);
                                        match result {
                                            Ok(updated) => {
                                                faucet.set(Some(updated));
                                                stores.notify(
                                                    Tone::Good,
                                                    "Faucet updated",
                                                    format!(
                                                        "Cooldown is now {}.",
                                                        format::duration(seconds),
                                                    ),
                                                );
                                                reload.run(());
                                            }
                                            Err(error) => failure
                                                .set(Some(crate::failure_text(&error))),
                                        }
                                    });
                                }
                            >"Save cooldown"</button>
                            <button
                                class=if enabled { "btn btn-danger" } else { "btn btn-primary" }
                                disabled=move || sending.get()
                                data-loading=move || sending.get().then_some("")
                                on:click=move |_| if enabled {
                                    confirming_stop.set(true);
                                } else {
                                    set_enabled(true);
                                }
                            >
                                {if enabled { "Stop the faucet" } else { "Start the faucet" }}
                            </button>
                        </>
                    }
                })}
            </div>
            // The panel reports its own failures: this one sits behind no
            // scrim, but a change that did nothing still has to say so.
            <div style="padding:0 var(--sp-16) var(--sp-16)">
                <Show when=move || !confirming_stop.get()>
                    <crate::ui::MutationFailure failure=failure />
                </Show>
            </div>
        </section>
    }
}

/// Every asset in a grant list, as `100,000.00 USD · 1.00000000 BTC`.
///
/// Never the first element alone: these lists run across eight assets and the
/// first of them is not a summary of the rest.
fn assets(grants: &[Grant]) -> String {
    if grants.is_empty() {
        return "\u{2014}".to_string();
    }
    grants
        .iter()
        .map(|grant| format!("{} {}", format::notional(grant.amount), grant.asset))
        .collect::<Vec<_>>()
        .join(" \u{00B7} ")
}

/// How many more grants the authority account can pay, across every asset.
///
/// The binding asset is the one that runs out first, which is the only honest
/// answer to "how long does this last". `None` when the grant is empty.
fn grants_remaining(balance: &[Grant], grant: &[Grant]) -> Option<u64> {
    // An empty balance list is the gateway not saying, which is not the same
    // as a faucet with nothing left — and "about 0 grants left" on a faucet
    // that is paying out fine is the misleading direction again.
    if balance.is_empty() {
        return None;
    }
    let mut fewest: Option<u64> = None;
    for line in grant.iter().filter(|line| line.amount > Decimal::ZERO) {
        let held = balance
            .iter()
            .find(|held| held.asset == line.asset)
            .map_or(Decimal::ZERO, |held| held.amount);
        let count = u64::try_from((held / line.amount).trunc()).unwrap_or(0);
        fewest = Some(fewest.map_or(count, |held: u64| held.min(count)));
    }
    fewest
}

/// The simulator's controls: the three-position switch and the scenario.
#[component]
fn SimControls(
    /// The simulator's state.
    sim: RwSignal<Option<SimStatus>>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let busy = RwSignal::new(false);

    let send = move |request: SimRequest| {
        let backend = backend.get_value();
        busy.set(true);
        leptos::task::spawn_local(async move {
            match backend.api.admin_set_sim(&request).await {
                Ok(updated) => {
                    stores.notify(
                        Tone::Good,
                        t(updated.mode.label()),
                        t(updated.mode.describe()),
                    );
                    sim.set(Some(updated));
                }
                Err(error) => crate::report(&backend, stores, &error),
            }
            busy.set(false);
        });
    };

    view! {
        <section class="panel spine-chain sim-panel" style="margin-top:var(--sp-16)" data-testid="sim-panel">
            <div class="panel-head">
                <Sprite name="robot" size=20 />
                <h3>{t("Simulation")}</h3>
                <span class="spacer"></span>
                {move || sim.read().as_ref().map(|sim| view! {
                    <span
                        class=if sim.running { "badge live" } else { "badge" }
                        data-testid="sim-mode"
                        data-mode=sim.mode.as_str()
                    >
                        {t(sim.mode.label())}
                    </span>
                })}
            </div>
            <div style="padding:var(--sp-16);display:flex;flex-direction:column;gap:var(--sp-16)">
                <p class="t-body" style="font-size:var(--fs-sm)">
                    {t("This is a simulator, so the operator may change the rules of the game. The switch decides who else is trading; the scenario decides what the oracle does to every derivative market's price.")}
                </p>
                {move || sim.read().as_ref().map(|state| {
                    let current = state.mode;
                    let scenario = state.scenario;
                    view! {
                        <>
                            <div class="mode-switch" role="radiogroup" aria-label="Simulation switch">
                                {SimMode::ALL.into_iter().map(|mode| view! {
                                    <button
                                        class="mode-option"
                                        role="radio"
                                        data-mode=mode.as_str()
                                        aria-checked=(current == mode).to_string()
                                        disabled=move || busy.get()
                                        on:click=move |_| send(SimRequest {
                                            action: "mode".into(),
                                            mode: Some(mode),
                                            scenario: None,
                                            seed: None,
                                        })
                                    >
                                        <Sprite
                                            name=match mode {
                                                SimMode::Off => "skull",
                                                SimMode::On => "robot",
                                                SimMode::Max => "rocket",
                                            }
                                            size=32
                                        />
                                        <b>{t(mode.label())}</b>
                                        <span>{t(mode.describe())}</span>
                                    </button>
                                }).collect_view()}
                            </div>
                            <div style="display:flex;gap:var(--sp-12);align-items:center;flex-wrap:wrap">
                                <span class="t-meta">{t("Scenario")}</span>
                                <div class="seg" role="group" aria-label="Scenario">
                                    {Scenario::ALL.into_iter().map(|option| view! {
                                        <button
                                            aria-pressed=(scenario == option).to_string()
                                            data-scenario=option.as_str()
                                            on:click=move |_| send(SimRequest {
                                                action: "scenario".into(),
                                                mode: None,
                                                scenario: Some(option),
                                                seed: None,
                                            })
                                        >{t(option.label())}</button>
                                    }).collect_view()}
                                </div>
                            </div>
                            <p class="t-meta">
                                {format!(
                                    "{} bots. Bots are badged wherever they appear in the \
                                     product, because a lone human needs a counterparty and \
                                     pretending the counterparty is human would make the \
                                     leaderboard dishonest.",
                                    state.bots.values().sum::<u32>(),
                                )}
                            </p>
                            <hr style="border:0;border-top:1px solid var(--border-subtle)" />
                            <div class="simfacts">
                                <span class="stat">
                                    <span class="k">{t("Elapsed")}</span>
                                    <span class="v">
                                        {format::duration(
                                            i64::try_from(state.elapsed_seconds).unwrap_or(0))}
                                    </span>
                                </span>
                                <span class="stat">
                                    <span class="k">{t("Transactions since switched on")}</span>
                                    <span class="v">{format::count(state.orders_submitted)}</span>
                                </span>
                                <span class="stat">
                                    <span class="k">{t("Seed")}</span>
                                    <span class="v">{state.seed.to_string()}</span>
                                </span>
                                // Anything but zero is a bug in the exchange, and
                                // the dashboard says so in vermilion.
                                <span class="stat">
                                    <span class="k">{t("Invariant violations")}</span>
                                    <span
                                        class="v"
                                        style={if state.invariant_violations == 0 {
                                            ""
                                        } else {
                                            "color:var(--ask)"
                                        }}
                                    >{format::count(state.invariant_violations)}</span>
                                </span>
                            </div>
                        </>
                    }
                })}
            </div>
        </section>
    }
}

/// Every confirmation the admin screen can raise.
#[component]
fn AdminDialogs(
    /// Which dialog is open.
    dialog: RwSignal<Dialog>,
    /// Called after anything lands, to refresh the page.
    on_done: Callback<()>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let reason = RwSignal::new(String::new());
    let price_one = RwSignal::new(String::new());
    let price_two = RwSignal::new(String::new());
    let override_band = RwSignal::new(false);
    let leverage = RwSignal::new(String::new());
    let initial = RwSignal::new(String::new());
    let maintenance = RwSignal::new(String::new());

    // One in-flight flag and one failure region for the dialogs that are
    // rendered here. A mutating dialog may not close until the gateway has
    // answered, and a failure is shown where the button was pressed.
    let sending = RwSignal::new(false);
    let failure = RwSignal::new(None::<(String, String)>);

    let close = move || {
        dialog.set(Dialog::None);
        sending.set(false);
        failure.set(None);
        reason.set(String::new());
        price_one.set(String::new());
        price_two.set(String::new());
        override_band.set(false);
        leverage.set(String::new());
        initial.set(String::new());
        maintenance.set(String::new());
    };

    view! {
        {move || match dialog.get() {
            Dialog::None => ().into_any(),

            Dialog::Suspend(user_id, handle) => view! {
                <Modal
                    title=format!("Suspend @{handle}")
                    icon="i-warn"
                    icon_colour="var(--ask)"
                    foot=move || {
                        let user_id = user_id.clone();
                        view! {
                            <>
                                <button class="btn btn-ghost" on:click=move |_| close()>"Cancel"</button>
                                <button
                                    class="btn btn-danger"
                                    disabled=move || reason.get().trim().is_empty() || sending.get()
                                    data-loading=move || sending.get().then_some("")
                                    on:click=move |_| {
                                        let backend = backend.get_value();
                                        let user_id = user_id.clone();
                                        let why = reason.get_untracked();
                                        sending.set(true);
                                        failure.set(None);
                                        leptos::task::spawn_local(async move {
                                            let result =
                                                backend.api.admin_suspend(&user_id, &why).await;
                                            sending.set(false);
                                            match result {
                                                Ok(()) => {
                                                    stores.notify(
                                                        Tone::Warn,
                                                        "Account suspended",
                                                        "Orders and faucet claims are stopped. \
                                                         Positions are untouched.",
                                                    );
                                                    on_done.run(());
                                                    close();
                                                }
                                                Err(error) => failure
                                                    .set(Some(crate::failure_text(&error))),
                                            }
                                        });
                                    }
                                >"Suspend the account"</button>
                            </>
                        }.into_any()
                    }
                >
                    <p style="font-size:var(--fs-sm)">
                        "Suspending stops orders and faucet claims. It does "<b>"not"</b>
                        " close anything: existing positions stay open and keep accruing \
                         funding, because force-closing somebody's position is a different and \
                         much larger decision than stopping them trading."
                    </p>
                    <div class="field" style="margin-top:var(--sp-16)">
                        <label for="sus-reason">"Reason"</label>
                        <input
                            class="input"
                            id="sus-reason"
                            placeholder="abusive order flow"
                            prop:value=move || reason.get()
                            on:input=move |event| reason.set(input_value(&event))
                        />
                        <span class="hint">
                            "This lands in the audit trail beside your address."
                        </span>
                    </div>
                    <crate::ui::MutationFailure failure=failure />
                </Modal>
            }
            .into_any(),

            Dialog::Halt(symbol) => view! {
                <Modal
                    title=format!("Halt {symbol}")
                    icon="i-warn"
                    icon_colour="var(--brass)"
                    foot=move || {
                        let symbol = symbol.clone();
                        view! {
                            <>
                                <button class="btn btn-ghost" on:click=move |_| close()>"Cancel"</button>
                                <button
                                    class="btn btn-danger"
                                    disabled=move || reason.get().trim().is_empty() || sending.get()
                                    data-loading=move || sending.get().then_some("")
                                    // A disabled button never hides its reason.
                                    title=move || if reason.get().trim().is_empty() {
                                        "Halting needs a reason, which lands in the audit trail"
                                    } else {
                                        ""
                                    }
                                    on:click=move |_| {
                                        let backend = backend.get_value();
                                        let symbol = symbol.clone();
                                        let why = reason.get_untracked();
                                        sending.set(true);
                                        failure.set(None);
                                        leptos::task::spawn_local(async move {
                                            let result =
                                                backend.api.admin_halt(&symbol, &why).await;
                                            sending.set(false);
                                            match result {
                                                Ok(()) => {
                                                    stores.notify(
                                                        Tone::Warn,
                                                        format!("{symbol} halted"),
                                                        "New orders are refused. Resting orders \
                                                         are untouched and can still be cancelled.",
                                                    );
                                                    on_done.run(());
                                                    close();
                                                }
                                                Err(error) => failure
                                                    .set(Some(crate::failure_text(&error))),
                                            }
                                        });
                                    }
                                >"Halt the market"</button>
                            </>
                        }.into_any()
                    }
                >
                    <p style="font-size:var(--fs-sm)">
                        "Halting stops new orders and leaves the resting book alone. Cancelling \
                         stays allowed, because trapping somebody's order with no way out would \
                         be worse than whatever caused the halt. Positions still accrue funding."
                    </p>
                    <div class="field" style="margin-top:var(--sp-16)">
                        <label for="halt-reason">"Reason"</label>
                        <input
                            class="input"
                            id="halt-reason"
                            placeholder="oracle feed stale"
                            prop:value=move || reason.get()
                            on:input=move |event| reason.set(input_value(&event))
                        />
                        <span class="hint">
                            "Required. It lands in the audit trail beside your address, and it is \
                             what a player asking why their market stopped is shown."
                        </span>
                    </div>
                    <crate::ui::MutationFailure failure=failure />
                </Modal>
            }
            .into_any(),

            Dialog::Settle(market) => view! {
                <SettleDialog
                    market=*market
                    price_one=price_one
                    price_two=price_two
                    override_band=override_band
                    on_close=Callback::new(move |()| close())
                    on_done=on_done
                />
            }
            .into_any(),

            Dialog::Risk(market) => view! {
                <RiskDialog
                    market=*market
                    leverage=leverage
                    initial=initial
                    maintenance=maintenance
                    on_close=Callback::new(move |()| close())
                    on_applied=Callback::new(move |(symbol, applied): (String, RiskApplied)| {
                        dialog.set(Dialog::RiskApplied(symbol, applied));
                        on_done.run(());
                    })
                />
            }
            .into_any(),

            Dialog::RiskApplied(symbol, applied) => view! {
                <Modal
                    title={if applied.accounts_now_below_maintenance > 0 {
                        format!(
                            "Applied \u{00B7} {} account{} moved below maintenance",
                            applied.accounts_now_below_maintenance,
                            if applied.accounts_now_below_maintenance == 1 { "" } else { "s" },
                        )
                    } else {
                        format!("Applied to {symbol}")
                    }}
                    icon={if applied.accounts_now_below_maintenance > 0 { "i-warn" } else { "i-check" }}
                    icon_colour={if applied.accounts_now_below_maintenance > 0 {
                        "var(--ask)"
                    } else {
                        "var(--bid)"
                    }}
                    foot=move || view! {
                        <button class="btn btn-primary" on:click=move |_| close()>"Done"</button>
                    }.into_any()
                >
                    {if applied.accounts_now_below_maintenance > 0 {
                        view! {
                            <Banner
                                tone="danger"
                                title=format!(
                                    "{} accounts are now below maintenance on {symbol}",
                                    applied.accounts_now_below_maintenance,
                                )
                                body="They are liquidated on the next block unless the parameters \
                                      go back or the accounts add margin. Restoring the old \
                                      values does not un-liquidate anything that has already \
                                      happened."
                            />
                        }
                        .into_any()
                    } else {
                        view! {
                            <Banner
                                tone="good"
                                title="No account was put below maintenance"
                                body="The gateway counts the accounts a change puts below \
                                      maintenance and returns the figure with the response. It \
                                      returned zero."
                            />
                        }
                        .into_any()
                    }}
                    <p class="t-meta" style="margin-top:var(--sp-12);line-height:var(--lh-snug)">
                        "This is the figure the change actually produced, which the dialog \
                         showed you as a dry run before you pressed Apply. It stays on screen \
                         rather than going into a toast you could miss."
                    </p>
                </Modal>
            }
            .into_any(),
        }}
    }
}

/// Force-settle, with five devices stacked against a mistyped price.
#[component]
fn SettleDialog(
    /// The market being settled.
    market: AdminMarket,
    /// The price, typed once.
    price_one: RwSignal<String>,
    /// The price, typed again, independently.
    price_two: RwSignal<String>,
    /// Whether the operator has accepted going outside the oracle band.
    override_band: RwSignal<bool>,
    /// Close without settling.
    on_close: Callback<()>,
    /// Refresh after settling.
    on_done: Callback<()>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let symbol = market.symbol.clone();
    // `None` when the oracle has never published. There is no such thing as a
    // deviation from a mark that does not exist, and computing one as `0/0`
    // reported every price as safe on exactly the markets where a typo is most
    // dangerous.
    let mark = market.mark();
    let sending = RwSignal::new(false);
    let failure = RwSignal::new(None::<(String, String)>);

    let typed = move || parse_typed(&price_one.get());
    let deviation = move || {
        let mark = mark?;
        typed().map(|price| ((price - mark) / mark * Decimal::from(100)).round_dp(2))
    };
    let outside_band =
        move || deviation().is_some_and(|dev| dev.abs() > Decimal::from(ORACLE_BAND_PCT));
    // No mark means the gateway's band cannot validate anything, so the
    // override stops being optional. Two of the five devices standing between
    // an operator and a mistyped settlement used to fail open here; now the
    // absence of a mark demands the stronger confirmation rather than the
    // weaker one.
    let unverifiable = move || mark.is_none();
    let needs_force = move || outside_band() || unverifiable();
    let matched = move || {
        let (one, two) = (typed(), parse_typed(&price_two.get()));
        one.is_some() && one == two
    };
    let can_settle = move || {
        typed().is_some_and(|price| price > Decimal::ZERO)
            && matched()
            && (!needs_force() || override_band.get())
            && !sending.get()
    };

    // `StoredValue` because the modal footer is an `Fn` slot: nothing inside it
    // may consume a captured value.
    let for_foot = StoredValue::new(symbol.clone());
    view! {
        <Modal
            title=format!("Force-settle {symbol}")
            icon="i-warn"
            icon_colour="var(--ask)"
            foot=move || {
                let symbol = for_foot.get_value();
                view! {
                    <>
                        <button class="btn btn-ghost" on:click=move |_| on_close.run(())>
                            "Cancel"
                        </button>
                        <button
                            class="btn btn-danger"
                            disabled=move || !can_settle()
                            data-loading=move || sending.get().then_some("")
                            on:click=move |_| {
                                let Some(price) = typed() else { return };
                                let backend = backend.get_value();
                                let symbol = symbol.clone();
                                let force = override_band.get_untracked();
                                sending.set(true);
                                failure.set(None);
                                leptos::task::spawn_local(async move {
                                    let result =
                                        backend.api.admin_settle(&symbol, price, force).await;
                                    sending.set(false);
                                    match result {
                                        Ok(()) => {
                                            stores.notify(
                                                Tone::Warn,
                                                format!("{symbol} settled"),
                                                format!(
                                                    "Every position closed at {}.",
                                                    format::fixed(price, 2),
                                                ),
                                            );
                                            on_done.run(());
                                            on_close.run(());
                                        }
                                        // The dialog stays open on a failure.
                                        // Closing it would say the irreversible
                                        // thing had happened when it had not.
                                        Err(error) => {
                                            failure.set(Some(crate::failure_text(&error)));
                                        }
                                    }
                                });
                            }
                        >
                            {move || match typed() {
                                Some(price) => format!(
                                    "Settle {} at {}",
                                    for_foot.get_value(),
                                    format::fixed(price, 2),
                                ),
                                None => format!("Settle {}", for_foot.get_value()),
                            }}
                        </button>
                    </>
                }
                .into_any()
            }
        >
            <p style="font-size:var(--fs-sm)">
                "This closes "<b style="color:var(--text-primary)">"every open position"</b>
                {format!(" in {}", market.symbol)}
                " at the price you type, pays the difference out of each account, and leaves \
                 zero open interest. It cannot be undone, and there is no unsettle."
            </p>
            <div class="ticket" style="padding:0;gap:var(--sp-6);margin:var(--sp-16) 0">
                <div class="row">
                    <span class="k">"Status"</span>
                    <span class="v">{market.status.label()}</span>
                </div>
                <div class="row">
                    <span class="k">"Open interest"</span>
                    <span class="v">{format::fixed(market.open_interest, 4)}</span>
                </div>
                <div class="row">
                    <span class="k">"Resting orders"</span>
                    <span class="v">{format::count(market.resting_levels)}</span>
                </div>
                <div class="row">
                    <span class="k">"Last oracle mark"</span>
                    <span class="v">
                        {mark.map_or_else(
                            || "\u{2014} never published".to_string(),
                            |mark| format::fixed(mark, 2))}
                    </span>
                </div>
            </div>

            <div class="field">
                <label for="sp1">"Settlement price"</label>
                <div class="numfield" style=move || if needs_force() {
                    "border-color:var(--ask)"
                } else {
                    ""
                }>
                    <input
                        id="sp1"
                        inputmode="decimal"
                        aria-invalid=move || needs_force().then_some("true")
                        prop:value=move || price_one.get()
                        on:input=move |event| price_one.set(input_value(&event))
                    />
                    <span class="unit">"USD"</span>
                </div>
            </div>

            <div style="display:flex;flex-direction:column;gap:var(--sp-8);margin-top:var(--sp-10)">
                // The magnitude in words. A dropped digit survives a numeral and
                // does not survive being read out.
                <div class="magnitude" class:bad=move || needs_force()>
                    <span class="k">"In words"</span>
                    <span class="v">
                        {move || typed().map_or_else(
                            || "\u{2014}".to_string(), magnitude_in_words)}
                    </span>
                </div>
                // No mark: no percentage, in vermilion, saying what is missing.
                // A figure here would be arithmetic on an absent number.
                <Show when=unverifiable>
                    <span class="devline bad">
                        <Icon name="i-warn" size=11 style="color:var(--ask)" />
                        <b>"There is no oracle mark to compare against."</b>
                        " The gateway's 10% band cannot validate this price, so nothing on this \
                         screen and nothing on the server will catch a mistyped digit."
                    </span>
                </Show>
                {move || deviation().map(|dev| {
                    let bad = outside_band();
                    let mark = mark.unwrap_or_default();
                    view! {
                        <span class="devline" class:warn=!bad class:bad=bad>
                            <Icon
                                name="i-warn"
                                size=11
                                style={if bad { "color:var(--ask)" } else { "color:var(--brass)" }}
                            />
                            <b>{format::pct(dev.abs())}</b>
                            {if dev >= Decimal::ZERO { " above " } else { " below " }}
                            "the last oracle mark of "
                            <b>{format::fixed(mark, 2)}</b>
                            {if bad { "." } else { ". Inside the gateway's 10% band." }}
                        </span>
                    }
                })}
            </div>

            <Show when=needs_force>
                <div style="margin-top:var(--sp-12)">
                    {move || if unverifiable() {
                        view! {
                            <Banner
                                tone="danger"
                                title="This market has no oracle mark"
                                body="The gateway checks a settlement price against the last \
                                      oracle mark and refuses anything more than 10% away. This \
                                      market has never had one, so that check cannot run and the \
                                      price you type is the only thing between every open \
                                      position and the wrong number."
                            />
                        }.into_any()
                    } else {
                        view! {
                            <Banner
                                tone="danger"
                                title="Outside the 10% oracle band"
                                body="The gateway rejects a settlement price more than 10% from \
                                      the last oracle mark. A price this far out is almost \
                                      always a digit that was typed twice. Check the mark before \
                                      you override."
                            />
                        }.into_any()
                    }}
                </div>
                <label class="check" style="margin-top:var(--sp-12);font-size:var(--fs-sm)">
                    <input
                        type="checkbox"
                        prop:checked=move || override_band.get()
                        on:change=move |_| override_band.update(|on| *on = !*on)
                    />
                    <span class="box">
                        <svg viewBox="0 0 12 12" aria-hidden="true"><use href="#i-check"></use></svg>
                    </span>
                    <span>
                        {move || {
                            let price = typed()
                                .map_or_else(|| "\u{2014}".to_string(), |p| format::fixed(p, 2));
                            match mark {
                                Some(mark) => format!(
                                    "Settle outside the band anyway. I have compared {price} \
                                     with the last mark of {} and I am sending force: true.",
                                    format::fixed(mark, 2),
                                ),
                                None => format!(
                                    "Settle at {price} with nothing to check it against. The \
                                     gateway cannot validate this price, I have confirmed it \
                                     from another source, and I am sending force: true.",
                                ),
                            }
                        }}
                    </span>
                </label>
            </Show>

            <crate::ui::MutationFailure failure=failure />

            <div class="field" style="margin-top:var(--sp-16)">
                <label for="sp2">"Type the settlement price again"</label>
                <div class="numfield">
                    <input
                        id="sp2"
                        inputmode="decimal"
                        placeholder="\u{2014}"
                        prop:value=move || price_two.get()
                        on:input=move |event| price_two.set(input_value(&event))
                    />
                    <span class="unit">"USD"</span>
                </div>
                {move || {
                    if price_two.get().trim().is_empty() {
                        view! {
                            <span class="err">
                                <Icon name="i-x" size=11 />
                                "The second price is empty, so nothing can be settled yet."
                            </span>
                        }.into_any()
                    } else if matched() {
                        view! {
                            <span class="hint">
                                <Icon
                                    name="i-check"
                                    size=11
                                    style="display:inline;vertical-align:-1px;color:var(--bid)"
                                />
                                " The two prices match. They are typed independently and never \
                                 show each other, because a slipped digit does not survive being \
                                 typed twice."
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <span class="err">
                                <Icon name="i-x" size=11 />
                                "The two prices are different."
                            </span>
                        }.into_any()
                    }
                }}
            </div>
        </Modal>
    }
}

/// Renders the magnitude of a price in words.
///
/// Not a full number-to-words conversion, and deliberately not: the point is the
/// **order of magnitude**, because that is what a dropped or doubled digit
/// changes, and a long sentence would be skipped. Two terms at most —
/// `3 thousand 200`, `32 thousand`, `2 billion 500 million` — so that the two
/// prices a slip could produce never read the same.
fn magnitude_in_words(price: Decimal) -> String {
    /// One scale and its name, largest first.
    const SCALES: [(i64, &str); 3] = [
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];

    fn render(whole: Decimal, terms: usize) -> String {
        if terms == 0 || whole.is_zero() {
            return String::new();
        }
        for (scale, name) in SCALES {
            let scale = Decimal::from(scale);
            if whole >= scale {
                let units = (whole / scale).trunc();
                let remainder = whole - units * scale;
                let tail = render(remainder, terms - 1);
                let head = format!("{} {name}", format::fixed(units, 0));
                return if tail.is_empty() {
                    head
                } else {
                    format!("{head} {tail}")
                };
            }
        }
        format::fixed(whole, 0)
    }

    let whole = price.trunc().abs();
    if whole.is_zero() {
        return "0".to_string();
    }
    render(whole, 2)
}

/// Risk parameters, with the count of accounts the change puts under water.
#[component]
fn RiskDialog(
    /// The market.
    market: AdminMarket,
    /// The new maximum leverage, as typed.
    leverage: RwSignal<String>,
    /// The new initial margin in basis points.
    initial: RwSignal<String>,
    /// The new maintenance margin in basis points.
    maintenance: RwSignal<String>,
    /// Close without applying.
    on_close: Callback<()>,
    /// Called with the symbol and what the gateway answered.
    on_applied: Callback<(String, RiskApplied)>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let symbol = market.symbol.clone();

    // The baseline comes from the public `/markets`, which is the endpoint that
    // actually carries the risk parameters. `/admin/markets` does not, and
    // defaulting its absent fields to 1×/0/0 printed a false "Now" column and
    // then pre-filled "New" from it — an operator who edited only the leverage
    // would have abolished the margin requirement believing they had touched
    // nothing.
    let public = stores.market_named(&symbol);
    let was_leverage = market
        .max_leverage
        .or_else(|| public.as_ref().and_then(|market| market.max_leverage));
    let was_initial = market
        .initial_margin_bps
        .or_else(|| public.as_ref().and_then(|market| market.initial_margin_bps));
    let was_maintenance = market.maintenance_margin_bps.or_else(|| {
        public
            .as_ref()
            .and_then(|market| market.maintenance_margin_bps)
    });
    let for_foot = StoredValue::new(symbol.clone());
    let for_preview = StoredValue::new(symbol.clone());

    let sending = RwSignal::new(false);
    let failure = RwSignal::new(None::<(String, String)>);
    let preview = RwSignal::new(None::<RiskApplied>);
    let previewing = RwSignal::new(false);

    // What the three fields would send. A field left alone sends what is there
    // now; a field whose current value is unknown sends nothing until it is
    // typed, which is why `submission` is an `Option`.
    let submission = move || {
        let leverage = parse_typed(&leverage.get()).or(was_leverage)?;
        let initial = parse_bps(&initial.get()).or(was_initial)?;
        let maintenance = parse_bps(&maintenance.get()).or(was_maintenance)?;
        Some((leverage, initial, maintenance))
    };

    // The dry run, re-asked whenever the numbers change. The contract gained it
    // after this dialog was written; the note that said it did not exist is
    // gone, and the count is on screen before the button rather than after it.
    Effect::new(move |_| {
        let Some((leverage, initial, maintenance)) = submission() else {
            preview.set(None);
            return;
        };
        let backend = backend.get_value();
        let symbol = for_preview.get_value();
        previewing.set(true);
        leptos::task::spawn_local(async move {
            let result = backend
                .api
                .admin_set_risk(&symbol, leverage, initial, maintenance, true)
                .await;
            previewing.set(false);
            match result {
                Ok(answer) => preview.set(Some(answer)),
                // A dry run that cannot be reached is not an error worth a
                // banner: the Apply button reports for itself. The preview says
                // it could not be computed instead of pretending it was zero.
                Err(_) => preview.set(None),
            }
        });
    });

    view! {
        <Modal
            title=format!("Risk parameters \u{00B7} {symbol}")
            icon="i-warn"
            icon_colour="var(--brass)"
            foot=move || {
                let symbol = for_foot.get_value();
                view! {
                    <>
                        <button class="btn btn-ghost" on:click=move |_| on_close.run(())>
                            "Cancel"
                        </button>
                        <button
                            class="btn btn-primary"
                            disabled=move || sending.get() || submission().is_none()
                            data-loading=move || sending.get().then_some("")
                            on:click=move |_| {
                                let Some((lev, im, mm)) = submission() else { return };
                                let backend = backend.get_value();
                                let symbol = symbol.clone();
                                sending.set(true);
                                failure.set(None);
                                leptos::task::spawn_local(async move {
                                    let result = backend
                                        .api
                                        .admin_set_risk(&symbol, lev, im, mm, false)
                                        .await;
                                    sending.set(false);
                                    match result {
                                        Ok(applied) => on_applied.run((symbol, applied)),
                                        // In the dialog, not in a toast behind
                                        // the scrim, and the dialog stays open:
                                        // an operator has to be able to see that
                                        // a change they made did not happen.
                                        Err(error) => {
                                            failure.set(Some(crate::failure_text(&error)));
                                            let _ = &stores;
                                        }
                                    }
                                });
                            }
                        >{format!("Apply to {}", for_foot.get_value())}</button>
                    </>
                }
                .into_any()
            }
        >
            <p style="font-size:var(--fs-sm)">
                "Margin requirements apply to positions that are already open, the moment they \
                 are saved. Raising them can put accounts below maintenance at once, and the \
                 liquidation engine acts on the next block."
            </p>
            <div style="margin:var(--sp-16) 0">
                <div class="paramrow" style="border-bottom-color:var(--border-default)">
                    <span class="nm t-meta">"Parameter"</span>
                    <span class="was t-meta">"Now"</span>
                    <span class="arr"></span>
                    <span class="t-meta">"New"</span>
                </div>
                <div class="paramrow">
                    <span class="nm">"Maximum leverage"</span>
                    <span class="was">
                        {was_leverage.map_or_else(
                            || "\u{2014}".to_string(),
                            |value| format!("{}\u{00D7}", format::fixed(value, 0)))}
                    </span>
                    <span class="arr">"\u{2192}"</span>
                    <div class="numfield">
                        <input
                            inputmode="numeric"
                            aria-label="Maximum leverage"
                            placeholder="\u{2014}"
                            prop:value=move || {
                                let typed = leverage.get();
                                if typed.is_empty() {
                                    was_leverage.map_or_else(
                                        String::new, |value| format::fixed(value, 0))
                                } else { typed }
                            }
                            on:input=move |event| leverage.set(input_value(&event))
                        />
                    </div>
                </div>
                <div class="paramrow">
                    <span class="nm">"Initial margin"</span>
                    <span class="was">
                        {was_initial.map_or_else(
                            || "\u{2014}".to_string(), |value| format!("{value} bps"))}
                    </span>
                    <span class="arr">"\u{2192}"</span>
                    <div class="numfield">
                        <input
                            inputmode="numeric"
                            aria-label="Initial margin in basis points"
                            placeholder="\u{2014}"
                            prop:value=move || {
                                let typed = initial.get();
                                if typed.is_empty() {
                                    was_initial.map_or_else(String::new, |v| v.to_string())
                                } else { typed }
                            }
                            on:input=move |event| initial.set(input_value(&event))
                        />
                    </div>
                </div>
                <div class="paramrow">
                    <span class="nm">"Maintenance margin"</span>
                    <span class="was">
                        {was_maintenance.map_or_else(
                            || "\u{2014}".to_string(), |value| format!("{value} bps"))}
                    </span>
                    <span class="arr">"\u{2192}"</span>
                    <div class="numfield">
                        <input
                            inputmode="numeric"
                            aria-label="Maintenance margin in basis points"
                            placeholder="\u{2014}"
                            prop:value=move || {
                                let typed = maintenance.get();
                                if typed.is_empty() {
                                    was_maintenance.map_or_else(String::new, |v| v.to_string())
                                } else { typed }
                            }
                            on:input=move |event| maintenance.set(input_value(&event))
                        />
                    </div>
                </div>
            </div>
            <Show when=move || was_leverage.is_none() || was_initial.is_none()
                    || was_maintenance.is_none()>
                <Banner
                    tone="warn"
                    title="This market's current figures are not published"
                    body="Neither /admin/markets nor /markets carries them for this contract, so \
                          there is nothing to compare against and nothing has been filled in for \
                          you. Type every field you intend to set."
                />
            </Show>
            // The consequence, before the button that causes it.
            <div class="banner info" role="status" style="margin-top:var(--sp-12)">
                <svg class="ico" width="14" height="14" viewBox="0 0 14 14"
                     style="color:var(--signal)" aria-hidden="true">
                    <use href="#i-info"></use>
                </svg>
                <div>
                    <strong>"What this change would do"</strong>
                    {move || {
                        if previewing.get() {
                            return "Asking the gateway\u{2026}".to_string();
                        }
                        let Some(answer) = preview.get() else {
                            return "The gateway could not be asked for a preview, so the count \
                                    will only be known once this is applied."
                                .to_string();
                        };
                        match answer.accounts_now_below_maintenance {
                            0 => "No account is left below maintenance margin by these figures."
                                .to_string(),
                            1 => "One account is left below maintenance margin, and the \
                                  liquidation engine acts on the next block."
                                .to_string(),
                            count => format!(
                                "{count} accounts are left below maintenance margin, and the \
                                 liquidation engine acts on the next block.",
                            ),
                        }
                    }}
                    {move || {
                        let handles: Vec<String> = preview
                            .get()
                            .map(|answer| answer
                                .affected
                                .iter()
                                .take(8)
                                .map(|account| format!("@{}", account.handle))
                                .collect())
                            .unwrap_or_default();
                        (!handles.is_empty()).then(|| view! {
                            <div class="t-mono" style="margin-top:var(--sp-6)">
                                {handles.join(" \u{00B7} ")}
                            </div>
                        })
                    }}
                </div>
            </div>
            <crate::ui::MutationFailure failure=failure />
        </Modal>
    }
}

/// Parses a basis-point field, or `None` when it does not say a number.
///
/// Deliberately not "fall back to what it was": the whole defect in this dialog
/// was a fallback that supplied a figure the operator had never seen.
fn parse_bps(typed: &str) -> Option<u32> {
    typed.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::magnitude_in_words;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap_or_default()
    }

    #[test]
    fn a_dropped_digit_changes_the_words() {
        assert_eq!(magnitude_in_words(d("3200.00")), "3 thousand 200");
        assert_eq!(magnitude_in_words(d("32000.00")), "32 thousand");
        assert_ne!(
            magnitude_in_words(d("3200.00")),
            magnitude_in_words(d("32000.00")),
        );
    }

    #[test]
    fn every_magnitude_has_a_name() {
        assert_eq!(magnitude_in_words(d("412.00")), "412");
        assert_eq!(magnitude_in_words(d("1000000")), "1 million");
        assert_eq!(magnitude_in_words(d("2500000000")), "2 billion 500 million");
        assert_eq!(magnitude_in_words(Decimal::ZERO), "0");
    }
}
