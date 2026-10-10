//! The candlestick chart and the depth curve, drawn on `<canvas>` in Rust.
//!
//! Kept in Rust rather than handed to a charting library for two reasons. The
//! first is the palette: every colour here is read from the design tokens at
//! draw time, so the chart follows the theme switch and obeys the rule that no
//! component hard-codes a market colour. A third-party chart would need its own
//! theme object kept in sync by hand, which is a second source of truth for the
//! one thing the design system is strictest about.
//!
//! The second is the same reason the rest of this crate exists: prices are
//! [`Decimal`]. They are converted to `f64` exactly once, in
//! [`crate::ui::canvas::px`], to become pixel coordinates — never to be compared,
//! stored or displayed. Every figure drawn as text here is formatted from the
//! `Decimal` by [`crate::format`].
//!
//! Down candles are **hollow** and up candles are filled, which is the shape
//! redundancy the design system requires so that direction survives without
//! colour.

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::{Candle, Market};
use crate::format;
use crate::state::use_stores;
use crate::ui::canvas::{num_font, palette, prepare, px, Painter, Palette};
use crate::ws::book::BookRow;

/// Space reserved on the right for the price axis, in CSS pixels.
const AXIS_W: f64 = 62.0;

/// Space reserved at the bottom for the time axis.
const AXIS_H: f64 = 18.0;

/// Fraction of the plot given over to the volume histogram.
const VOLUME_SHARE: f64 = 0.18;

/// A linear scale between a value range and a pixel range.
#[derive(Debug, Clone, Copy)]
struct Scale {
    low: f64,
    span: f64,
    top: f64,
    height: f64,
}

impl Scale {
    fn new(low: f64, high: f64, top: f64, height: f64) -> Self {
        let span = (high - low).max(f64::EPSILON);
        Self {
            low,
            span,
            top,
            height,
        }
    }

    /// The y coordinate for a value.
    fn y(&self, value: f64) -> f64 {
        self.top + self.height - ((value - self.low) / self.span) * self.height
    }

    /// The value at a y coordinate, for the crosshair's price label.
    fn value(&self, y: f64) -> f64 {
        self.low + ((self.top + self.height - y) / self.height) * self.span
    }
}

/// Draws a dashed horizontal line with a label plate at the right-hand edge.
#[allow(clippy::too_many_arguments)]
fn marker_line(
    painter: &Painter,
    scale: Scale,
    plot_right: f64,
    value: f64,
    text: &str,
    colour: &str,
    plate_text: &str,
    dashes: &[f64],
) {
    let ctx = &painter.ctx;
    let y = scale.y(value);
    let pattern = js_sys::Array::new();
    for dash in dashes {
        pattern.push(&(*dash).into());
    }
    let _ = ctx.set_line_dash(&pattern);
    ctx.set_stroke_style_str(colour);
    ctx.set_line_width(1.0);
    ctx.begin_path();
    ctx.move_to(0.0, y.round() + 0.5);
    ctx.line_to(plot_right, y.round() + 0.5);
    ctx.stroke();
    let _ = ctx.set_line_dash(&js_sys::Array::new());

    ctx.set_fill_style_str(colour);
    ctx.fill_rect(plot_right, y - 8.0, AXIS_W, 16.0);
    ctx.set_fill_style_str(plate_text);
    ctx.set_font(&num_font(10));
    let _ = ctx.fill_text(text, plot_right + 4.0, y + 3.5);
}

