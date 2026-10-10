//! axum-based dashboard server.

use crate::metrics::Metrics;
use crate::perps::{self, FundingRow, InsuranceSnapshot, LiquidationRow, OracleRow, PositionRow};
use crate::templates::{render_index, BlockSummary, IndexData, PerpSummary};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Json, Router,
};
use cb_block::Block;
use cb_rpc::server::read_best_bid_ask;
use cb_state::{
    perps::ORACLE_KIND_ORACLE, Accounts, Blocks, Database, Mempool, Meta, MirrorBlocks,
    OracleState, PerpMarkets, CF_POSITIONS,
};
use cb_types::{Address, AssetId};
use serde::Serialize;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Static blockchain config (not a runtime metric) surfaced to the dashboard so the
/// UI can show the operator whether pruning is on and the retention window.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PruningInfo {
    pub enabled: bool,
    pub keep_recent: u64,
}

#[derive(Clone)]
struct AppState {
    db: Database,
    metrics: Arc<Metrics>,
    pruning: PruningInfo,
}

#[derive(Serialize)]
struct AccountResponse {
    addr: String,
    nonce: u64,
    /// asset name → free balance (decimal string; balances can exceed u64)
    balances: BTreeMap<String, String>,
    /// asset name → locked-in-orders balance
    locked: BTreeMap<String, String>,
}

pub async fn serve(
    db: Database,
    addr: SocketAddr,
    metrics: Arc<Metrics>,
    pruning: PruningInfo,
) -> anyhow::Result<()> {
    let app = build_router(db, metrics, pruning);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Construct the dashboard's axum `Router` without binding a listener.
///
/// Exposed for integration tests to drive the router via
/// `tower::ServiceExt::oneshot` instead of opening a TCP port.
pub fn build_router(db: Database, metrics: Arc<Metrics>, pruning: PruningInfo) -> Router {
    let state = AppState {
        db,
        metrics,
        pruning,
    };
    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(status_handler))
        .route("/api/account/:addr", get(account_handler))
        .route("/api/perps/funding", get(perps_funding_handler))
        .route("/api/perps/positions", get(perps_positions_handler))
        .route("/api/perps/oracle", get(perps_oracle_handler))
        .route("/api/perps/liquidations", get(perps_liquidations_handler))
        .route("/api/perps/insurance", get(perps_insurance_handler))
        .with_state(state)
}

async fn index_handler(State(s): State<AppState>) -> Html<String> {
    let data = collect_index(&s.db, &s.metrics, s.pruning);
    Html(render_index(&data))
}

async fn status_handler(State(s): State<AppState>) -> Json<IndexData> {
    Json(collect_index(&s.db, &s.metrics, s.pruning))
}

async fn account_handler(
    State(s): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<AccountResponse>, (StatusCode, String)> {
    let parsed =
        parse_addr(&addr).ok_or((StatusCode::BAD_REQUEST, format!("invalid address: {addr}")))?;
    let acct = Accounts::new(&s.db).get(&parsed).unwrap_or_default();

    let mut balances = BTreeMap::new();
    let mut locked = BTreeMap::new();
    for asset in AssetId::ALL {
        let b = acct.balance(asset);
        let l = acct.locked_of(asset);
        if b > 0 || l > 0 {
            balances.insert(asset_name(asset), b.to_string());
            locked.insert(asset_name(asset), l.to_string());
        }
    }
    Ok(Json(AccountResponse {
        addr: format!("{}", parsed),
        nonce: acct.nonce,
        balances,
        locked,
    }))
}

async fn perps_funding_handler(State(s): State<AppState>) -> Json<Vec<FundingRow>> {
    Json(perps::collect_funding(&s.db))
}

async fn perps_positions_handler(State(s): State<AppState>) -> Json<Vec<PositionRow>> {
    Json(perps::collect_positions(&s.db))
}

async fn perps_oracle_handler(State(s): State<AppState>) -> Json<Vec<OracleRow>> {
    Json(perps::collect_oracle(&s.db))
}

async fn perps_liquidations_handler(State(s): State<AppState>) -> Json<Vec<LiquidationRow>> {
    Json(perps::collect_liquidations(&s.db))
}

async fn perps_insurance_handler(State(s): State<AppState>) -> Json<InsuranceSnapshot> {
    Json(perps::collect_insurance(&s.db))
}

fn parse_addr(s: &str) -> Option<Address> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(Address(out))
}

/// Lower-case asset label for the JSON account view. Delegates to `cb-types`
/// so the dashboard cannot drift from the canonical spelling.
fn asset_name(a: AssetId) -> String {
    a.to_string()
}

