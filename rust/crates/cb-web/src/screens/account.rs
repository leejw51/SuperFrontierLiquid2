//! The user's own account page.
//!
//! The portfolio next door is about open risk right now; this is the record.
//!
//! # The two addresses
//!
//! The one piece of real design on this page. A person who opens it and sees an
//! unfamiliar address holding their balance will conclude they have been robbed,
//! and they will conclude it in about a second, so the reassurance has to arrive
//! in the same glance as the problem. Two cards with a labelled arrow between
//! them, each leading with **who holds the key** rather than with the hex; the
//! note's first clause is *your balance sits at the trading address, and nothing
//! has been moved*, and the explanation of the derivation comes second.
//!
//! Neither address is truncated here. Everywhere else in the product an address
//! is `0x8f3b…c21d`; on this one screen both are shown in full, because this is
//! the page whose job is to let somebody compare an address against what their
//! wallet shows.

use std::sync::Arc;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::screens::landing::{copy_to_clipboard, input_value};
use crate::state::{use_stores, Load, Tone};
use crate::ticket::handle_problem;
use crate::ui::{AppShell, Banner, EmptyState, Icon, MenuPanel, Modal, RpcPanel, TableState};

/// How many activity entries to load at a time.
const PAGE: u32 = 60;

/// The account screen.
#[component]
pub fn AccountPage() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let navigate = crate::nav::navigator();

    let profile = RwSignal::new(None::<Profile>);
    let sessions = RwSignal::new(Vec::<UserSession>::new());
    let activity = RwSignal::new(Vec::<ActivityEntry>::new());
    let cursor = RwSignal::new(None::<String>);
    let filter = RwSignal::new(None::<ActivityKind>);
    let editing = RwSignal::new(false);
    // These two regions rendered a column header row over blank space whether
    // the request was in flight, had failed, or had genuinely returned nothing.
    let sessions_load = RwSignal::new(Load::Loading);
    let activity_load = RwSignal::new(Load::Loading);

    let reload = move || {
        let backend = backend.get_value();
        sessions_load.set(Load::Loading);
        activity_load.set(Load::Loading);
        leptos::task::spawn_local(async move {
            if let Ok(found) = backend.api.profile().await {
                profile.set(Some(found));
            }
            let response = backend.api.sessions().await;
            sessions_load.set(Load::from_result(&response));
            if let Ok(response) = response {
                sessions.set(response.sessions);
            }
            let response = backend.api.activity(PAGE, None).await;
            activity_load.set(Load::from_result(&response));
            if let Ok(response) = response {
                activity.set(response.entries);
                cursor.set(response.next_cursor);
            }
        });
    };

    Effect::new(move |_| {
        if stores.session.read().is_some() {
            reload();
        }
    });

    let load_more = move |_| {
        let backend = backend.get_value();
        let next = cursor.get_untracked();
        leptos::task::spawn_local(async move {
            if let Ok(response) = backend.api.activity(PAGE, next.as_deref()).await {
                activity.update(|held| held.extend(response.entries));
                cursor.set(response.next_cursor);
            }
        });
    };

    let visible = move || {
        activity
            .get()
            .into_iter()
            .filter(|entry| filter.get().is_none_or(|want| entry.kind == want))
            .collect::<Vec<_>>()
    };
    let others = move || sessions.read().iter().filter(|s| !s.current).count();

    view! {
        <AppShell>
            <Show
                when=move || stores.session.read().is_some()
                fallback=|| view! {
                    <EmptyState
                        title=t("Nobody is signed in")
                        body="This page is your own record: your handle, your two addresses, your \
                              sessions and everything you have done."
                    >
                        <a class="btn btn-primary" href="/">{t("Sign in")}</a>
                    </EmptyState>
                }
            >
                <section class="panel spine-account">
                    <div class="panel-head">
                        <h3>{t("Profile")}</h3>
                        <span class="spacer"></span>
                        <span class="t-meta">
                            {t("Only the handle can be changed; the address is the identity")}
                        </span>
                    </div>
                    <div class="acct-head">
                        <span class="avatar">
                            {move || profile.read().as_ref()
                                .and_then(|profile| profile.handle.chars().next())
                                .map_or_else(String::new, |initial| initial.to_string())}
                        </span>
                        <span class="acct-id">
                            <span class="h">
                                {move || profile.read().as_ref().map_or_else(
                                    || "\u{2014}".to_string(),
                                    |profile| format!("@{}", profile.handle))}
                            </span>
                            <span class="m">
                                {move || profile.read().as_ref().map_or_else(
                                    String::new,
                                    |profile| format!(
                                        "Joined {} \u{00B7} last seen {} \u{00B7} {}",
                                        format::parse_rfc3339(&profile.joined_at)
                                            .map_or_else(
                                                || profile.joined_at.clone(), format::calendar),
                                        format::parse_rfc3339(&profile.last_seen_at).map_or_else(
                                            || profile.last_seen_at.clone(),
                                            |at| format::age(stores.now_ms.get() - at * 1_000)),
                                        profile.user_id,
                                    ))}
                            </span>
                        </span>
                        {move || profile.read().as_ref().map(|profile| {
                            profile.roles.iter().map(|role| view! {
                                <span class=if role == "admin" { "badge warn" } else { "badge" }>
                                    {if role == "admin" { t("Operator") } else { t("User") }}
                                </span>
                            }).collect_view()
                        })}
                        <span style="margin-left:auto;display:flex;gap:var(--sp-8)">
                            <button
                                class="btn btn-secondary btn-sm"
                                on:click=move |_| editing.set(true)
                            >{t("Edit handle")}</button>
                            <button
                                class="btn btn-ghost btn-sm"
                                on:click=move |_| {
                                    let backend = backend.get_value();
                                    leptos::task::spawn_local(async move {
                                        let _ = backend.api.logout().await;
                                        crate::sign_out(&backend, stores);
                                        navigate("/");
                                    });
                                }
                            >{t("Sign out")}</button>
                        </span>
                    </div>
                </section>

                <MenuPanel />

                <TwoAddresses />

                <RpcPanel />

                <section class="panel spine-account" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>{t("Statistics")}</h3>
                        <span class="spacer"></span>
                        <span class="t-meta">
                            "Volume and realised profit are 30-day figures; the rest are lifetime"
                        </span>
                    </div>
                    <div class="statrow">
                        {move || profile.read().as_ref().map(|profile| {
                            let stats = &profile.stats;
                            let realised = stats.realized_pnl_30d;
                            view! {
                                <>
                                    <span class="stat">
                                        <span class="k">"Trades"</span>
                                        <span class="v">{format::count(stats.trades)}</span>
                                    </span>
                                    <span class="stat">
                                        <span class="k">"Volume \u{00B7} 30 days"</span>
                                        <span class="v">{format::fixed(stats.volume_30d, 2)}</span>
                                    </span>
                                    <span class="stat">
                                        <span class="k">"Realised \u{00B7} 30 days"</span>
                                        <span
                                            class="v"
                                            // A jade profit of nothing is not a
                                            // profit. No hue on a zero.
                                            style={if realised.is_zero() {
                                                ""
                                            } else if realised > Decimal::ZERO {
                                                "color:var(--bid)"
                                            } else {
                                                "color:var(--ask)"
                                            }}
                                        >{format::pnl(realised)}</span>
                                    </span>
                                    <span class="stat">
                                        <span class="k">"Markets traded"</span>
                                        <span class="v">{stats.markets_traded.to_string()}</span>
                                    </span>
                                    // A count of liquidations takes no sign and
                                    // no colour: a count is not a direction.
                                    <span class="stat">
                                        <span class="k">"Liquidations"</span>
                                        <span class="v">{stats.liquidations.to_string()}</span>
                                    </span>
                                    <span class="stat">
                                        <span class="k">"Faucet claims"</span>
                                        <span class="v">{stats.faucet_claims.to_string()}</span>
                                    </span>
                                    <span class="stat">
                                        <span class="k">"Best rank"</span>
                                        <span class="v">
                                            {stats.best_rank.map_or_else(
                                                || "\u{2014}".to_string(), |rank| rank.to_string())}
                                        </span>
                                    </span>
                                </>
                            }
                        })}
                    </div>
                </section>

                <section class="panel spine-account" style="margin-top:var(--sp-16)">
                    <div class="panel-head">
                        <h3>{t("Active sessions")}</h3>
                        <span class="badge">{move || sessions.read().len().to_string()}</span>
                        <span class="spacer"></span>
                        // Hidden rather than disabled when there is nothing to
                        // sign out: a destructive-looking button offered for an
                        // action with no subject is noise, and "Sign out 0 other
                        // sessions" is not a sentence anybody needs.
                        <Show when=move || others() != 0>
                        <button
                            class="btn btn-danger btn-sm"
                            on:click=move |_| {
                                let backend = backend.get_value();
                                leptos::task::spawn_local(async move {
                                    match backend.api.revoke_other_sessions().await {
                                        Ok(result) => {
                                            stores.notify(
                                                Tone::Good,
                                                "Other sessions signed out",
                                                format!(
                                                    "{} token{} will get a 401 on the next \
                                                     request.",
                                                    result.revoked,
                                                    if result.revoked == 1 { "" } else { "s" },
                                                ),
                                            );
                                            if let Ok(response) = backend.api.sessions().await {
                                                sessions.set(response.sessions);
                                            }
                                        }
                                        Err(error) => crate::report(&backend, stores, &error),
                                    }
                                });
                            }
                        >
                            // The bulk action leaves the session you are reading
                            // this in alone, which is why it counts the others.
                            {move || format!(
                                "Sign out {} other session{}",
                                others(),
                                if others() == 1 { "" } else { "s" },
                            )}
                        </button>
                        </Show>
                    </div>
                    <table class="tbl">
                        <thead>
                            <tr>
                                <th>"Client"</th><th>"Network"</th><th>"Signed in"</th>
                                <th>"Last used"</th><th>"Expires"</th><th style="width:96px"></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For
                                each=move || sessions.get()
                                key=|session| session.session_id.clone()
                                let:session
                            >
                                {
                                    let id = session.session_id.clone();
                                    let current = session.current;
                                    let when = |value: &str| format::parse_rfc3339(value)
                                        .map_or_else(|| value.to_string(), format::calendar);
                                    view! {
                                        <tr class:sess-cur=current>
                                            <td class="strong">{session.user_agent.clone()}</td>
                                            <td class="num">{session.ip_prefix.clone()}</td>
                                            <td class="num">{when(&session.issued_at)}</td>
                                            <td>{when(&session.last_used_at)}</td>
                                            <td class="num">{when(&session.expires_at)}</td>
                                            <td>
                                                {if current {
                                                    view! {
                                                        <span class="badge">"This session"</span>
                                                    }.into_any()
                                                } else {
                                                    view! {
                                                        <span class="rowact">
                                                            <button
                                                                class="btn btn-ghost btn-sm"
                                                                on:click=move |_| {
                                                                    let backend = backend.get_value();
                                                                    let id = id.clone();
                                                                    leptos::task::spawn_local(async move {
                                                                        let _ = backend.api
                                                                            .revoke_session(&id).await;
                                                                        if let Ok(response) =
                                                                            backend.api.sessions().await
                                                                        {
                                                                            sessions.set(response.sessions);
                                                                        }
                                                                    });
                                                                }
                                                            >{t("Sign out")}</button>
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
                    <TableState
                        load=Signal::derive(move || sessions_load.get())
                        empty=Signal::derive(move || sessions.read().is_empty())
                        title="No sessions listed"
                        body="The session you are reading this in should be here. If it is not, \
                              the gateway did not return it."
                    />
                    <div class="addr-note">
                        <Icon name="i-info" size=14 style="color:var(--text-muted);flex:0 0 auto;margin-top:2px" />
                        <span>
                            "Signing out revokes that token at once; anything using it gets a "
                            <code>"401"</code>" on its next request. The bulk action leaves the \
                             session you are reading this in alone. Networks are shown as a "
                            <span class="num">"/24"</span>" prefix \u{2014} a session list is a \
                             security feature and does not need to be a location log."
                        </span>
                    </div>
                </section>

                <section class="panel spine-account" style="margin-top:var(--sp-16)">
                    <div class="tabbar" role="tablist">
                        <ActivityTab filter=filter this=None label="All activity" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Fill) label="Fills" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Order) label="Orders" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Funding) label="Funding" />
                        <ActivityTab filter=filter this=Some(ActivityKind::TransferOut) label="Transfers" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Faucet) label="Faucet" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Liquidation) label="Liquidations" />
                        <ActivityTab filter=filter this=Some(ActivityKind::Settlement) label="Settlements" />
                    </div>
                    <table class="tbl">
                        <thead>
                            <tr>
                                <th style="width:132px">"Kind"</th>
                                <th>"What happened"</th>
                                <th class="n" style="width:130px">"Amount"</th>
                                <th class="n" style="width:82px">"Block"</th>
                                <th style="width:116px">"Transaction"</th>
                                <th class="n" style="width:112px">"Time"</th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=visible key=|entry| entry.id.clone() let:entry>
                                {
                                    let at = format::parse_rfc3339(&entry.at)
                                        .map_or_else(|| entry.at.clone(), format::clock);
                                    let amount = entry.amount;
                                    view! {
                                        <tr>
                                            <td>
                                                <span class="act-kind">
                                                    <span class=kind_glyph_class(entry.kind)>
                                                        {kind_glyph(entry.kind)}
                                                    </span>
                                                    {entry.kind.label()}
                                                </span>
                                            </td>
                                            // The gateway already wrote this
                                            // sentence, so the client does not
                                            // compose prose the server supplied.
                                            <td>{entry.summary.clone()}</td>
                                            <td
                                                class="n"
                                                class:dim=amount.is_none()
                                                style=match amount {
                                                    Some(amount) if amount > Decimal::ZERO =>
                                                        "color:var(--bid)",
                                                    Some(amount) if amount < Decimal::ZERO =>
                                                        "color:var(--ask)",
                                                    _ => "",
                                                }
                                            >
                                                // An order entry has no amount and
                                                // shows an em dash, never a zero.
                                                {amount.map_or_else(
                                                    || "\u{2014}".to_string(), format::pnl)}
                                            </td>
                                            <td class="n">
                                                {if entry.height > 0 {
                                                    format::count(entry.height)
                                                } else {
                                                    "\u{2014}".to_string()
                                                }}
                                            </td>
                                            <td class="n dim">
                                                {entry.tx_hash.as_deref()
                                                    .map_or_else(String::new, format::hash)}
                                            </td>
                                            <td class="n">{at}</td>
                                        </tr>
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                    <TableState
                        load=Signal::derive(move || activity_load.get())
                        empty=Signal::derive(move || visible().is_empty())
                        title="Nothing here yet"
                        body="Every order, fill, funding payment, faucet grant and liquidation on \
                              this account lands here with the block that carried it. Claim from \
                              the faucet and place an order, and the first two lines appear."
                    />
                    <div style="display:flex;align-items:center;gap:var(--sp-12);padding:var(--sp-12) var(--sp-16);border-top:var(--bw-hair) solid var(--border-subtle)">
                        <button
                            class="btn btn-secondary btn-sm"
                            disabled=move || cursor.read().is_none()
                            on:click=load_more
                        >"Load older activity"</button>
                        <span class="t-meta">
                            "Paged by "<code>"next_cursor"</code>", not by page number, so an \
                             entry that lands while you are reading cannot push a row onto the \
                             page you already passed."
                        </span>
                    </div>
                </section>
            </Show>
        </AppShell>

        <Show when=move || editing.get()>
            <EditHandle
                editing=editing
                current=Signal::derive(move || profile.read().as_ref()
                    .map_or_else(String::new, |profile| profile.handle.clone()))
                on_saved=Callback::new(move |updated: Profile| {
                    profile.set(Some(updated));
                    editing.set(false);
                })
            />
        </Show>
    }
}

/// One tab of the activity feed.
#[component]
fn ActivityTab(
    /// The active filter.
    filter: RwSignal<Option<ActivityKind>>,
    /// The filter this tab selects.
    this: Option<ActivityKind>,
    /// The label.
    label: &'static str,
) -> impl IntoView {
    view! {
        <button
            class="tab"
            role="tab"
            aria-selected=move || (filter.get() == this).to_string()
            on:click=move |_| filter.set(this)
        >
            {label}
        </button>
    }
}

/// The glyph a kind wears. A shape as well as a word, never a colour alone.
const fn kind_glyph(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Fill => "F",
        ActivityKind::Order => "O",
        ActivityKind::Faucet => "+",
        ActivityKind::Funding => "%",
        ActivityKind::TransferIn => "\u{2192}",
        ActivityKind::TransferOut => "\u{2190}",
        ActivityKind::Liquidation => "!",
        ActivityKind::Settlement => "=",
        ActivityKind::Other => "\u{00B7}",
    }
}

