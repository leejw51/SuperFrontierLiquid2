//! The virtualised table.
//!
//! The design system makes fixed row heights a contract rather than a style
//! choice, and this is why: the scroll body is `rows × var(--row-h)` tall and
//! each rendered row is absolutely positioned at `index × var(--row-h)`. A row
//! that grew to fit its content would break the arithmetic and the scroller with
//! it, which is why anything that will not fit is truncated rather than wrapped.
//!
//! `--row-h` is read off the live element rather than assumed, so the scroller
//! follows the density switch without being told about it.
//!
//! Tables under two hundred rows do not need this, but they are rendered through
//! it anyway: one scroller with one set of behaviours is easier to get right
//! than two, and below the threshold the whole set is inside the window in any
//! case.

use std::sync::Arc;

use leptos::prelude::*;

/// Rows rendered either side of the viewport, so a fast scroll does not show a
/// gap before the next frame lands.
const OVERSCAN: usize = 9;

/// Builds the cells of one row.
pub type RowRenderer = Arc<dyn Fn(usize) -> AnyView + Send + Sync>;

/// Builds the header cells.
pub type HeaderRenderer = Arc<dyn Fn() -> AnyView + Send + Sync>;

/// Reads a numeric pixel value from a custom property on an element.
fn row_height(element: &web_sys::Element) -> f64 {
    let value = web_sys::window()
        .and_then(|window| window.get_computed_style(element).ok().flatten())
        .and_then(|style| style.get_property_value("--row-h").ok())
        .unwrap_or_default();
    value
        .trim()
        .trim_end_matches("px")
        .parse::<f64>()
        .ok()
        .filter(|height| *height > 1.0)
        .unwrap_or(32.0)
}

/// A scrolling table body that only renders the rows on screen.
#[component]
pub fn VirtualTable(
    /// How many rows there are in total, after sorting and filtering — which
    /// operate on the whole set, never on the rendered window.
    #[prop(into)]
    count: Signal<usize>,
    /// The grid template for both the header and the rows.
    #[prop(into)]
    columns: String,
    /// The header cells.
    header: HeaderRenderer,
    /// Builds the cells of row `index`.
    row: RowRenderer,
    /// An accessible name for the table.
    #[prop(into)]
    label: String,
    /// Whether the rows have been answered for yet.
    ///
    /// Defaults to [`crate::state::Load::Ready`], because most callers hold
    /// data they already have. Pass the real thing wherever a fetch fills the
    /// table, so that a request still in flight — or one that failed — is not
    /// rendered as a column header over nothing.
    #[prop(into, optional)]
    load: Option<Signal<crate::state::Load>>,
    /// The empty state's heading. Without one the table renders its header row
    /// over blank space, which says nothing to anybody.
    #[prop(into, optional)]
    empty_title: Option<String>,
    /// The empty state's one line.
    #[prop(into, optional)]
    empty_body: Option<String>,
) -> impl IntoView {
    let scroller: NodeRef<leptos::html::Div> = NodeRef::new();
    let (scroll_top, set_scroll_top) = signal(0.0_f64);
    let (viewport, set_viewport) = signal(0.0_f64);
    let (row_h, set_row_h) = signal(32.0_f64);

    // Measure once the element exists, and again whenever the density changes —
    // which is a change to `--row-h`, so re-reading it here is the whole story.
    Effect::new(move |_| {
        if let Some(element) = scroller.get() {
            set_row_h.set(row_height(&element));
            set_viewport.set(f64::from(element.client_height()));
        }
    });

    let window = move || {
        let height = row_h.get().max(1.0);
        let total = count.get();
        let first = ((scroll_top.get() / height).floor() as usize).saturating_sub(OVERSCAN);
        let visible = (viewport.get() / height).ceil() as usize + OVERSCAN * 2;
        let last = (first + visible).min(total);
        (first, last)
    };

    let style = format!("--cols:{columns}");
    let load = load.unwrap_or_else(|| Signal::derive(|| crate::state::Load::Ready));
    let has_empty_state = empty_title.is_some();
    let empty_title = StoredValue::new(empty_title);
    let empty_body = StoredValue::new(empty_body);
    // True when something other than rows belongs under the header.
    let in_place_of_rows = move || {
        let load = load.get();
        load.is_loading() || load.failure().is_some() || (count.get() == 0 && has_empty_state)
    };
    // Below the header, one of: a skeleton, a failure, an empty state, or rows.
    let state = move || {
        let load = load.get();
        if load.is_loading() {
            return Some(view! { <crate::ui::SkeletonRows rows=6 /> }.into_any());
        }
        if let Some(message) = load.failure() {
            return Some(
                view! {
                    <crate::ui::ErrorState title="That did not load" body=message.to_string() />
                }
                .into_any(),
            );
        }
        if count.get() > 0 {
            return None;
        }
        let title = empty_title.get_value()?;
        let body = empty_body.get_value().unwrap_or_default();
        Some(view! { <crate::ui::EmptyState title=title body=body /> }.into_any())
    };

    view! {
        <div class="vtbl" style=style role="table" aria-label=label>
            <div class="vtbl-head" role="row">{header()}</div>
            {state}
            <div
                // Hidden rather than absent, so the scroller keeps its measured
                // row height and nothing jumps when the rows arrive.
                style=move || if in_place_of_rows() { "display:none" } else { "" }
                class="vtbl-scroll"
                node_ref=scroller
                tabindex="0"
                on:scroll=move |_| {
                    if let Some(element) = scroller.get() {
                        set_scroll_top.set(element.scroll_top().into());
                        set_viewport.set(f64::from(element.client_height()));
                    }
                }
            >
                <div
                    class="vtbl-sizer"
                    style=move || format!("height:{}px", count.get() as f64 * row_h.get())
                >
                    <For
                        each=move || {
                            let (first, last) = window();
                            (first..last).collect::<Vec<_>>()
                        }
                        key=|index| *index
                        let:index
                    >
                        <div
                            class="vrow"
                            role="row"
                            style=move || format!("top:{}px", index as f64 * row_h.get())
                        >
                            {row(index)}
                        </div>
                    </For>
                </div>
            </div>
        </div>
    }
}
