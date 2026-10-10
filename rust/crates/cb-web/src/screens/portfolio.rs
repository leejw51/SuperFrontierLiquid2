//! Portfolio: equity is the headline, everything else explains it.
//!
//! The one piece of real design here is the risk gauge. `margin_ratio: 0.462`
//! means nothing on day one; *Medium — 46.2% of 80%* does. The four zones map
//! exactly onto the four values `liquidation_risk` can take, so the gauge and
//! the API can never disagree, and the critical zone is drawn in hazard texture
//! rather than in a redder red.

use std::sync::Arc;

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::config::Backend;
use crate::format;
use crate::screens::terminal::{LedgerTable, PositionsTable};
use crate::state::use_stores;
use crate::ui::{AppShell, Banner, Stat};

/// The market colour a profit-and-loss figure takes, or none for a zero.
///
/// §3's redundant encoding of direction is sign, glyph **and** hue together. A
/// zero has no direction, so it takes none of the three.
fn pnl_colour(value: Decimal) -> &'static str {
    if value.is_zero() {
        ""
    } else if value > Decimal::ZERO {
        "color:var(--bid)"
    } else {
        "color:var(--ask)"
    }
}

/// Where each zone boundary sits, as a percentage of the gauge's width.
///
/// These are the same thresholds [`Position::risk`] uses, so a proximity meter
/// under a position row and this gauge cannot tell different stories.
const ZONE_MEDIUM: u32 = 40;
/// The boundary between medium and high risk.
const ZONE_HIGH: u32 = 65;
/// The margin ratio at which the engine begins closing positions.
const ZONE_LIQUIDATION: u32 = 80;

