//! Landing, and the join flow.
//!
//! The landing screen does all of the teaching, so the terminal never has to
//! apologise for being dense. The hero is not an illustration of a market: it is
//! [`crate::ui::book::BookLadder`], the production component, at production
//! density, on live data.
//!
//! # The join flow
//!
//! `docs/api-contract.md` §2 makes identity an EVM key pair, so the account is
//! twelve BIP-39 words and the words never leave the browser. Two paths, neither
//! of them the default:
//!
//! * **Create a wallet** → reveal → confirm three words → sign in. The phrase is
//!   covered until asked for, covers itself again after ninety seconds, and is
//!   hidden before the confirmation step, so the three words are typed from what
//!   the person actually wrote down rather than copied off the screen above.
//! * **Paste a phrase** → derive and show the address → sign. The address is
//!   shown *before* anything is signed, because reading it is the only way to
//!   catch a phrase that is right at an index that is not.
//!
//! There is no download. A seed phrase in a downloads folder is the worst place
//! it could sit, and the design reference says so outright.

use std::sync::Arc;

use gloo_storage::Storage as _;
use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::Challenge;
use crate::api::ErrorCode;
use crate::config::{Backend, SESSION_TOKEN_KEY};
use crate::i18n::t;
use crate::state::{use_stores, Tone};
use crate::ticket::handle_problem;
use crate::ui::book::BookLadder;
use crate::ui::{Banner, ChainRail, ConnIndicator, Icon, Modal, Signed, Sprite, Wordmark};
use crate::wallet::{normalise_phrase, Wallet};
use crate::ws::FeedHandle;

/// How long the revealed phrase stays on screen before covering itself.
const REVEAL_SECONDS: i64 = 90;

/// What an expired or spent challenge is told to the person, rather than to a
/// developer. The remedy is the button beside it, which asks for a new one.
const EXPIRED_WINDOW: &str =
    "A sign-in window lasts five minutes and can be used once. Sign again and a new one is \
     issued for you \u{2014} nothing has been lost, and your recovery phrase has not changed.";

/// The three positions confirmed after the reveal, spread across three rows so
/// that a phrase copied in the wrong order fails here rather than in a month.
const VERIFY_POSITIONS: [usize; 3] = [3, 7, 11];

/// Which step of the join flow is on screen.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// Neither path chosen yet.
    Closed,
    /// The twelve words, covered until asked for.
    Reveal,
    /// Three words re-entered by ordinal, and a handle claimed.
    Confirm,
    /// A phrase pasted, with the address it derives.
    Paste,
    /// The exact message about to be signed.
    Sign(Box<Challenge>),
}

