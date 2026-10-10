//! The REST surface, exactly as `docs/api-contract.md` freezes it.
//!
//! ## The contract is the specification
//!
//! A frontend is being written against that document in parallel. Every path,
//! field name, status code and error identifier here is taken from it; a
//! deviation would not fail a build, it would silently break the other side.
//! The tests in this module and in `tests/` assert shapes rather than merely
//! status codes for that reason.
//!
//! ## Where the layers sit
//!
//! Handlers do three things: parse and validate input, call a service, and
//! render the contract's shape. They hold no business logic and name no
//! backend. Anything a handler needs comes from [`crate::AppState`], which is
//! trait objects the composition root chose.
//!
//! ## Cross-cutting behaviour
//!
//! Every request gets a request id — echoed in `x-request-id` and attached to
//! its tracing span — so a report of "my order vanished" can be followed
//! through the logs. Mutating endpoints pass through the rate limiter, keyed
//! on both the session and the client address; reads do not, because reads are
//! what a trading UI does constantly and throttling them would degrade the
//! product to defend against something that is not a threat.

pub mod account;
pub mod admin;
pub mod auth;
pub mod chain;
pub mod dto;
pub mod faucet;
pub mod market;
pub mod misc;
pub mod trading;
pub mod user;

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::state::AppState;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use cb_ports::User;
use cb_types::Address;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// A JSON body, refused in the contract's own words when it is malformed.
///
/// `axum::Json` answers a body it cannot read with a bare-text `400`, `415`
/// or `422` — "Failed to deserialize the JSON body…" — and no error envelope.
/// The contract says every non-2xx carries `{"error":{"code":…}}` and that a
/// malformed body is `400 VALIDATION_FAILED`; a client branching on `code` got
/// nothing to branch on, and a `422` it had no entry for. Every handler reads
/// its body through this wrapper instead, which keeps axum's parsing and
/// replaces only its refusal. A body over the size limit keeps its `413`,
/// because "too large" is a different instruction to the client than "wrong".
///
/// It is also the response type, so a handler reads and writes one name.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

#[async_trait::async_trait]
impl<T, S> axum::extract::FromRequest<S> for Json<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request<axum::body::Body>, state: &S) -> Result<Self, Response> {
        use axum::response::IntoResponse;
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                let status = rejection.status();
                let mut response =
                    ApiError::validation(format!("request body: {}", rejection.body_text()))
                        .into_response();
                if status == StatusCode::PAYLOAD_TOO_LARGE {
                    *response.status_mut() = status;
                }
                Err(response)
            }
        }
    }
}

impl<T: serde::Serialize> axum::response::IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// The header the gateway echoes its request id in.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Build the router.
///
/// `cors_origins` is the list of browser origins allowed to call the API. An
/// entry of `*` permits any origin — appropriate for a local simulator and
/// for nothing else, which is why it has to be asked for. An empty list
/// permits none (security review, finding 10).
pub fn router(state: Arc<AppState>, cors_origins: &[String]) -> Router {
    router_with(state, cors_origins, false)
}