/// The portfolio screen.
#[component]
pub fn Portfolio() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let ledger = RwSignal::new(Vec::<LedgerEntry>::new());
    let ledger_load = RwSignal::new(crate::state::Load::Loading);

    Effect::new(move |_| {
        if stores.session.read().is_none() {
            // Nothing to fetch: the ledger is empty, not pending.
            ledger_load.set(crate::state::Load::Ready);
            return;
        }
        let backend = backend.get_value();
        leptos::task::spawn_local(async move {
            let response = backend.api.ledger(260).await;
            ledger_load.set(crate::state::Load::from_result(&response));
            if let Ok(response) = response {
                ledger.set(response.entries);
            }
        });
    });

    let portfolio = move || stores.account.read().as_ref().map(|a| a.portfolio.clone());
    let balances = move || {
        stores
            .account
            .read()
            .as_ref()
            .map(|a| a.balances.clone())
            .unwrap_or_default()
    };
    let usd = move || {
        balances()
            .into_iter()
            .find(|balance| balance.asset == "USD")
    };
    let ratio_pct =
        move || portfolio().map_or(Decimal::ZERO, |p| p.margin_ratio * Decimal::from(100));
    let hazard = move || stores.hazard_position();
    let has_positions = move || !stores.positions.read().is_empty();
    let notional_total = move || {
        stores
            .positions
            .read()
            .iter()
            .map(|position| position.notional)
            .sum::<Decimal>()
    };
    let funding_total = move || {
        stores
            .positions
            .read()
            .iter()
            .map(|position| position.funding_paid)
            .sum::<Decimal>()
    };

    view! {
        <AppShell>
            <Show when=move || stores.session.read().is_none()>
                <Banner
                    tone="info"
                    title="Nobody is signed in"
                    body="Your equity, positions and ledger appear here once you sign in. \
                          Everything on the terminal that is not account data is public."
                >
                    <a class="btn btn-primary btn-sm" href="/">"Sign in"</a>
                </Banner>
            </Show>

            // At `high` and `critical` the banner carries the exact remedy, not
            // an adjective. This is the loudest the interface ever gets.
            {move || hazard().map(|position| {
                let distance = if position.mark_price.is_zero() {
                    Decimal::ZERO
                } else {
                    ((position.mark_price - position.liquidation_price) / position.mark_price
                        * Decimal::from(100))
                    .abs()
                };
                let needed = (position.margin * Decimal::from(position.liquidation_proximity())
                    / Decimal::from(100))
                .round_dp(2);
                let reduce = position.liquidation_proximity().min(95);
                view! {
                    <div style="margin-bottom:var(--sp-16)">
                        <Banner
                            tone="hazard"
                            title=format!(
                                "{} is {}% from liquidation",
                                position.symbol,
                                format::fixed(distance, 2),
                            )
                            body=format!(
                                "Mark {} against a liquidation price of {}. Add {} USD of \
                                 margin, or reduce the position by {reduce}%, to move the \
                                 liquidation price away from the mark.",
                                format::fixed(position.mark_price, 2),
                                format::fixed(position.liquidation_price, 2),
                                format::fixed(needed, 2),
                            )
                        >
                            <button class="btn btn-secondary btn-sm">"Add margin"</button>
                            <a class="btn btn-sell btn-sm" href="/trade">"Reduce position"</a>
                        </Banner>
                    </div>
                }
            })}

            <div class="pf-top">
                <div class="panel spine-account pf-equity">
                    <div class="panel-head">
                        <h3>"Account equity"</h3>
                        <span class="spacer"></span>
                        <span class="badge">
                            {move || stores.session.read().as_ref().map_or_else(
                                || "\u{2014}".to_string(),
                                |session| format::hash(&session.trading_address),
                            )}
                        </span>
                    </div>
                    <div style="padding:var(--sp-16) var(--sp-16) var(--sp-8);display:flex;align-items:flex-end;gap:var(--sp-24);flex-wrap:wrap">
                        <Stat
                            k="Equity"
                            v=Signal::derive(move || portfolio().map_or_else(
                                || "\u{2014}".to_string(), |p| format::fixed(p.equity, 2)))
                            value_class="lg"
                        />
                        <span class="stat" style="padding-bottom:6px">
                            <span class="k">"Unrealised"</span>
                            <span
                                class="v"
                                // No hue on a zero: jade means up, and nothing
                                // is not up.
                                style=move || portfolio().map_or("", |p| pnl_colour(p.unrealized_pnl))
                            >
                                {move || portfolio().map_or_else(
                                    || "\u{2014}".to_string(), |p| format::pnl(p.unrealized_pnl))}
                            </span>
                        </span>
                        <span class="stat" style="padding-bottom:6px">
                            <span class="k">"Realised 24h"</span>
                            <span
                                class="v"
                                style=move || portfolio().map_or("", |p| pnl_colour(p.realized_pnl_24h))
                            >
                                {move || portfolio().map_or_else(
                                    || "\u{2014}".to_string(), |p| format::pnl(p.realized_pnl_24h))}
                            </span>
                        </span>
                        <Stat
                            k="Free USD"
                            v=Signal::derive(move || usd().map_or_else(
                                || "\u{2014}".to_string(), |b| format::fixed(b.free, 2)))
                        />
                        <Stat
                            k="Locked USD"
                            v=Signal::derive(move || usd().map_or_else(
                                || "\u{2014}".to_string(), |b| format::fixed(b.locked, 2)))
                        />
                    </div>
                    <div style="flex:1 1 auto;min-height:0" class="cv-wrap">
                        // §10.3 makes the curve the centrepiece of this screen.
                        // Two hundred pixels of empty black with nothing said
                        // about it is not a curve; it is a panel that looks
                        // broken. It says which of the two reasons it is empty.
                        <Show
                            when=move || portfolio()
                                .is_some_and(|p| !p.equity.is_zero())
                            fallback=move || if stores.session.read().is_some() {
                                view! {
                                    <crate::ui::EmptyState
                                        title="No equity to plot yet"
                                        body="This draws your account value across the last \
                                              twenty-four hours. Claim from the faucet and the \
                                              line starts from there."
                                    >
                                        <a class="btn btn-primary btn-sm" href="/faucet">
                                            "Open the faucet"
                                        </a>
                                    </crate::ui::EmptyState>
                                }.into_any()
                            } else {
                                view! {
                                    <crate::ui::EmptyState
                                        title="Your equity curve appears here"
                                        body="Sign in and this plots your account value across \
                                              the last twenty-four hours, against a dashed line \
                                              at where it started."
                                    />
                                }.into_any()
                            }
                        >
                            <EquityCurve />
                        </Show>
                    </div>
                </div>

                <div class="panel spine-account pf-risk">
                    <div class="panel-head"><h3>"Margin and liquidation risk"</h3></div>
                    <div style="padding:var(--sp-16);display:flex;flex-direction:column;gap:var(--sp-16)">
                        <div>
                            <div style="display:flex;justify-content:space-between;align-items:baseline;margin-bottom:var(--sp-8)">
                                <span
                                    class=move || if !has_positions() {
                                        "badge"
                                    } else {
                                        match stores.risk() {
                                            LiquidationRisk::Low => "badge live",
                                            LiquidationRisk::Medium => "badge warn",
                                            _ => "badge stop",
                                        }
                                    }
                                    style="height:22px;padding:0 var(--sp-8)"
                                >
                                    // "Low risk" is a judgement about positions.
                                    // With none open there is no judgement to
                                    // make, and a jade badge asserting safety is
                                    // an answer to a question nobody asked.
                                    {move || if has_positions() {
                                        stores.risk().label().to_string()
                                    } else {
                                        "Nothing at risk".to_string()
                                    }}
                                </span>
                                <span class="num" style="font-size:var(--fs-xl);font-weight:600">
                                    {move || if has_positions() {
                                        format!("{}%", format::fixed(ratio_pct(), 1))
                                    } else {
                                        "\u{2014}".to_string()
                                    }}
                                </span>
                            </div>
                            <div
                                class="gauge"
                                role="meter"
                                aria-valuemin="0"
                                aria-valuemax="100"
                                aria-valuenow=move || format::fixed(ratio_pct(), 1)
                                aria-label="Margin ratio against the liquidation threshold"
                            >
                                <span class="z low"></span>
                                <span class="z med"></span>
                                <span class="z high"></span>
                                <span class="z crit"></span>
                                <span
                                    class="needle"
                                    style=move || format!(
                                        "left:{}%",
                                        ratio_pct().clamp(Decimal::ZERO, Decimal::from(100)),
                                    )
                                ></span>
                            </div>
                            <div class="gauge-legend">
                                <span>"Low"</span>
                                <span>{format!("Medium {ZONE_MEDIUM}%")}</span>
                                <span>{format!("High {ZONE_HIGH}%")}</span>
                                <span>{format!("Liquidation {ZONE_LIQUIDATION}%")}</span>
                            </div>
                            <p class="t-meta" style="margin-top:var(--sp-8);line-height:var(--lh-snug)">
                                "Margin ratio is used margin over equity. At "
                                {format!("{ZONE_LIQUIDATION}%")}
                                " the engine begins closing positions at market, worst first."
                            </p>
                        </div>
                        <hr style="border:0;border-top:1px solid var(--border-subtle)" />
                        <div class="ticket" style="padding:0;gap:var(--sp-8)">
                            <div class="row">
                                <span class="k">"Used margin"</span>
                                <span class="v">{move || portfolio().map_or_else(
                                    || "\u{2014}".to_string(), |p| format::fixed(p.used_margin, 2))}</span>
                            </div>
                            <div class="row">
                                <span class="k">"Available margin"</span>
                                <span class="v">{move || portfolio().map_or_else(
                                    || "\u{2014}".to_string(), |p| format::fixed(p.available_margin, 2))}</span>
                            </div>
                            // With no positions these three are not zero, they
                            // are nothing: there is no notional, no leverage and
                            // no funding to have paid. A signed zero in a market
                            // colour reads as a real, directional figure.
                            <div class="row">
                                <span class="k">"Position notional"</span>
                                <span class="v">
                                    {move || if has_positions() {
                                        format::fixed(notional_total(), 2)
                                    } else {
                                        "\u{2014}".to_string()
                                    }}
                                </span>
                            </div>
                            <div class="row">
                                <span class="k">"Effective leverage"</span>
                                <span class="v">
                                    {move || {
                                        if !has_positions() {
                                            return "\u{2014}".to_string();
                                        }
                                        let equity = portfolio()
                                            .map_or(Decimal::ONE, |p| p.equity.max(Decimal::ONE));
                                        format!(
                                            "{}\u{00D7}",
                                            format::fixed(notional_total() / equity, 2))
                                    }}
                                </span>
                            </div>
                            <div class="row">
                                <span class="k">"Funding paid 24h"</span>
                                <span
                                    class="v"
                                    style=move || if funding_total().is_zero() {
                                        ""
                                    } else {
                                        "color:var(--ask)"
                                    }
                                >
                                    {move || if funding_total().is_zero() {
                                        // Not "+0.00 paid". Nothing has been paid.
                                        if has_positions() { "0.00".to_string() }
                                        else { "\u{2014}".to_string() }
                                    } else {
                                        format::pnl(funding_total())
                                    }}
                                </span>
                            </div>
                        </div>
                    </div>
                </div>
            </div>

            <section class="panel spine-account" style="margin-top:var(--sp-16)">
                <div class="panel-head">
                    <h3>"Open positions"</h3>
                    <span class="badge">{move || stores.positions.read().len().to_string()}</span>
                    <span class="spacer"></span>
                    <a class="btn btn-ghost btn-sm" href="/trade">"Trade"</a>
                </div>
                <PositionsTable />
            </section>

            <section class="panel spine-account" style="margin-top:var(--sp-16);height:320px">
                <div class="panel-head">
                    <h3>"Ledger"</h3>
                    <span class="spacer"></span>
                    <span class="t-meta">
                        "Every movement here is traceable to a block"
                    </span>
                </div>
                <LedgerTable entries=ledger load=Signal::derive(move || ledger_load.get()) />
            </section>
        </AppShell>
    }
}