/// The landing screen.
#[component]
pub fn Landing() -> impl IntoView {
    let stores = use_stores();
    // Has the hero's market ever traded? A last price of zero is not a price.
    let traded = move || {
        stores
            .quote()
            .is_some_and(|quote| quote.last > Decimal::ZERO)
    };
    let step = RwSignal::new(Step::Closed);
    // The minted or pasted wallet. In memory, for this tab, until the session
    // token is stored — and never written anywhere.
    let wallet = StoredValue::new_local(None::<Wallet>);
    let handle = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let failure = RwSignal::new(None::<String>);

    view! {
        <div class="lp">
            <header class="lp-top">
                <Wordmark size=17 />
                <nav class="lp-nav">
                    <a href="#how">{t("How it works")}</a>
                    <a href="/chain">{t("Dashboard")}</a>
                    <a href="/leaderboard">{t("Leaderboard")}</a>
                    <button
                        class="btn btn-secondary btn-sm"
                        on:click=move |_| step.set(Step::Paste)
                    >{t("Sign in")}</button>
                </nav>
            </header>

            <div class="lp-hero">
                <div class="lp-copy">
                    <div class="lp-logo">
                        <img src="/assets/title_logo.png" alt="" aria-hidden="true" draggable="false" />
                    </div>
                    <h1>{t("The order book is real.")}<br />{t("The money is not.")}</h1>
                    <span class="lp-press">{t("Insert coin \u{25B8} press start")}</span>
                    <p class="lead">
                        {t("SuperFrontierLiquid is a personal experiment in exchange and blockchain design. Run this prototype locally, claim simulated funds, and explore spot, perpetual and dated-futures trading against bots. Use disposable test wallets only; this unfinished software is not suitable for real funds.")}
                    </p>

                    <div class="tape-rule" style="margin:var(--sp-24) 0 var(--sp-16)"></div>
                    <div class="lp-facts">
                        <div>
                            <b class="num">"100ms"</b>
                            <span>{t("blocks, and every order is one transaction in one of them")}</span>
                        </div>
                        <div>
                            <b class="num">{move || stores.markets.read().len().to_string()}</b>
                            <span>{t("markets across spot, perpetual and dated futures")}</span>
                        </div>
                        <div>
                            <b class="num">"0"</b>
                            <span>
                                "personal data. No email and no password \u{2014} a signature \
                                 from your own key is the login"
                            </span>
                        </div>
                    </div>

                    // Two paths, side by side, neither one the default.
                    <div class="lp-paths">
                        <div class="lp-path">
                            <Sprite name="mascot_trader" size=44 />
                            <h4>{t("I need an account")}</h4>
                            <p>
                                "This generates twelve words and derives an address from them. \
                                 Write the words down: they are the account, and they are shown \
                                 once."
                            </p>
                            <button
                                class="btn btn-primary btn-block"
                                on:click=move |_| {
                                    match Wallet::generate() {
                                        Ok(minted) => {
                                            wallet.set_value(Some(minted));
                                            failure.set(None);
                                            step.set(Step::Reveal);
                                        }
                                        Err(error) => stores.notify(
                                            Tone::Bad, "No wallet was created", error.to_string()),
                                    }
                                }
                            >{t("Create a wallet")}</button>
                        </div>
                        <div class="lp-path">
                            <Sprite name="coin_btc" size=44 />
                            <h4>{t("I have a recovery phrase")}</h4>
                            <p>
                                "Use a recovery phrase created for this prototype. Never paste \
                                 a phrase from a wallet holding real assets. The phrase is \
                                 checked in the browser and never sent."
                            </p>
                            <button
                                class="btn btn-secondary btn-block"
                                on:click=move |_| { failure.set(None); step.set(Step::Paste); }
                            >{t("Paste a phrase")}</button>
                        </div>
                    </div>
                    <p class="lp-hint">
                        "Test wallets use BIP-39 and Ethereum-compatible addresses, derived at "
                        <code>"m/44'/60'/0'/0/0"</code>
                        ". Create a fresh wallet for this local experiment and never fund it \
                         with real assets. The browser derives the key and signs a login \
                         challenge; the recovery phrase is never sent to the server."
                    </p>
                </div>

                <div class="lp-demo">
                    <div class="lp-demo-head">
                        <span class="sym-row">
                            <b style="font-size:var(--fs-md)">{move || stores.selected.get()}</b>
                            <span class="badge">
                                {move || stores.market().map_or_else(
                                    String::new, |market| market.kind.label().to_string())}
                            </span>
                        </span>
                        <ConnIndicator />
                    </div>
                    // A market that has never traded has no price. It used to
                    // show `0.000000` in jade beside `+0.00%` — a green zero,
                    // which is the colour that means *up*, as the first thing a
                    // stranger sees.
                    <div class="lp-price">
                        <span
                            class="num"
                            class:up=move || traded() && stores.quote()
                                .is_some_and(|q| q.change_pct > Decimal::ZERO)
                            class:down=move || traded() && stores.quote()
                                .is_some_and(|q| q.change_pct < Decimal::ZERO)
                            class:dim=move || !traded()
                        >
                            {move || if traded() {
                                stores.quote().map_or_else(
                                    || "\u{2014}".to_string(),
                                    |quote| crate::format::price(
                                        quote.last,
                                        stores.market().map_or(2, |m| m.price_decimals),
                                    ),
                                )
                            } else {
                                "\u{2014}".to_string()
                            }}
                        </span>
                        <Show
                            when=traded
                            fallback=|| view! {
                                <span class="t-meta">{t("no trades yet")}</span>
                            }
                        >
                            <Signed
                                value=Signal::derive(move || stores.quote()
                                    .map_or(Decimal::ZERO, |q| q.change_pct))
                                percent=true
                                glyph=true
                            />
                        </Show>
                    </div>
                    <div class="panel spine-market">
                        <BookLadder depth=6 />
                    </div>
                    <p class="lp-caption">
                        "This is the order book from the terminal itself, at the density you \
                         will trade in. Nothing on this page is a picture of the product."
                    </p>
                </div>
            </div>

            <div class="lp-teach" id="how">
                <div class="lp-card">
                    <div class="lp-card-demo">
                        <div class="grantrow">
                            <span class="a">"USD"</span><span class="num q">"100,000.00"</span>
                        </div>
                        <div class="grantrow">
                            <span class="a">"BTC"</span><span class="num q">"1.00000000"</span>
                        </div>
                        <button class="btn btn-primary btn-sm btn-block" style="margin-top:var(--sp-8)">
                            {t("Claim")}
                        </button>
                    </div>
                    <h3><Sprite name="faucet" size=28 />{t("Start with a hundred thousand of nothing")}</h3>
                    <p>
                        "The faucet grants play funds on an hourly cooldown, recorded in an \
                         append-only ledger. Run out and you wait an hour, the same as everyone else."
                    </p>
                </div>
                <div class="lp-card">
                    <div class="lp-card-demo">
                        <div class="pos-mini">
                            <span class="sym">"BTC-PERP "<span class="badge">"10\u{00D7}"</span></span>
                            <span class="side-badge long"><span class="dir">"\u{25B2}"</span>"Long"</span>
                            <span class="num" style="color:var(--bid);text-align:right">"+1,872.15"</span>
                        </div>
                        <div class="liqmeter" data-risk="medium" style="--risk:48;margin:var(--sp-8) 0 0">
                            <i></i>
                        </div>
                        <p class="t-meta" style="margin-top:6px">
                            "Liquidation 58,200.00 \u{00B7} funding \u{2212}12.40 paid"
                        </p>
                    </div>
                    <h3><Sprite name="rocket" size=28 />{t("Take leverage you would never take")}</h3>
                    <p>
                        "Up to fifty times, cross or isolated margin, with funding accruing every \
                         hour and a liquidation engine that does not care how you feel about it."
                    </p>
                </div>
                <div class="lp-card">
                    <div class="lp-card-demo">
                        <div style="display:flex;align-items:center;gap:var(--sp-8);margin-bottom:var(--sp-8)">
                            <span style="font-size:var(--fs-micro);color:var(--text-muted)">"Height"</span>
                            <span class="num" style="font-size:var(--fs-md);font-weight:600">
                                {move || stores.chain.read().as_ref().map_or_else(
                                    || "\u{2014}".to_string(),
                                    |chain| crate::format::count(chain.height),
                                )}
                            </span>
                        </div>
                        <span class="blocks" style="display:flex;gap:3px">
                            {move || stores.blocks.read().iter().take(9).rev().map(|block| {
                                let fill = block.tx_count.clamp(8, 95);
                                view! { <span class="blk" style=format!("--fill:{fill}")><i></i></span> }
                            }).collect_view()}
                        </span>
                        <p class="t-meta" style="margin-top:8px">
                            {move || match stores.chain.read().as_ref() {
                                None => "Connecting to the chain".to_string(),
                                // Genesis. "0.0 tx/s from 0 validators, state
                                // root 0x0000…0000" is four true figures that
                                // together read as a broken system; this says
                                // the same thing as a sentence.
                                Some(chain) if chain.height == 0 => {
                                    "Genesis. No block has been produced yet \u{2014} the first \
                                     transaction makes block one.".to_string()
                                }
                                Some(chain) => format!(
                                    "{} tx/s from {} validators, state root {}",
                                    crate::format::fixed(chain.tps_1m, 1),
                                    chain.validators,
                                    crate::format::hash(&chain.state_root),
                                ),
                            }}
                        </p>
                    </div>
                    <h3><Sprite name="block" size=28 />{t("Look underneath any time you like")}</h3>
                    <p>
                        "Orders, fills, funding and liquidations are transactions in blocks. The \
                         chain rail sits along the bottom of every screen so you can watch yours land."
                    </p>
                </div>
            </div>

            <footer class="lp-foot">
                <span>
                    "Open source under MIT and Apache\u{00A0}2.0. An independent project. \
                     Nothing here settles into a real asset and no real money is ever at stake."
                </span>
                <span class="lp-foot-links">
                    <a href="/chain">{t("Dashboard")}</a>
                    <a href="/leaderboard">{t("Leaderboard")}</a>
                    <a href="/trade">{t("Markets")}</a>
                </span>
            </footer>

            <ChainRail />
        </div>

        <JoinFlow step=step wallet=wallet handle=handle busy=busy failure=failure />
    }
}

