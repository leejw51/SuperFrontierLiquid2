//! Identity: challenge, login, development login, logout, session.
//!
//! There is no registration endpoint. Per the API contract a first successful
//! login creates the account, which is why the login request carries a handle
//! and the reply carries `is_new`.

use super::dto::*;
use super::Json;
use super::{Caller, ClientAddress};
use crate::auth::{eth, Authenticated};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use std::sync::Arc;

/// `POST /auth/challenge`.
pub async fn challenge(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ChallengeRequest>,
) -> ApiResult<Json<ChallengeResponse>> {
    let address = eth::parse_checked_address(&request.address)
        .map_err(|e| ApiError::validation(e.to_string()))?;
    let challenge = state.auth.challenge(&address);
    Ok(Json(ChallengeResponse {
        nonce: challenge.nonce,
        message: challenge.message,
        expires_at: challenge.expires_at.to_rfc3339(),
    }))
}

/// `POST /auth/login`.
pub async fn login(
    State(state): State<Arc<AppState>>,
    client: Option<axum::Extension<ClientAddress>>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Json<LoginResponse>> {
    let address = eth::parse_checked_address(&request.address)
        .map_err(|e| ApiError::validation(e.to_string()))?;
    let result = state
        .auth
        .login(&address, &request.signature, request.handle.as_deref())
        .await?;
    describe(&state, &headers, client.as_deref(), &result);
    audit(&state, &result.user.user_id, "auth.login").await;
    Ok(Json(login_response(result)))
}

/// Attach the request context to the session just issued, for the user page's
/// session list.
///
/// The address comes from the [`ClientAddress`] the router resolved — the real
/// peer, unless a configured proxy forwarded it — and not from the request's
/// own `x-forwarded-for`. "Where you signed in from" is worth showing only if
/// the person who signed in could not choose what it says (finding 9).
fn describe(
    state: &AppState,
    headers: &HeaderMap,
    client: Option<&ClientAddress>,
    result: &Authenticated,
) {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    state.auth.describe_session(
        &result.user.user_id,
        &result.session_token,
        user_agent,
        ClientAddress::of(client),
    );
}

/// `POST /auth/dev-login` — mounted only when the gateway was started with
/// `--dev-login` and bound to a loopback address.
pub async fn dev_login(
    State(state): State<Arc<AppState>>,
    client: Option<axum::Extension<ClientAddress>>,
    headers: HeaderMap,
    Json(request): Json<DevLoginRequest>,
) -> ApiResult<Json<LoginResponse>> {
    let result = state
        .auth
        .dev_login(
            &request.mnemonic,
            request.index.unwrap_or(0),
            request.handle.as_deref(),
        )
        .await?;
    describe(&state, &headers, client.as_deref(), &result);
    tracing::warn!(
        target: "cb_gateway::http",
        user = %result.user.user_id,
        "development login used; a mnemonic was sent to the server"
    );
    audit(&state, &result.user.user_id, "auth.dev_login").await;
    Ok(Json(login_response(result)))
}

/// `POST /auth/logout` → `204`.
pub async fn logout(State(state): State<Arc<AppState>>, caller: Caller) -> ApiResult<Response> {
    state.auth.logout(&caller.token).await?;
    // The socket this session opened goes with it; a logged-out tab that keeps
    // receiving the account channel has not logged out (§14 of the flow
    // review).
    state
        .hub
        .disconnect_session(&crate::auth::session_token_hash(&caller.token));
    audit(&state, &caller.user.user_id, "auth.logout").await;
    Ok(super::no_content())
}

/// `GET /auth/session`.
pub async fn session(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<SessionInfo>> {
    let window = state.auth.session_window(&caller.token).await?;
    let (issued_at, expires_at) = window.ok_or_else(|| {
        // The extractor already authenticated, so a missing row here means the
        // session was deleted between the two reads.
        ApiError::unauthorized("missing or invalid session")
    })?;
    Ok(Json(SessionInfo {
        user_id: caller.user.user_id.clone(),
        handle: caller.handle.clone(),
        login_address: super::format_address(&caller.login_address),
        trading_address: super::format_address(&caller.trading_address),
        roles: caller.roles.clone(),
        issued_at: issued_at.to_rfc3339(),
        expires_at: expires_at.to_rfc3339(),
    }))
}

fn login_response(result: Authenticated) -> LoginResponse {
    LoginResponse {
        user_id: result.user.user_id,
        handle: result.user.handle,
        address: result.user.address,
        session_token: result.session_token,
        expires_at: result.expires_at.to_rfc3339(),
        is_new: result.is_new,
    }
}

/// Record an authentication event.
///
/// Best-effort: an audit write that fails must not fail the login it is
/// describing, but it must be visible in the logs.
async fn audit(state: &AppState, user_id: &str, action: &str) {
    let record = cb_ports::AuditRecord {
        record_id: format!("aud_{}", uuid::Uuid::new_v4().simple()),
        actor: Some(user_id.to_string()),
        action: action.to_string(),
        subject: user_id.to_string(),
        detail: serde_json::json!({}),
        source_ip: None,
        at: state.clock.now(),
    };
    if let Err(e) = state.db.audit().append(&record).await {
        tracing::warn!(target: "cb_gateway::http", error = %e, action, "audit write failed");
    }
}
