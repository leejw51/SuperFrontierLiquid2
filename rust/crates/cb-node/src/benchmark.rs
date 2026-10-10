//! In-process benchmark generator. Spawns N OS worker threads (each with a disjoint
//! trader slice — no nonce contention) that sign synthetic Transfer txs in parallel and
//! push them as `VerifiedTx` into a bounded crossbeam channel. The sequencer drains
//! that channel directly each block, completely bypassing the mempool round-trip.
//!
//! Why the bypass? The mempool stores raw bytes keyed by tx hash. Drain is hash-ordered,
//! which means per-account nonces appear in random order — fine when mempool size ≤
//! max_block_txs, but it caps the achievable inflight depth. The channel is FIFO per
//! worker, so nonces always arrive in order and we can run with a much larger pipeline.
//!
//! # This generator measures transaction throughput, not order throughput
//!
//! Every transaction it signs is a one-unit `TxBody::Transfer`. It never places
//! an order, never touches a book and never produces a fill, so no figure it
//! produces is an order rate. It used to call `Metrics::record_submit_side`,
//! which moved the buy/sell and per-market *order* counters for each transfer;
//! the console then displayed a six-figure "orders per second" while zero
//! orders existed. It now calls `Metrics::record_tx_submitted`, which moves the
//! transaction counter alone.
//!
//! For order throughput — submitted, matched, latency and block time — use
//! `cb_node::order_bench` and the `cb-order-bench` binary.
//!
//! # `BenchWorkload::Orders`
//!
//! The premise `make start2` runs under: order validation belongs to a risk
//! engine in front of this node, so the matcher receives orders it may treat
//! as already checked. This workload is that stream — buyers and sellers,
//! makers (even trader index, `Gtc` a random level behind the touch) and
//! takers (odd index, `Ioc` priced through the book) on BTC/USD and ETH/USD,
//! the shape `cb-sim` sends over RPC — but signed on these worker threads and
//! handed to the sequencer as `VerifiedTx` with the sender attached. No
//! signature is recovered in the node. What the dashboard then shows is the
//! block loop and the matcher, and nothing in front of them.

use cb_dashboard::Metrics;
use cb_sim::{Trader, TraderSet};
use cb_state::{Accounts, Database};
use cb_tx::{validate_static, SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{MarketId, Side};
use crossbeam_channel::{bounded, Receiver, Sender};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::Arc;
use std::thread;

/// What the generator signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchWorkload {
    /// One-unit transfers between traders. Measures transaction throughput.
    Transfers,
    /// Spot orders from buyers and sellers, pre-verified. Measures the
    /// matcher and the block loop with nothing in front of them.
    Orders,
}

