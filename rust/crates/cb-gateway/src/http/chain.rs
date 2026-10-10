//! Chain status and recent blocks.
//!
//! Two fields the contract asks for have no RPC behind them: `mempool_depth`
//! and `validators`. They are reported as zero, and that is recorded as a gap
//! rather than filled with a plausible-looking number — an operator reading a
//! dashboard cannot tell an invented figure from a real one.

use super::dto::*;
use super::format_address;
use super::Json;
use crate::error::ApiResult;
use crate::node::{NodeClient, NodeEvent};
use crate::state::AppState;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::Arc;

/// How many recent block headers to keep for `/chain/blocks`.
const BLOCK_RING: usize = 256;

/// How many recent transactions to keep for `/chain/txs`.
const TX_RING: usize = 512;

/// Query for `GET /chain/txs`.
#[derive(Debug, Deserialize)]
pub struct TxsQuery {
    /// How many transactions.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One remembered transaction, flattened to what a dashboard row shows.
#[derive(Debug, Clone)]
pub struct TxRecord {
    /// Transaction hash.
    pub hash: String,
    /// The block that carried it.
    pub height: u64,
    /// Position in that block.
    pub index: u32,
    /// Who signed it.
    pub sender: String,
    /// The transaction's kind, as the chain names it: `PlaceOrder`,
    /// `Transfer`, `OracleUpdate` and so on.
    pub kind: String,
    /// The market it touched, as a contract symbol, when it touched one.
    pub market: Option<String>,
    /// `buy` or `sell`, when the transaction has a side.
    pub side: Option<String>,
    /// When its block was produced.
    pub at: DateTime<Utc>,
}

/// Query for `GET /chain/blocks`.
#[derive(Debug, Deserialize)]
pub struct BlocksQuery {
    /// How many headers.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Recent block headers and the counters derived from them.
///
/// The node serves one block at a time by height and keeps no summary, so the
/// gateway remembers what it has watched go past. A restart starts the ring
/// empty; `tx_count` is therefore transactions *since the gateway started*,
/// which is what it is documented as.
#[derive(Default)]
pub struct ChainLog {
    blocks: Mutex<VecDeque<BlockRecord>>,
    txs: Mutex<VecDeque<TxRecord>>,
    tx_count: std::sync::atomic::AtomicU64,
}

/// One remembered header.
#[derive(Debug, Clone)]
pub struct BlockRecord {
    /// Height.
    pub height: u64,
    /// Block hash.
    pub hash: String,
    /// State root.
    pub state_root: String,
    /// Transactions carried.
    pub tx_count: u32,
    /// When it was produced.
    pub at: DateTime<Utc>,
    /// Proposer, recovered from the header signature.
    pub proposer: Option<String>,
}

impl std::fmt::Debug for ChainLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainLog")
            .field("blocks", &self.blocks.lock().len())
            .finish_non_exhaustive()
    }
}