/// Build the router, optionally mounting the development login.
///
/// `dev_login` is refused unless the caller has already checked that the
/// gateway is bound to a loopback address: the contract makes that affordance
/// deliberately hard to turn on by accident, and a default-off flag alone is
/// not hard enough.
pub fn router_with(state: Arc<AppState>, cors_origins: &[String], dev_login: bool) -> Router {
    let mut v1 = Router::new()
        .route("/auth/challenge", post(auth::challenge))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session))
        .route("/faucet", get(faucet::status))
        .route("/faucet/claim", post(faucet::claim))
        .route("/markets", get(market::list))
        .route("/markets/:symbol/ticker", get(market::ticker))
        .route("/markets/:symbol/orderbook", get(market::orderbook))
        .route("/markets/:symbol/trades", get(market::trades))
        .route("/markets/:symbol/candles", get(market::candles))
        .route("/markets/:symbol/funding", get(market::funding))
        .route("/account", get(account::overview))
        .route("/account/orders", get(account::orders))
        .route("/account/positions", get(account::positions))
        .route("/account/fills", get(account::fills))
        .route("/account/transfers", get(account::transfers))
        .route("/orders", post(trading::place_order))
        .route("/orders/cancel-all", post(trading::cancel_all))
        .route("/orders/:order_id", delete(trading::cancel_order))
        .route("/positions/close", post(trading::close_position))
        .route("/positions/margin", post(trading::adjust_margin))
        .route("/positions/leverage", post(trading::set_leverage))
        .route("/transfers", post(trading::transfer))
        .route(
            "/user/profile",
            get(user::profile).patch(user::update_profile),
        )
        .route("/user/sessions", get(user::sessions))
        .route("/user/sessions/revoke-all", post(user::revoke_all_sessions))
        .route("/user/sessions/:session_id", delete(user::revoke_session))
        .route("/user/activity", get(user::activity))
        .route("/admin/overview", get(admin::overview))
        .route("/admin/users", get(admin::users))
        .route("/admin/users/:user_id", get(admin::user_detail))
        .route("/admin/users/:user_id/suspend", post(admin::suspend))
        .route("/admin/users/:user_id/unsuspend", post(admin::unsuspend))
        .route("/admin/markets", get(admin::markets))
        .route("/admin/markets/:symbol/halt", post(admin::halt_market))
        .route("/admin/markets/:symbol/resume", post(admin::resume_market))
        .route("/admin/markets/:symbol/settle", post(admin::settle_market))
        .route(
            "/admin/markets/:symbol/risk",
            axum::routing::patch(admin::set_risk),
        )
        .route(
            "/admin/faucet",
            get(admin::faucet_status).patch(admin::configure_faucet),
        )
        .route("/admin/faucet/grant", post(admin::grant_faucet))
        .route("/admin/audit", get(admin::audit_trail))
        .route(
            "/admin/sim",
            get(admin::sim_status).post(admin::sim_control),
        )
        .route("/chain/status", get(chain::status))
        .route("/chain/blocks", get(chain::blocks))
        .route("/chain/txs", get(chain::txs))
        .route("/leaderboard", get(misc::leaderboard));

    if dev_login {
        tracing::warn!(
            target: "cb_gateway::http",
            "mounting POST /auth/dev-login; it accepts a mnemonic over the wire"
        );
        v1 = v1.route("/auth/dev-login", post(auth::dev_login));
    }

    Router::new()
        .nest("/api/v1", v1)
        .route("/health", get(misc::health))
        .route("/ws", get(misc::websocket))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            rate_limit,
        ))
        .layer(axum::middleware::from_fn(request_id))
        .layer(cors_layer(cors_origins))
        .with_state(state)
}

/// Build the CORS layer.
///
/// The method list must cover every method the router actually mounts, or a
/// browser refuses the request at the preflight and the page sees a network
/// error it cannot distinguish from the server being down. `PATCH` was missing
/// here while three routes used it — the handle change, the market risk change
/// and the faucet configuration — so all three appeared to succeed in the UI
/// and did nothing at all. The list below is checked against the router by
/// [`tests::every_method_the_router_mounts_is_allowed_through_cors`]; `PUT` is
/// absent from both, deliberately, and adding a `PUT` route means adding it
/// here too.
fn cors_layer(origins: &[String]) -> CorsLayer {
    // Only an explicit `*` is permissive. An empty list used to be too, which
    // silently disagreed with an operator who configured nothing: finding 10
    // of the security review, recorded as fixed while this line still read
    // `origins.is_empty() || …`.
    let permissive = origins.iter().any(|o| o == "*");
    if permissive {
        tracing::warn!(
            target: "cb_gateway::http",
            "CORS allows any origin (`*`); appropriate for a local simulator only"
        );
    }
    let layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
        ])
        .expose_headers([HeaderName::from_static(REQUEST_ID_HEADER)]);
    if permissive {
        layer.allow_origin(AllowOrigin::any())
    } else {
        let parsed: Vec<HeaderValue> = origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect();
        // Credentials are only meaningful against a closed origin list;
        // allowing them alongside `*` is a configuration browsers refuse.
        layer.allow_origin(parsed).allow_credentials(true)
    }
}

