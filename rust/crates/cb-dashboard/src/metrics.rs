//! Shared throughput counters. Producers (sequencer block-builder, replica
//! applier, in-process benchmark) call the `record_*` methods; the dashboard
//! reads via `snapshot()`.
//!
//! # Transactions are not orders
//!
//! These two are counted separately and never share a counter. The reason is a
//! bug this module used to have: the in-process benchmark signs
//! `TxBody::Transfer` and called `record_submit_side(side, market)` for each
//! one, so `buys`, `sells`, `btc_orders` and `eth_orders` all climbed while
//! zero orders had been placed, and the console displayed a six-figure "orders
//! per second" for a workload containing no orders. The number was real; the
//! label was false.
//!
//! So:
//!
//! * [`Metrics::record_tx_submitted`] is for any transaction. It moves
//!   `submitted` and nothing else.
//! * [`Metrics::record_order_submitted`] is for a transaction that actually
//!   places an order (`PlaceOrder` / `PlacePerpOrder`). It moves `submitted`,
//!   `orders_submitted`, the side counters and the per-market counters.
//! * [`Metrics::record_block`] takes the block's transaction count **and** its
//!   order count, so the apply side can report both without inferring one from
//!   the other.
//!
//! Every snapshot field carrying "order" in its name therefore counts orders.
//! Transaction rates keep the `tps` names (`tps_5s`, `tps_60s`, `avg_tps`) that
//! the Makefile summary and the runbook already grep for.
//!
//! Rolling rates are computed over a sliding window of full-counter samples
//! taken on each block.

use cb_types::{MarketId, Side};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Long retention window for rolling-rate samples. Samples are kept up to this many
/// seconds; both the 5-second and 60-second rates are derived from the same buffer.
const WINDOW_SECS: u64 = 60;
const SHORT_WINDOW_SECS: u64 = 5;
const MAX_SAMPLES: usize = 4096;

#[derive(Default)]
pub struct Metrics {
    // Submit-side counters. `submitted` counts every transaction;
    // `orders_submitted` counts only the subset that places an order.
    submitted: AtomicU64,
    orders_submitted: AtomicU64,
    rejected: AtomicU64,
    buys: AtomicU64,
    sells: AtomicU64,
    btc_orders: AtomicU64,
    eth_orders: AtomicU64,
    /// Every listed spot market other than BTC/USD and ETH/USD, rolled up.
    /// The dashboard has two named market tiles; the rest land here so the
    /// submitted-order total still reconciles.
    other_orders: AtomicU64,

    // Apply-side counters (sequencer / replica call record_block per block).
    // `applied` counts transactions that landed in a block; `orders_applied`
    // counts the subset of those that were order placements.
    applied: AtomicU64,
    orders_applied: AtomicU64,
    blocks: AtomicU64,
    block_txs_max: AtomicU64,
    /// Min initialised lazily — u64::MAX while no block has been produced.
    block_txs_min: AtomicU64,
    block_txs_last: AtomicU64,

    // Pipeline depth gauges (overwritten per block, not cumulative).
    channel_depth: AtomicU64,

    started_at: OnceLock<Instant>,
    samples: Mutex<Vec<Sample>>,
}

