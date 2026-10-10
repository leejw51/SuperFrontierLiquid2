//! §10 — the operator surface.
//!
//! ## The role is enforced here, not believed from the client
//!
//! Every handler in this module takes the [`Admin`](super::Admin) extractor,
//! which reads the role from the server's own record of the account. The
//! `roles` field in a session response exists so the frontend knows whether to
//! draw the admin navigation; the contract is explicit that it is not a
//! security boundary, and nothing here consults it.
//!
//! A non-admin session gets `403` with the same message any forbidden request
//! gets, so the response does not disclose that the endpoint exists for
//! somebody else.
//!
//! ## Everything that changes something is audited
//!
//! Each mutating handler writes an [`cb_ports::AuditRecord`] naming the acting
//! operator's **login address** — the thing a person actually holds — so "who
//! halted this market" always has an answer. The audit write happens after the
//! action succeeds and before the reply, and a failed audit write is loud:
//! an unaudited operator action is the one thing this module must not produce
//! quietly.
//!
//! ## Two behaviours worth reading the code for
//!
//! * **A halt still permits cancellation.** Halting stops new orders and
//!   leaves resting ones alone. Trapping somebody's order with no way out
//!   would be worse than whatever caused the halt, so the gateway's order
//!   validation refuses placement on a halted market while the cancel path
//!   does not consult market status at all.
//! * **Raising margin says who it breaks.** A margin increase can put open
//!   accounts below maintenance the moment it lands. The response counts them,
//!   so the operator finds out before the liquidation engine does.

use super::dto::*;
use super::Json;
use super::{format_address, resolve_market, Admin};
use crate::decimal::{format_port_decimal, format_scaled, parse_scaled};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::faucet::Grant;
use crate::node::NodeClient;
use crate::sim::SimCommand;
use crate::state::AppState;
use crate::symbols::{InstrumentKind, MarketRef};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use cb_ports::AuditRecord;
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Write an audit record for an operator action.
///
/// The acting operator's login address is the actor, because that is what a
/// person controls and what an investigation can act on.
async fn audit(
    state: &AppState,
    admin: &Admin,
    action: &str,
    subject: &str,
    detail: serde_json::Value,
) {
    let record = AuditRecord {
        record_id: format!("aud_{}", uuid::Uuid::new_v4().simple()),
        actor: Some(format_address(&admin.0.login_address)),
        action: action.to_string(),
        subject: subject.to_string(),
        detail,
        source_ip: None,
        at: state.clock.now(),
    };
    if let Err(e) = state.db.audit().append(&record).await {
        // Loud on purpose. An operator action nobody can trace is the failure
        // this module exists to prevent.
        tracing::error!(
            target: "cb_gateway::admin",
            error = %e,
            action,
            subject,
            actor = %format_address(&admin.0.login_address),
            "AUDIT WRITE FAILED for an operator action"
        );
    }
}

/// What an operator control that lives in this process is called on the wire.
///
/// Halting a market and changing its risk parameters are **gateway-local**.
/// The chain has no transaction for either — `PerpMarket.paused` is set in
/// genesis and the executor margins against its own table — so an override
/// changes what this gateway accepts and publishes, and binds nothing else. It
/// is labelled everywhere it appears rather than presented as an exchange-wide
/// action, because an operator who believes a halt stopped the market when the
/// book is still matching will make the next decision on that belief (§19 of
/// the flow review).
const GATEWAY_LOCAL: &str = "gateway_local";

/// What a halt actually does, in one sentence.
const HALT_ENFORCEMENT: &str = "this gateway refuses new orders on the market and publishes it \
     as halted. Resting orders stay on the book, cancellation stays allowed, and any client \
     trading the sequencer's RPC directly is unaffected. It does not survive a gateway restart.";

/// What a risk change actually does, in one sentence.
const RISK_ENFORCEMENT: &str = "this gateway validates new orders and renders liquidation prices \
     against these numbers. The executor gates margin on its own table and does not read them, \
     so `affected` is who the change would break if the chain enforced it. It does not survive \
     a gateway restart.";

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

