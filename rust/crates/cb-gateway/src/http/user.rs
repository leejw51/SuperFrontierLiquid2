//! §9 — the user page.
//!
//! A person's own account: who they are, what they have done, and which
//! sessions are open. It is deliberately distinct from `/account`, which is
//! about open risk right now: this is the page you read, that is the panel you
//! trade from.
//!
//! Everything here is scoped to the caller's own user id. There is no
//! parameter that names another account, which is the simplest way to be sure
//! one cannot be reached.

use super::dto::*;
use super::Caller;
use super::Json;
use crate::decimal::{format_port_decimal, format_scaled, format_scaled_signed};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use cb_ports::LeaderboardWindow;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Query for the activity feed.
#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    /// How many entries.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /user/profile`.
pub async fn profile(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<UserProfile>> {
    Ok(Json(build_profile(&state, &caller).await?))
}

/// `PATCH /user/profile`.
///
/// Only the handle is mutable: the address is the identity and changing it
/// would be creating a different account, not editing this one.
pub async fn update_profile(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Json(request): Json<UpdateProfileRequest>,
) -> ApiResult<Json<UserProfile>> {
    let Some(handle) = request.handle else {
        return Err(ApiError::validation(
            "only `handle` is mutable, and none was supplied",
        ));
    };
    let handle = state.auth.rename(&caller.user, &handle).await?;
    let caller = Caller { handle, ..caller };
    Ok(Json(build_profile(&state, &caller).await?))
}

/// `GET /user/sessions`.
pub async fn sessions(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<SessionsResponse>> {
    let current = crate::directory::Directory::session_id_for(
        &crate::auth::secrets::hash_session_token(&caller.token),
    );
    let sessions = state
        .auth
        .sessions_of(&caller.user.user_id)?
        .into_iter()
        .map(|meta| SessionSummary {
            current: meta.session_id == current,
            session_id: meta.session_id,
            issued_at: meta.issued_at.to_rfc3339(),
            expires_at: meta.expires_at.to_rfc3339(),
            last_used_at: meta.last_used_at.to_rfc3339(),
            user_agent: meta.user_agent,
            ip_prefix: meta.ip_prefix,
        })
        .collect();
    Ok(Json(SessionsResponse { sessions }))
}

/// `DELETE /user/sessions/{session_id}` → `204`.
pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Path(session_id): Path<String>,
) -> ApiResult<Response> {
    // Scoped to the caller's own id, so naming somebody else's session id
    // finds nothing rather than revoking it.
    let token_hash = state
        .auth
        .revoke_session(&caller.user.user_id, &session_id)
        .await?;
    // A revoked session must stop delivering private data on the socket it
    // opened, not merely on the next REST call (§14 of the flow review).
    let closed = state.hub.disconnect_session(&token_hash);
    if closed > 0 {
        tracing::info!(
            target: "cb_gateway::user",
            user = %caller.user.user_id,
            closed,
            "closed websocket connections belonging to a revoked session"
        );
    }
    Ok(super::no_content())
}

/// `POST /user/sessions/revoke-all`.
pub async fn revoke_all_sessions(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<RevokedResponse>> {
    let revoked = state
        .auth
        .revoke_other_sessions(&caller.user.user_id, &caller.token)
        .await?;
    // Every socket but the caller's own. "Revoke all sessions" that left the
    // other tabs receiving fills and balances would be revoking nothing that
    // matters.
    let keep = crate::auth::session_token_hash(&caller.token);
    let closed = state.hub.disconnect_user(&caller.user.user_id, Some(&keep));
    tracing::info!(
        target: "cb_gateway::user",
        user = %caller.user.user_id,
        revoked,
        closed,
        "revoked every other session and closed its sockets"
    );
    Ok(Json(RevokedResponse { revoked }))
}

/// `GET /user/activity`.
///
/// One newest-first feed across everything that happened to the account. The
/// per-type endpoints in §5 and §6 are what a table renders; this is what a
/// person reads.
pub async fn activity(
    State(state): State<Arc<AppState>>,
    caller: Caller,
    Query(query): Query<ActivityQuery>,
) -> ApiResult<Json<ActivityResponse>> {
    let limit = state.clamp_limit(query.limit, 100) as usize;
    let before = match query.cursor.as_deref() {
        Some(cursor) => Some(decode_cursor(cursor)?),
        None => None,
    };
    let catalog = state.markets.snapshot();

    let mut entries: Vec<ActivityEntry> = Vec::new();

    // Fills.
    for fill in state
        .tracker
        .fills_for_user(&caller.user.user_id, None, usize::MAX)
    {
        let (price_decimals, qty_decimals) = catalog
            .entry(&fill.symbol)
            .map(|e| (e.scales.price_decimals, e.scales.qty_decimals))
            .unwrap_or((8, 8));
        let notional = catalog
            .entry(&fill.symbol)
            .map(|entry| crate::orders::notional_micro_usd(entry, fill.price, fill.qty))
            .unwrap_or(0) as i128;
        let signed = match fill.side {
            cb_types::Side::Buy => -notional,
            cb_types::Side::Sell => notional,
        };
        entries.push(ActivityEntry {
            id: format!("act_{}", fill.fill_id),
            kind: "fill".into(),
            at: fill.at,
            summary: format!(
                "{} {} {} at {}",
                match fill.side {
                    cb_types::Side::Buy => "Bought",
                    cb_types::Side::Sell => "Sold",
                },
                format_scaled(fill.qty, qty_decimals, 4),
                fill.symbol,
                format_scaled(fill.price, price_decimals, 2),
            ),
            symbol: Some(fill.symbol.clone()),
            amount: format_scaled_signed(signed, 6, 2),
            asset: "USD".into(),
            tx_hash: None,
            height: Some(fill.height),
        });
    }

    // Ledger movements: faucet grants, funding, transfers, liquidations,
    // settlements — everything the off-chain ledger recorded.
    for entry in state
        .db
        .ledger()
        .for_user(&caller.user.user_id, None, 1_000)
        .await?
    {
        entries.push(ActivityEntry {
            id: format!("act_{}", entry.entry_id),
            kind: entry.kind.clone(),
            at: entry.at,
            summary: summarise_ledger(&entry),
            symbol: None,
            amount: format_port_decimal(entry.amount, 2),
            asset: entry.asset.clone(),
            tx_hash: entry.tx_hash.clone(),
            height: entry.height,
        });
    }

    entries.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    if let Some(before) = before {
        entries.retain(|entry| entry.at < before);
    }

    let has_more = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = has_more
        .then(|| entries.last().map(|entry| encode_cursor(entry.at)))
        .flatten();

    Ok(Json(ActivityResponse {
        entries: entries
            .into_iter()
            .map(|entry| ActivityItem {
                id: entry.id,
                kind: entry.kind,
                at: entry.at.to_rfc3339(),
                summary: entry.summary,
                symbol: entry.symbol,
                amount: entry.amount,
                asset: entry.asset,
                tx_hash: entry.tx_hash,
                height: entry.height,
            })
            .collect(),
        next_cursor,
    }))
}

/// One activity entry before rendering, so the merge can sort on a real time.
struct ActivityEntry {
    id: String,
    kind: String,
    at: DateTime<Utc>,
    summary: String,
    symbol: Option<String>,
    amount: String,
    asset: String,
    tx_hash: Option<String>,
    height: Option<u64>,
}

fn summarise_ledger(entry: &cb_ports::LedgerEntry) -> String {
    let amount = format_port_decimal(entry.amount.abs(), 2);
    match entry.kind.as_str() {
        "faucet" => format!("Claimed {amount} {} from the faucet", entry.asset),
        "transfer_in" => format!("Received {amount} {}", entry.asset),
        "transfer_out" => format!("Sent {amount} {}", entry.asset),
        "funding" => format!("Funding {amount} {}", entry.asset),
        "realized_pnl" => format!("Realised {amount} {}", entry.asset),
        "liquidation" => format!("Liquidated for {amount} {}", entry.asset),
        "settlement" => format!("Settled {amount} {}", entry.asset),
        "fee" => format!("Paid {amount} {} in fees", entry.asset),
        other => format!("{other} {amount} {}", entry.asset),
    }
}

/// Cursors are opaque to the client and cheap for the server: the instant of
/// the last entry returned, hex encoded so it does not look parseable.
fn encode_cursor(at: DateTime<Utc>) -> String {
    hex::encode(at.timestamp_millis().to_be_bytes())
}

fn decode_cursor(cursor: &str) -> ApiResult<DateTime<Utc>> {
    let bytes = hex::decode(cursor).map_err(|_| ApiError::validation("cursor is not valid"))?;
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| ApiError::validation("cursor is not valid"))?;
    DateTime::from_timestamp_millis(i64::from_be_bytes(raw))
        .ok_or_else(|| ApiError::validation("cursor is not valid"))
}

/// Assemble the profile, including the statistics the page shows.
pub async fn build_profile(state: &AppState, caller: &Caller) -> ApiResult<UserProfile> {
    let now = state.clock.now();
    let since = now - Duration::days(30);
    let catalog = state.markets.snapshot();

    let fills = state
        .tracker
        .fills_for_user(&caller.user.user_id, None, usize::MAX);
    let mut volume: i128 = 0;
    let mut markets: BTreeSet<String> = BTreeSet::new();
    let mut trades = 0u64;
    for fill in &fills {
        markets.insert(fill.symbol.clone());
        trades += 1;
        if fill.at >= since {
            if let Some(entry) = catalog.entry(&fill.symbol) {
                volume += crate::orders::notional_micro_usd(entry, fill.price, fill.qty) as i128;
            }
        }
    }

    let ledger = state
        .db
        .ledger()
        .for_user(&caller.user.user_id, None, 1_000)
        .await?;
    let mut realized: i128 = 0;
    let mut liquidations = 0u64;
    for entry in &ledger {
        if entry.kind == "liquidation" {
            liquidations += 1;
        }
        if entry.at >= since && matches!(entry.kind.as_str(), "realized_pnl" | "funding") {
            realized += (entry.amount * rust_decimal::Decimal::from(1_000_000))
                .round()
                .try_into()
                .unwrap_or(0i128);
        }
    }

    let faucet_claims = state.db.faucet().claim_count(&caller.user.user_id).await?;
    let best_rank = best_rank_of(state, &caller.handle).await;

    Ok(UserProfile {
        user_id: caller.user.user_id.clone(),
        handle: caller.handle.clone(),
        address: super::format_address(&caller.login_address),
        trading_address: super::format_address(&caller.trading_address),
        joined_at: caller.user.created_at.to_rfc3339(),
        last_seen_at: caller.user.last_seen_at.to_rfc3339(),
        is_bot: caller.user.is_bot,
        roles: caller.roles.clone(),
        stats: UserStats {
            trades,
            volume_30d: format_scaled_signed(volume, 6, 2),
            realized_pnl_30d: format_scaled_signed(realized, 6, 2),
            markets_traded: markets.len() as u64,
            liquidations,
            faucet_claims,
            best_rank,
        },
    })
}

/// The best rank the player holds across the stored windows.
async fn best_rank_of(state: &AppState, handle: &str) -> Option<u32> {
    let mut best: Option<u32> = None;
    for window in [
        LeaderboardWindow::Day,
        LeaderboardWindow::Week,
        LeaderboardWindow::AllTime,
    ] {
        if let Ok(Some(entry)) = state.db.leaderboard().for_handle(window, handle).await {
            best = Some(best.map_or(entry.rank, |current| current.min(entry.rank)));
        }
    }
    best
}