/// The glyph's class, which carries its tint.
const fn kind_glyph_class(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Liquidation => "g liq",
        ActivityKind::Faucet => "g faucet",
        ActivityKind::Settlement => "g chainy",
        _ => "g",
    }
}

/// The two addresses, and the sentence that has to land first.
#[component]
fn TwoAddresses() -> impl IntoView {
    let stores = use_stores();
    let login = move || {
        stores
            .session
            .read()
            .as_ref()
            .map_or_else(String::new, |session| session.login_address.clone())
    };
    let trading = move || {
        stores
            .session
            .read()
            .as_ref()
            .map_or_else(String::new, |session| session.trading_address.clone())
    };

    view! {
        <section class="panel spine-account" style="margin-top:var(--sp-16)">
            <div class="panel-head">
                <h3>{t("Your two addresses")}</h3>
                <span class="spacer"></span>
                <span class="badge">{t("Both of them are yours")}</span>
            </div>
            <div class="addr-pair" style="padding:var(--sp-16)">
                <div class="addr-card you">
                    <span class="lbl"><Icon name="i-key" size=12 />{t("Login address")}</span>
                    <span class="who">{t("You hold the key")}</span>
                    <span class="a">
                        // Shown in full. This is the one screen whose job is to
                        // let somebody compare an address against their wallet.
                        <code>{login}</code>
                        <button
                            class="btn btn-ghost btn-sm"
                            aria-label="Copy the login address"
                            on:click=move |_| {
                                copy_to_clipboard(&login());
                                stores.notify(Tone::Good, "Login address copied", login());
                            }
                        ><Icon name="i-copy" size=12 /></button>
                    </span>
                    <p>
                        {t("Derived from your twelve words. This is the address you signed in with. The gateway has never seen your phrase and cannot sign anything as you.")}
                    </p>
                </div>
                <div class="addr-link">
                    <svg width="30" height="15" viewBox="0 0 24 12" style="color:var(--text-muted)" aria-hidden="true">
                        <use href="#i-arrow"></use>
                    </svg>
                    <span class="t">{t("computed from")}<br />{t("the one on the left")}</span>
                </div>
                <div class="addr-card gw">
                    <span class="lbl"><Icon name="i-vault" size=12 />{t("Trading address")}</span>
                    <span class="who">{t("The gateway holds the key")}</span>
                    <span class="a">
                        <code>{trading}</code>
                        <button
                            class="btn btn-ghost btn-sm"
                            aria-label="Copy the trading address"
                            on:click=move |_| {
                                copy_to_clipboard(&trading());
                                stores.notify(Tone::Good, "Trading address copied", trading());
                            }
                        ><Icon name="i-copy" size=12 /></button>
                    </span>
                    <p>
                        {t("The on-chain account holding your test money. The gateway signs your orders with it, which is why placing a trade is one click and not a wallet prompt.")}
                    </p>
                </div>
            </div>
            <div class="addr-note">
                <Icon name="i-info" size=14 style="color:var(--text-muted);flex:0 0 auto;margin-top:2px" />
                <span>
                    <b>"Your balance sits at the trading address, and nothing has been moved."</b>
                    " The gateway computes that address from your login address with a one-way \
                     function, so one login address always maps to this same trading account and \
                     to no other. Sign in with the same twelve words on another machine and you \
                     arrive back here. It holds test money the faucet mints; nothing in this \
                     product settles into a real asset. A production exchange would have the \
                     chain accept orders signed by your own key instead, and that trade-off is \
                     written up in "<code>"docs/api-contract.md"</code>" \u{00A7}2 rather than \
                     pretended away."
                </span>
            </div>
        </section>
    }
}