impl ChainLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember a block, and the transactions it carried.
    ///
    /// `symbol_for` turns a chain market handle into a contract symbol; the
    /// log does not know the catalogue and does not want to.
    pub fn observe_with(
        &self,
        block: &crate::node::BlockSummary,
        symbol_for: &dyn Fn(crate::symbols::MarketRef) -> Option<String>,
    ) {
        let at =
            DateTime::from_timestamp_millis(block.timestamp_ms as i64).unwrap_or_else(Utc::now);
        {
            let mut txs = self.txs.lock();
            for tx in &block.txs {
                let (kind, market, side) = describe(&tx.body, symbol_for);
                txs.push_back(TxRecord {
                    hash: super::format_hash(&tx.hash),
                    height: block.height,
                    index: tx.index,
                    sender: format_address(&tx.sender),
                    kind,
                    market,
                    side,
                    at,
                });
            }
            while txs.len() > TX_RING {
                txs.pop_front();
            }
        }
        self.observe(block);
    }

    /// Recent transactions, newest first.
    pub fn recent_txs(&self, limit: usize) -> Vec<TxRecord> {
        let txs = self.txs.lock();
        txs.iter().rev().take(limit).cloned().collect()
    }

    /// Remember a block header.
    pub fn observe(&self, block: &crate::node::BlockSummary) {
        let record = BlockRecord {
            height: block.height,
            hash: super::format_hash(&block.hash),
            state_root: super::format_hash(&block.state_root),
            tx_count: block.tx_count(),
            at: DateTime::from_timestamp_millis(block.timestamp_ms as i64).unwrap_or_else(Utc::now),
            proposer: block.proposer.as_ref().map(format_address),
        };
        self.tx_count
            .fetch_add(record.tx_count as u64, std::sync::atomic::Ordering::Relaxed);
        let mut blocks = self.blocks.lock();
        blocks.push_back(record);
        while blocks.len() > BLOCK_RING {
            blocks.pop_front();
        }
    }

    /// Recent headers, newest first.
    pub fn recent(&self, limit: usize) -> Vec<BlockRecord> {
        let blocks = self.blocks.lock();
        blocks.iter().rev().take(limit).cloned().collect()
    }

    /// Transactions counted since the gateway started.
    pub fn total_transactions(&self) -> u64 {
        self.tx_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Transactions per second over the trailing minute.
    pub fn tps_1m(&self, now: DateTime<Utc>) -> f64 {
        let blocks = self.blocks.lock();
        let cutoff = now - chrono::Duration::seconds(60);
        let recent: u64 = blocks
            .iter()
            .filter(|b| b.at >= cutoff)
            .map(|b| b.tx_count as u64)
            .sum();
        recent as f64 / 60.0
    }
}