/// Attach a request id to the span and echo it to the caller.
///
/// A client-supplied `x-request-id` is honoured so a trace can be followed
/// from the browser through the gateway; one is minted when absent.
async fn request_id(request: Request<axum::body::Body>, next: Next) -> Response {
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    let span = tracing::info_span!(
        "request",
        request_id = %id,
        method = %request.method(),
        path = %request.uri().path(),
    );
    let _entered = span.enter();
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

/// Whether a method changes something.
///
/// One definition, used by the rate limiter and by the suspension check, so
/// the two cannot disagree about what counts as an action.
fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::DELETE | Method::PUT | Method::PATCH
    )
}

/// Apply the rate limiter to mutating requests.
///
/// This is the innermost layer, so every request passes through it; the
/// resolved client address is attached to the request here so that handlers
/// downstream (session attribution, in particular) read the same trusted value
/// the limiter used rather than re-reading a forgeable header.
async fn rate_limit(
    State(state): State<Arc<AppState>>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let client = ClientAddress(resolve_client_address(
        &request,
        &state.settings.trusted_proxies,
    ));
    request.extensions_mut().insert(client.clone());

    // Reads are not limited; see the module docs.
    if !is_mutating(request.method()) {
        return next.run(request).await;
    }

    let session = bearer_token(request.headers());
    let client = client.0.clone();
    match state.limits.check(session.as_deref(), &client) {
        crate::ratelimit::Decision::Allow => next.run(request).await,
        crate::ratelimit::Decision::Deny {
            retry_after_seconds,
        } => ApiError::new(
            ErrorCode::RateLimited,
            "too many requests; slow down and try again shortly",
        )
        .with_details(serde_json::json!({
            "retry_after_seconds": retry_after_seconds,
        }))
        .into_response_with_retry_after(retry_after_seconds),
    }
}

impl ApiError {
    /// Render with a `retry-after` header, which a rate limit should carry.
    fn into_response_with_retry_after(self, seconds: u64) -> Response {
        use axum::response::IntoResponse;
        let mut response = self.into_response();
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

/// The bearer token from an `Authorization` header, if there is one.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// The address a request is attributed to: the real peer unless a trusted
/// proxy said otherwise.
///
/// Attached to every request by the [`rate_limit`] layer, so nothing
/// downstream has to decide again — and so the value a user is shown as "where
/// you signed in from" is the value the limiter counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAddress(pub String);

impl ClientAddress {
    /// The address as a string, or `unknown` when the extension is absent
    /// (which only happens off the router, in unit tests).
    pub fn of(extension: Option<&Self>) -> &str {
        extension.map_or("unknown", |value| value.0.as_str())
    }
}

/// The client's address, for rate limiting and attribution.
///
/// `x-forwarded-for` is honoured **only** when the immediate TCP peer is an
/// address the operator named with `--trusted-proxy`, and even then only its
/// first element and only if that element parses as an IP. Anything else uses
/// the socket's peer address.
///
/// The trade this makes: a gateway genuinely deployed behind a reverse proxy
/// now needs that proxy configured, and until it is, everyone behind it shares
/// one rate-limit bucket. The alternative is what finding 3 demonstrated — a
/// limiter that any client turns off by inventing a header, and whose key map
/// grows by whatever the client puts in it.
fn resolve_client_address(
    request: &Request<axum::body::Body>,
    trusted: &std::collections::HashSet<std::net::IpAddr>,
) -> String {
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());

    if let Some(peer) = peer {
        if trusted.contains(&peer) {
            if let Some(forwarded) = request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .map(str::trim)
                .and_then(|value| value.parse::<std::net::IpAddr>().ok())
            {
                return forwarded.to_string();
            }
        }
        return peer.to_string();
    }
    "unknown".to_string()
}

/// An authenticated caller.
///
/// Extracting one is how an endpoint becomes 🔒. The ways a session can fail —
/// absent, unknown, expired, revoked — are distinguished internally and
/// collapsed to one `401` here, because telling a caller which one they hit
/// says whether a token was ever real.
///
/// A suspended account is refused here too, so suspension takes effect on the
/// next request rather than at the next login.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The authenticated user.
    pub user: User,
    /// The token they presented, for logout and session listing.
    pub token: String,
    /// The address they signed in with.
    pub login_address: Address,
    /// The on-chain account the gateway trades from on their behalf.
    pub trading_address: Address,
    /// The handle they currently go by.
    pub handle: String,
    /// The roles they hold.
    pub roles: Vec<String>,
}

