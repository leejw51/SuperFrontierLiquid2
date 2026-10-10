//! The leaderboard.
//!
//! Two rules do most of the work here.
//!
//! **Bots are marked, always.** They are on the board because a lone human needs
//! a counterparty, and pretending the counterparty is human would make the
//! ranking a lie. The badge names the strategy, in signal blue, exactly as the
//! admin screen does.
//!
//! **Rank comes from the API and is never recomputed.** Hiding bots filters
//! rows; it does not renumber them, so rank 3 stays rank 3 and the gap tells you
//! how much of the board is machine.

use std::sync::Arc;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::LeaderboardEntry;
use crate::api::LeaderboardWindow;
use crate::config::Backend;
use crate::format;
use crate::state::use_stores;
use crate::ui::AppShell;

/// The leaderboard screen.
#[component]
pub fn Leaderboard() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let window = RwSignal::new(LeaderboardWindow::Day);
    let hide_bots = RwSignal::new(false);
    let entries = RwSignal::new(Vec::<LeaderboardEntry>::new());
    let loading = RwSignal::new(true);

    Effect::new(move |_| {
        let backend = backend.get_value();
        let chosen = window.get();
        loading.set(true);
        leptos::task::spawn_local(async move {
            if let Ok(response) = backend.api.leaderboard(chosen, 100).await {
                entries.set(response.entries);
            }
            loading.set(false);
        });
    });

    let me = move || {
        stores
            .session
            .read()
            .as_ref()
            .map(|session| session.handle.clone())
    };
    let visible = move || {
        entries
            .get()
            .into_iter()
            .filter(|entry| !(hide_bots.get() && entry.is_bot))
            .collect::<Vec<_>>()
    };
    // The bar is a share of the window, scaled against the biggest move in
    // either direction so that gains and losses are directly comparable.
    let widest = move || {
        entries
            .read()
            .iter()
            .map(|entry| entry.pnl.abs())
            .max()
            .unwrap_or(Decimal::ONE)
            .max(Decimal::ONE)
    };

    view! {
        <AppShell>
            <div style="display:flex;align-items:flex-end;justify-content:space-between;gap:var(--sp-16);margin-bottom:var(--sp-16);flex-wrap:wrap">
                <div>
                    <h1 class="t-h2" style="margin-bottom:var(--sp-4)">"Leaderboard"</h1>
                    <p class="t-body" style="font-size:var(--fs-sm)">
                        "Ranked by profit and loss over the window. Bots are market makers and \
                         scripted traders that keep the books alive; they are marked and they \
                         are ranked, because hiding them would make the table a lie."
                    </p>
                </div>
                <div style="display:flex;gap:var(--sp-12);align-items:center;flex:0 0 auto">
                    <label class="switch">
                        <input
                            type="checkbox"
                            prop:checked=move || hide_bots.get()
                            on:change=move |_| hide_bots.update(|on| *on = !*on)
                        />
                        <span class="track"></span>
                        "Hide bots"
                    </label>
                    <div class="seg" role="group" aria-label="Window">
                        {LeaderboardWindow::ALL
                            .into_iter()
                            .map(|option| view! {
                                <button
                                    aria-pressed=move || (window.get() == option).to_string()
                                    on:click=move |_| window.set(option)
                                >
                                    {option.label()}
                                </button>
                            })
                            .collect_view()}
                    </div>
                </div>
            </div>

            <section class="panel spine-account">
                <div class="tbl-wrap">
                    <Show
                        when=move || !loading.get()
                        fallback=|| view! { <crate::ui::SkeletonRows rows=10 /> }
                    >
                        <table class="tbl lb">
                            <thead>
                                <tr>
                                    <th class="n" style="width:56px">"Rank"</th>
                                    <th>"Trader"</th>
                                    <th>"Address"</th>
                                    <th class="n">"Equity"</th>
                                    <th class="n">"Profit and loss"</th>
                                    <th class="n">"Return"</th>
                                    <th class="n">"Volume"</th>
                                    <th class="n">"Trades"</th>
                                    <th style="width:140px">"Share of window"</th>
                                </tr>
                            </thead>
                            <tbody>
                                // Keyed on the figures as well as the rank: a
                                // window switch keeps the ranks and changes
                                // everything else.
                                <For
                                    each=visible
                                    key=|entry| format!("{}:{}:{}", entry.rank, entry.handle, entry.pnl)
                                    let:entry
                                >
                                    {
                                        let is_me = me().is_some_and(|handle| handle == entry.handle);
                                        let up = entry.pnl >= Decimal::ZERO;
                                        let width = ((entry.pnl.abs() / widest())
                                            * Decimal::from(100))
                                            .round()
                                            .try_into()
                                            .unwrap_or(0_u32);
                                        view! {
                                            <tr class:me=is_me>
                                                <td class="n rk">
                                                    {if entry.rank == 1 {
                                                        view! { <span class="rank1">"1"</span> }
                                                            .into_any()
                                                    } else {
                                                        view! { {entry.rank.to_string()} }.into_any()
                                                    }}
                                                </td>
                                                <td class="strong">
                                                    <span class="who">
                                                        {entry.handle.clone()}
                                                        {is_me.then(|| view! {
                                                            <span class="badge warn">"You"</span>
                                                        })}
                                                        {entry.is_bot.then(|| view! {
                                                            <span class="badge bot">"Bot"</span>
                                                        })}
                                                        {entry.bot_strategy.clone().map(|strategy| view! {
                                                            <span
                                                                class="dim"
                                                                style="font-size:var(--fs-micro)"
                                                            >{strategy}</span>
                                                        })}
                                                    </span>
                                                </td>
                                                <td class="n dim">{format::hash(&entry.address)}</td>
                                                <td class="n strong">
                                                    {format::fixed(entry.equity, 2)}
                                                </td>
                                                <td
                                                    class="n"
                                                    style={if up {
                                                        "color:var(--bid)"
                                                    } else {
                                                        "color:var(--ask)"
                                                    }}
                                                >{format::pnl(entry.pnl)}</td>
                                                <td
                                                    class="n"
                                                    style={if up {
                                                        "color:var(--bid)"
                                                    } else {
                                                        "color:var(--ask)"
                                                    }}
                                                >{format::pct(entry.pnl_pct)}</td>
                                                <td class="n">{format::fixed(entry.volume, 2)}</td>
                                                <td class="n">{format::count(entry.trades)}</td>
                                                <td>
                                                    // Gains grow from the left and losses from
                                                    // the right, so the sign is carried by
                                                    // anchor as well as by colour.
                                                    <span
                                                        class="lbbar"
                                                        class:neg=!up
                                                        style=format!("--w:{width}")
                                                    ></span>
                                                </td>
                                            </tr>
                                        }
                                    }
                                </For>
                            </tbody>
                        </table>
                    </Show>
                </div>
            </section>
            <p class="t-meta" style="margin-top:var(--sp-12);max-width:var(--measure);line-height:var(--lh-relaxed)">
                "Rank comes from the API and is never recomputed on the client. Hiding bots \
                 filters rows; it does not renumber them, so rank 3 stays rank 3 and the gap \
                 tells you how much of the board is machine."
            </p>
        </AppShell>
    }
}