fn collect_index(db: &Database, metrics: &Metrics, pruning: PruningInfo) -> IndexData {
    let meta = Meta::new(db);
    let head_height = meta.head_height();
    let mut recent_blocks = Vec::new();
    let n = head_height.min(10);
    for i in 0..n {
        let h = head_height - i;
        if h == 0 {
            break;
        }
        // CF_BLOCKS is the leader's bincode store; CF_MIRROR_BLOCKS is the
        // fast-mirror replica store. Try the leader path first (carries full
        // tx + state_root info we want to display); fall back to the mirror
        // path on a fast-mirror replica where CF_BLOCKS is empty. The fast-
        // mirror entry only knows height/hash/blocktime — that's all we
        // committed to disk by design.
        if let Some(bytes) = Blocks::new(db).get(h) {
            if let Ok(block) = bincode::deserialize::<Block>(&bytes) {
                recent_blocks.push(BlockSummary {
                    height: block.header.height,
                    hash: format!("{}", block.header.hash()),
                    tx_count: block.txs.len(),
                    timestamp: block.header.timestamp,
                    state_root: format!("{}", block.header.state_root),
                });
                continue;
            }
        }
        if let Some((hash, blocktime, _capnp_bytes)) = MirrorBlocks::new(db).get(h) {
            recent_blocks.push(BlockSummary {
                height: h,
                hash: format!("{}", hash),
                tx_count: 0,
                timestamp: blocktime,
                state_root: String::new(),
            });
        }
    }
    // O(1): Meta::last_qc_height is bumped whenever quorum forms in the vote
    // pool. Reading from a CF_QC scan would require an unbounded backward walk
    // since followers can vote on blocks far behind the leader's head.
    let last_qc_height = Meta::new(db).last_qc_height();
    let perps = collect_perps(db);
    let funding = perps::collect_funding(db);
    let positions = perps::collect_positions(db);
    let oracle = perps::collect_oracle(db);
    let liquidations = perps::collect_liquidations(db);
    let insurance = perps::collect_insurance(db);
    IndexData {
        chain_id: meta.chain_id(),
        head_height,
        head_hash: format!("{}", meta.head_hash()),
        state_root: format!("{}", meta.state_root()),
        mempool_empty: Mempool::new(db).is_empty(),
        company_addr: format!("{}", meta.company_addr()),
        recent_blocks,
        metrics: metrics.snapshot(),
        pruning,
        last_qc_height,
        perps,
        funding,
        positions,
        oracle,
        liquidations,
        insurance,
    }
}

/// Build the dashboard's "Perps" section: per-market mark, oracle, last,
/// funding rate, open interest, top-of-book, and the M4 reduce-only halt
/// flag (when M4 lands; today defaults to `false`).
///
/// Open interest is the sum of `|qty|` across every open position on the
/// market. We compute it by walking `CF_POSITIONS` once and grouping by
/// market — bounded by the number of open positions, which the operator
/// already monitors via the metrics block.
fn collect_perps(db: &Database) -> Vec<PerpSummary> {
    let perp_markets = PerpMarkets::new(db).list();
    if perp_markets.is_empty() {
        return Vec::new();
    }
    // Tally OI per market via a single CF scan. Map: deriv-id-bytes -> u128.
    let mut oi: BTreeMap<[u8; 4], u128> = BTreeMap::new();
    if let Ok(it) = db.iter_prefix(CF_POSITIONS, &[]) {
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(p) = bincode::deserialize::<cb_types::Position>(&v) {
                *oi.entry(p.market.as_bytes()).or_insert(0) = oi
                    .get(&p.market.as_bytes())
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(p.qty);
            }
        }
    }
    let oracle_cf = OracleState::new(db);
    let _ = ORACLE_KIND_ORACLE; // keep import live in case cb-state changes shape.

    let mut out = Vec::with_capacity(perp_markets.len());
    for m in perp_markets {
        let mark_px = oracle_cf
            .get_mark(&m.id)
            .map(|x| x.px_q8.to_string())
            .unwrap_or_else(|| "-".into());
        let oracle_px = oracle_cf
            .get_oracle(&m.id)
            .map(|x| x.px_q8.to_string())
            .unwrap_or_else(|| "-".into());
        let last_px = if m.last_trade_px_q8 == 0 && m.last_trade_height == 0 {
            "-".to_string()
        } else {
            m.last_trade_px_q8.to_string()
        };
        // funding_rate is read from FundingIndex; absent => 0 (default).
        let funding_rate_q9 = cb_state::FundingIndices::new(db)
            .get(&m.id)
            .map(|f| f.rate_q9)
            .unwrap_or(0);
        let oi_v = oi.get(&m.id.as_bytes()).copied().unwrap_or(0);
        let (best_bid, best_ask) = read_best_bid_ask(db, m.id);
        let bid = best_bid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        let ask = best_ask
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".into());
        out.push(PerpSummary {
            market: format!("{}", m.id),
            underlying: asset_name(m.underlying),
            mark_px_q8: mark_px,
            oracle_px_q8: oracle_px,
            last_px_q8: last_px,
            funding_rate_q9,
            open_interest: oi_v.to_string(),
            best_bid_q8: bid,
            best_ask_q8: ask,
            paused: m.paused,
            // M4 hook — see comment on `PerpMarket.paused_reduce_only_only`.
            // Pre-M4 the field doesn't exist on the struct yet; default to
            // `false` so the dashboard stays representable. Switch to the
            // real field when M4 merges.
            paused_reduce_only_only: false,
        });
    }
    out
}