/// What a transaction is, for a dashboard row: its kind, the market it names
/// and the side it takes.
fn describe(
    body: &cb_tx::TxBody,
    symbol_for: &dyn Fn(crate::symbols::MarketRef) -> Option<String>,
) -> (String, Option<String>, Option<String>) {
    use crate::symbols::MarketRef;
    use cb_tx::TxBody as B;
    let side_name = |side: &cb_types::Side| {
        Some(match side {
            cb_types::Side::Buy => "buy".to_string(),
            cb_types::Side::Sell => "sell".to_string(),
        })
    };
    match body {
        B::Transfer { .. } => ("Transfer".into(), None, None),
        B::Deposit { .. } => ("Deposit".into(), None, None),
        B::Withdraw { .. } => ("Withdraw".into(), None, None),
        B::WithdrawSettled { .. } => ("WithdrawSettled".into(), None, None),
        B::PlaceOrder { market, side, .. } => (
            "PlaceOrder".into(),
            symbol_for(MarketRef::Spot(*market)),
            side_name(side),
        ),
        B::CancelOrder { .. } => ("CancelOrder".into(), None, None),
        B::BatchCancel { market } => (
            "BatchCancel".into(),
            symbol_for(MarketRef::Spot(*market)),
            None,
        ),
        B::PlacePerpOrder { market, side, .. } => (
            "PlacePerpOrder".into(),
            symbol_for(MarketRef::Deriv(*market)),
            side_name(side),
        ),
        B::CancelPerpOrder { .. } => ("CancelPerpOrder".into(), None, None),
        B::OpenPosition { market, side, .. } => (
            "OpenPosition".into(),
            symbol_for(MarketRef::Deriv(*market)),
            side_name(side),
        ),
        B::ClosePosition { market, .. } => (
            "ClosePosition".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
        B::AdjustMargin { market, .. } => (
            "AdjustMargin".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
        B::LiquidationTrigger { market, .. } => (
            "LiquidationTrigger".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
        B::OracleUpdate { market, .. } => (
            "OracleUpdate".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
        B::FundingTick { market } => (
            "FundingTick".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
        B::FuturesSettle { market } => (
            "FuturesSettle".into(),
            symbol_for(MarketRef::Deriv(*market)),
            None,
        ),
    }
}

/// `GET /chain/txs`.
pub async fn txs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TxsQuery>,
) -> ApiResult<Json<TxsResponse>> {
    let limit = state.clamp_limit(query.limit, 50) as usize;
    Ok(Json(TxsResponse {
        txs: state
            .chain_log
            .recent_txs(limit)
            .into_iter()
            .map(|record| TxSummary {
                hash: record.hash,
                height: record.height,
                index: record.index,
                sender: record.sender,
                kind: record.kind,
                market: record.market,
                side: record.side,
                at: record.at.to_rfc3339(),
            })
            .collect(),
    }))
}

/// `GET /chain/status`.
pub async fn status(State(state): State<Arc<AppState>>) -> ApiResult<Json<ChainStatus>> {
    let health = state.node.health();
    let chain_id = match health.chain_id {
        Some(id) => id,
        None => state.node.chain_info().await?.chain_id,
    };
    let now = state.clock.now();
    let head = state.chain_log.recent(1).into_iter().next();

    Ok(Json(ChainStatus {
        chain_id,
        height: health.last_height,
        state_root: head
            .as_ref()
            .map(|b| b.state_root.clone())
            .unwrap_or_else(|| format!("0x{}", "0".repeat(64))),
        block_time_ms: state.settings.block_time_ms,
        tps_1m: format!("{:.1}", state.chain_log.tps_1m(now)),
        tx_count: state.chain_log.total_transactions(),
        // No mempool query exists on the node.
        mempool_depth: 0,
        // No validator-set query exists on the node.
        validators: 0,
        synced: health.is_synced(),
        uptime_seconds: (now - state.started_at).num_seconds().max(0) as u64,
        node_rpc: state.settings.node_rpc.clone(),
        gateway_listen: state.settings.gateway_listen.clone(),
        ws_path: "/ws".into(),
        oracle_control: state.settings.oracle_control.clone(),
        sim_mode: state.sim.mode().await.as_str().to_string(),
    }))
}

/// `GET /chain/blocks`.
pub async fn blocks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<BlocksQuery>,
) -> ApiResult<Json<BlocksResponse>> {
    let limit = state.clamp_limit(query.limit, 20) as usize;
    Ok(Json(BlocksResponse {
        blocks: state
            .chain_log
            .recent(limit)
            .into_iter()
            .map(|record| BlockSummary {
                height: record.height,
                hash: record.hash,
                state_root: record.state_root,
                tx_count: record.tx_count,
                at: record.at.to_rfc3339(),
                proposer: record.proposer,
            })
            .collect(),
    }))
}

/// Publish a block to the `chain` WebSocket channel.
pub fn block_envelope(record: &BlockRecord) -> crate::ws::Envelope {
    crate::ws::Envelope::new(
        "chain",
        "block",
        serde_json::json!({
            "height": record.height,
            "hash": record.hash,
            "tx_count": record.tx_count,
            "at": record.at.to_rfc3339(),
        }),
    )
}

/// Whether an event is a block, for the watcher's dispatch.
pub fn is_block(event: &NodeEvent) -> bool {
    matches!(event, NodeEvent::Block(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::H256;

    fn block(height: u64, txs: u32, at_ms: u64) -> crate::node::BlockSummary {
        crate::node::BlockSummary {
            height,
            hash: H256([height as u8; 32]),
            state_root: H256([1; 32]),
            timestamp_ms: at_ms,
            proposer: Some(cb_types::Address([7; 20])),
            txs: (0..txs)
                .map(|i| crate::node::BlockTx {
                    applied: None,
                    hash: H256([i as u8; 32]),
                    sender: cb_types::Address([2; 20]),
                    index: i,
                    body: cb_tx::TxBody::Transfer {
                        to: cb_types::Address([3; 20]),
                        asset: cb_types::AssetId::Usd,
                        amount: 1,
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn recent_blocks_come_back_newest_first() {
        let log = ChainLog::new();
        for height in 1..=5 {
            log.observe(&block(height, 2, 1_000 * height));
        }
        let recent = log.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].height, 5);
        assert_eq!(recent[0].tx_count, 2);
        assert_eq!(log.total_transactions(), 10);
    }

    #[test]
    fn the_ring_is_bounded() {
        let log = ChainLog::new();
        for height in 1..=(BLOCK_RING as u64 + 50) {
            log.observe(&block(height, 0, height));
        }
        assert_eq!(log.recent(usize::MAX).len(), BLOCK_RING);
    }

    #[test]
    fn throughput_counts_only_the_trailing_minute() {
        let log = ChainLog::new();
        let now = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .expect("static")
            .with_timezone(&Utc);
        // One block two minutes ago, one just now.
        log.observe(&block(1, 60, (now.timestamp_millis() - 120_000) as u64));
        log.observe(&block(2, 60, now.timestamp_millis() as u64));
        assert_eq!(log.tps_1m(now), 1.0, "only the recent block counts");
    }

    #[test]
    fn the_proposer_is_carried_through() {
        let log = ChainLog::new();
        log.observe(&block(1, 0, 1_000));
        let record = &log.recent(1)[0];
        assert_eq!(
            record.proposer.as_deref(),
            Some("0x0707070707070707070707070707070707070707")
        );
    }
}

#[cfg(test)]
mod tx_ring_tests {
    use super::*;
    use cb_types::H256;

    fn block_with(height: u64, bodies: Vec<cb_tx::TxBody>) -> crate::node::BlockSummary {
        crate::node::BlockSummary {
            height,
            hash: H256([height as u8; 32]),
            state_root: H256([1; 32]),
            timestamp_ms: 1_700_000_000_000 + height,
            proposer: None,
            txs: bodies
                .into_iter()
                .enumerate()
                .map(|(i, body)| crate::node::BlockTx {
                    applied: None,
                    hash: H256([i as u8; 32]),
                    sender: cb_types::Address([2; 20]),
                    index: i as u32,
                    body,
                })
                .collect(),
        }
    }

    fn symbol(market: crate::symbols::MarketRef) -> Option<String> {
        match market {
            crate::symbols::MarketRef::Spot(id) => Some(crate::symbols::spot_symbol(id)),
            crate::symbols::MarketRef::Deriv(_) => Some("BTC-PERP".into()),
        }
    }

    #[test]
    fn transactions_are_remembered_newest_first_with_kind_market_and_side() {
        let log = ChainLog::new();
        log.observe_with(
            &block_with(
                7,
                vec![
                    cb_tx::TxBody::Transfer {
                        to: cb_types::Address([3; 20]),
                        asset: cb_types::AssetId::Usd,
                        amount: 1,
                    },
                    cb_tx::TxBody::PlaceOrder {
                        market: cb_types::MarketId::BtcUsd,
                        side: cb_types::Side::Sell,
                        price: 1,
                        qty: 1,
                        tif: cb_types::Tif::Gtc,
                        post_only: false,
                        client_order_id: H256([0; 32]),
                    },
                ],
            ),
            &symbol,
        );
        let txs = log.recent_txs(10);
        assert_eq!(txs.len(), 2);
        assert_eq!(txs[0].kind, "PlaceOrder", "newest first");
        assert_eq!(txs[0].market.as_deref(), Some("BTC-USD"));
        assert_eq!(txs[0].side.as_deref(), Some("sell"));
        assert_eq!(txs[0].height, 7);
        assert_eq!(txs[1].kind, "Transfer");
        assert!(txs[1].market.is_none());
        // The header ring and counter still see the block.
        assert_eq!(log.total_transactions(), 2);
        assert_eq!(log.recent(1)[0].height, 7);
    }

    #[test]
    fn the_transaction_ring_is_bounded() {
        let log = ChainLog::new();
        for h in 1..=(TX_RING as u64 + 40) {
            log.observe_with(
                &block_with(
                    h,
                    vec![cb_tx::TxBody::Transfer {
                        to: cb_types::Address([3; 20]),
                        asset: cb_types::AssetId::Usd,
                        amount: 1,
                    }],
                ),
                &symbol,
            );
        }
        let txs = log.recent_txs(10_000);
        assert_eq!(txs.len(), TX_RING);
        assert_eq!(txs[0].height, TX_RING as u64 + 40);
    }
}