impl Caller {
    /// The on-chain account the gateway acts on. Kept as a method because
    /// every trading handler asks for it and the name is the intent.
    pub fn address(&self) -> ApiResult<Address> {
        Ok(self.trading_address)
    }

    /// The key that signs this account's transactions.
    pub fn trading_key(&self, state: &AppState) -> ApiResult<cb_crypto::SecretKey> {
        Ok(state.auth.trading_key_for(&self.login_address)?.0)
    }

    /// Whether this caller holds the operator role.
    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|r| r == crate::auth::ROLE_ADMIN)
    }
}

#[async_trait::async_trait]
impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let Some(token) = bearer_token(&parts.headers) else {
            return Err(crate::auth::AuthFailure::Missing.into());
        };
        let user = match state.auth.authenticate(&token).await {
            Ok(user) => user,
            Err(failure) => {
                tracing::debug!(
                    target: "cb_gateway::http",
                    reason = failure.reason(),
                    "rejecting a request with an unusable session"
                );
                return Err(failure.into());
            }
        };

        // A suspension stops an account **acting**, not looking at itself.
        //
        // The contract suspends placement and faucet claims and says existing
        // positions stay open, so a suspended account is still carrying risk:
        // a position that can still be liquidated, collateral that is still
        // moving with the mark. Refusing its reads too — which is what a blanket
        // check here did — left a person holding exposure they were not allowed
        // to look at, and no way to see why they were cut off. Mutating methods
        // are refused, by the same classification the rate limiter uses; reads
        // and the account channel continue.
        if is_mutating(&parts.method) && state.auth.directory().is_suspended(&user.user_id) {
            return Err(ApiError::new(
                ErrorCode::AccountSuspended,
                "this account has been suspended by an operator; you can still see your \
                 balances and positions, but you cannot trade or claim",
            ));
        }

        let login_address =
            crate::auth::eth::parse_checked_address(&user.address).map_err(|_| {
                ApiError::internal("the stored login address for this account is malformed")
            })?;
        let trading_address = state.auth.trading_address_for(&login_address)?;
        let handle = state.auth.effective_handle(&user);
        let roles = state.auth.roles_of(&user);

        // Note the session was used, for the user page's session list.
        let session_id = crate::directory::Directory::session_id_for(
            &crate::auth::secrets::hash_session_token(&token),
        );
        let _ = state
            .auth
            .directory()
            .touch_session(&user.user_id, &session_id, state.clock.now());

        Ok(Caller {
            user,
            token,
            login_address,
            trading_address,
            handle,
            roles,
        })
    }
}

/// An authenticated caller holding the operator role.
///
/// Extracting one is how a §10 endpoint is gated. The role is read from the
/// server's own record of the account, never from anything the client sent:
/// the contract is explicit that `roles` in a session response is for deciding
/// what to render and is not a security boundary.
///
/// A non-admin session gets `403` with the same message a non-existent
/// endpoint would produce, so the response does not reveal that the endpoint
/// exists for anybody else.
#[derive(Debug, Clone)]
pub struct Admin(pub Caller);

