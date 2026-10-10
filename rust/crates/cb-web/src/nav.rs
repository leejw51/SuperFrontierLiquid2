//! Moving between routes without throwing the application away.
//!
//! Every route change in this client is a **client-side** one. A full page load
//! re-runs [`crate::config::backend`], re-dials the socket, re-fetches every
//! public call and — before the selection was persisted — silently swapped the
//! live gateway for the fixtures, because the backend was chosen from a query
//! string that the redirect did not carry. A signed-in user then saw somebody
//! else's equity, somebody else's positions and a liquidation banner for a
//! position they did not hold.
//!
//! So: no `location.href` anywhere in this crate. [`navigator`] hands back a
//! closure that drives the router, and it appends the query string that
//! reproduces the current backend so that a URL copied out of the address bar
//! lands the next person on the same exchange.

use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use leptos_router::NavigateOptions;

/// A route change that preserves the application, and the backend it is on.
///
/// **Call this in a component body, not inside a task.** It reads the router
/// from context, which only exists while a component is rendering; capturing
/// the closure and moving it into an `async` block is the supported shape.
#[must_use]
pub fn navigator() -> impl Fn(&str) + Copy + 'static {
    // `Copy`, by way of the arena, because a route change is triggered from
    // event handlers and from tasks that each need their own copy. A `Clone`
    // closure forces a `.clone()` at every one of those call sites and makes
    // the enclosing view closure `FnOnce`.
    let navigate = StoredValue::new_local(use_navigate());
    move |path: &str| {
        let query = crate::config::selection_query();
        // A path that already carries a query (`/trade?side=buy`) keeps it,
        // and the backend selection is appended rather than starting a
        // second `?`.
        let target = if path.contains('?') {
            format!("{path}&{}", query.trim_start_matches('?'))
        } else {
            format!("{path}{query}")
        };
        navigate.with_value(|navigate| navigate(&target, NavigateOptions::default()));
    }
}
