//! The faucet: where the test money comes from.
//!
//! Two states from one endpoint, shown side by side so the wait never hides the
//! reward. The rule that shapes the screen is that **the button never lies about
//! being available**: `available` decides whether it is enabled, the countdown
//! is drawn from `next_available_at` and nothing else, and a `429
//! FAUCET_COOLDOWN` renders as a brass banner naming the exact unlock time
//! rather than as a shrug.

use std::sync::Arc;

use leptos::prelude::*;

use crate::api::types::*;
use crate::api::ErrorCode;
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::state::{use_stores, Toast, Tone};
use crate::ui::{AppShell, Banner, Icon};

/// The faucet screen.
#[component]
pub fn Faucet() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let status = RwSignal::new(None::<FaucetStatus>);
    let history = RwSignal::new(Vec::<LedgerEntry>::new());
    let claiming = RwSignal::new(false);
    let cooldown_error = RwSignal::new(None::<String>);

    let reload = move || {
        let backend = backend.get_value();
        leptos::task::spawn_local(async move {
            if let Ok(response) = backend.api.faucet().await {
                status.set(Some(response));
            }
            if let Ok(response) = backend.api.ledger(200).await {
                history.set(
                    response
                        .entries
                        .into_iter()
                        .filter(|entry| entry.kind == LedgerKind::Faucet)
                        .collect(),
                );
            }
        });
    };

    Effect::new(move |_| {
        if stores.session.read().is_some() {
            reload();
        }
    });
    // The grant lands a block after the claim answers, so the ledger read
    // straight after the claim is one block too early. The balance update
    // that follows changes `stores.account`; that is the moment the history
    // and the cooldown are worth reading again.
    Effect::new(move |_| {
        let _ = stores.account.get();
        if stores.session.read_untracked().is_some() {
            reload();
        }
    });

    let next_at = move || {
        status
            .read()
            .as_ref()
            .and_then(|status| status.next_available_at.as_ref())
            .and_then(|at| format::parse_rfc3339(at))
    };
    let seconds_left = move || next_at().map_or(0, |at| (at - stores.now_ms.get() / 1_000).max(0));
    let available = move || {
        status
            .read()
            .as_ref()
            .is_some_and(|status| status.available)
            && seconds_left() == 0
    };
    let grant = move || {
        status
            .read()
            .as_ref()
            .map(|status| status.grant.clone())
            .unwrap_or_default()
    };
    // Nothing has been asked of the gateway until somebody signs in, so nothing
    // is known. Asserting a cooldown of zero — brass badge, full ring, a
    // disabled button hinted "00:00 left" — was the screen saying four
    // contradictory things about a state it had not looked up.
    let known = move || status.read().is_some();

    let claim = move |_| {
        if claiming.get_untracked() {
            return;
        }
        let backend = backend.get_value();
        claiming.set(true);
        cooldown_error.set(None);
        leptos::task::spawn_local(async move {
            let result = backend.api.faucet_claim().await;
            claiming.set(false);
            match result {
                Ok(claim) => {
                    let total = claim
                        .granted
                        .iter()
                        .map(|grant| format!("{} {}", format::fixed(grant.amount, 2), grant.asset))
                        .collect::<Vec<_>>()
                        .join(" \u{00B7} ");
                    stores.toast(Toast {
                        id: 0,
                        tone: Tone::Good,
                        title: t("Grant claimed").to_string(),
                        body: format!("{total}, as {}", format::hash(&claim.tx_hash)),
                        height: None,
                        action: None,
                        expires_ms: Some(crate::clock::now_millis() + 5_000),
                    });
                    crate::refresh_account(Arc::clone(&backend), stores);
                    if let Ok(response) = backend.api.faucet().await {
                        status.set(Some(response));
                    }
                    if let Ok(response) = backend.api.ledger(200).await {
                        history.set(
                            response
                                .entries
                                .into_iter()
                                .filter(|entry| entry.kind == LedgerKind::Faucet)
                                .collect(),
                        );
                    }
                }
                Err(error) if error.code() == Some(ErrorCode::FaucetCooldown) => {
                    // The cooldown is not a toast: it belongs on the screen that
                    // is about the cooldown, naming the exact unlock time.
                    cooldown_error.set(Some(error.message()));
                    if let Ok(response) = backend.api.faucet().await {
                        status.set(Some(response));
                    }
                }
                Err(error) => crate::report(&backend, stores, &error),
            }
        });
    };

    view! {
        <AppShell>
            <h1 class="t-h2" style="margin-bottom:var(--sp-4)">{t("Faucet")}</h1>
            <p class="t-body" style="font-size:var(--fs-sm);margin-bottom:var(--sp-20)">
                "Play funds, granted on an hourly cooldown and written to the same append-only \
                 ledger as every other movement in your account. There is no way to buy more and \
                 no way to run out permanently."
            </p>

            <Show when=move || stores.session.read().is_none()>
                <div style="margin-bottom:var(--sp-16)">
                    <Banner
                        tone="info"
                        title="The faucet needs an account"
                        body="Grants are recorded against an address, so the faucet is one of the \
                              few things here that is not public."
                    >
                        <a class="btn btn-primary btn-sm" href="/">"Sign in"</a>
                    </Banner>
                </div>
            </Show>

            <div class="fa-grid">
                <div class="panel spine-account">
                    <div class="panel-head">
                        <h3>
                            {move || if !known() {
                                "What the faucet pays"
                            } else if available() {
                                {t("Ready to claim")}
                            } else {
                                {t("Next grant")}
                            }}
                        </h3>
                        <span class="spacer"></span>
                        <span class=move || if !known() {
                            "badge"
                        } else if available() {
                            "badge live"
                        } else {
                            "badge warn"
                        }>
                            {move || if !known() {
                                {t("Sign in to claim")}
                            } else if available() {
                                "Available"
                            } else {
                                {t("On cooldown")}
                            }}
                        </span>
                    </div>
                    <div class="fa-body">
                        // The grant itself is behind the session, because it is
                        // recorded against an address. A visitor is told that,
                        // rather than shown an empty list with no explanation.
                        <Show
                            when=move || !grant().is_empty()
                            fallback=|| view! {
                                <crate::ui::EmptyState
                                    title="What you get is on the other side of sign-in"
                                    body="The grant is recorded against an address, so the \
                                          gateway will not say what it pays until it knows whose \
                                          claim it is. It is the same for everybody."
                                />
                            }
                        >
                        <div class="grantlist" class:dimmed=move || known() && !available()>
                            <For each=grant key=|grant| grant.asset.clone() let:grant>
                                <div class="grantrow big">
                                    <span class="a">{grant.asset.clone()}</span>
                                    <span class="num q">
                                        {format::fixed(
                                            grant.amount,
                                            if grant.asset == "USD" { 2 } else { 8 },
                                        )}
                                    </span>
                                </div>
                            </For>
                        </div>
                        </Show>
                        <button
                            class="btn btn-primary btn-lg btn-block"
                            disabled=move || !available() || claiming.get()
                                || stores.session.read().is_none()
                            data-loading=move || claiming.get().then_some("")
                            on:click=claim
                        >
                            {t("Claim grant")}
                        </button>
                        <p class="t-meta" style="text-align:center">
                            {move || if !known() {
                                "Sign in and this pays out in one block. The wait between \
                                 claims is the same for everybody, bots included."
                                    .to_string()
                            } else if available() {
                                "Next claim available one hour after this one.".to_string()
                            } else {
                                format!(
                                    "The wait is the same for everybody, bots included. {} left.",
                                    format::duration(seconds_left()),
                                )
                            }}
                        </p>
                        <hr style="border:0;border-top:1px solid var(--border-subtle);margin:0" />
                        <div class="ticket" style="padding:0;gap:var(--sp-8)">
                            <div class="row">
                                <span class="k">{t("Cooldown")}</span>
                                <span class="v">
                                    {move || status.read().as_ref().map_or_else(
                                        || "\u{2014}".to_string(),
                                        |status| format::duration(status.cooldown_seconds),
                                    )}
                                </span>
                            </div>
                            <div class="row">
                                <span class="k">{t("Claims made")}</span>
                                <span class="v">
                                    {move || status.read().as_ref().map_or_else(
                                        || "\u{2014}".to_string(),
                                        |status| status.claims_made.to_string(),
                                    )}
                                </span>
                            </div>
                            <div class="row">
                                <span class="k">"Recorded as"</span>
                                <span class="v">"ledger kind \u{201C}faucet\u{201D}"</span>
                            </div>
                            <div class="row">
                                <span class="k">"Settles in"</span>
                                <span class="v">"1 block, about 100ms"</span>
                            </div>
                        </div>
                    </div>
                </div>

                <div class="panel spine-account">
                    <div class="panel-head">
                        <h3>{t("Cooldown")}</h3>
                        <span class="spacer"></span>
                        <span class=move || if !known() || available() {
                            "badge"
                        } else {
                            "badge warn"
                        }>
                            <Icon name="i-clock" size=9 />
                            {move || if !known() {
                                "Not known yet".to_string()
                            } else if available() {
                                "Not waiting".to_string()
                            } else {
                                format!("{} left", format::duration(seconds_left()))
                            }}
                        </span>
                    </div>
                    <div class="fa-body">
                        <div class="cooldown">
                            <CooldownRing
                                left=Signal::derive(seconds_left)
                                total=Signal::derive(move || status
                                    .read()
                                    .as_ref()
                                    .map_or(3_600, |status| status.cooldown_seconds))
                            />
                            <div class="cooldown-ctr">
                                <span class="num t">
                                    {move || if !known() {
                                        "\u{2014}".to_string()
                                    } else if available() {
                                        "Ready".to_string()
                                    } else {
                                        format::duration(seconds_left())
                                    }}
                                </span>
                                <span class="l">
                                    {move || if !known() {
                                        "not known yet".to_string()
                                    } else {
                                        next_at().map_or_else(
                                            || "no wait".to_string(),
                                            |at| format!("until {}", format::clock(at)),
                                        )
                                    }}
                                </span>
                            </div>
                        </div>
                        <p class="t-meta" style="text-align:center;line-height:var(--lh-snug)">
                            "The ring drains toward the moment the gateway will accept the next \
                             claim. It is drawn from "<code>"next_available_at"</code>
                            " and from nothing else, so it cannot disagree with the button."
                        </p>
                    </div>
                </div>

                <div class="panel spine-account fa-hist">
                    <div class="panel-head">
                        <h3>{t("Grant history")}</h3>
                        <span class="spacer"></span>
                        <span class="badge">
                            {move || format!("{} claims", history.read().len())}
                        </span>
                    </div>
                    <div class="tbl-wrap">
                        <Show
                            when=move || !history.read().is_empty()
                            fallback=|| view! {
                                <crate::ui::EmptyState
                                    title="No grants yet"
                                    body="Every claim is written to the append-only ledger with \
                                          its block and its transaction, and appears here."
                                />
                            }
                        >
                            <table class="tbl">
                                <thead>
                                    <tr>
                                        <th class="n">"Claim"</th><th>"Granted"</th>
                                        <th class="n">"Block"</th><th class="n">"Time"</th>
                                        <th>"Transaction"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For
                                        each=move || history.get()
                                        key=|entry| entry.id.clone()
                                        let:entry
                                    >
                                        <tr>
                                            <td class="n strong">{entry.id.clone()}</td>
                                            <td>
                                                {format!(
                                                    "{} {}",
                                                    format::fixed(entry.amount, 2),
                                                    entry.asset,
                                                )}
                                            </td>
                                            <td class="n">{format::count(entry.height)}</td>
                                            <td class="n">
                                                {format::parse_rfc3339(&entry.at)
                                                    .map_or_else(
                                                        || entry.at.clone(), format::calendar)}
                                            </td>
                                            <td class="n dim">{format::hash(&entry.tx_hash)}</td>
                                        </tr>
                                    </For>
                                </tbody>
                            </table>
                        </Show>
                    </div>
                </div>
            </div>

            {move || cooldown_error.get().map(|message| view! {
                <div style="margin-top:var(--sp-16)">
                    <Banner
                        tone="warn"
                        title="Claimed too soon"
                        body=format!(
                            "The gateway answered 429 FAUCET_COOLDOWN. {message}",
                        )
                    />
                </div>
            })}
        </AppShell>
    }
}