/// The four dialogs of the join flow.
#[component]
fn JoinFlow(
    /// Which step is showing.
    step: RwSignal<Step>,
    /// The wallet in play, held only in memory.
    wallet: StoredValue<Option<Wallet>, LocalStorage>,
    /// The handle being claimed.
    handle: RwSignal<String>,
    /// True while a request is in flight.
    busy: RwSignal<bool>,
    /// The last failure, shown in place rather than as a toast.
    failure: RwSignal<Option<String>>,
) -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let feed = StoredValue::new(use_context::<FeedHandle>());
    // Read here, in the component body, because `use_navigate` reads router
    // context. A full page load at the end of this flow would drop the chosen
    // backend and land a real signed-in user in the fixtures.
    let navigate = crate::nav::navigator();

    // --- the handle latch.
    //
    // Once the gateway has said this address needs a handle, it needs one for
    // the rest of the flow. Deriving that from the last error message meant the
    // field vanished the moment anything else failed, taking the value the user
    // had typed with it.
    let needs_handle = RwSignal::new(false);
    // --- whether the challenge on screen has been signed and sent.
    //
    // A challenge nonce is single use. The gateway spends it on the attempt
    // that comes back `HANDLE_REQUIRED`, so signing the same one again is a
    // guaranteed `401` — which is exactly what a new user hit at the one step
    // where the account had, in fact, just been created.
    let spent = RwSignal::new(false);

    // --- reveal state
    let revealed = RwSignal::new(false);
    let revealed_at = RwSignal::new(0_i64);
    let seconds_left = move || {
        let elapsed = (stores.now_ms.get() - revealed_at.get()) / 1_000;
        (REVEAL_SECONDS - elapsed).max(0)
    };
    // The phrase covers itself again without anyone having to remember to hide it.
    Effect::new(move |_| {
        if revealed.get() && seconds_left() == 0 {
            revealed.set(false);
        }
    });

    // --- confirmation state
    let typed = RwSignal::new([String::new(), String::new(), String::new()]);
    let word_at = move |position: usize| {
        wallet
            .with_value(|wallet| {
                wallet.as_ref().and_then(|wallet| {
                    wallet
                        .phrase()
                        .split_whitespace()
                        .nth(position.saturating_sub(1))
                        .map(str::to_string)
                })
            })
            .unwrap_or_default()
    };
    let word_ok = move |index: usize| {
        let position = VERIFY_POSITIONS.get(index).copied().unwrap_or(1);
        typed
            .read()
            .get(index)
            .map(String::as_str)
            .unwrap_or_default()
            .trim()
            == word_at(position)
    };
    let all_words_ok = move || (0..3).all(word_ok);

    // --- paste state
    let pasted = RwSignal::new(String::new());
    let masked = RwSignal::new(true);
    let index = RwSignal::new(0_u32);
    let derived = move || Wallet::from_phrase(&pasted.get(), index.get()).ok();
    let word_count = move || normalise_phrase(&pasted.get()).split_whitespace().count();

    // --- the handshake, shared by both paths
    let request_challenge = move |ready: Wallet| {
        let backend = backend.get_value();
        let address = ready.address().to_string();
        wallet.set_value(Some(ready));
        busy.set(true);
        failure.set(None);
        leptos::task::spawn_local(async move {
            let result = backend.api.challenge(&address).await;
            busy.set(false);
            match result {
                Ok(challenge) => {
                    spent.set(false);
                    step.set(Step::Sign(Box::new(challenge)));
                }
                Err(error) => failure.set(Some(error.message())),
            }
        });
    };

    let sign_and_login = move |challenge: Challenge, claim: Option<String>| {
        let backend = backend.get_value();
        let feed = feed.get_value();
        let Some(signature) = wallet.with_value(|wallet| {
            wallet
                .as_ref()
                .and_then(|wallet| wallet.sign_personal(&challenge.message).ok())
        }) else {
            failure.set(Some("The key could not sign that message.".to_string()));
            return;
        };
        let address = wallet
            .with_value(|wallet| wallet.as_ref().map(|wallet| wallet.address().to_string()))
            .unwrap_or_default();
        busy.set(true);
        failure.set(None);
        // Whatever comes back, this nonce is now used. The next attempt has to
        // be signed over a fresh one.
        spent.set(true);
        leptos::task::spawn_local(async move {
            let result = backend
                .api
                .login(&address, &signature, claim.as_deref())
                .await;
            busy.set(false);
            match result {
                Ok(session) => {
                    let _ =
                        gloo_storage::LocalStorage::set(SESSION_TOKEN_KEY, &session.session_token);
                    backend.api.set_token(Some(session.session_token.clone()));
                    if let Some(feed) = feed {
                        feed.authenticate(Some(session.session_token.clone()));
                        feed.subscribe(vec!["account".to_string()]);
                    }
                    if let Ok(info) = backend.api.session().await {
                        stores.session.set(Some(info));
                    }
                    crate::refresh_account(Arc::clone(&backend), stores);
                    // The phrase leaves memory the moment it is no longer needed.
                    wallet.set_value(None);
                    // A client-side route change, so the backend this session
                    // was issued by survives the transition.
                    navigate("/trade");
                }
                Err(error) => {
                    // The client cannot know in advance whether an address is
                    // new, so the contract has it ask only once it is told.
                    match error.code() {
                        Some(ErrorCode::HandleRequired) => {
                            needs_handle.set(true);
                            failure.set(None);
                        }
                        // The sign-in window is a state, not a failure, and the
                        // remedy is the button already on screen: it asks for a
                        // fresh challenge and signs that. Naming an endpoint the
                        // user cannot call is not a remedy.
                        Some(ErrorCode::ChallengeExpired | ErrorCode::Unauthorized) => {
                            failure.set(Some(EXPIRED_WINDOW.to_string()));
                        }
                        _ => failure.set(Some(error.message())),
                    }
                }
            }
        });
    };

    // The one entry point the Sign step's button uses. It is the whole of
    // finding 2: sign the challenge on screen while it is unspent, and ask for a
    // fresh one the moment it is not.
    let submit_sign_in = move |challenge: Challenge, claim: Option<String>| {
        if !spent.get_untracked() {
            sign_and_login(challenge, claim);
            return;
        }
        let backend = backend.get_value();
        let address = wallet
            .with_value(|wallet| wallet.as_ref().map(|wallet| wallet.address().to_string()))
            .unwrap_or_default();
        busy.set(true);
        failure.set(None);
        leptos::task::spawn_local(async move {
            match backend.api.challenge(&address).await {
                Ok(fresh) => {
                    busy.set(false);
                    spent.set(false);
                    // The screen shows the message it is about to sign, so the
                    // panel is updated before the signature is made rather than
                    // after: the expiry beside it has to be the live one.
                    step.set(Step::Sign(Box::new(fresh.clone())));
                    sign_and_login(fresh, claim);
                }
                Err(error) => {
                    busy.set(false);
                    failure.set(Some(error.message()));
                }
            }
        });
    };

    view! {
        {move || match step.get() {
            Step::Closed => ().into_any(),

            // ---------------------------------------------------------- reveal
            Step::Reveal => {
                let words: Vec<String> = wallet
                    .with_value(|wallet| {
                        wallet.as_ref().map(|wallet| {
                            wallet.phrase().split_whitespace().map(str::to_string).collect()
                        })
                    })
                    .unwrap_or_default();
                view! {
                    <Modal
                        title="Write these twelve words down"
                        icon="i-warn"
                        icon_colour="var(--brass)"
                        width="w-wide"
                        steps=move || view! {
                            <JoinSteps current=2 />
                        }.into_any()
                        foot=move || view! {
                            <>
                                <button class="btn btn-ghost" on:click=move |_| {
                                    wallet.set_value(None);
                                    step.set(Step::Closed);
                                }>"Cancel"</button>
                                <button
                                    class="btn btn-primary"
                                    disabled=move || !revealed.get()
                                    on:click=move |_| { revealed.set(false); step.set(Step::Confirm); }
                                >"Hide the words and confirm"</button>
                            </>
                        }.into_any()
                    >
                        <p style="font-size:var(--fs-sm)">
                            "These twelve words "<b style="color:var(--text-primary)">"are"</b>
                            " the account. Anyone who reads them can trade as you, and if you \
                             lose them nobody \u{2014} us included \u{2014} can give the account \
                             back. There is no email on it and no password to reset."
                        </p>

                        <div class="mnem" style="margin:var(--sp-16) 0 var(--sp-8)">
                            <div class="mnem-grid">
                                {words
                                    .iter()
                                    .enumerate()
                                    .map(|(position, word)| {
                                        // Every BIP-39 word is unique in its
                                        // first four letters, so those four are
                                        // the ones set in full contrast.
                                        let (head, tail) = word.split_at(word.len().min(4));
                                        view! {
                                            <span class="mw">
                                                <span class="i">{format!("{:02}", position + 1)}</span>
                                                <span class="w">
                                                    <b>{head.to_string()}</b>{tail.to_string()}
                                                </span>
                                            </span>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                            <Show when=move || !revealed.get()>
                                <div class="mnem-cover">
                                    <Icon name="i-eyeoff" size=28 style="color:var(--text-faint)" />
                                    <p>
                                        "The words are covered until you ask for them. Make sure \
                                         nobody is reading over your shoulder and nothing is \
                                         recording this screen."
                                    </p>
                                    <button
                                        class="btn btn-secondary btn-sm"
                                        on:click=move |_| {
                                            revealed.set(true);
                                            revealed_at.set(crate::clock::now_millis());
                                        }
                                    >"Show the phrase"</button>
                                </div>
                            </Show>
                        </div>
                        <p class="t-meta" style="line-height:var(--lh-snug)">
                            "Read across, left to right, the way you would read a line. Copy the \
                             number with the word \u{2014} the order matters more than the \
                             spelling, because every BIP-39 word is unique in its first four \
                             letters and those four are the ones set in full contrast."
                        </p>

                        <Show when=move || revealed.get()>
                            <div style="display:flex;align-items:center;gap:var(--sp-12);margin-top:var(--sp-16);flex-wrap:wrap">
                                <button
                                    class="btn btn-secondary btn-sm"
                                    on:click=move |_| {
                                        let phrase = wallet.with_value(|wallet| {
                                            wallet.as_ref().map(|w| w.phrase().to_string())
                                        });
                                        if let Some(phrase) = phrase {
                                            copy_to_clipboard(&phrase);
                                            stores.notify(
                                                Tone::Good,
                                                "Phrase copied",
                                                "Paste it into a password manager, then copy \
                                                 something else.",
                                            );
                                        }
                                    }
                                >
                                    <Icon name="i-copy" size=12 />
                                    "Copy for a password manager"
                                </button>
                                <span class="mnem-timer">
                                    <Icon name="i-clock" size=12 style="color:var(--brass)" />
                                    "Hiding again in "
                                    <span class="num">
                                        {move || crate::format::duration(seconds_left())}
                                    </span>
                                    <span
                                        class="track"
                                        style=move || format!(
                                            "--left:{}", seconds_left() * 100 / REVEAL_SECONDS)
                                    ><i></i></span>
                                </span>
                                <button
                                    class="btn btn-ghost btn-sm"
                                    style="margin-left:auto"
                                    on:click=move |_| revealed.set(false)
                                >"Hide now"</button>
                            </div>
                        </Show>
                        <p class="t-meta" style="margin-top:var(--sp-8);line-height:var(--lh-snug)">
                            "Copying puts the phrase on the system clipboard, where other \
                             applications can read it. Paste it straight into a password \
                             manager, then copy something else. There is no download, because a \
                             seed phrase in a downloads folder is the worst place it could sit."
                        </p>
                    </Modal>
                }
                .into_any()
            }

            // --------------------------------------------------------- confirm
            Step::Confirm => view! {
                <Modal
                    title="Confirm three of the words"
                    icon="i-eyeoff"
                    width="w-wide"
                    steps=move || view! { <JoinSteps current=3 /> }.into_any()
                    foot=move || view! {
                        <>
                            <button class="btn btn-ghost" on:click=move |_| step.set(Step::Reveal)>
                                "Show the phrase again"
                            </button>
                            <button
                                class="btn btn-primary"
                                disabled=move || !all_words_ok()
                                    || handle_problem(&handle.get()).is_some()
                                    || busy.get()
                                data-loading=move || busy.get().then_some("")
                                on:click=move |_| {
                                    if let Some(ready) = wallet.with_value(|wallet| {
                                        wallet.as_ref().and_then(|wallet|
                                            Wallet::from_phrase(wallet.phrase(), 0).ok())
                                    }) {
                                        request_challenge(ready);
                                    }
                                }
                            >"Create account and sign in"</button>
                        </>
                    }.into_any()
                >
                    <p style="font-size:var(--fs-sm)">
                        "The phrase is hidden. Type words three, seven and eleven from what you \
                         wrote down \u{2014} three positions spread across three rows, so a \
                         phrase copied in the wrong order fails here rather than a month from now."
                    </p>
                    <div class="verify-grid" style="margin:var(--sp-16) 0">
                        {(0..3)
                            .map(|slot| {
                                let position = VERIFY_POSITIONS.get(slot).copied().unwrap_or(1);
                                view! {
                                    <div class="field">
                                        <label for=format!("v{position}")>
                                            {format!("Word {position:02}")}
                                        </label>
                                        <div style="position:relative;display:flex;align-items:center">
                                            <input
                                                class="input"
                                                id=format!("v{position}")
                                                autocomplete="off"
                                                autocapitalize="off"
                                                spellcheck="false"
                                                prop:value=move || typed.read()
                                                    .get(slot).cloned().unwrap_or_default()
                                                on:input=move |event| {
                                                    let value = input_value(&event);
                                                    typed.update(|words| {
                                                        if let Some(slot) = words.get_mut(slot) {
                                                            *slot = value;
                                                        }
                                                    });
                                                }
                                            />
                                            <Show when=move || word_ok(slot)>
                                                <Icon
                                                    name="i-check"
                                                    size=12
                                                    class="ok-mark"
                                                    style="position:absolute;right:10px"
                                                />
                                            </Show>
                                        </div>
                                    </div>
                                }
                            })
                            .collect_view()}
                    </div>
                    <hr style="border:0;border-top:1px solid var(--border-subtle);margin:var(--sp-16) 0" />
                    <HandleField handle=handle />
                    <div class="derived" style="margin-top:var(--sp-16)">
                        <Icon name="i-key" size=14 style="color:var(--signal)" />
                        <span class="k">"Your address"</span>
                        <span class="a">
                            {move || wallet.with_value(|wallet| wallet
                                .as_ref()
                                .map_or_else(String::new, |wallet| wallet.address().to_string()))}
                        </span>
                    </div>
                    {move || failure.get().map(|message| view! {
                        <div style="margin-top:var(--sp-12)">
                            <Banner tone="danger" title="That did not go through" body=message />
                        </div>
                    })}
                </Modal>
            }
            .into_any(),

            // ----------------------------------------------------------- paste
            Step::Paste => view! {
                <Modal
                    title=t("Sign in with your recovery phrase")
                    icon="i-key"
                    width="w-wide"
                    foot=move || view! {
                        <>
                            <button class="btn btn-ghost" on:click=move |_| step.set(Step::Closed)>
                                {t("Cancel")}
                            </button>
                            <button
                                class="btn btn-primary"
                                disabled=move || derived().is_none() || busy.get()
                                data-loading=move || busy.get().then_some("")
                                on:click=move |_| {
                                    if let Some(ready) = derived() {
                                        request_challenge(ready);
                                    }
                                }
                            >{t("Continue")}</button>
                        </>
                    }.into_any()
                >
                    <p style="font-size:var(--fs-sm)">
                        "Enter the twelve BIP-39 words from your disposable test wallet. Never \
                         use a recovery phrase associated with real assets. The words are \
                         checked in this tab; only a signature leaves the browser."
                    </p>
                    <div class="phrasefield" style="margin-top:var(--sp-16)">
                        <label
                            for="ph"
                            style="display:block;font-size:var(--fs-xs);color:var(--text-muted);margin-bottom:var(--sp-6)"
                        >{t("Recovery phrase")}</label>
                        <textarea
                            id="ph"
                            spellcheck="false"
                            autocomplete="off"
                            autocapitalize="off"
                            aria-invalid=move || (word_count() == 12 && derived().is_none())
                                .then_some("true")
                            style=move || if masked.get() {
                                "-webkit-text-security:disc;text-security:disc"
                            } else {
                                ""
                            }
                            prop:value=move || pasted.get()
                            on:input=move |event| pasted.set(input_value(&event))
                        ></textarea>
                        <div class="phrase-meta">
                            <span class="wordcount">
                                <b>{move || word_count().to_string()}</b>" "{t("of 12 words")}
                            </span>
                            {move || {
                                if word_count() == 0 {
                                    ().into_any()
                                } else if derived().is_some() {
                                    view! {
                                        <span class="badge live">
                                            <Icon name="i-check" size=9 />{t("Checksum valid")}
                                        </span>
                                    }.into_any()
                                } else if word_count() < 12 {
                                    view! { <span class="badge">{t("Keep going")}</span> }.into_any()
                                } else {
                                    view! {
                                        <span class="badge stop">
                                            <Icon name="i-x" size=9 />{t("Checksum does not match")}
                                        </span>
                                    }.into_any()
                                }
                            }}
                            <span class="spacer"></span>
                            <button
                                class="btn btn-ghost btn-sm"
                                on:click=move |_| masked.update(|on| *on = !*on)
                            >
                                <Icon name="i-eyeoff" size=12 />
                                {move || if masked.get() { "Show" } else { "Hide" }}
                            </button>
                        </div>
                    </div>

                    <div class="derived" style="margin-top:var(--sp-16)">
                        <Icon name="i-key" size=14 style="color:var(--signal)" />
                        <span class="k">"This phrase controls"</span>
                        <span class="a">
                            {move || derived().map_or_else(
                                || "\u{2014}".to_string(),
                                |wallet| wallet.address().to_string(),
                            )}
                        </span>
                        <span class="badge" style="margin-left:auto">
                            {move || format!("Index {}", index.get())}
                        </span>
                    </div>
                    <p class="t-meta" style="margin-top:var(--sp-8);line-height:var(--lh-snug)">
                        "Read the address before you sign it. If your wallet shows a different \
                         one, the phrase is right and the account index is not \u{2014} the same \
                         phrase holds a different address at every index of "
                        <code>"m/44'/60'/0'/0/i"</code>"."
                    </p>
                    <div style="display:flex;align-items:flex-end;gap:var(--sp-12);margin-top:var(--sp-12)">
                        <div class="field" style="width:132px">
                            <label for="idx">"Account index"</label>
                            <div class="select">
                                <select
                                    id="idx"
                                    on:change=move |event| {
                                        index.set(select_value(&event).parse().unwrap_or(0));
                                    }
                                >
                                    {(0..4_u32).map(|i| view! {
                                        <option value=i.to_string()>{i.to_string()}</option>
                                    }).collect_view()}
                                </select>
                            </div>
                        </div>
                        <p class="t-meta" style="padding-bottom:10px">
                            "Leave this at zero unless you know you used another."
                        </p>
                    </div>
                    {move || failure.get().map(|message| view! {
                        <div style="margin-top:var(--sp-12)">
                            <Banner tone="danger" title="That did not go through" body=message />
                        </div>
                    })}
                </Modal>
            }
            .into_any(),

            // ------------------------------------------------------------ sign
            Step::Sign(challenge) => {
                let address = wallet
                    .with_value(|wallet| wallet
                        .as_ref()
                        .map_or_else(String::new, |wallet| wallet.address().to_string()));
                let expires = crate::format::parse_rfc3339(&challenge.expires_at)
                    .map_or_else(|| challenge.expires_at.clone(), crate::format::clock);
                let message = challenge.message.clone();
                let for_click = challenge.clone();
                view! {
                    <Modal
                        title=format!("Sign in as {}", crate::format::hash(&address))
                        icon="i-check"
                        width="w-mid"
                        foot=move || {
                            let challenge = (*for_click).clone();
                            view! {
                                <>
                                    <button class="btn btn-ghost" on:click=move |_| {
                                        step.set(Step::Closed);
                                    }>{t("Back")}</button>
                                    <button
                                        class="btn btn-primary"
                                        disabled=move || busy.get()
                                            || (needs_handle.get()
                                                && handle_problem(&handle.get()).is_some())
                                        data-loading=move || busy.get().then_some("")
                                        on:click=move |_| {
                                            let claim = (!handle.get_untracked().is_empty())
                                                .then(|| handle.get_untracked());
                                            submit_sign_in(challenge.clone(), claim);
                                        }
                                    >
                                        {move || if spent.get() {
                                            t("Sign again and continue")
                                        } else {
                                            t("Sign and continue")
                                        }}
                                    </button>
                                </>
                            }
                            .into_any()
                        }
                    >
                        <p style="font-size:var(--fs-sm)">
                            "Signing proves you hold the key for this address. It costs nothing, \
                             moves nothing and authorises nothing except this login."
                        </p>
                        // Shown verbatim, and signed verbatim: the client must
                        // not rebuild this text, so the gateway can change its
                        // wording without breaking every client.
                        <div class="msgbox" style="margin:var(--sp-16) 0">{message}</div>
                        <div class="ticket" style="padding:0;gap:var(--sp-6)">
                            <div class="row">
                                <span class="k">{t("Method")}</span>
                                <span class="v">"EIP-191 personal_sign"</span>
                            </div>
                            <div class="row">
                                <span class="k">{t("Nonce expires")}</span>
                                <span class="v">
                                    {move || if spent.get() {
                                        // A used nonce has no useful expiry, and
                                        // printing a future time beside a failure
                                        // is the screen contradicting itself.
                                        "used \u{2014} the next attempt asks for a new one"
                                            .to_string()
                                    } else {
                                        expires.clone()
                                    }}
                                </span>
                            </div>
                            <div class="row">
                                <span class="k">{t("Session length")}</span>
                                <span class="v">{t("24 hours")}</span>
                            </div>
                        </div>
                        // The handle request is a state of the flow, not a
                        // failure of it: brass, above a field that keeps what
                        // was typed in it however many attempts it takes.
                        <Show when=move || needs_handle.get()>
                            <div style="margin-top:var(--sp-16)">
                                <Banner
                                    tone="warn"
                                    title="This address has not signed in here before"
                                    body="Pick the name you will trade under. It is what the \
                                          leaderboard, the tape and other players see; the address \
                                          itself is never shown to anybody else."
                                />
                            </div>
                            <div style="margin-top:var(--sp-12)">
                                <HandleField handle=handle />
                            </div>
                        </Show>
                        {move || failure.get().map(|message| {
                            let expired = message == EXPIRED_WINDOW;
                            view! {
                                <div style="margin-top:var(--sp-16)">
                                    <Banner
                                        tone={if expired { "warn" } else { "danger" }}
                                        title={if expired {
                                            "That sign-in window has closed"
                                        } else {
                                            "The gateway refused that sign-in"
                                        }}
                                        body=message
                                    />
                                </div>
                            }
                        })}
                    </Modal>
                }
                .into_any()
            }
        }}
    }
}

/// The three-step progress indicator. Three steps, never more.
#[component]
fn JoinSteps(
    /// Which step is current, counting from one.
    current: u32,
) -> impl IntoView {
    let labels = ["Wallet created", "Write it down", "Confirm"];
    view! {
        <div class="joinsteps" aria-label="Progress">
            {labels
                .into_iter()
                .enumerate()
                .map(|(index, label)| {
                    let number = index as u32 + 1;
                    let done = number < current;
                    let is_current = number == current;
                    view! {
                        <>
                            <span
                                class="s"
                                class:done=done
                                attr:aria-current=is_current.then_some("step")
                            >
                                <span class="n">
                                    {if done {
                                        view! { <Icon name="i-check" size=8 /> }.into_any()
                                    } else {
                                        view! { {number.to_string()} }.into_any()
                                    }}
                                </span>
                                {label}
                            </span>
                            {(number < 3).then(|| view! { <span class="bar"></span> })}
                        </>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// The handle field, validated as you type against the contract's own rule.
///
/// Checking here means `409 HANDLE_TAKEN` is the only failure the server ever
/// has to report.
#[component]
fn HandleField(
    /// The handle being claimed.
    handle: RwSignal<String>,
) -> impl IntoView {
    let problem = move || {
        let typed = handle.get();
        if typed.is_empty() {
            None
        } else {
            handle_problem(&typed)
        }
    };
    view! {
        <div class="field" style="max-width:320px">
            <label for="c-handle">{t("Pick a handle")}</label>
            <div class="handlefield">
                <span class="at">"@"</span>
                <input
                    class="input"
                    id="c-handle"
                    autocomplete="off"
                    autocapitalize="off"
                    spellcheck="false"
                    aria-invalid=move || problem().is_some().then_some("true")
                    prop:value=move || handle.get()
                    on:input=move |event| handle.set(input_value(&event).to_lowercase())
                />
                <Show when=move || !handle.get().is_empty() && problem().is_none()>
                    <span class="ok"><Icon name="i-check" size=12 />"looks good"</span>
                </Show>
            </div>
            {move || match problem() {
                Some(message) => view! {
                    <span class="err"><Icon name="i-warn" size=11 />{message}</span>
                }.into_any(),
                None => view! {
                    <span class="hint">
                        "3\u{2013}20 characters, lower case letters, digits and underscores. \
                         The handle is the only thing on this account that can be changed \
                         later; the address cannot."
                    </span>
                }.into_any(),
            }}
        </div>
    }
}

/// Reads an input's or textarea's value from an event.
pub fn input_value(event: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    event
        .target()
        .and_then(|target| {
            target
                .dyn_ref::<web_sys::HtmlInputElement>()
                .map(web_sys::HtmlInputElement::value)
                .or_else(|| {
                    target
                        .dyn_ref::<web_sys::HtmlTextAreaElement>()
                        .map(web_sys::HtmlTextAreaElement::value)
                })
        })
        .unwrap_or_default()
}

/// Reads a `<select>`'s value from an event.
pub fn select_value(event: &leptos::ev::Event) -> String {
    use wasm_bindgen::JsCast;
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .map(|select| select.value())
        .unwrap_or_default()
}

/// Puts text on the clipboard, ignoring a browser that refuses.
pub fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}