#[async_trait::async_trait]
impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        if !caller.is_admin() {
            tracing::warn!(
                target: "cb_gateway::http",
                user = %caller.user.user_id,
                "non-operator attempted an admin endpoint"
            );
            return Err(ApiError::forbidden("not permitted"));
        }
        Ok(Admin(caller))
    }
}

/// Parse a `0x`-prefixed address, validating any EIP-55 checksum it carries.
pub fn parse_address(text: &str) -> Option<Address> {
    crate::auth::eth::parse_checked_address(text).ok()
}

/// Render an address the way the contract does: EIP-55 checksummed hex.
///
/// The gateway always emits the checksummed form, so an address copied out of
/// a response and pasted into a wallet is validated on the way back in.
pub fn format_address(address: &Address) -> String {
    crate::auth::eth::checksum_address(address)
}

/// Render a 32-byte hash the way the contract does.
pub fn format_hash(hash: &cb_types::H256) -> String {
    format!("0x{}", hex::encode(hash.0))
}

/// Resolve a symbol to its catalogue entry, telling a malformed symbol
/// (`400`) apart from an unlisted one (`404`).
pub fn resolve_market<'a>(
    catalog: &'a crate::markets::Catalog,
    symbol: &str,
) -> ApiResult<(&'a crate::symbols::MarketEntry, &'a cb_ports::MarketMeta)> {
    let upper = symbol.to_ascii_uppercase();
    if !crate::symbols::is_well_formed(&upper) {
        return Err(ApiError::validation(format!(
            "`{symbol}` is not a market symbol; expected forms are BTC-USD, BTC-PERP or ETH-260626"
        )));
    }
    let entry = catalog
        .entry(&upper)
        .ok_or_else(|| ApiError::not_found(format!("no market `{upper}` is listed")))?;
    let meta = catalog
        .meta(&upper)
        .ok_or_else(|| ApiError::not_found(format!("no market `{upper}` is listed")))?;
    Ok((entry, meta))
}

