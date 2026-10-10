//! Which backend the application talks to, and how to change it.
//!
//! **To point the application at the live gateway, change one line: set
//! [`DEFAULT_BACKEND`] to [`BackendKind::Live`].** Nothing else in the crate
//! knows or cares which implementation it is holding, because everything goes
//! through [`crate::api::Api`] and [`crate::ws::Transport`].
//!
//! There are two other ways to switch, both of which exist so that a reviewer
//! does not need a Rust toolchain to try the other side:
//!
//! * build with `--features live-gateway`, which flips the default;
//! * append `?gateway=live` or `?gateway=mock` to the URL, which overrides both,
//!   and `?api=http://host:port/api/v1` to point at a gateway elsewhere.
//!
//! The mock is the default deliberately. The gateway is being written in
//! parallel, and an interface that only works when another team's server is
//! running is an interface nobody can review.

use std::sync::Arc;

use crate::api::mock::MockApi;
use crate::api::{http::HttpApi, Api};
use crate::ws::mockfeed::MockTransport;
use crate::ws::{transport::WebSocketTransport, Transport};

/// Which gateway implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The fixtures from the design reference, served locally.
    Mock,
    /// A running `cb-gateway`.
    Live,
}

/// **The one line.** Change this to [`BackendKind::Live`] to use the gateway.
pub const DEFAULT_BACKEND: BackendKind = if cfg!(feature = "live-gateway") {
    BackendKind::Live
} else {
    BackendKind::Mock
};

/// Where the live gateway is, relative to wherever this page is served from.
pub const DEFAULT_API_BASE: &str = "/api/v1";

/// A chosen backend: a gateway, a feed, and the instrumentation the mock allows.
pub struct Backend {
    /// The gateway.
    pub api: Arc<dyn Api>,
    /// The websocket transport the feed driver dials.
    pub transport: Arc<dyn Transport>,
    /// Present only under the mock, where a simulated outage can be triggered
    /// to exercise the disconnected and reconnecting states for real.
    pub sim: Option<Arc<MockTransport>>,
    /// Which one this is, for the badge in the top bar.
    pub kind: BackendKind,
}

impl Backend {
    /// True when the data on screen is fixture data rather than an exchange.
    ///
    /// The interface says so in the top bar. A demo that does not admit it is a
    /// demo is one screenshot away from being mistaken for a running system.
    #[must_use]
    pub const fn is_mock(&self) -> bool {
        matches!(self.kind, BackendKind::Mock)
    }
}

/// Reads a query parameter from the current URL.
#[must_use]
pub fn query_param(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let search = search.strip_prefix('?').unwrap_or(&search);
    search.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace("%3A", ":").replace("%2F", "/"))
    })
}

/// Turns an HTTP base URL into the websocket URL.
///
/// The socket lives at `/ws` on the gateway's ROOT, not under the `/api/v1`
/// base the REST calls use. Appending `/ws` to the API base produced
/// `/api/v1/ws`, which the gateway answers with a 404 and which a browser
/// reports only as a failed handshake. The API path is therefore stripped
/// before the socket path is attached.
pub fn websocket_url(api_base: &str) -> String {
    /// Drop everything from the first path segment after the authority, so
    /// `http://host:8081/api/v1` becomes `host:8081`.
    fn authority(rest: &str) -> &str {
        rest.split('/').next().unwrap_or(rest)
    }
    if let Some(rest) = api_base.strip_prefix("https://") {
        return format!("wss://{}/ws", authority(rest));
    }
    if let Some(rest) = api_base.strip_prefix("http://") {
        return format!("ws://{}/ws", authority(rest));
    }
    // A relative base: resolve it against the page's own origin.
    let origin = web_sys::window()
        .and_then(|window| window.location().host().ok())
        .unwrap_or_default();
    let secure = web_sys::window()
        .and_then(|window| window.location().protocol().ok())
        .map(|protocol| protocol.starts_with("https"))
        .unwrap_or(false);
    let scheme = if secure { "wss" } else { "ws" };
    let _ = api_base;
    format!("{scheme}://{origin}/ws")
}

/// The backend selection, as the URL and storage between them decide it.
///
/// The query string wins, and what it says is **remembered**. A selection that
/// only lives in the URL is lost by the first full page load — which is exactly
/// what the post-login redirect used to be — and the application then quietly
/// shows fixture data to somebody holding a real session token. That is the one
/// failure this whole indirection exists to prevent, so the choice is persisted
/// beside the session token rather than carried in a query string alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Which implementation to build.
    pub kind: BackendKind,
    /// Where the gateway is, for [`BackendKind::Live`].
    pub api_base: String,
}