#[derive(Clone, Copy)]
struct Sample {
    t: Instant,
    submitted: u64,
    orders_submitted: u64,
    applied: u64,
    orders_applied: u64,
    buys: u64,
    sells: u64,
    btc: u64,
    eth: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MetricsSnapshot {
    /// Transactions handed to the pipeline, of every kind.
    pub submitted: u64,
    /// The subset of `submitted` that places an order.
    pub orders_submitted: u64,
    pub rejected: u64,
    /// Transactions that landed in a block, of every kind.
    pub applied: u64,
    /// The subset of `applied` that places an order.
    pub orders_applied: u64,
    pub blocks: u64,
    pub buys: u64,
    pub sells: u64,
    pub btc_orders: u64,
    pub eth_orders: u64,
    /// Orders on every listed spot market other than BTC/USD and ETH/USD.
    pub other_orders: u64,

    pub elapsed_secs: f64,

    /// Apply-side rates (txs landed in a block).
    pub avg_tps: f64,
    pub recent_tps: f64,
    /// 5-second rolling apply-side TPS — same as `recent_tps`, surfaced under a
    /// stable name so the multi-node summary script can grep for it.
    pub tps_5s: f64,
    /// 60-second rolling apply-side TPS — the headline "sustained" figure to
    /// compare against Hyperliquid's published ~20k–50k orders/sec.
    pub tps_60s: f64,
    /// Apply-side TPS over just the most recent block interval. Computed from
    /// the last two samples; useful for spotting per-block stalls.
    pub last_block_tps: f64,

    /// Submit-side rate: **transactions** of every kind ingressed into the
    /// pipeline, over the short window. Not an order rate.
    pub recent_submitted_per_sec: f64,
    /// Submit-side rate: order placements only, over the short window.
    pub recent_orders_submitted_per_sec: f64,
    /// Apply-side rate: order placements that landed in a block, over the
    /// short window. This is the closest thing the console has to an order
    /// throughput figure, and it is still not a *matched*-order rate — the
    /// executor does not report fills to this module. See
    /// `cb_node::order_bench` for measured matched-order throughput.
    pub recent_orders_applied_per_sec: f64,
    pub recent_buys_per_sec: f64,
    pub recent_sells_per_sec: f64,
    pub recent_btc_per_sec: f64,
    pub recent_eth_per_sec: f64,

    pub block_txs_min: u64,
    pub block_txs_max: u64,
    pub block_txs_last: u64,
    pub block_txs_avg: f64,

    pub channel_depth: u64,

    pub running: bool,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            block_txs_min: AtomicU64::new(u64::MAX),
            ..Default::default()
        }
    }

    fn ensure_started(&self) -> Instant {
        *self.started_at.get_or_init(Instant::now)
    }

    pub fn mark_started(&self) {
        self.ensure_started();
    }

    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Submit-time record for a transaction that is **not** an order. Moves
    /// only the transaction counter. Transfers, deposits, oracle updates and
    /// funding ticks all belong here.
    pub fn record_tx_submitted(&self) {
        self.ensure_started();
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    /// Submit-time record for a transaction that places an order. Moves the
    /// transaction counter, the order counter, the side counters and the
    /// per-market counters.
    pub fn record_order_submitted(&self, side: Side, market: MarketId) {
        self.ensure_started();
        self.submitted.fetch_add(1, Ordering::Relaxed);
        self.orders_submitted.fetch_add(1, Ordering::Relaxed);
        // The side and per-market counters are moved by
        // [`Metrics::record_applied_order`] when the block lands, not here:
        // counting them at submission as well doubled every buy, sell, BTC
        // and ETH figure under the in-process generator, which is the one
        // producer that reports submissions at all.
        let _ = (side, market);
    }

    /// Record one order that a block has applied, by side and (for spot)
    /// market. This is the only mover of the side and per-market counters;
    /// the totals come from [`Metrics::record_block`]. Before this the side
    /// and market tiles moved only for submissions the in-process generator
    /// reported, so under real RPC traffic they sat at zero while orders were
    /// matching.
    pub fn record_applied_order(&self, side: Side, market: Option<MarketId>) {
        match side {
            Side::Buy => self.buys.fetch_add(1, Ordering::Relaxed),
            Side::Sell => self.sells.fetch_add(1, Ordering::Relaxed),
        };
        match market {
            Some(MarketId::BtcUsd) => self.btc_orders.fetch_add(1, Ordering::Relaxed),
            Some(MarketId::EthUsd) => self.eth_orders.fetch_add(1, Ordering::Relaxed),
            _ => self.other_orders.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn record_submit_rejected(&self) {
        self.ensure_started();
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one produced (or applied) block.
    ///
    /// `applied_count` is the block's transaction count; `order_count` is how
    /// many of those transactions placed an order. Callers classify the block's
    /// bodies — the metrics module deliberately cannot infer one from the
    /// other, because inferring is exactly how the two got conflated before.
    pub fn record_block(&self, applied_count: u64, order_count: u64) {
        self.ensure_started();
        self.blocks.fetch_add(1, Ordering::Relaxed);
        self.orders_applied
            .fetch_add(order_count, Ordering::Relaxed);
        let total_applied =
            self.applied.fetch_add(applied_count, Ordering::Relaxed) + applied_count;
        self.block_txs_last.store(applied_count, Ordering::Relaxed);
        // CAS-loop max
        let mut prev = self.block_txs_max.load(Ordering::Relaxed);
        while applied_count > prev {
            match self.block_txs_max.compare_exchange_weak(
                prev,
                applied_count,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => prev = p,
            }
        }
        // CAS-loop min
        let mut prev = self.block_txs_min.load(Ordering::Relaxed);
        while applied_count < prev {
            match self.block_txs_min.compare_exchange_weak(
                prev,
                applied_count,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(p) => prev = p,
            }
        }

        let sample = Sample {
            t: Instant::now(),
            submitted: self.submitted.load(Ordering::Relaxed),
            orders_submitted: self.orders_submitted.load(Ordering::Relaxed),
            applied: total_applied,
            orders_applied: self.orders_applied.load(Ordering::Relaxed),
            buys: self.buys.load(Ordering::Relaxed),
            sells: self.sells.load(Ordering::Relaxed),
            btc: self.btc_orders.load(Ordering::Relaxed),
            eth: self.eth_orders.load(Ordering::Relaxed),
        };
        let mut s = self.samples.lock().expect("metrics samples poisoned");
        s.push(sample);
        let cutoff = sample.t.checked_sub(Duration::from_secs(WINDOW_SECS));
        if let Some(cutoff) = cutoff {
            s.retain(|x| x.t >= cutoff);
        }
        if s.len() > MAX_SAMPLES {
            let drop = s.len() - MAX_SAMPLES;
            s.drain(0..drop);
        }
    }

    /// Sequencer reports the current bypass-channel fill so the dashboard can show
    /// pipeline back-pressure.
    pub fn set_channel_depth(&self, depth: u64) {
        self.channel_depth.store(depth, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let started = self.started_at.get().copied();
        let now = Instant::now();
        let elapsed_secs = started
            .map(|s| now.duration_since(s).as_secs_f64())
            .unwrap_or(0.0);
        let submitted = self.submitted.load(Ordering::Relaxed);
        let orders_submitted = self.orders_submitted.load(Ordering::Relaxed);
        let rejected = self.rejected.load(Ordering::Relaxed);
        let applied = self.applied.load(Ordering::Relaxed);
        let orders_applied = self.orders_applied.load(Ordering::Relaxed);
        let blocks = self.blocks.load(Ordering::Relaxed);
        let buys = self.buys.load(Ordering::Relaxed);
        let sells = self.sells.load(Ordering::Relaxed);
        let btc_orders = self.btc_orders.load(Ordering::Relaxed);
        let eth_orders = self.eth_orders.load(Ordering::Relaxed);
        let other_orders = self.other_orders.load(Ordering::Relaxed);
        let block_txs_max = self.block_txs_max.load(Ordering::Relaxed);
        let block_txs_min = {
            let m = self.block_txs_min.load(Ordering::Relaxed);
            if m == u64::MAX {
                0
            } else {
                m
            }
        };
        let block_txs_last = self.block_txs_last.load(Ordering::Relaxed);
        let block_txs_avg = if blocks > 0 {
            applied as f64 / blocks as f64
        } else {
            0.0
        };
        let channel_depth = self.channel_depth.load(Ordering::Relaxed);

        let avg_tps = if elapsed_secs > 0.0 {
            applied as f64 / elapsed_secs
        } else {
            0.0
        };

        // Rolling-window rates from samples. We compute one rate over the full
        // 60s buffer (the headline `tps_60s`) and a second over only the last 5s
        // (`tps_5s` / `recent_tps`). Submit-side rates stay on the short window
        // since the dashboard cares about instantaneous order-flow.
        let (
            recent_tps,
            tps_5s,
            tps_60s,
            last_block_tps,
            recent_submitted_per_sec,
            recent_orders_submitted_per_sec,
            recent_orders_applied_per_sec,
            recent_buys_per_sec,
            recent_sells_per_sec,
            recent_btc_per_sec,
            recent_eth_per_sec,
        ) = {
            let s = self.samples.lock().expect("metrics samples poisoned");
            let tps_60s_v = match (s.first(), s.last()) {
                (Some(a), Some(b)) if b.t > a.t => {
                    let dt = b.t.duration_since(a.t).as_secs_f64();
                    if dt > 0.0 {
                        (b.applied - a.applied) as f64 / dt
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };
            // Last-block TPS — derived from the last two samples (each sample
            // is taken on `record_block`, so this is "txs in last block / dt
            // since previous block").
            let last_block_tps_v = if s.len() >= 2 {
                let a = &s[s.len() - 2];
                let b = &s[s.len() - 1];
                let dt = b.t.duration_since(a.t).as_secs_f64();
                if dt > 0.0 {
                    (b.applied - a.applied) as f64 / dt
                } else {
                    0.0
                }
            } else {
                0.0
            };
            // Slice to the last SHORT_WINDOW_SECS for the 5-second view.
            let short_cutoff = now.checked_sub(Duration::from_secs(SHORT_WINDOW_SECS));
            let short_first = short_cutoff
                .and_then(|c| s.iter().find(|x| x.t >= c))
                .or_else(|| s.first());
            let short_last = s.last();
            match (short_first, short_last) {
                (Some(a), Some(b)) if b.t > a.t => {
                    let dt = b.t.duration_since(a.t).as_secs_f64();
                    if dt > 0.0 {
                        let tps_5s_v = (b.applied - a.applied) as f64 / dt;
                        (
                            tps_5s_v,
                            tps_5s_v,
                            tps_60s_v,
                            last_block_tps_v,
                            (b.submitted - a.submitted) as f64 / dt,
                            (b.orders_submitted - a.orders_submitted) as f64 / dt,
                            (b.orders_applied - a.orders_applied) as f64 / dt,
                            (b.buys - a.buys) as f64 / dt,
                            (b.sells - a.sells) as f64 / dt,
                            (b.btc - a.btc) as f64 / dt,
                            (b.eth - a.eth) as f64 / dt,
                        )
                    } else {
                        (
                            0.0,
                            0.0,
                            tps_60s_v,
                            last_block_tps_v,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                        )
                    }
                }
                _ => (
                    0.0,
                    0.0,
                    tps_60s_v,
                    last_block_tps_v,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                ),
            }
        };

        MetricsSnapshot {
            submitted,
            orders_submitted,
            rejected,
            applied,
            orders_applied,
            blocks,
            buys,
            sells,
            btc_orders,
            eth_orders,
            other_orders,
            elapsed_secs,
            avg_tps,
            recent_tps,
            tps_5s,
            tps_60s,
            last_block_tps,
            recent_submitted_per_sec,
            recent_orders_submitted_per_sec,
            recent_orders_applied_per_sec,
            recent_buys_per_sec,
            recent_sells_per_sec,
            recent_btc_per_sec,
            recent_eth_per_sec,
            block_txs_min,
            block_txs_max,
            block_txs_last,
            block_txs_avg,
            channel_depth,
            running: started.is_some(),
        }
    }
}