/// The `204 No Content` reply.
pub fn no_content() -> Response {
    use axum::response::IntoResponse;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn a_bearer_token_is_read_case_insensitively_and_trimmed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sess_abc"),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("sess_abc"));

        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("bearer   sess_abc  "),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("sess_abc"));
    }

    #[test]
    fn a_malformed_authorization_header_yields_no_token() {
        let mut headers = HeaderMap::new();
        assert!(bearer_token(&headers).is_none());
        for value in ["sess_abc", "Basic abc", "Bearer ", "Bearer"] {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(value).expect("header"),
            );
            assert!(
                bearer_token(&headers).is_none(),
                "`{value}` is not a bearer token"
            );
        }
    }

    /// A request carrying a peer address and an optional forwarded header.
    fn request_from(peer: Option<&str>, forwarded: Option<&str>) -> Request<axum::body::Body> {
        let mut builder = Request::builder().uri("/");
        if let Some(forwarded) = forwarded {
            builder = builder.header("x-forwarded-for", forwarded);
        }
        let mut request = builder
            .body(axum::body::Body::empty())
            .expect("static request");
        if let Some(peer) = peer {
            let addr: std::net::SocketAddr = peer.parse().expect("static address");
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(addr));
        }
        request
    }

    #[test]
    fn a_forged_forwarded_header_does_not_choose_the_rate_limit_key() {
        // Finding 3: the header is free for any client to invent, so a
        // limiter keyed on it is a limiter the client switches off. With no
        // trusted proxy configured it must be ignored entirely.
        let none = std::collections::HashSet::new();
        assert_eq!(
            resolve_client_address(
                &request_from(Some("203.0.113.7:51000"), Some("1.2.3.4")),
                &none
            ),
            "203.0.113.7"
        );
        // Including the 8 KB header the reviewer used to grow the key map.
        let huge = "a".repeat(8 * 1024);
        assert_eq!(
            resolve_client_address(&request_from(Some("203.0.113.7:51000"), Some(&huge)), &none),
            "203.0.113.7"
        );
    }

    #[test]
    fn a_forwarded_header_is_honoured_only_from_a_configured_proxy() {
        let mut trusted = std::collections::HashSet::new();
        trusted.insert("10.0.0.1".parse::<std::net::IpAddr>().expect("static"));

        // From the proxy: believed, and only the first element.
        assert_eq!(
            resolve_client_address(
                &request_from(Some("10.0.0.1:4000"), Some("198.51.100.9, 10.0.0.1")),
                &trusted
            ),
            "198.51.100.9"
        );
        // From anyone else: ignored, even though the header is well formed.
        assert_eq!(
            resolve_client_address(
                &request_from(Some("203.0.113.7:51000"), Some("198.51.100.9")),
                &trusted
            ),
            "203.0.113.7"
        );
        // From the proxy, but not an IP: ignored, so the key space stays
        // bounded by the set of IP addresses.
        assert_eq!(
            resolve_client_address(
                &request_from(Some("10.0.0.1:4000"), Some("not-an-ip")),
                &trusted
            ),
            "10.0.0.1"
        );
    }

    #[test]
    fn a_request_with_no_peer_information_is_keyed_uniformly() {
        let none = std::collections::HashSet::new();
        assert_eq!(
            resolve_client_address(&request_from(None, Some("1.2.3.4")), &none),
            "unknown"
        );
    }

    #[test]
    fn addresses_are_emitted_checksummed_and_round_trip() {
        let address = Address([0x8f; 20]);
        let text = format_address(&address);
        assert!(text.starts_with("0x"));
        assert_eq!(text.len(), 42);
        assert_eq!(parse_address(&text), Some(address));
        assert!(parse_address("0xnothex").is_none());
        assert!(parse_address("0x1234").is_none());
    }

    #[test]
    fn a_mixed_case_address_with_a_broken_checksum_is_not_accepted() {
        // The contract: a wrong checksum means a typo, and crediting a
        // different account is the consequence of accepting it.
        assert!(parse_address("0x5AAeb6053F3E94C9b9A09f33669435E7Ef1BeAed").is_none());
    }

    #[test]
    fn a_malformed_symbol_is_a_400_and_an_unlisted_one_is_a_404() {
        let catalog = crate::markets::Catalog {
            symbols: crate::symbols::SymbolTable::with_spot_markets(),
            metas: Default::default(),
            refreshed_at: None,
        };
        assert_eq!(
            resolve_market(&catalog, "not a symbol")
                .expect_err("malformed")
                .code,
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            resolve_market(&catalog, "ZZZ-PERP")
                .expect_err("unlisted")
                .code,
            ErrorCode::NotFound
        );
    }

    /// Every method the router mounts must survive a browser preflight.
    ///
    /// The regression this guards is §4 of the UI review: `PATCH` was missing
    /// from the CORS allow list while three routes used it, so the handle
    /// change, the market risk change and the faucet configuration all failed
    /// in the browser before the request was ever sent — with no status code,
    /// no error body and nothing on screen.
    #[tokio::test]
    async fn every_method_the_router_mounts_is_allowed_through_cors() {
        use tower::ServiceExt;

        let harness = crate::testing::Harness::new();
        for method in ["GET", "POST", "PATCH", "DELETE"] {
            let request = Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/v1/user/profile")
                .header("origin", "http://127.0.0.1:3000")
                .header("access-control-request-method", method)
                .body(axum::body::Body::empty())
                .expect("preflight request");
            let response = harness
                .router()
                .oneshot(request)
                .await
                .expect("the router answers a preflight");
            let allowed = response
                .headers()
                .get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                allowed.contains(method),
                "the CORS preflight does not allow {method}; it allows `{allowed}`"
            );
        }
    }
}