/// Reads the selection from the URL, falling back to what was remembered.
///
/// Writes back whatever the URL said, so the next full page load agrees with
/// this one.
#[must_use]
pub fn selection() -> Selection {
    let from_url = query_param("gateway");
    let kind = match from_url.as_deref() {
        Some("live") => Some(BackendKind::Live),
        Some("mock") => Some(BackendKind::Mock),
        _ => None,
    };
    let api_from_url = query_param("api");

    if let Some(kind) = kind {
        remember(
            GATEWAY_KEY,
            if matches!(kind, BackendKind::Live) {
                "live"
            } else {
                "mock"
            },
        );
        // `gateway` and `api` are independent. A URL that names one and not the
        // other must not clear the other: `?gateway=live` on its own used to
        // forget a remembered gateway address and fall back to the page's own
        // origin, which signed the user out on the spot. Saying nothing about
        // the address means leaving it alone; `?api=/api/v1` restores the
        // default explicitly.
        if let Some(base) = api_from_url.as_deref() {
            remember(API_BASE_KEY, base);
        }
        return Selection {
            kind,
            api_base: api_from_url
                .or_else(|| recall(API_BASE_KEY))
                .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
        };
    }

    let remembered = recall(GATEWAY_KEY);
    let kind = match remembered.as_deref() {
        Some("live") => BackendKind::Live,
        Some("mock") => BackendKind::Mock,
        _ => DEFAULT_BACKEND,
    };
    Selection {
        kind,
        api_base: api_from_url
            .or_else(|| recall(API_BASE_KEY))
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
    }
}

/// The query string that reproduces the current selection, `?gateway=…` and all.
///
/// Carried through every in-app navigation so that a URL copied out of the
/// address bar lands somebody else on the same backend.
#[must_use]
pub fn selection_query() -> String {
    let selection = selection();
    let gateway = match selection.kind {
        BackendKind::Live => "live",
        BackendKind::Mock => "mock",
    };
    if selection.api_base == DEFAULT_API_BASE {
        format!("?gateway={gateway}")
    } else {
        format!(
            "?gateway={gateway}&api={}",
            selection.api_base.replace(':', "%3A").replace('/', "%2F")
        )
    }
}

/// Stores a value, ignoring a browser that refuses storage.
fn remember(key: &str, value: &str) {
    if let Some(storage) =
        web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    {
        let _ = storage.set_item(key, value);
    }
}

/// Reads a stored value.
fn recall(key: &str) -> Option<String> {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .filter(|value| !value.is_empty())
}

/// Builds the backend this session will use.
#[must_use]
pub fn backend() -> Backend {
    let Selection { kind, api_base } = selection();

    match kind {
        BackendKind::Mock => {
            let api = MockApi::new();
            let transport = Arc::new(MockTransport::new(api.state()));
            Backend {
                api: Arc::new(api),
                transport: Arc::clone(&transport) as Arc<dyn Transport>,
                sim: Some(transport),
                kind,
            }
        }
        BackendKind::Live => {
            let base = api_base;
            let transport = Arc::new(WebSocketTransport::new(&websocket_url(&base)));
            Backend {
                api: Arc::new(HttpApi::new(&base)),
                transport,
                sim: None,
                kind,
            }
        }
    }
}

/// The localStorage key the session token is kept under.
///
/// **Only the token.** The recovery phrase and the private key derived from it
/// never touch storage: a phrase in localStorage is a phrase in every backup,
/// every extension with storage access and every shared machine. A reload
/// therefore asks the user to sign in again, which is the correct behaviour and
/// not a gap.
pub const SESSION_TOKEN_KEY: &str = "cbl.session_token";

/// The localStorage key the theme preference is kept under.
pub const THEME_KEY: &str = "cbl.theme";

/// The localStorage key the density preference is kept under.
pub const DENSITY_KEY: &str = "cbl.density";

/// The localStorage key the chosen backend is kept under.
///
/// It sits beside the session token deliberately: the token and the backend
/// that issued it are one fact, and separating them is how a signed-in user
/// ends up looking at somebody else's fixture portfolio.
pub const GATEWAY_KEY: &str = "cbl.gateway";

/// The localStorage key the gateway's base URL is kept under.
pub const API_BASE_KEY: &str = "cbl.api_base";