/// `GET /admin/overview` — the dashboard's single fetch.
pub async fn overview(
    State(state): State<Arc<AppState>>,
    admin: Admin,
) -> ApiResult<Json<AdminOverview>> {
    let _ = &admin;
    let now = state.clock.now();
    let health = state.node.health();
    let catalog = state.markets.snapshot();

    let chain = ChainSummary {
        height: health.last_height,
        block_time_ms: state.settings.block_time_ms,
        tps_1m: format!("{:.1}", state.chain_log.tps_1m(now)),
        // Neither is queryable over the node's RPC; see the gaps in the
        // module docs of `crate::http::chain`.
        mempool_depth: 0,
        validators: 0,
        synced: health.is_synced(),
        state_root: state
            .chain_log
            .recent(1)
            .first()
            .map(|b| b.state_root.clone())
            .unwrap_or_else(|| format!("0x{}", "0".repeat(64))),
        uptime_seconds: (now - state.started_at).num_seconds().max(0) as u64,
    };

    let all_users = state.db.users().list(true, 100_000).await?;
    let day_ago = now - Duration::hours(24);
    let users = UserCounts {
        total: all_users.len() as u64,
        humans: all_users.iter().filter(|u| !u.is_bot).count() as u64,
        bots: all_users.iter().filter(|u| u.is_bot).count() as u64,
        active_24h: all_users
            .iter()
            .filter(|u| u.last_seen_at >= day_ago)
            .count() as u64,
        new_24h: all_users.iter().filter(|u| u.created_at >= day_ago).count() as u64,
    };

    let markets = MarketCounts {
        total: catalog.metas.len() as u64,
        trading: catalog
            .metas
            .values()
            .filter(|m| m.status == "trading")
            .count() as u64,
        halted: catalog
            .metas
            .values()
            .filter(|m| m.status == "halted")
            .count() as u64,
        expired: catalog
            .metas
            .values()
            .filter(|m| m.status == "expired")
            .count() as u64,
    };

    let mut volume_24h: i128 = 0;
    let mut trades_24h: u64 = 0;
    let mut fees_24h: i128 = 0;
    for user in &all_users {
        for fill in state
            .tracker
            .fills_for_user(&user.user_id, None, usize::MAX)
        {
            if fill.at < day_ago {
                continue;
            }
            trades_24h += 1;
            fees_24h += fill.fee as i128;
            if let Some(entry) = catalog.entry(&fill.symbol) {
                volume_24h +=
                    crate::orders::notional_micro_usd(entry, fill.price, fill.qty) as i128;
            }
        }
    }

    // One claim writes one row per granted asset, so counting rows counts
    // assets. The default grant is two, which reported every figure here at
    // twice the usage — and would have scaled with the grant's size rather than
    // with how much the faucet was actually used. The user's own `claims_made`
    // counts claims, and the two must agree.
    let mut claims_24h: BTreeSet<String> = BTreeSet::new();
    let mut granted_24h: BTreeMap<String, rust_decimal::Decimal> = BTreeMap::new();
    for user in &all_users {
        for claim in state.db.faucet().history(&user.user_id, 1_000).await? {
            if claim.claimed_at >= day_ago {
                *granted_24h.entry(claim.asset.clone()).or_default() += claim.amount;
                claims_24h.insert(claim.claim_id.clone());
            }
        }
    }
    let claims_24h = claims_24h.len() as u64;
    let total_granted = state.db.faucet().total_granted().await?;

    let (open_interest, below_maintenance) = risk_totals(&state, &all_users).await;

    Ok(Json(AdminOverview {
        chain,
        users,
        markets,
        volume: VolumeSummary {
            volume_24h: crate::decimal::format_scaled_signed(volume_24h, 6, 2),
            trades_24h,
            fees_24h: crate::decimal::format_scaled_signed(fees_24h, 6, 2),
        },
        faucet: FaucetSummary {
            claims_24h,
            granted_24h: granted_24h
                .into_iter()
                .map(|(asset, amount)| AssetAmount {
                    asset,
                    amount: format_port_decimal(amount, 2),
                })
                .collect(),
            total_granted: total_granted
                .into_iter()
                .map(|(asset, amount)| AssetAmount {
                    asset,
                    amount: format_port_decimal(amount, 2),
                })
                .collect(),
        },
        risk: RiskSummary {
            open_interest: crate::decimal::format_scaled_signed(open_interest, 6, 2),
            // The chain exposes no insurance-fund query.
            insurance_fund: "0.00".into(),
            accounts_below_maintenance: below_maintenance,
            // Liquidations are only counted once the gateway has observed them
            // in a user's ledger.
            liquidations_24h: liquidations_since(&state, &all_users, day_ago).await,
        },
        health: component_health(&state).await,
    }))
}

/// Open interest across every account the gateway knows, and how many are
/// below maintenance margin.
async fn risk_totals(state: &AppState, users: &[cb_ports::User]) -> (i128, u64) {
    let catalog = state.markets.snapshot();
    let mut open_interest: i128 = 0;
    let mut below = 0u64;
    for user in users {
        let Ok((_key, address)) = state.auth.trading_key_of(user) else {
            continue;
        };
        let Ok(positions) = state.node.positions(address).await else {
            continue;
        };
        let mut notional: i128 = 0;
        let mut maintenance: i128 = 0;
        for position in positions.iter().filter(|p| p.qty > 0) {
            let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(position.market)) else {
                continue;
            };
            let mark = state
                .node
                .mark_price(position.market)
                .await
                .map(|m| m.px_q8)
                .unwrap_or(position.entry_px_q8);
            let value = crate::orders::notional_micro_usd(entry, mark, position.qty) as i128;
            notional += value;
            let bps = catalog
                .meta(&entry.symbol)
                .and_then(|m| m.maintenance_margin_bps)
                .unwrap_or(0) as i128;
            maintenance += value.saturating_mul(bps) / 10_000;
        }
        open_interest += notional;
        if maintenance > 0 {
            if let Ok(margin) = state.node.margin_account(address).await {
                let equity = margin.collateral as i128 + margin.unrealized_pnl;
                if equity < maintenance {
                    below += 1;
                }
            }
        }
    }
    (open_interest, below)
}

async fn liquidations_since(
    state: &AppState,
    users: &[cb_ports::User],
    since: chrono::DateTime<Utc>,
) -> u64 {
    let kinds = vec!["liquidation".to_string()];
    let mut total = 0;
    for user in users {
        if let Ok(entries) = state
            .db
            .ledger()
            .for_user(&user.user_id, Some(&kinds), 1_000)
            .await
        {
            total += entries.into_iter().filter(|e| e.at >= since).count() as u64;
        }
    }
    total
}