/// The handle editor. The handle is the only mutable field on the account.
#[component]
fn EditHandle(
    /// Whether the dialog is open.
    editing: RwSignal<bool>,
    /// The handle as it stands.
    #[prop(into)]
    current: Signal<String>,
    /// Called with the updated profile.
    on_saved: Callback<Profile>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let typed = RwSignal::new(current.get_untracked());
    let busy = RwSignal::new(false);
    let failure = RwSignal::new(None::<String>);
    let problem = move || handle_problem(&typed.get());

    view! {
        <Modal
            title="Edit handle"
            icon="i-info"
            width="w-mid"
            foot=move || view! {
                <>
                    <button class="btn btn-ghost" on:click=move |_| editing.set(false)>
                        "Cancel"
                    </button>
                    <button
                        class="btn btn-primary"
                        disabled=move || problem().is_some() || busy.get()
                            || typed.get() == current.get()
                        data-loading=move || busy.get().then_some("")
                        on:click=move |_| {
                            let backend = backend.get_value();
                            let wanted = typed.get_untracked();
                            busy.set(true);
                            failure.set(None);
                            leptos::task::spawn_local(async move {
                                let result = backend.api.update_handle(&wanted).await;
                                busy.set(false);
                                match result {
                                    Ok(profile) => {
                                        stores.session.update(|session| {
                                            if let Some(session) = session.as_mut() {
                                                session.handle = profile.handle.clone();
                                            }
                                        });
                                        stores.notify(
                                            Tone::Good,
                                            "Handle changed",
                                            format!("You are now @{}", profile.handle),
                                        );
                                        on_saved.run(profile);
                                    }
                                    Err(error) => failure.set(Some(error.message())),
                                }
                            });
                        }
                    >"Save"</button>
                </>
            }.into_any()
        >
            <p style="font-size:var(--fs-sm)">
                "The handle is the only thing on this account that can be changed. The address \
                 is the identity and cannot, which is also why the audit trail records an \
                 address rather than a handle."
            </p>
            <div class="field" style="max-width:320px;margin-top:var(--sp-16)">
                <label for="edit-handle">"Handle"</label>
                <div class="handlefield">
                    <span class="at">"@"</span>
                    <input
                        class="input"
                        id="edit-handle"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                        aria-invalid=move || problem().is_some().then_some("true")
                        prop:value=move || typed.get()
                        on:input=move |event| typed.set(input_value(&event).to_lowercase())
                    />
                </div>
                {move || match problem() {
                    Some(message) => view! {
                        <span class="err"><Icon name="i-warn" size=11 />{message}</span>
                    }.into_any(),
                    None => view! {
                        <span class="hint">
                            "3\u{2013}20 characters, lower case letters, digits and underscores."
                        </span>
                    }.into_any(),
                }}
            </div>
            {move || failure.get().map(|message| view! {
                <div style="margin-top:var(--sp-12)">
                    <Banner tone="danger" title="That handle was refused" body=message />
                </div>
            })}
        </Modal>
    }
}