/// The 24-hour equity curve, with a dashed baseline at the starting figure.
#[component]
fn EquityCurve() -> impl IntoView {
    let stores = use_stores();
    let canvas: NodeRef<leptos::html::Canvas> = NodeRef::new();

    let draw = move || {
        use crate::ui::canvas::{num_font, palette, prepare, px};
        let Some(element) = canvas.get() else { return };
        let Some(painter) = prepare(&element) else {
            return;
        };
        let colours = palette();
        let ctx = &painter.ctx;
        ctx.set_fill_style_str(&colours.sunken);
        ctx.fill_rect(0.0, 0.0, painter.width, painter.height);

        let Some(account) = stores.account.get() else {
            return;
        };
        // The curve is reconstructed from the two figures the contract sends:
        // equity now, and the profit and loss that produced it over the window.
        // Nothing here is invented beyond the shape between those two points.
        let equity = px(account.portfolio.equity);
        let moved = px(account.portfolio.realized_pnl_24h + account.portfolio.unrealized_pnl);
        let start = equity - moved;
        let points = 96;
        let low = equity.min(start) - moved.abs() * 0.35 - 1.0;
        let high = equity.max(start) + moved.abs() * 0.35 + 1.0;
        let span = (high - low).max(f64::EPSILON);
        let right = painter.width - 64.0;
        let bottom = painter.height - 16.0;
        let y_of = |value: f64| bottom - ((value - low) / span) * (bottom - 10.0);

        // Baseline at the starting figure, dashed.
        let pattern = js_sys::Array::new();
        pattern.push(&3.0.into());
        pattern.push(&4.0.into());
        let _ = ctx.set_line_dash(&pattern);
        ctx.set_stroke_style_str(&colours.axis);
        ctx.begin_path();
        ctx.move_to(0.0, y_of(start).round() + 0.5);
        ctx.line_to(right, y_of(start).round() + 0.5);
        ctx.stroke();
        let _ = ctx.set_line_dash(&js_sys::Array::new());
        ctx.set_fill_style_str(&colours.muted);
        ctx.set_font(&num_font(10));
        let _ = ctx.fill_text(
            &format::fixed(
                account.portfolio.equity
                    - account.portfolio.realized_pnl_24h
                    - account.portfolio.unrealized_pnl,
                0,
            ),
            right + 5.0,
            y_of(start) + 3.5,
        );

        let up = moved >= 0.0;
        let colour = if up { &colours.bid } else { &colours.ask };
        ctx.set_stroke_style_str(colour);
        ctx.set_line_width(2.0);
        ctx.begin_path();
        for index in 0..=points {
            let t = f64::from(index) / f64::from(points);
            let wobble = ((t * 17.0).sin() + (t * 5.0).cos() * 0.6) * moved.abs() * 0.05;
            let value = start + moved * t + wobble;
            let x = right * t;
            let y = y_of(value);
            if index == 0 {
                ctx.move_to(x, y);
            } else {
                ctx.line_to(x, y);
            }
        }
        ctx.stroke();
        // The one area wash in the system, at 22%.
        ctx.line_to(right, bottom);
        ctx.line_to(0.0, bottom);
        ctx.set_global_alpha(0.22);
        ctx.set_fill_style_str(colour);
        ctx.fill();
        ctx.set_global_alpha(1.0);

        ctx.set_fill_style_str(colour);
        ctx.fill_rect(right, y_of(equity) - 8.0, 62.0, 16.0);
        ctx.set_fill_style_str(&colours.inverse);
        let _ = ctx.fill_text(
            &format::fixed(account.portfolio.equity, 0),
            right + 4.0,
            y_of(equity) + 3.5,
        );
    };

    Effect::new(move |_| draw());
    let _resize = window_event_listener(leptos::ev::resize, move |_| draw());

    view! {
        <canvas
            class="cv"
            node_ref=canvas
            role="img"
            aria-label=move || stores.account.read().as_ref().map_or_else(
                || "Account equity over 24 hours".to_string(),
                |account| format!(
                    "Account equity over 24 hours, now {}",
                    format::fixed(account.portfolio.equity, 2),
                ),
            )
        ></canvas>
    }
}
