//! The eight screens.
//!
//! Each module is one route and owns no state of its own beyond what is local to
//! its controls: everything it shows comes from [`crate::state::Stores`]. That is
//! what keeps the market strip, the order book and the order ticket from
//! disagreeing about the price by a round trip.

pub mod account;
pub mod admin;
pub mod chain;
pub mod faucet;
pub mod landing;
pub mod leaderboard;
pub mod portfolio;
pub mod terminal;

use leptos::prelude::*;
use leptos_router::components::A;

/// The route that does not exist.
///
/// An error state, not a joke: it says what happened and offers the way back.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <crate::ui::AppShell>
            <crate::ui::ErrorState
                title="There is nothing at this address"
                body="The link may be out of date, or the screen may have been renamed."
            >
                <A href="/trade" attr:class="btn btn-primary">"Go to the terminal"</A>
            </crate::ui::ErrorState>
        </crate::ui::AppShell>
    }
}