/// The draining ring that counts down to the next claim.
#[component]
fn CooldownRing(
    /// Seconds remaining.
    #[prop(into)]
    left: Signal<i64>,
    /// The full cooldown, for the proportion.
    #[prop(into)]
    total: Signal<i64>,
) -> impl IntoView {
    // Circumference of a 52px radius circle, which is what the reference draws.
    const CIRCUMFERENCE: f64 = 326.7;
    let offset = move || {
        let total = total.get().max(1);
        let fraction = f64::from(u32::try_from(left.get().clamp(0, total)).unwrap_or(0))
            / f64::from(u32::try_from(total).unwrap_or(1));
        CIRCUMFERENCE * (1.0 - fraction)
    };
    view! {
        <svg viewBox="0 0 120 120" width="118" height="118" aria-hidden="true">
            <circle cx="60" cy="60" r="52" fill="none" stroke="var(--border-subtle)" stroke-width="8"></circle>
            <circle
                cx="60"
                cy="60"
                r="52"
                fill="none"
                stroke="var(--brass)"
                stroke-width="8"
                stroke-linecap="round"
                stroke-dasharray=CIRCUMFERENCE.to_string()
                stroke-dashoffset=move || offset().to_string()
                transform="rotate(-90 60 60)"
            ></circle>
        </svg>
    }
}