async fn component_health(state: &AppState) -> Vec<ComponentHealth> {
    let node = state.node.health();
    let mut components = vec![ComponentHealth {
        component: "node".into(),
        status: if node.connected { "ok" } else { "down" }.into(),
        detail: Some(format!("height {}", node.last_height)),
    }];
    components.push(match state.db.schema_version().await {
        Ok(version) => ComponentHealth {
            component: "database".into(),
            status: "ok".into(),
            detail: Some(format!("schema {version}")),
        },
        Err(e) => ComponentHealth {
            component: "database".into(),
            status: "down".into(),
            detail: Some(e.to_string()),
        },
    });
    components.push(match state.bus.health_check().await {
        Ok(()) => ComponentHealth {
            component: "bus".into(),
            status: "ok".into(),
            detail: None,
        },
        Err(e) => ComponentHealth {
            component: "bus".into(),
            status: "down".into(),
            detail: Some(e.to_string()),
        },
    });
    components.push(match state.feed.health_check().await {
        Ok(()) => ComponentHealth {
            component: "feed".into(),
            status: "ok".into(),
            detail: None,
        },
        // The feed is not load-bearing for anything the gateway serves, so a
        // failure is `degraded` rather than `down`.
        Err(e) => ComponentHealth {
            component: "feed".into(),
            status: "degraded".into(),
            detail: Some(e.to_string()),
        },
    });
    components
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// Query for `GET /admin/users`.
#[derive(Debug, Deserialize)]
pub struct UserQuery {
    /// Matches a handle or an address.
    #[serde(default)]
    pub query: Option<String>,
    /// Restrict to bots or to humans.
    #[serde(default)]
    pub is_bot: Option<bool>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /admin/users`.
pub async fn users(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Query(query): Query<UserQuery>,
) -> ApiResult<Json<AdminUsersResponse>> {
    let _ = &admin;
    let limit = state.clamp_limit(query.limit, 100) as usize;
    let needle = query.query.as_deref().map(str::to_ascii_lowercase);
    let day_ago = state.clock.now() - Duration::hours(24);

    let mut all = state.db.users().list(true, 100_000).await?;
    all.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.user_id.cmp(&b.user_id))
    });
    if let Some(cursor) = query.cursor.as_deref() {
        // The cursor is the last user id of the previous page.
        if let Some(position) = all.iter().position(|u| u.user_id == cursor) {
            all.drain(..=position);
        }
    }

    let prices = state.leaderboard.mark_prices().await;
    let mut rows = Vec::new();
    for user in all {
        if let Some(is_bot) = query.is_bot {
            if user.is_bot != is_bot {
                continue;
            }
        }
        let handle = state.auth.effective_handle(&user);
        if let Some(needle) = &needle {
            let matches = handle.to_ascii_lowercase().contains(needle)
                || user.address.to_ascii_lowercase().contains(needle);
            if !matches {
                continue;
            }
        }
        if rows.len() > limit {
            break;
        }

        let (equity, open_positions) = match state.auth.trading_key_of(&user) {
            Ok((_key, address)) => {
                let equity = state
                    .leaderboard
                    .equity_of(address, &prices)
                    .await
                    .unwrap_or(0);
                let positions = state
                    .node
                    .positions(address)
                    .await
                    .map(|p| p.iter().filter(|p| p.qty > 0).count() as u64)
                    .unwrap_or(0);
                (equity, positions)
            }
            Err(_) => (0, 0),
        };
        let trades_24h = state
            .tracker
            .fills_for_user(&user.user_id, None, usize::MAX)
            .into_iter()
            .filter(|f| f.at >= day_ago)
            .count() as u64;

        rows.push(AdminUserRow {
            user_id: user.user_id.clone(),
            handle,
            address: user.address.clone(),
            is_bot: user.is_bot,
            joined_at: user.created_at.to_rfc3339(),
            last_seen_at: user.last_seen_at.to_rfc3339(),
            equity: crate::decimal::format_scaled_signed(equity, 6, 2),
            open_positions,
            trades_24h,
            faucet_claims: state.db.faucet().claim_count(&user.user_id).await?,
            status: if state.auth.directory().is_suspended(&user.user_id) {
                "suspended".into()
            } else {
                "active".into()
            },
        });
    }

    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = has_more
        .then(|| rows.last().map(|row| row.user_id.clone()))
        .flatten();

    Ok(Json(AdminUsersResponse {
        users: rows,
        next_cursor,
    }))
}

/// `GET /admin/users/{user_id}` — the full record.
pub async fn user_detail(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(user_id): Path<String>,
) -> ApiResult<Json<AdminUserDetail>> {
    let _ = &admin;
    let user = state
        .db
        .users()
        .by_id(&user_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no user `{user_id}`")))?;

    let (_key, trading_address) = state.auth.trading_key_of(&user)?;
    let account = state
        .node
        .account(trading_address)
        .await
        .unwrap_or_default();
    let balances = account
        .balances
        .iter()
        .map(|(asset, total)| Balance {
            asset: asset.symbol().to_string(),
            // The chain's balance is the total; free is what is left after the
            // locks against it.
            free: super::market::asset_amount(*asset, account.available(*asset)),
            locked: super::market::asset_amount(*asset, account.locked_of(*asset)),
            total: super::market::asset_amount(*asset, *total),
        })
        .collect();

    let positions = state
        .node
        .positions(trading_address)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.qty > 0)
        .map(|p| {
            let catalog = state.markets.snapshot();
            let symbol = catalog
                .symbols
                .symbol_for(MarketRef::Deriv(p.market))
                .unwrap_or("unknown")
                .to_string();
            json!({
                "symbol": symbol,
                "side": match p.side { cb_types::Side::Buy => "long", cb_types::Side::Sell => "short" },
                "qty": format_scaled(p.qty, 8, 4),
                "entry_price": format_scaled(p.entry_px_q8, 8, 2),
            })
        })
        .collect();

    let activity = state
        .db
        .ledger()
        .for_user(&user_id, None, 50)
        .await?
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.entry_id,
                "kind": entry.kind,
                "asset": entry.asset,
                "amount": format_port_decimal(entry.amount, 2),
                "at": entry.at.to_rfc3339(),
            })
        })
        .collect();

    let audit_trail = state
        .db
        .audit()
        .for_subject(&user_id, 50)
        .await?
        .into_iter()
        .map(render_audit)
        .collect();

    Ok(Json(AdminUserDetail {
        user_id: user.user_id.clone(),
        handle: state.auth.effective_handle(&user),
        address: user.address.clone(),
        trading_address: format_address(&trading_address),
        is_bot: user.is_bot,
        joined_at: user.created_at.to_rfc3339(),
        last_seen_at: user.last_seen_at.to_rfc3339(),
        status: if state.auth.directory().is_suspended(&user_id) {
            "suspended".into()
        } else {
            "active".into()
        },
        suspension_reason: state
            .auth
            .directory()
            .suspension(&user_id)?
            .map(|s| s.reason),
        nonce: account.nonce,
        balances,
        positions,
        activity,
        audit: audit_trail,
    }))
}

/// `POST /admin/users/{user_id}/suspend`.
pub async fn suspend(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(user_id): Path<String>,
    Json(request): Json<SuspendRequest>,
) -> ApiResult<Response> {
    let user = state
        .db
        .users()
        .by_id(&user_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no user `{user_id}`")))?;
    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::validation("a suspension needs a reason"));
    }

    state
        .auth
        .directory()
        .suspend(&crate::directory::Suspension {
            user_id: user.user_id.clone(),
            reason: reason.to_string(),
            by: format_address(&admin.0.login_address),
            at: state.clock.now(),
        })?;
    // The account's sockets stay open on purpose. A suspension stops it
    // trading and claiming; it does not stop it watching the position it is
    // still carrying, which the contract leaves open. See `crate::http::Caller`
    // for where the refusal actually happens.
    // Existing positions stay open. Force-closing somebody's position is a
    // different and much larger decision than stopping them trading, and the
    // contract says so explicitly.
    audit(
        &state,
        &admin,
        "user.suspend",
        &user_id,
        json!({ "reason": reason }),
    )
    .await;
    Ok(super::no_content())
}

/// `POST /admin/users/{user_id}/unsuspend` → `204`.
pub async fn unsuspend(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(user_id): Path<String>,
) -> ApiResult<Response> {
    state.auth.directory().unsuspend(&user_id)?;
    audit(&state, &admin, "user.unsuspend", &user_id, json!({})).await;
    Ok(super::no_content())
}

// ---------------------------------------------------------------------------
// Markets
// ---------------------------------------------------------------------------

/// `GET /admin/markets`.
pub async fn markets(
    State(state): State<Arc<AppState>>,
    admin: Admin,
) -> ApiResult<Json<AdminMarketsResponse>> {
    let _ = &admin;
    let catalog = state.markets.snapshot();
    let head = state.head_height();
    let mut rows = Vec::new();

    for entry in catalog.symbols.entries() {
        let Some(meta) = catalog.meta(&entry.symbol) else {
            continue;
        };
        let book = super::market::fetch_book(&state, entry, state.settings.max_depth)
            .await
            .unwrap_or_default();
        let resting = (book.bids.len() + book.asks.len()) as u64;

        let (mark, oracle_age) = match entry.market {
            MarketRef::Deriv(id) => {
                let mark = state.node.mark_price(id).await.ok();
                let age = mark
                    .map(|m| head.saturating_sub(m.computed_at_height))
                    .unwrap_or(u64::MAX);
                (mark.map(|m| m.px_q8).unwrap_or(0), age)
            }
            MarketRef::Spot(_) => (0, 0),
        };
        let last = state.marketdata.last_price(&entry.symbol).unwrap_or(mark);

        rows.push(AdminMarketRow {
            symbol: entry.symbol.clone(),
            kind: meta.kind.clone(),
            status: meta.status.clone(),
            last_price: format_scaled(last, entry.scales.price_decimals, meta.price_decimals),
            mark_price: format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
            open_interest: "0.00".into(),
            oracle_age_blocks: if oracle_age == u64::MAX {
                None
            } else {
                Some(oracle_age)
            },
            oracle_fresh: state.mark_is_fresh(head.saturating_sub(oracle_age)),
            resting_levels: resting,
            overridden: state.markets.override_for(&entry.symbol).is_some(),
            override_scope: state
                .markets
                .override_for(&entry.symbol)
                .map(|_| GATEWAY_LOCAL.to_string()),
        });
    }

    Ok(Json(AdminMarketsResponse { markets: rows }))
}

/// `POST /admin/markets/{symbol}/halt`.
pub async fn halt_market(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(symbol): Path<String>,
    Json(request): Json<HaltRequest>,
) -> ApiResult<Response> {
    let symbol = {
        let catalog = state.markets.snapshot();
        resolve_market(&catalog, &symbol)?.0.symbol.clone()
    };
    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::validation("a halt needs a reason"));
    }

    // Placement is refused on a halted market; cancellation is not, because
    // the cancel path never consults market status.
    state
        .markets
        .amend_override(&symbol, |patch| patch.status = Some("halted".into()));
    audit(
        &state,
        &admin,
        "market.halt",
        &symbol,
        json!({
            "reason": reason,
            "scope": GATEWAY_LOCAL,
            "enforcement": HALT_ENFORCEMENT,
        }),
    )
    .await;
    // Loud, and specific about what it is not. An operator who halts a market
    // during an incident needs to know in the moment that the book is still
    // matching for everyone who is not coming through this process.
    tracing::warn!(
        target: "cb_gateway::admin",
        symbol = %symbol,
        reason,
        scope = GATEWAY_LOCAL,
        "market halted at the gateway only: the chain has no halt, so direct RPC \
         clients keep trading it, and this halt does not survive a restart"
    );
    Ok(super::no_content())
}

/// `POST /admin/markets/{symbol}/resume` → `204`.
pub async fn resume_market(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(symbol): Path<String>,
) -> ApiResult<Response> {
    let symbol = {
        let catalog = state.markets.snapshot();
        resolve_market(&catalog, &symbol)?.0.symbol.clone()
    };
    state
        .markets
        .amend_override(&symbol, |patch| patch.status = None);
    audit(
        &state,
        &admin,
        "market.resume",
        &symbol,
        json!({ "scope": GATEWAY_LOCAL, "enforcement": HALT_ENFORCEMENT }),
    )
    .await;
    tracing::warn!(
        target: "cb_gateway::admin",
        symbol = %symbol,
        scope = GATEWAY_LOCAL,
        "market resumed at the gateway"
    );
    Ok(super::no_content())
}

/// `POST /admin/markets/{symbol}/settle`.
///
/// **This gateway cannot force-settle a market, and says so rather than
/// pretending.** Three things were wrong with the version that tried (§18 of
/// the flow review):
///
/// * `TxBody::FuturesSettle` carries a market and **no price**. The chain
///   computes settlement from its own oracle and TWAP, so the
///   `settlement_price` the contract documents — "closes every position at the
///   given price" — was parsed, validated, and then thrown away.
/// * `apply_futures_settle` requires `sender == ctx.proposer_addr`. The gateway
///   signed with the operator's own trading key, which can never match, so the
///   transaction failed `NotAuthorized` every time — and a failing transaction
///   currently takes its whole block with it.
/// * The audit record was written unconditionally, *after* submission, saying
///   the market had settled at a price that was never used. The one place an
///   investigation would look recorded a settlement that did not happen.
///
/// So the request is validated in full — the market, its status, the price, the
/// mark it is checked against — and then refused with `501 NOT_SUPPORTED`, and
/// the refusal is what gets audited. The validation is kept rather than dropped
/// because it is the part an operator can act on: it tells them whether the
/// settlement they wanted was sane, and `docs/api-contract.md` §10 now records
/// that settling a dated future is the chain's own business, at expiry.
pub async fn settle_market(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(symbol): Path<String>,
    Json(request): Json<SettleRequest>,
) -> ApiResult<Json<TxResponse>> {
    let catalog = state.markets.snapshot();
    let (entry, meta) = resolve_market(&catalog, &symbol)?;
    if entry.kind != InstrumentKind::Future {
        return Err(ApiError::validation(
            "only a dated future can be force-settled",
        ));
    }
    let MarketRef::Deriv(market) = entry.market else {
        return Err(ApiError::validation("that market has no chain handle"));
    };
    if !matches!(meta.status.as_str(), "expired" | "halted" | "settling") {
        return Err(ApiError::new(
            ErrorCode::MarketHalted,
            "a future must be expired or halted before it can be force-settled",
        ));
    }

    let price = parse_scaled(&request.settlement_price, entry.scales.price_decimals)
        .map_err(|e| ApiError::validation(format!("settlement_price: {e}")))?;
    if price == 0 {
        return Err(ApiError::validation(
            "settlement_price must be greater than zero",
        ));
    }

    // A settlement price is only checkable against a mark, and a market with no
    // mark has nothing to check it against. Refusing here is what stops the
    // "within 10% of the mark" guard from passing vacuously on a chain whose
    // oracle has never posted — zero divided by zero is not a deviation of
    // nought percent, and both the band check and the force prompt that hangs
    // off it fail open when it is treated as one.
    let mark = state
        .node
        .mark_price(market)
        .await
        .ok()
        .filter(|m| m.px_q8 > 0 && state.mark_is_fresh(m.computed_at_height));
    let Some(mark) = mark else {
        return Err(ApiError::new(
            ErrorCode::OracleStale,
            format!(
                "{} has no fresh mark price, so a settlement price cannot be checked against \
                 anything; post an oracle update first",
                entry.symbol
            ),
        ));
    };
    let mark = mark.px_q8;

    // A settlement price far from the market's own mark is far more likely to
    // be a typo than a decision, and this is irreversible.
    let deviation = price.abs_diff(mark).saturating_mul(10_000) / mark;
    if deviation > 1_000 && !request.force {
        return Err(ApiError::validation(format!(
            "settlement price is {}% from the last mark of {}; send \"force\": true if that is intended",
            deviation / 100,
            format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
        ))
        .with_details(json!({
            "mark_price": format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
            "deviation_bps": deviation,
        })));
    }

    // Audited as what it is: an operator asked for a settlement and did not get
    // one. Written before the reply so the record exists whether or not the
    // caller is still listening.
    audit(
        &state,
        &admin,
        "market.settle",
        &entry.symbol,
        json!({
            "outcome": "refused",
            "reason": "the chain settles a dated future itself, at expiry, from its own \
                       oracle and TWAP; FuturesSettle carries no price and is accepted only \
                       from the block proposer",
            "settlement_price": request.settlement_price,
            "forced": request.force,
            "mark_price": format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
            "deviation_bps": deviation,
        }),
    )
    .await;

    tracing::warn!(
        target: "cb_gateway::admin",
        symbol = %entry.symbol,
        actor = %format_address(&admin.0.login_address),
        "force-settle refused: this chain accepts FuturesSettle only from the block proposer"
    );

    Err(ApiError::new(
        ErrorCode::NotSupported,
        format!(
            "{} cannot be force-settled through the gateway. On this chain a dated future \
             settles itself at its expiry height, at a price the engine computes from the \
             oracle and the mark TWAP: the settlement transaction carries no price and is \
             accepted only from the block proposer. The request was recorded in the audit \
             trail as refused.",
            entry.symbol
        ),
    )
    .with_details(json!({
        "settlement_price": request.settlement_price,
        "mark_price": format_scaled(mark, entry.scales.price_decimals, meta.price_decimals),
        "expires_at": meta.expiry.map(|at| at.to_rfc3339()),
    })))
}

/// Query for `PATCH /admin/markets/{symbol}/risk`.
#[derive(Debug, Deserialize)]
pub struct RiskQuery {
    /// Compute the consequence and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

/// `PATCH /admin/markets/{symbol}/risk`.
///
/// Raising a margin requirement can put open accounts below maintenance the
/// moment it lands, so the response names them. `?dry_run=true` computes
/// exactly the same answer and changes nothing: an operator should be able to
/// find out what a change would do without doing it, and a bare count does not
/// say whom to warn.
pub async fn set_risk(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Path(symbol): Path<String>,
    Query(query): Query<RiskQuery>,
    Json(request): Json<RiskRequest>,
) -> ApiResult<Json<RiskResponse>> {
    let symbol = {
        let catalog = state.markets.snapshot();
        let (entry, _meta) = resolve_market(&catalog, &symbol)?;
        if entry.kind == InstrumentKind::Spot {
            return Err(ApiError::validation(
                "spot markets carry no margin requirements",
            ));
        }
        entry.symbol.clone()
    };

    let max_leverage = match request.max_leverage.as_deref() {
        Some(text) => {
            let scaled = parse_scaled(text, 2)
                .map_err(|e| ApiError::validation(format!("max_leverage: {e}")))?;
            if scaled == 0 {
                return Err(ApiError::validation("max_leverage must be positive"));
            }
            Some(
                crate::decimal::to_port_decimal(scaled, 2)
                    .ok_or_else(|| ApiError::validation("max_leverage is out of range"))?,
            )
        }
        None => None,
    };
    for (name, value) in [
        ("initial_margin_bps", request.initial_margin_bps),
        ("maintenance_margin_bps", request.maintenance_margin_bps),
    ] {
        if let Some(bps) = value {
            if bps == 0 || bps > 10_000 {
                return Err(ApiError::validation(format!(
                    "{name} must be between 1 and 10000"
                )));
            }
        }
    }
    if let (Some(initial), Some(maintenance)) =
        (request.initial_margin_bps, request.maintenance_margin_bps)
    {
        if maintenance > initial {
            return Err(ApiError::validation(
                "maintenance margin may not exceed initial margin",
            ));
        }
    }

    // Computed against the *proposed* parameters before anything is written,
    // so a dry run and a real change return the same answer.
    let affected = affected_accounts(&state, &symbol, request.maintenance_margin_bps).await?;

    if !query.dry_run {
        state.markets.amend_override(&symbol, |patch| {
            if max_leverage.is_some() {
                patch.max_leverage = max_leverage;
            }
            if request.initial_margin_bps.is_some() {
                patch.initial_margin_bps = request.initial_margin_bps;
            }
            if request.maintenance_margin_bps.is_some() {
                patch.maintenance_margin_bps = request.maintenance_margin_bps;
            }
        });

        audit(
            &state,
            &admin,
            "market.risk",
            &symbol,
            json!({
                "max_leverage": request.max_leverage,
                "initial_margin_bps": request.initial_margin_bps,
                "maintenance_margin_bps": request.maintenance_margin_bps,
                "accounts_now_below_maintenance": affected.len(),
                "affected": affected.iter().map(|a| &a.user_id).collect::<Vec<_>>(),
                "scope": GATEWAY_LOCAL,
                "enforcement": RISK_ENFORCEMENT,
            }),
        )
        .await;
        tracing::warn!(
            target: "cb_gateway::admin",
            symbol = %symbol,
            scope = GATEWAY_LOCAL,
            below_maintenance = affected.len(),
            "risk parameters changed at the gateway only: the executor margins \
             against its own table and does not read these numbers, and the change \
             does not survive a restart"
        );
    }

    Ok(Json(RiskResponse {
        applied: !query.dry_run,
        accounts_now_below_maintenance: affected.len() as u64,
        affected,
        scope: GATEWAY_LOCAL.to_string(),
        enforcement: RISK_ENFORCEMENT.to_string(),
    }))
}

/// The accounts a maintenance-margin change leaves below maintenance.
///
/// Every account below maintenance *after* the change is reported, with the
/// ratio before it as well as after. That is a superset of "newly below", and
/// the before-ratio distinguishes the two: an account already above 1.0 before
/// the change was in trouble regardless, and one that crosses 1.0 is the
/// operator's doing. A list of only the newly-broken would hide the first
/// group at exactly the moment somebody is looking at risk.
///
/// The ratio is the maintenance requirement over account equity, so `1.0` is
/// the liquidation boundary and anything above it is past it.
async fn affected_accounts(
    state: &AppState,
    symbol: &str,
    proposed_maintenance_bps: Option<u32>,
) -> ApiResult<Vec<AffectedAccount>> {
    let catalog = state.markets.snapshot();
    let mut affected = Vec::new();

    for user in state.db.users().list(true, 100_000).await? {
        let Ok((_key, address)) = state.auth.trading_key_of(&user) else {
            continue;
        };
        let Ok(positions) = state.node.positions(address).await else {
            continue;
        };
        let open: Vec<_> = positions.into_iter().filter(|p| p.qty > 0).collect();
        if open.is_empty() {
            continue;
        }

        let mut before: i128 = 0;
        let mut after: i128 = 0;
        for position in &open {
            let Some(entry) = catalog.symbols.by_market(MarketRef::Deriv(position.market)) else {
                continue;
            };
            let Some(meta) = catalog.meta(&entry.symbol) else {
                continue;
            };
            let mark = state
                .node
                .mark_price(position.market)
                .await
                .map(|m| m.px_q8)
                .unwrap_or(position.entry_px_q8);
            let notional = crate::orders::notional_micro_usd(entry, mark, position.qty) as i128;
            let current_bps = meta.maintenance_margin_bps.unwrap_or(0) as i128;
            // Only the market being changed moves; the rest keep their own
            // requirement in both figures.
            let proposed_bps = if entry.symbol.eq_ignore_ascii_case(symbol) {
                proposed_maintenance_bps
                    .map(|bps| bps as i128)
                    .unwrap_or(current_bps)
            } else {
                current_bps
            };
            before += notional.saturating_mul(current_bps) / 10_000;
            after += notional.saturating_mul(proposed_bps) / 10_000;
        }
        if after <= 0 {
            continue;
        }

        let equity = match state.node.margin_account(address).await {
            Ok(margin) => margin.collateral as i128 + margin.unrealized_pnl,
            Err(_) => continue,
        };
        if equity >= after {
            continue;
        }
        affected.push(AffectedAccount {
            user_id: user.user_id.clone(),
            handle: state.auth.effective_handle(&user),
            margin_ratio_before: crate::decimal::format_ratio(before, equity.max(1), 3),
            margin_ratio_after: crate::decimal::format_ratio(after, equity.max(1), 3),
        });
    }
    Ok(affected)
}

// ---------------------------------------------------------------------------
// Faucet
// ---------------------------------------------------------------------------

/// `GET /admin/faucet`.
pub async fn faucet_status(
    State(state): State<Arc<AppState>>,
    admin: Admin,
) -> ApiResult<Json<AdminFaucetResponse>> {
    let _ = &admin;
    let config = state.faucet.config();
    let day_ago = state.clock.now() - Duration::hours(24);

    // Claims, not rows: a two-asset grant is one claim. See `overview`.
    let mut claims_24h: BTreeSet<String> = BTreeSet::new();
    for user in state.db.users().list(true, 100_000).await? {
        for claim in state.db.faucet().history(&user.user_id, 1_000).await? {
            if claim.claimed_at >= day_ago {
                claims_24h.insert(claim.claim_id.clone());
            }
        }
    }
    let claims_24h = claims_24h.len() as u64;

    // A faucet that has run dry looks exactly like a broken faucet to a user,
    // so the dashboard shows what is left.
    let authority_balance = state
        .faucet
        .authority_balance(state.node.as_ref())
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(asset, amount)| AssetAmount {
            asset: asset.symbol().to_string(),
            amount: super::market::asset_amount(asset, amount),
        })
        .collect();

    Ok(Json(AdminFaucetResponse {
        enabled: config.enabled,
        cooldown_seconds: config.cooldown_seconds,
        grant: config
            .grants
            .iter()
            .map(|g| AssetAmount {
                asset: g.asset.symbol().to_string(),
                amount: g.amount_string(),
            })
            .collect(),
        max_claims_per_user: config.max_claims_per_user,
        claims_24h,
        granted_total: state
            .db
            .faucet()
            .total_granted()
            .await?
            .into_iter()
            .map(|(asset, amount)| AssetAmount {
                asset,
                amount: format_port_decimal(amount, 2),
            })
            .collect(),
        authority_balance,
    }))
}

/// `PATCH /admin/faucet`.
pub async fn configure_faucet(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Json(request): Json<AdminFaucetPatch>,
) -> ApiResult<Json<AdminFaucetResponse>> {
    let mut config = state.faucet.config();
    if let Some(enabled) = request.enabled {
        config.enabled = enabled;
    }
    if let Some(cooldown) = request.cooldown_seconds {
        config.cooldown_seconds = cooldown;
    }
    if let Some(max) = request.max_claims_per_user {
        config.max_claims_per_user = max;
    }
    if let Some(grants) = &request.grant {
        config.grants = parse_grants(grants)?;
        if config.grants.is_empty() {
            return Err(ApiError::validation(
                "a faucet that grants nothing is a faucet nobody can use; disable it instead",
            ));
        }
    }
    state.faucet.reconfigure(config);

    audit(
        &state,
        &admin,
        "faucet.configure",
        "faucet",
        serde_json::to_value(&request).unwrap_or_else(|_| json!({})),
    )
    .await;
    faucet_status(State(state), admin).await
}

/// `POST /admin/faucet/grant` — bypasses the cooldown.
pub async fn grant_faucet(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Json(request): Json<AdminGrantRequest>,
) -> ApiResult<Json<AdminGrantResponse>> {
    let user = state
        .db
        .users()
        .by_id(&request.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no user `{}`", request.user_id)))?;
    let grants = parse_grants(&request.grants)?;
    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::validation("a manual grant needs a reason"));
    }

    let (_key, trading_address) = state.auth.trading_key_of(&user)?;
    let receipt = state
        .faucet
        .grant(&user.user_id, trading_address, &grants, reason)
        .await?;

    audit(
        &state,
        &admin,
        "faucet.grant",
        &user.user_id,
        json!({
            "reason": reason,
            "claim_id": receipt.claim_id,
            "grants": request.grants,
        }),
    )
    .await;

    Ok(Json(AdminGrantResponse {
        claim_id: receipt.claim_id,
        tx_hash: receipt.tx_hash,
    }))
}

fn parse_grants(items: &[AssetAmountRequest]) -> ApiResult<Vec<Grant>> {
    let mut grants = Vec::with_capacity(items.len());
    for item in items {
        let asset = crate::symbols::parse_asset(&item.asset)
            .ok_or_else(|| ApiError::validation(format!("unknown asset `{}`", item.asset)))?;
        let amount = parse_scaled(&item.amount, asset.decimals() as u32)
            .map_err(|e| ApiError::validation(format!("{}: {e}", item.asset)))?;
        if amount == 0 {
            return Err(ApiError::validation("a grant amount must be positive"));
        }
        // The faucet refuses at mint time an amount its ledger cannot record.
        // Refused here too, so the operator who typed it is told, rather than
        // a configuration being accepted that turns every player's claim into
        // a validation error about a request they did not write.
        if crate::decimal::to_port_decimal(amount, asset.decimals() as u32).is_none() {
            return Err(ApiError::validation(format!(
                "{}: {} is too large for the faucet's ledger to record",
                item.asset, item.amount
            )));
        }
        grants.push(Grant { asset, amount });
    }
    Ok(grants)
}

// ---------------------------------------------------------------------------
// Audit and simulation
// ---------------------------------------------------------------------------

/// Query for `GET /admin/audit`.
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Restrict to one actor.
    #[serde(default)]
    pub actor: Option<String>,
    /// Restrict to one action.
    #[serde(default)]
    pub action: Option<String>,
    /// Restrict to one subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// How many records.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /admin/audit`.
pub async fn audit_trail(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<AuditResponse>> {
    let _ = &admin;
    let limit = state.clamp_limit(query.limit, 100) as usize;

    // `AuditRepository` indexes by actor and by subject, not by everything, so
    // the narrowest available index is used and the rest is filtered.
    let mut records = match (&query.subject, &query.actor) {
        (Some(subject), _) => state.db.audit().for_subject(subject, 1_000).await?,
        (None, Some(actor)) => state.db.audit().for_actor(actor, 1_000).await?,
        (None, None) => {
            // No index covers "everything", so the trail is assembled from the
            // subjects the gateway knows about. This is a gap in the port, not
            // a design choice.
            let mut all = Vec::new();
            for user in state.db.users().list(true, 1_000).await? {
                all.extend(state.db.audit().for_subject(&user.user_id, 100).await?);
            }
            for symbol in state.markets.snapshot().symbols_list() {
                all.extend(state.db.audit().for_subject(&symbol, 100).await?);
            }
            all.extend(state.db.audit().for_subject("faucet", 100).await?);
            all
        }
    };

    if let Some(action) = &query.action {
        records.retain(|r| r.action == *action);
    }
    if let Some(actor) = &query.actor {
        records.retain(|r| r.actor.as_deref() == Some(actor.as_str()));
    }
    records.sort_by(|a, b| b.at.cmp(&a.at).then(a.record_id.cmp(&b.record_id)));
    records.dedup_by(|a, b| a.record_id == b.record_id);

    if let Some(cursor) = query.cursor.as_deref() {
        if let Some(position) = records.iter().position(|r| r.record_id == cursor) {
            records.drain(..=position);
        }
    }

    let has_more = records.len() > limit;
    records.truncate(limit);
    let next_cursor = has_more
        .then(|| records.last().map(|r| r.record_id.clone()))
        .flatten();

    Ok(Json(AuditResponse {
        records: records.into_iter().map(render_audit).collect(),
        next_cursor,
    }))
}

fn render_audit(record: AuditRecord) -> AuditRow {
    AuditRow {
        record_id: record.record_id,
        actor: record.actor,
        action: record.action,
        subject: record.subject,
        detail: record.detail,
        at: record.at.to_rfc3339(),
    }
}

/// `GET /admin/sim`.
pub async fn sim_status(
    State(state): State<Arc<AppState>>,
    admin: Admin,
) -> ApiResult<Json<SimStatusResponse>> {
    let _ = &admin;
    Ok(Json(render_sim(state.sim.status().await?)))
}

/// `POST /admin/sim`.
pub async fn sim_control(
    State(state): State<Arc<AppState>>,
    admin: Admin,
    Json(request): Json<SimCommandRequest>,
) -> ApiResult<Json<SimStatusResponse>> {
    let command = SimCommand {
        action: request.action.clone(),
        mode: request.mode.clone(),
        scenario: request.scenario.clone(),
        seed: request.seed,
        bots: request.bots.clone(),
    };
    command.validate()?;
    let status = state.sim.control(command).await?;
    audit(
        &state,
        &admin,
        "sim.control",
        "simulation",
        json!({
            "action": request.action,
            "mode": request.mode,
            "scenario": request.scenario,
            "seed": request.seed,
        }),
    )
    .await;
    Ok(Json(render_sim(status)))
}

fn render_sim(status: crate::sim::SimStatus) -> SimStatusResponse {
    SimStatusResponse {
        running: status.running,
        mode: status.mode.as_str().to_string(),
        scenario: status.scenario,
        seed: status.seed,
        bots: status.bots,
        elapsed_seconds: status.elapsed_seconds,
        orders_submitted: status.orders_submitted,
        invariant_violations: status.invariant_violations,
    }
}
