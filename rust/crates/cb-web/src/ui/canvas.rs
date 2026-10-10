//! Canvas plumbing: back-buffer sizing and design-token colours.
//!
//! Two problems every canvas in this application has, solved once here.
//!
//! **Sizing.** A canvas has a CSS size and a back-buffer size, and if they
//! disagree the drawing is blurry or stretched. The back buffer is set to the
//! CSS size times the device pixel ratio on every draw, so a chart is sharp on a
//! retina display and stays sharp when the window is resized or the panel
//! collapses at a container query.
//!
//! **Colour.** `fillStyle` cannot take `var(--bid)`. If a canvas hard-coded the
//! hex instead, it would ignore the light theme and it would break the design
//! system's rule that no component hard-codes a market colour. So the tokens are
//! read off the document element with `getComputedStyle` on every draw, which
//! also means the theme switch repaints correctly for free.

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// A canvas ready to be drawn on, in CSS pixels.
pub struct Painter {
    /// The 2D context, already scaled so that one unit is one CSS pixel.
    pub ctx: CanvasRenderingContext2d,
    /// Width in CSS pixels.
    pub width: f64,
    /// Height in CSS pixels.
    pub height: f64,
}

/// Prepares a canvas: resizes the back buffer and clears it.
///
/// Returns `None` when the element has no layout yet, which happens on the frame
/// before a panel is measured. Drawing into a zero-sized canvas would throw.
#[must_use]
pub fn prepare(canvas: &HtmlCanvasElement) -> Option<Painter> {
    let width = f64::from(canvas.client_width());
    let height = f64::from(canvas.client_height());
    if width < 1.0 || height < 1.0 {
        return None;
    }
    let ratio = web_sys::window().map_or(1.0, |window| window.device_pixel_ratio().max(1.0));
    let buffer_width = (width * ratio).round() as u32;
    let buffer_height = (height * ratio).round() as u32;
    if canvas.width() != buffer_width {
        canvas.set_width(buffer_width);
    }
    if canvas.height() != buffer_height {
        canvas.set_height(buffer_height);
    }

    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()?
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;
    ctx.reset_transform().ok()?;
    ctx.scale(ratio, ratio).ok()?;
    ctx.clear_rect(0.0, 0.0, width, height);
    Some(Painter { ctx, width, height })
}

/// The design tokens a chart needs, resolved for the current theme.
#[derive(Debug, Clone)]
pub struct Palette {
    /// Rising: bodies, the bid side, profit.
    pub bid: String,
    /// Falling: outlines, the ask side, loss.
    pub ask: String,
    /// The chart's plot background.
    pub sunken: String,
    /// Horizontal rules.
    pub grid: String,
    /// The axis line.
    pub axis: String,
    /// The crosshair.
    pub cross: String,
    /// Volume bars.
    pub volume: String,
    /// The mark-price line.
    pub mark: String,
    /// The liquidation-price line.
    pub liq: String,
    /// Body text.
    pub text: String,
    /// Column headers and axis labels.
    pub muted: String,
    /// Text on a jade or vermilion fill.
    pub inverse: String,
}

/// Reads one custom property off the document element.
fn token(style: &web_sys::CssStyleDeclaration, name: &str, fallback: &str) -> String {
    let value = style
        .get_property_value(name)
        .unwrap_or_default()
        .trim()
        .to_string();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

/// Resolves the palette for whichever theme is currently applied.
///
/// The fallbacks exist so that a chart still draws if the stylesheet has not
/// loaded; they are the dark theme's own values, not invented colours.
#[must_use]
pub fn palette() -> Palette {
    let style = web_sys::window().and_then(|window| {
        window
            .document()
            .and_then(|document| document.document_element())
            .and_then(|root| window.get_computed_style(&root).ok().flatten())
    });
    let Some(style) = style else {
        return Palette {
            bid: "#34E0B4".into(),
            ask: "#FF7053".into(),
            sunken: "#0A0908".into(),
            grid: "#221E1A".into(),
            axis: "#3A332D".into(),
            cross: "#7A7168".into(),
            volume: "rgba(244,239,231,0.10)".into(),
            mark: "#78B4E0".into(),
            liq: "#FF7053".into(),
            text: "#F4EFE7".into(),
            muted: "#9C9286".into(),
            inverse: "#0E0C0B".into(),
        };
    };
    Palette {
        bid: token(&style, "--bid", "#34E0B4"),
        ask: token(&style, "--ask", "#FF7053"),
        sunken: token(&style, "--surface-sunken", "#0A0908"),
        grid: token(&style, "--chart-grid", "#221E1A"),
        axis: token(&style, "--chart-axis", "#3A332D"),
        cross: token(&style, "--chart-cross", "#7A7168"),
        volume: token(&style, "--chart-volume", "rgba(244,239,231,0.10)"),
        mark: token(&style, "--chart-mark", "#78B4E0"),
        liq: token(&style, "--chart-liq", "#FF7053"),
        text: token(&style, "--text-primary", "#F4EFE7"),
        muted: token(&style, "--text-muted", "#9C9286"),
        inverse: token(&style, "--text-inverse", "#0E0C0B"),
    }
}

/// The numeric face the design system reserves for digits, at `size` pixels.
#[must_use]
pub fn num_font(size: u32) -> String {
    format!(
        "{size}px ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, \
         \"Cascadia Mono\", \"Roboto Mono\", monospace"
    )
}

/// Converts a [`Decimal`](rust_decimal::Decimal) to an `f64` **for pixel
/// geometry only**.
///
/// This is the single permitted `Decimal`-to-float conversion in the crate. The
/// result is a coordinate on a canvas: it is never stored, never compared
/// against a price, never sent to the gateway and never shown to the user as a
/// number. Every figure a person reads is formatted from the `Decimal` itself by
/// [`crate::format`].
#[must_use]
pub fn px(value: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64().unwrap_or(0.0)
}