impl std::str::FromStr for BenchWorkload {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "transfers" => Ok(Self::Transfers),
            "orders" => Ok(Self::Orders),
            other => Err(format!(
                "unknown workload `{other}`: use transfers or orders"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BenchConfig {
    pub workload: BenchWorkload,
    pub chain_id: u32,
    pub num_traders: u32,
    pub seed_prefix: String,
    pub rng_seed: u64,
    /// Number of OS threads producing signed txs. Must be ≥ 1 and ≤ num_traders.
    pub workers: usize,
    /// Bounded channel capacity. Producers block on send when full — this is the only
    /// backpressure mechanism, replacing the inflight counter from the previous design.
    pub channel_capacity: usize,
}

/// Spawn the benchmark workers and return the receive end of the verified-tx channel.
/// The sequencer drains the channel each block.
pub fn spawn_benchmark(
    db: &Database,
    metrics: Arc<Metrics>,
    cfg: BenchConfig,
) -> anyhow::Result<Receiver<VerifiedTx>> {
    anyhow::ensure!(cfg.num_traders >= 1, "need at least 1 benchmark trader");
    anyhow::ensure!(cfg.workers >= 1, "need at least 1 benchmark worker");
    anyhow::ensure!(
        (cfg.workers as u32) <= cfg.num_traders,
        "workers ({}) must be ≤ num_traders ({})",
        cfg.workers,
        cfg.num_traders
    );

    let mut traders = TraderSet::derive(&cfg.seed_prefix, cfg.num_traders).traders;
    let accts = Accounts::new(db);
    for t in traders.iter_mut() {
        t.nonce = accts.get(&t.addr).map(|s| s.nonce).unwrap_or(0);
    }

    let (tx, rx) = bounded::<VerifiedTx>(cfg.channel_capacity);
    metrics.mark_started();
    tracing::info!(
        traders = traders.len(),
        workers = cfg.workers,
        channel_capacity = cfg.channel_capacity,
        "in-process benchmark spawning"
    );

    // Partition traders across workers as evenly as possible. Each worker owns its
    // slice and is the sole nonce author for those addresses → no cross-worker locking.
    let mut chunks: Vec<Vec<Trader>> = (0..cfg.workers).map(|_| Vec::new()).collect();
    for (i, t) in traders.into_iter().enumerate() {
        chunks[i % cfg.workers].push(t);
    }

    for (worker_ix, mut my_traders) in chunks.into_iter().enumerate() {
        let tx = tx.clone();
        let metrics = metrics.clone();
        let cfg = cfg.clone();
        thread::Builder::new()
            .name(format!("cb-bench-{worker_ix}"))
            .spawn(move || {
                let mut rng =
                    StdRng::seed_from_u64(cfg.rng_seed.wrapping_add(worker_ix as u64 * 0x9E37));
                match cfg.workload {
                    BenchWorkload::Transfers => {
                        run_worker(&tx, &metrics, &cfg, &mut my_traders, &mut rng)
                    }
                    BenchWorkload::Orders => {
                        run_order_worker(&tx, &metrics, &cfg, &mut my_traders, &mut rng)
                    }
                }
            })?;
    }
    drop(tx); // workers each hold a Sender; the original goes out of scope here.
    Ok(rx)
}

fn run_worker(
    tx_chan: &Sender<VerifiedTx>,
    metrics: &Metrics,
    cfg: &BenchConfig,
    traders: &mut [Trader],
    rng: &mut StdRng,
) {
    let n = traders.len();
    loop {
        let buyer_ix = rng.gen_range(0..n);
        let mut seller_ix = if n > 1 { rng.gen_range(0..n) } else { buyer_ix };
        if n > 1 {
            while seller_ix == buyer_ix {
                seller_ix = rng.gen_range(0..n);
            }
        }
        let market = if rng.gen_bool(0.5) {
            MarketId::BtcUsd
        } else {
            MarketId::EthUsd
        };
        let side = if rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };
        let (sender_ix, receiver_ix, asset) = match side {
            Side::Buy => (buyer_ix, seller_ix, market.quote()),
            Side::Sell => (seller_ix, buyer_ix, market.base()),
        };
        let receiver_addr = traders[receiver_ix].addr;
        let sender_addr = traders[sender_ix].addr;
        let sender = &mut traders[sender_ix];
        let raw_tx = Transaction {
            nonce: sender.nonce,
            chain_id: cfg.chain_id,
            body: TxBody::Transfer {
                to: receiver_addr,
                asset,
                amount: 1,
            },
        };
        if validate_static(&raw_tx).is_err() {
            metrics.record_submit_rejected();
            continue;
        }
        let signed = match sign_or_stub(&sender.sk, raw_tx) {
            Ok(s) => s,
            Err(_) => {
                metrics.record_submit_rejected();
                continue;
            }
        };
        let verified = VerifiedTx::from_parts_unchecked(signed, sender_addr);
        sender.nonce += 1;
        metrics.record_tx_submitted();
        if tx_chan.send(verified).is_err() {
            // Receiver dropped — sequencer is shutting down.
            return;
        }
    }
}
/// Sign `raw`, or — when `CB_BENCH_UNSIGNED` is set — stamp it with a
/// placeholder signature instead. The generator's own signing is the one
/// cost `make start2` cannot otherwise remove: the node recovers nothing
/// on this path, so a placeholder changes nothing it does, but signing at
/// ~57,000/s a thread bounded eight threads at 458,000 orders/s while the
/// block loop had room for a million. The placeholder is non-zero:
/// all-zero marks a synthetic system transaction.
fn sign_or_stub(sk: &secp256k1::SecretKey, raw: Transaction) -> Result<SignedTx, cb_tx::SignError> {
    static UNSIGNED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *UNSIGNED.get_or_init(|| std::env::var_os("CB_BENCH_UNSIGNED").is_some()) {
        return Ok(SignedTx {
            tx: raw,
            sig_r: [1; 32],
            sig_s: [1; 32],
            sig_recovery: 0,
        });
    }
    SignedTx::sign_with(sk, raw)
}

/// Price and size conventions for one spot market, matching
/// `genesis-full.jsonl` and `cb_sim::runner::SpotMarket`. The executor's
/// notional is `price * qty / 1e8` whatever the base's decimals, which is why
/// ETH/USD is priced synthetically: a real ETH price is not representable.

#[derive(Clone, Copy)]
struct SpotMarket {
    id: MarketId,
    tick: u128,
    qty: u128,
    mid: u128,
}

const BTC: SpotMarket = SpotMarket {
    id: MarketId::BtcUsd,
    tick: 100_000,
    qty: 100_000,
    mid: 50_000 * 1_000_000,
};
const ETH: SpotMarket = SpotMarket {
    id: MarketId::EthUsd,
    tick: 1,
    qty: 10_000_000_000_000,
    mid: 100,
};
const LEVELS: u128 = 20;
/// A maker cancels its resting orders after this many, so depth stays bounded.
const CANCEL_EVERY: u32 = 256;

impl SpotMarket {
    fn resting(&self, side: Side, level: u128) -> u128 {
        match side {
            Side::Buy => self.mid - self.tick - self.tick * level,
            Side::Sell => self.mid + self.tick + self.tick * level,
        }
    }
    fn crossing(&self, side: Side) -> u128 {
        let span = self.tick * (LEVELS + 2);
        match side {
            Side::Buy => self.mid + self.tick + span,
            Side::Sell => (self.mid - self.tick).saturating_sub(span).max(self.tick),
        }
    }
}

fn run_order_worker(
    tx_chan: &Sender<VerifiedTx>,
    metrics: &Metrics,
    cfg: &BenchConfig,
    traders: &mut [Trader],
    rng: &mut StdRng,
) {
    use cb_types::{Tif, H256};
    let n = traders.len();
    let mut placed: Vec<u32> = vec![0; n];
    let mut counter: u64 = 0;
    loop {
        let ix = rng.gen_range(0..n);
        let market = if rng.gen_bool(0.5) { BTC } else { ETH };
        let side = if rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };
        let maker = traders[ix].index % 2 == 0;
        placed[ix] += 1;
        counter += 1;
        let body = if maker && placed[ix] % CANCEL_EVERY == 0 {
            TxBody::BatchCancel { market: market.id }
        } else {
            let (price, tif) = if maker {
                (market.resting(side, rng.gen_range(0..LEVELS)), Tif::Gtc)
            } else {
                (market.crossing(side), Tif::Ioc)
            };
            let mut cid = [0u8; 32];
            cid[..8].copy_from_slice(&counter.to_be_bytes());
            TxBody::PlaceOrder {
                market: market.id,
                side,
                price,
                qty: market.qty,
                tif,
                post_only: false,
                client_order_id: H256(cid),
            }
        };
        let is_order = matches!(body, TxBody::PlaceOrder { .. });
        let sender_addr = traders[ix].addr;
        let sender = &mut traders[ix];
        let raw_tx = Transaction {
            nonce: sender.nonce,
            chain_id: cfg.chain_id,
            body,
        };
        if validate_static(&raw_tx).is_err() {
            metrics.record_submit_rejected();
            continue;
        }
        let signed = match sign_or_stub(&sender.sk, raw_tx) {
            Ok(s) => s,
            Err(_) => {
                metrics.record_submit_rejected();
                continue;
            }
        };
        let verified = VerifiedTx::from_parts_unchecked(signed, sender_addr);
        sender.nonce += 1;
        if is_order {
            metrics.record_order_submitted(side, market.id);
        } else {
            metrics.record_tx_submitted();
        }
        if tx_chan.send(verified).is_err() {
            return;
        }
    }
}