/// The candlestick chart with its volume histogram, mark and liquidation lines.
#[component]
pub fn CandleChart(
    /// Oldest first.
    #[prop(into)]
    candles: Signal<Vec<Candle>>,
    /// The market, for its price and size decimals.
    #[prop(into)]
    market: Signal<Option<Market>>,
    /// The mark price, drawn as a signal-blue dashed line.
    #[prop(into)]
    mark: Signal<Option<Decimal>>,
    /// The liquidation price of an open position in this market, if any.
    #[prop(into)]
    liquidation: Signal<Option<Decimal>>,
    /// Which candle the pointer is over, published for the toolbar's readout.
    hovered: RwSignal<Option<usize>>,
) -> impl IntoView {
    let canvas: NodeRef<leptos::html::Canvas> = NodeRef::new();
    let (pointer, set_pointer) = signal(None::<(f64, f64)>);
    let stores = use_stores();

    let draw = move || {
        let Some(element) = canvas.get() else { return };
        let Some(painter) = prepare(&element) else {
            return;
        };
        let series = candles.get();
        let colours = palette();
        let price_dp = market.get().map_or(2, |market| market.price_decimals);

        let ctx = &painter.ctx;
        ctx.set_fill_style_str(&colours.sunken);
        ctx.fill_rect(0.0, 0.0, painter.width, painter.height);
        if series.is_empty() {
            ctx.set_fill_style_str(&colours.muted);
            ctx.set_font(&num_font(11));
            let _ = ctx.fill_text("Waiting for candles", 12.0, 22.0);
            return;
        }

        let plot_right = (painter.width - AXIS_W).max(20.0);
        let plot_bottom = (painter.height - AXIS_H).max(20.0);
        let volume_top = plot_bottom - plot_bottom * VOLUME_SHARE;

        // --- ranges. Decimal to f64 happens here and only here.
        let mut low = f64::MAX;
        let mut high = f64::MIN;
        let mut max_volume: f64 = 0.0;
        for candle in &series {
            low = low.min(px(candle.l));
            high = high.max(px(candle.h));
            max_volume = max_volume.max(px(candle.v));
        }
        // The mark always belongs in the range: it is inside the market. A
        // liquidation price can be a long way off, and letting it set the range
        // would squash every candle into a band a few pixels tall — so it only
        // widens the range when it is close enough to be worth seeing to scale,
        // and otherwise pins to the edge with its label still drawn.
        if let Some(mark) = mark.get() {
            low = low.min(px(mark));
            high = high.max(px(mark));
        }
        let candle_span = (high - low).max(f64::EPSILON);
        let mut liquidation_in_range = false;
        if let Some(liquidation) = liquidation.get() {
            let value = px(liquidation);
            if value > low - candle_span && value < high + candle_span {
                low = low.min(value);
                high = high.max(value);
                liquidation_in_range = true;
            }
        }
        let pad = (high - low) * 0.06;
        let scale = Scale::new(low - pad, high + pad, 6.0, volume_top - 12.0);

        // --- grid and price axis
        ctx.set_font(&num_font(10));
        ctx.set_line_width(1.0);
        for step in 1..5 {
            let value = scale.low + scale.span * f64::from(step) / 5.0;
            let y = scale.y(value).round() + 0.5;
            ctx.set_stroke_style_str(&colours.grid);
            ctx.begin_path();
            ctx.move_to(0.0, y);
            ctx.line_to(plot_right, y);
            ctx.stroke();
            ctx.set_fill_style_str(&colours.muted);
            let label = format::fixed(
                Decimal::from_f64_retain(value).unwrap_or_default(),
                price_dp.min(2),
            );
            let _ = ctx.fill_text(&label, plot_right + 5.0, y + 3.5);
        }
        ctx.set_stroke_style_str(&colours.axis);
        ctx.begin_path();
        ctx.move_to(0.0, volume_top.round() + 0.5);
        ctx.line_to(plot_right, volume_top.round() + 0.5);
        ctx.stroke();

        // --- candles
        let slot = plot_right / series.len() as f64;
        let body = (slot * 0.62).clamp(1.0, 14.0);
        for (index, candle) in series.iter().enumerate() {
            let centre = slot * (index as f64 + 0.5);
            let rising = candle.c >= candle.o;
            let colour = if rising { &colours.bid } else { &colours.ask };
            let (open_y, close_y) = (scale.y(px(candle.o)), scale.y(px(candle.c)));

            ctx.set_stroke_style_str(colour);
            ctx.set_line_width(1.0);
            ctx.begin_path();
            ctx.move_to(centre.round() + 0.5, scale.y(px(candle.h)));
            ctx.line_to(centre.round() + 0.5, scale.y(px(candle.l)));
            ctx.stroke();

            let top = open_y.min(close_y);
            let height = (open_y - close_y).abs().max(1.0);
            if rising {
                // Up candles are filled; down candles are hollow. Shape carries
                // the direction so colour is never doing the work alone.
                ctx.set_fill_style_str(colour);
                ctx.fill_rect(centre - body / 2.0, top, body, height);
            } else {
                ctx.set_fill_style_str(&colours.sunken);
                ctx.fill_rect(centre - body / 2.0, top, body, height);
                ctx.stroke_rect(centre - body / 2.0, top, body, height);
            }

            if max_volume > 0.0 {
                let bar = (px(candle.v) / max_volume) * (plot_bottom - volume_top - 2.0);
                ctx.set_fill_style_str(&colours.volume);
                ctx.fill_rect(centre - body / 2.0, plot_bottom - bar, body, bar);
            }
        }

        // --- mark and liquidation
        if let Some(mark) = mark.get() {
            marker_line(
                &painter,
                scale,
                plot_right,
                px(mark),
                &format::price(mark, price_dp),
                &colours.mark,
                &colours.inverse,
                &[4.0, 4.0],
            );
        }
        if let Some(liquidation) = liquidation.get() {
            let y = if liquidation_in_range {
                scale.y(px(liquidation))
            } else if px(liquidation) < scale.low {
                volume_top - 14.0
            } else {
                8.0
            };
            let pattern = js_sys::Array::new();
            pattern.push(&2.0.into());
            pattern.push(&5.0.into());
            let _ = ctx.set_line_dash(&pattern);
            ctx.set_stroke_style_str(&colours.liq);
            ctx.begin_path();
            ctx.move_to(0.0, y.round() + 0.5);
            ctx.line_to(plot_right, y.round() + 0.5);
            ctx.stroke();
            let _ = ctx.set_line_dash(&js_sys::Array::new());
            ctx.set_fill_style_str(&colours.liq);
            ctx.set_font("10px ui-sans-serif, system-ui, sans-serif");
            let label = if liquidation_in_range {
                format!("Liquidation {}", format::price(liquidation, price_dp))
            } else {
                // Said out loud rather than drawn to scale, because a line
                // pinned to the edge would otherwise read as a nearby price.
                format!(
                    "Liquidation {} \u{2014} off the chart",
                    format::price(liquidation, price_dp),
                )
            };
            let _ = ctx.fill_text(&label, 8.0, y - 5.0);
        }

        // --- time axis
        ctx.set_fill_style_str(&colours.muted);
        ctx.set_font(&num_font(10));
        let ticks = (series.len() / 6).max(1);
        for (index, candle) in series.iter().enumerate().step_by(ticks) {
            let centre = slot * (index as f64 + 0.5);
            if centre > plot_right - 20.0 {
                break;
            }
            if let Some(epoch) = format::parse_rfc3339(&candle.t) {
                let _ = ctx.fill_text(
                    &format::clock_hm(epoch),
                    centre - 14.0,
                    painter.height - 4.0,
                );
            }
        }

        // --- crosshair
        if let Some((x, y)) = pointer.get() {
            if x < plot_right && y < volume_top {
                ctx.set_stroke_style_str(&colours.cross);
                let pattern = js_sys::Array::new();
                pattern.push(&3.0.into());
                pattern.push(&3.0.into());
                let _ = ctx.set_line_dash(&pattern);
                ctx.begin_path();
                ctx.move_to(x.round() + 0.5, 0.0);
                ctx.line_to(x.round() + 0.5, plot_bottom);
                ctx.move_to(0.0, y.round() + 0.5);
                ctx.line_to(plot_right, y.round() + 0.5);
                ctx.stroke();
                let _ = ctx.set_line_dash(&js_sys::Array::new());
                ctx.set_fill_style_str(&colours.muted);
                ctx.fill_rect(plot_right, y - 8.0, AXIS_W, 16.0);
                ctx.set_fill_style_str(&colours.inverse);
                ctx.set_font(&num_font(10));
                let value = Decimal::from_f64_retain(scale.value(y)).unwrap_or_default();
                let _ = ctx.fill_text(&format::price(value, price_dp), plot_right + 4.0, y + 3.5);
            }
        }

        // Reading the tick keeps the chart repainting as blocks arrive, which is
        // what makes the last candle grow rather than jump when the user looks
        // away and back.
        let _ = stores.block_tick.get();
    };

    Effect::new(move |_| draw());

    // A window resize changes the canvas's CSS size but not its back buffer, so
    // the chart has to be told. Container queries resize it without a window
    // resize as well, which the density and theme effects above pick up.
    let _resize = window_event_listener(leptos::ev::resize, move |_| draw());

    view! {
        <canvas
            class="cv"
            node_ref=canvas
            role="img"
            aria-label=move || {
                let count = candles.get().len();
                let last = candles.get().last().map(|candle| candle.c);
                match last {
                    Some(last) => format!("Candlestick chart, {count} periods, last {last}"),
                    None => "Candlestick chart, loading".to_string(),
                }
            }
            on:mousemove=move |event| {
                let (x, y) = (f64::from(event.offset_x()), f64::from(event.offset_y()));
                set_pointer.set(Some((x, y)));
                if let Some(element) = canvas.get() {
                    let width = (f64::from(element.client_width()) - AXIS_W).max(1.0);
                    let count = candles.get().len();
                    if count > 0 && x < width {
                        let index = ((x / width) * count as f64).floor() as usize;
                        hovered.set(Some(index.min(count - 1)));
                    }
                }
            }
            on:mouseleave=move |_| {
                set_pointer.set(None);
                hovered.set(None);
            }
        ></canvas>
    }
}

