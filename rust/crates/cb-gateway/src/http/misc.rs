//! The leaderboard, the simulator controls, health, and the WebSocket upgrade.

use super::dto::*;
use super::Json;
use crate::error::{ApiError, ApiResult};
use crate::node::NodeClient;
use crate::state::AppState;
use crate::ws;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::response::Response;
use cb_ports::LeaderboardWindow;
use serde::Deserialize;
use std::sync::Arc;

/// Query for `GET /leaderboard`.
#[derive(Debug, Deserialize)]
pub struct LeaderboardQuery {
    /// `24h`, `7d` or `all`.
    #[serde(default)]
    pub window: Option<String>,
    /// How many rows.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `GET /leaderboard`.
pub async fn leaderboard(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LeaderboardQuery>,
) -> ApiResult<Json<LeaderboardResponse>> {
    let window_name = query.window.as_deref().unwrap_or("24h");
    let window = LeaderboardWindow::parse(window_name)
        .ok_or_else(|| ApiError::validation("window must be one of 24h, 7d, all"))?;
    let limit = state.clamp_limit(query.limit, 100);

    let entries = state.leaderboard.top(window, limit).await?;
    Ok(Json(LeaderboardResponse {
        window: window.as_str().to_string(),
        entries: entries
            .into_iter()
            .map(|entry| LeaderboardRow {
                rank: entry.rank,
                handle: entry.handle,
                address: entry.address,
                equity: crate::decimal::format_port_decimal(entry.equity, 2),
                pnl: crate::decimal::format_port_decimal(entry.pnl, 2),
                pnl_pct: crate::decimal::format_port_decimal(entry.pnl_pct, 2),
                volume: crate::decimal::format_port_decimal(entry.volume, 2),
                trades: entry.trades,
                is_bot: entry.is_bot,
            })
            .collect(),
    }))
}

/// What an unauthenticated caller is told about a dependency that is down.
///
/// Which dependency, and whether it is up, is the operational fact `/health`
/// exists to publish. *Why* it is down is a sentence containing an internal
/// address and a driver's error text, and this endpoint needs no session.
const UNREACHABLE: &str = "unreachable; see the gateway log";

/// `GET /health`.
///
/// Reports each dependency separately rather than one boolean: "the gateway is
/// unhealthy" sends an operator looking, and "the sequencer is unreachable but
/// storage is fine" tells them where.
pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let node_health = state.node.health();
    if let Some(error) = node_health.last_error.as_deref() {
        // The operator needs the real reason; the endpoint is unauthenticated,
        // so the caller gets the classification only. The live sample in
        // finding 8 was this field echoing the sequencer's RPC address and an
        // OS error string to anyone who asked.
        tracing::debug!(target: "cb_gateway::http", error, "sequencer is down");
    }
    let mut dependencies = vec![DependencyHealth {
        name: "sequencer".into(),
        status: if node_health.connected { "up" } else { "down" }.into(),
        detail: node_health
            .last_error
            .as_ref()
            .map(|_| UNREACHABLE.to_string()),
    }];

    dependencies.push(match state.db.health_check().await {
        Ok(()) => DependencyHealth {
            name: "database".into(),
            status: "up".into(),
            detail: None,
        },
        Err(e) => {
            tracing::debug!(target: "cb_gateway::http", dependency = "database", error = %e, "dependency is down");
            DependencyHealth {
                name: "database".into(),
                status: "down".into(),
                detail: Some(UNREACHABLE.to_string()),
            }
        }
    });

    dependencies.push(match state.bus.health_check().await {
        Ok(()) => DependencyHealth {
            name: "event_bus".into(),
            status: "up".into(),
            detail: None,
        },
        Err(e) => {
            tracing::debug!(target: "cb_gateway::http", dependency = "event_bus", error = %e, "dependency is down");
            DependencyHealth {
                name: "event_bus".into(),
                status: "down".into(),
                detail: Some(UNREACHABLE.to_string()),
            }
        }
    });

    // The market data feed is optional: nothing the gateway serves depends on
    // it today, so it is reported and does not decide overall health.
    dependencies.push(match state.feed.health_check().await {
        Ok(()) => DependencyHealth {
            name: "market_data_feed".into(),
            status: "up".into(),
            detail: None,
        },
        Err(e) => {
            tracing::debug!(target: "cb_gateway::http", dependency = "market_data_feed", error = %e, "dependency is down");
            DependencyHealth {
                name: "market_data_feed".into(),
                status: "down".into(),
                detail: Some(UNREACHABLE.to_string()),
            }
        }
    });

    let required_up = dependencies
        .iter()
        .filter(|d| d.name != "market_data_feed")
        .all(|d| d.status == "up");

    Json(HealthResponse {
        status: if required_up { "ok" } else { "degraded" }.into(),
        uptime_seconds: (state.clock.now() - state.started_at).num_seconds().max(0) as u64,
        dependencies,
        height: node_health.last_height,
        websocket_connections: state.hub.len(),
        markets: state.markets.snapshot().metas.len(),
    })
}

/// `GET /ws` — the WebSocket upgrade.
pub async fn websocket(State(state): State<Arc<AppState>>, upgrade: WebSocketUpgrade) -> Response {
    let hub = Arc::clone(&state.hub);
    let auth = Arc::clone(&state.auth);
    let config = ws::ConnectionConfig {
        idle_timeout: state.settings.ws_idle,
        session_recheck: state.settings.ws_session_recheck,
    };
    let snapshot_state = Arc::clone(&state);

    upgrade.on_upgrade(move |socket| async move {
        // Sending a fresh snapshot on subscribe is the contract's guarantee,
        // and it comes from the cached book the deltas are diffed against, so
        // a subscriber never sees a delta against a book it was not given.
        let on_snapshot: Arc<dyn Fn(ws::ConnectionId, String) + Send + Sync> = {
            let state = Arc::clone(&snapshot_state);
            Arc::new(move |connection, symbol| {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    crate::watcher::send_book_snapshot(&state, connection, &symbol).await;
                });
            })
        };
        ws::run_connection(socket, hub, auth, config, on_snapshot).await;
    })
}