/// The cumulative depth curve for the selected market.
///
/// Two step areas meeting at the spread. The fill grows outward from the middle,
/// which is the same direction the order book's bars grow inward from — together
/// they say the same thing twice, in two shapes.
#[component]
pub fn DepthChart(
    /// Bids, best first, with cumulative totals.
    #[prop(into)]
    bids: Signal<Vec<BookRow>>,
    /// Asks, best first, with cumulative totals.
    #[prop(into)]
    asks: Signal<Vec<BookRow>>,
    /// The market, for its decimals.
    #[prop(into)]
    market: Signal<Option<Market>>,
) -> impl IntoView {
    let canvas: NodeRef<leptos::html::Canvas> = NodeRef::new();

    let draw = move || {
        let Some(element) = canvas.get() else { return };
        let Some(painter) = prepare(&element) else {
            return;
        };
        let colours = palette();
        let ctx = &painter.ctx;
        ctx.set_fill_style_str(&colours.sunken);
        ctx.fill_rect(0.0, 0.0, painter.width, painter.height);

        let (bids, asks) = (bids.get(), asks.get());
        if bids.is_empty() || asks.is_empty() {
            return;
        }
        let price_dp = market.get().map_or(2, |market| market.price_decimals);

        let low = px(bids.last().map_or(Decimal::ZERO, |row| row.price));
        let high = px(asks.last().map_or(Decimal::ZERO, |row| row.price));
        let span = (high - low).max(f64::EPSILON);
        let deepest = px(bids
            .last()
            .map_or(Decimal::ONE, |row| row.cumulative)
            .max(asks.last().map_or(Decimal::ONE, |row| row.cumulative)))
        .max(f64::EPSILON);

        let bottom = painter.height - 14.0;
        let x_of = |price: f64| ((price - low) / span) * painter.width;
        let y_of = |cumulative: f64| bottom - (cumulative / deepest) * (bottom - 6.0);

        let side = |rows: &[BookRow], colour: &str| {
            ctx.set_stroke_style_str(colour);
            ctx.set_line_width(1.5);
            ctx.begin_path();
            let mut previous_x = x_of(px(rows.first().map_or(Decimal::ZERO, |row| row.price)));
            ctx.move_to(previous_x, bottom);
            for row in rows {
                let x = x_of(px(row.price));
                let y = y_of(px(row.cumulative));
                ctx.line_to(previous_x, y);
                ctx.line_to(x, y);
                previous_x = x;
            }
            ctx.line_to(previous_x, bottom);
            ctx.stroke();
            ctx.set_global_alpha(0.16);
            ctx.set_fill_style_str(colour);
            ctx.fill();
            ctx.set_global_alpha(1.0);
        };
        side(&bids, &colours.bid);
        side(&asks, &colours.ask);

        ctx.set_fill_style_str(&colours.muted);
        ctx.set_font(&num_font(10));
        let _ = ctx.fill_text(
            &format::price(bids.last().map_or(Decimal::ZERO, |row| row.price), price_dp),
            2.0,
            painter.height - 2.0,
        );
        let label = format::price(asks.last().map_or(Decimal::ZERO, |row| row.price), price_dp);
        let width = ctx.measure_text(&label).map(|m| m.width()).unwrap_or(48.0);
        let _ = ctx.fill_text(&label, painter.width - width - 2.0, painter.height - 2.0);
    };

    Effect::new(move |_| draw());
    let _resize = window_event_listener(leptos::ev::resize, move |_| draw());

    view! {
        <canvas
            class="cv"
            node_ref=canvas
            role="img"
            aria-label="Cumulative depth on both sides of the spread"
        ></canvas>
    }
}

/// Re-exported so the chart's palette type is nameable by callers.
pub type ChartPalette = Palette;
