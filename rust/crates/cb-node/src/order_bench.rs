//! Order-throughput benchmark: places and matches **real spot CLOB orders**.
//!
//! This module exists because the pre-existing in-process generator
//! ([`crate::benchmark`]) signs `TxBody::Transfer` and never places an order,
//! while the dashboard counted each transfer as an order. Every number here is
//! produced by `TxBody::PlaceOrder` going through the real matcher
//! (`cb_engine::spot_book`), the real executor (`cb_block::apply_tx_at`), the
//! real intra-block ordering (`cb_block::order_for_block`) and the real trie
//! commit (`cb_block::BlockState::commit`).
//!
//! # The two paths, and what each one includes
//!
//! | | in-process | RPC |
//! |---|---|---|
//! | ECDSA signing | bench producer threads | bench producer threads |
//! | ECDSA **recovery** | **not on the path** (`VerifiedTx::from_parts_unchecked`) | **on the path** (`SignedTx::sender()` in the RPC server) |
//! | Cap'n Proto encode/decode | no | yes |
//! | TCP loopback | no | yes |
//! | Mempool (RocksDB `CF_MEMPOOL` put + hash-ordered drain) | no | yes |
//! | `order_for_block` | yes | yes |
//! | matcher + executor + book CF writes | yes | yes |
//! | trie hashing + batched RocksDB commit | yes | yes |
//! | block encode + persist + head advance | yes | yes |
//! | consensus voting / QC | no | no |
//!
//! The in-process path is the engine's ceiling. The RPC path is what a real
//! client pays. Neither is "the" number; quoting one without saying which is
//! the mistake this module was written to stop.
//!
//! # The order mix, and why it is this mix
//!
//! An order that rests is cheap; an order that crosses is not. A benchmark
//! where nothing matches measures admission and book hydration, not matching.
//! So the workload is explicitly split and both halves are reported:
//!
//! * **Maker orders** (`--makers-per-block`) — `Gtc`, `post_only: false`,
//!   priced strictly inside the ladder so they always rest and never cross.
//!   These replenish depth consumed by takers and are the "cheap" path.
//! * **Taker orders** (`--takers-per-block`) — `Ioc`. A configurable share
//!   (`--cross-rate-bps`) is priced through the whole ladder so it crosses and
//!   fills; the rest is priced away from the book so it matches nothing and is
//!   discarded. The crossing share is the "expensive" path and the source of
//!   the headline `matched_orders_per_sec`.
//!
//! Defaults are 50/50 maker/taker with `cross_rate_bps = 10_000`, i.e. every
//! taker crosses. That maximises matching work per submitted order, which is
//! the conservative choice for a throughput claim: it is the *hardest* mix,
//! not the flattering one. Lower `--cross-rate-bps` to see how much of the
//! cost is matching versus admission.
//!
//! ## Why the workload cannot produce a rejected transaction
//!
//! `build_and_apply` propagates the first `ExecError` and abandons the whole
//! block, so a benchmark that produces rejects silently loses every tx in the
//! block and reports a throughput number for work that never landed. The
//! workload is therefore constructed so no reject is reachable:
//!
//! * Makers and takers are **disjoint address sets**, so self-trade (which is
//!   `SelfTradeAction::CancelTaker`, an `Err`) cannot occur.
//! * Makers are never `post_only` and never priced to cross, so
//!   `PostOnlyCrossed` cannot occur.
//! * No order uses `Fok`, so `FokUnfilled` cannot occur. An `Ioc` that matches
//!   nothing returns `Ok` and is discarded — that is the intended cheap path.
//! * Every address is funded far past what the run can spend, and prices and
//!   quantities are tick- and lot-aligned by construction.
//! * Each producer thread owns a disjoint slice of addresses and is the sole
//!   author of their nonces; the channel is FIFO per producer and
//!   `order_for_block` keeps each sender's transactions together in nonce
//!   order (groups ranked cancel > post-only > rest), so nonces always
//!   apply in order.
//!
//! The reports still carry `orders_rejected`, and the binary fails loudly if it
//! is non-zero. A benchmark that tolerates silent rejects is measuring nothing.
//!
//! # How "matched" is counted
//!
//! The executor returns `Result<(), ExecError>`; a filled `Ioc` and an empty
//! `Ioc` are indistinguishable from the return value, and the chain has no
//! trades column family. So a taker order counts as **matched** iff the
//! taker's base-asset balance changed across its own `apply_tx_at` call — which
//! for a spot fill it always does, on both sides. The check is two
//! `BlockState::load` calls, i.e. two `HashMap` lookups against the block's
//! already-warm account cache, and it is inside the timed region. It costs
//! well under a percent of per-order time and is disclosed rather than hidden.
//!
//! `matched_orders_per_sec` therefore counts **taker orders that produced at
//! least one fill** — the crossing event. Each such order consumed one or more
//! resting maker orders, so the number of *trades* is greater than or equal to
//! it. Passive maker fills are deliberately not counted again.
//!
//! # Determinism
//!
//! The order stream is seeded from `--rng-seed` and the address set from
//! `--seed-prefix`. Two runs with the same config submit the same orders in
//! the same per-address sequence. Block *boundaries* still depend on producer
//! timing, so the final state root is compared across runs only by the
//! `--deterministic` mode, which runs the generator single-threaded with a
//! fixed number of orders per block and no channel.

use crate::sequencer::PruningCfg;
use cb_block::{build_and_apply, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, sign, PublicKey, SecretKey};
use cb_dashboard::Metrics;
use cb_state::{
    AccountState, Accounts, Database, MarketConfig, Markets, Mempool, Meta, Orders as OrdersCf,
};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, MarketId, Side, Tif, H256};
use crossbeam_channel::{bounded, Receiver, Sender};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Basis-point denominator.
const BPS: u32 = 10_000;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Everything the order benchmark needs to build a market, an address set and
/// an order stream. Every field is CLI-settable so a run is reproducible from
/// the command line alone.
#[derive(Clone, Debug)]
pub struct OrderBenchConfig {
    /// Chain id stamped into every signed transaction. Must match `Meta::chain_id`.
    pub chain_id: u32,
    /// The spot market orders are placed against.
    pub market: MarketId,
    /// Tick size in quote base units. Prices must be a multiple of this.
    pub tick_size: u128,
    /// Minimum (and lot) quantity in base-asset units.
    pub min_qty: u128,
    /// Taker fee in basis points, charged to the crossing side.
    pub fee_bps: u16,
    /// Mid price in quote base units per one whole unit of the base asset.
    /// For BTC/USD, `50_000 * 1e6` is $50,000.
    pub mid_price: u128,
    /// Number of resting price levels on each side of the book.
    pub price_levels: u32,
    /// Resting orders held on each side of the book. Warmup builds this depth
    /// before the measured window opens, and replenishment maintains it.
    pub book_depth: u32,
    /// Quantity of every order, in base-asset units. Taker and maker sizes are
    /// equal so one crossing taker consumes exactly one resting maker.
    pub order_qty: u128,
    /// Blocks in the measured window.
    pub blocks: u64,
    /// Ceiling on transactions drained into one block.
    pub max_block_txs: usize,
    /// Taker (`Ioc`) orders the producers aim to emit per block.
    pub takers_per_block: u32,
    /// Maker (`Gtc`, resting) orders the producers aim to emit per block.
    pub makers_per_block: u32,
    /// Share of taker orders priced to cross the book, in basis points.
    /// `10_000` means every taker crosses.
    pub cross_rate_bps: u32,
    /// Distinct maker addresses.
    pub num_makers: u32,
    /// Distinct taker addresses.
    pub num_takers: u32,
    /// Producer threads. Each owns a disjoint slice of both address sets.
    pub producers: usize,
    /// Bounded channel capacity between producers and the block loop. This is
    /// the only backpressure mechanism, and it is also what bounds queueing
    /// delay: the default is two blocks' worth of orders, so the reported
    /// latency percentiles describe a pipeline two blocks deep rather than an
    /// unbounded queue. Raising it raises throughput slightly and latency a
    /// lot; say which you set when quoting either.
    pub channel_capacity: usize,
    /// Fill the producer channel to `channel_capacity` before the measured
    /// window opens, so the window times the block path and not how fast the
    /// producers can sign. Once the engine outruns the signers, an open-loop
    /// run measures the signers; this measures the engine. Latency figures
    /// from a prefilled run describe a full queue and mean nothing.
    pub prefill: bool,
    /// Target block interval in milliseconds. The block loop sleeps only when
    /// the channel is empty; under load blocks are produced as fast as the
    /// executor allows, which is what makes block-time degradation visible.
    pub block_interval_ms: u64,
    /// PRNG seed. Same seed, same order stream.
    pub rng_seed: u64,
    /// Prefix for deterministic key derivation.
    pub seed_prefix: String,
    /// Collect a per-phase time breakdown. Adds two `Instant::now()` calls per
    /// phase per block, not per order, so the overhead is negligible.
    pub phase_timing: bool,
    /// Optional submission-rate cap, in orders per second across all producers.
    ///
    /// Without it the producers run open-loop and the pipeline saturates, so
    /// the throughput figure is the system's ceiling and the latency figure is
    /// the depth of the queue in front of it — a true number that answers a
    /// different question than "how long does an order take". Set this below
    /// the measured ceiling to get a latency that describes service time
    /// rather than backlog. Always quote which of the two a latency came from.
    pub target_orders_per_sec: Option<u64>,
    /// Write each block to RocksDB before the next one starts, as the node
    /// does with `--sync-writes`. Off by default so the benchmark describes
    /// the node as it ships: the write happens on a writer thread while the
    /// next block is built, and the measured window ends only once every
    /// queued write has landed.
    pub sync_writes: bool,
}

impl Default for OrderBenchConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            market: MarketId::BtcUsd,
            // Matches `genesis-full.jsonl`'s BTC/USD row: $0.10 ticks,
            // 1e-5 BTC lots, 10 bps taker fee.
            tick_size: 100_000,
            min_qty: 1_000,
            fee_bps: 10,
            mid_price: 50_000 * 1_000_000,
            price_levels: 20,
            book_depth: 2_000,
            order_qty: 100_000,
            blocks: 200,
            max_block_txs: 20_000,
            takers_per_block: 1_000,
            makers_per_block: 1_000,
            cross_rate_bps: BPS,
            num_makers: 64,
            num_takers: 64,
            producers: 4,
            channel_capacity: 4_000,
            prefill: false,
            block_interval_ms: 50,
            rng_seed: 0x0_ADDE_C0DE,
            seed_prefix: "cb-order-bench-".to_string(),
            phase_timing: true,
            target_orders_per_sec: None,
            sync_writes: false,
        }
    }
}

impl OrderBenchConfig {
    /// Best ask price: one tick above mid.
    fn best_ask(&self) -> u128 {
        self.mid_price + self.tick_size
    }

    /// Best bid price: one tick below mid.
    fn best_bid(&self) -> u128 {
        self.mid_price - self.tick_size
    }

    /// Price of ask level `level` (0 = best).
    fn ask_at(&self, level: u32) -> u128 {
        self.best_ask() + self.tick_size * level as u128
    }

    /// Price of bid level `level` (0 = best).
    fn bid_at(&self, level: u32) -> u128 {
        self.best_bid() - self.tick_size * level as u128
    }

    /// Limit price for a taker that must cross every resting level on the
    /// opposite side. Price-time priority means it still consumes the best
    /// level first.
    fn crossing_price(&self, side: Side) -> u128 {
        let span = self.tick_size * (self.price_levels as u128 + 2);
        match side {
            Side::Buy => self.best_ask() + span,
            Side::Sell => self.best_bid().saturating_sub(span).max(self.tick_size),
        }
    }

    /// Limit price for a taker that must NOT cross: parked far on its own side
    /// of the book. Because these orders are `Ioc` they match nothing and are
    /// discarded rather than resting.
    fn non_crossing_price(&self, side: Side) -> u128 {
        let span = self.tick_size * (self.price_levels as u128 + 2);
        match side {
            Side::Buy => self
                .bid_at(self.price_levels)
                .saturating_sub(span)
                .max(self.tick_size),
            Side::Sell => self.ask_at(self.price_levels) + span,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.producers >= 1, "need at least 1 producer thread");
        anyhow::ensure!(self.num_makers >= 1, "need at least 1 maker address");
        anyhow::ensure!(self.num_takers >= 1, "need at least 1 taker address");
        anyhow::ensure!(
            self.producers as u32 <= self.num_makers.min(self.num_takers),
            "producers ({}) must be <= min(num_makers, num_takers) = {}",
            self.producers,
            self.num_makers.min(self.num_takers)
        );
        anyhow::ensure!(self.price_levels >= 1, "need at least 1 price level");
        anyhow::ensure!(self.tick_size >= 1, "tick_size must be >= 1");
        anyhow::ensure!(self.min_qty >= 1, "min_qty must be >= 1");
        anyhow::ensure!(
            self.order_qty % self.min_qty == 0,
            "order_qty ({}) must be a multiple of min_qty ({})",
            self.order_qty,
            self.min_qty
        );
        anyhow::ensure!(
            self.mid_price % self.tick_size == 0,
            "mid_price ({}) must be a multiple of tick_size ({})",
            self.mid_price,
            self.tick_size
        );
        anyhow::ensure!(
            self.cross_rate_bps <= BPS,
            "cross_rate_bps must be <= {BPS}"
        );
        anyhow::ensure!(
            self.takers_per_block + self.makers_per_block >= 1,
            "need at least one order per block"
        );
        anyhow::ensure!(self.blocks >= 1, "need at least 1 block");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Where per-block wall time went. Totals across the measured window, in
/// microseconds. Only populated when `phase_timing` is set.
///
/// The split is by *stage*, not by function: `execute` is the `apply_tx_at`
/// loop, which is admission + book hydration + matching + account mutation +
/// order/book-index CF writes. `commit` is `BlockState::commit`, which is the
/// three trie hashes plus the single batched RocksDB write. `persist` is block
/// encode, header signing, `Blocks::put` and `Meta::set_head`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseTotals {
    /// Draining the producer channel (in-process) or the mempool (RPC).
    pub drain_us: u64,
    /// `order_for_block`.
    pub ordering_us: u64,
    /// The `apply_tx_at` loop: matching, state mutation, book CF writes.
    pub execute_us: u64,
    /// `BlockState::commit`: trie hashing + one batched RocksDB write.
    pub commit_us: u64,
    /// Block encode, header sign, `Blocks::put`, `Meta::set_head`.
    pub persist_us: u64,
}

impl PhaseTotals {
    /// Sum of every phase, in microseconds.
    pub fn total_us(&self) -> u64 {
        self.drain_us + self.ordering_us + self.execute_us + self.commit_us + self.persist_us
    }

    /// Each phase as a share of the total, as `(label, microseconds, percent)`.
    pub fn breakdown(&self) -> Vec<(&'static str, u64, f64)> {
        let total = self.total_us().max(1) as f64;
        vec![
            ("drain", self.drain_us, self.drain_us as f64 * 100.0 / total),
            (
                "ordering",
                self.ordering_us,
                self.ordering_us as f64 * 100.0 / total,
            ),
            (
                "execute (match + state)",
                self.execute_us,
                self.execute_us as f64 * 100.0 / total,
            ),
            (
                "commit (trie + rocksdb)",
                self.commit_us,
                self.commit_us as f64 * 100.0 / total,
            ),
            (
                "persist (block + head)",
                self.persist_us,
                self.persist_us as f64 * 100.0 / total,
            ),
        ]
    }
}

/// Latency percentiles in microseconds, computed from a full sample vector.
#[derive(Clone, Copy, Debug, Default)]
pub struct Percentiles {
    /// Number of samples the percentiles were computed from.
    pub count: usize,
    /// 50th percentile, microseconds.
    pub p50_us: u64,
    /// 99th percentile, microseconds.
    pub p99_us: u64,
    /// 99.9th percentile, microseconds.
    pub p999_us: u64,
    /// Largest sample, microseconds.
    pub max_us: u64,
    /// Arithmetic mean, microseconds.
    pub mean_us: f64,
}

impl Percentiles {
    /// Sort `samples` in place and extract the percentiles.
    pub fn from_samples(samples: &mut [u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let pick = |q: f64| -> u64 {
            let n = samples.len();
            let idx = ((n as f64 - 1.0) * q).round() as usize;
            samples[idx.min(n - 1)]
        };
        let sum: u128 = samples.iter().map(|v| *v as u128).sum();
        Self {
            count: samples.len(),
            p50_us: pick(0.50),
            p99_us: pick(0.99),
            p999_us: pick(0.999),
            max_us: samples[samples.len() - 1],
            mean_us: sum as f64 / samples.len() as f64,
        }
    }
}

/// The result of one benchmark run. Every rate here names exactly what it
/// counts; nothing in this struct conflates transactions with orders.
#[derive(Clone, Debug, Default)]
pub struct OrderBenchReport {
    /// Which path produced these numbers.
    pub path: &'static str,
    /// Blocks produced inside the measured window.
    pub blocks: u64,
    /// Wall-clock seconds of the measured window.
    pub wall_secs: f64,

    /// `PlaceOrder` transactions the producers signed and handed off.
    pub orders_submitted: u64,
    /// `PlaceOrder` transactions that landed in a block and returned `Ok`.
    pub orders_applied: u64,
    /// `PlaceOrder` transactions the executor rejected. Must be zero.
    pub orders_rejected: u64,
    /// Orders that were submitted but never landed in a block inside the
    /// window (still queued, or dropped by a failed block build).
    pub orders_unaccounted: u64,

    /// Applied taker (`Ioc`) orders.
    pub taker_orders: u64,
    /// Applied taker orders that produced at least one fill.
    pub taker_orders_matched: u64,
    /// Applied maker (`Gtc`) orders.
    pub maker_orders: u64,
    /// Base-asset units that changed hands across all matched taker orders.
    pub matched_base_qty: u128,

    /// Orders submitted per second, over the measured wall clock.
    pub orders_submitted_per_sec: f64,
    /// Orders applied in a block per second, over the measured wall clock.
    pub orders_applied_per_sec: f64,
    /// **Matched** orders per second: taker orders that produced a fill.
    pub matched_orders_per_sec: f64,

    /// Submit-to-fill latency for matched taker orders.
    pub fill_latency: Percentiles,
    /// Submit-to-inclusion latency for every applied order.
    pub inclusion_latency: Percentiles,
    /// Wall time from the start of one block build to the start of the next.
    pub block_time: Percentiles,
    /// Orders per block: min, max, mean.
    pub block_orders_min: u64,
    /// Largest order count in a single block.
    pub block_orders_max: u64,
    /// Mean order count per block.
    pub block_orders_mean: f64,

    /// Per-phase totals, when `phase_timing` was on.
    pub phases: PhaseTotals,
    /// Resting orders left on the book at the end of the run.
    pub final_book_orders: u64,
    /// State root after the last block. Equal across two runs of
    /// `run_deterministic` with the same config.
    pub final_state_root: H256,
}

impl OrderBenchReport {
    /// Render the report as the block of text the binary prints. Each line
    /// states what it measures.
    pub fn render(&self, cfg: &OrderBenchConfig) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "path                      = {}", self.path);
        let _ = writeln!(
            s,
            "mix                       = {} maker (Gtc, rests) + {} taker (Ioc) per block, cross_rate {:.1}%",
            cfg.makers_per_block,
            cfg.takers_per_block,
            cfg.cross_rate_bps as f64 / 100.0
        );
        let _ = writeln!(
            s,
            "book                      = depth {} per side over {} levels, qty {} base units",
            cfg.book_depth, cfg.price_levels, cfg.order_qty
        );
        let _ = writeln!(s, "blocks                    = {}", self.blocks);
        let _ = writeln!(
            s,
            "wall_secs                 = {:.3}  (ends when the last block is on disk)",
            self.wall_secs
        );
        let _ = writeln!(s, "orders_submitted          = {}", self.orders_submitted);
        let _ = writeln!(s, "orders_applied            = {}", self.orders_applied);
        let _ = writeln!(s, "orders_rejected           = {}", self.orders_rejected);
        let _ = writeln!(
            s,
            "orders_unaccounted        = {} (submitted, not in a block when the window closed)",
            self.orders_unaccounted
        );
        let _ = writeln!(s, "maker_orders (rested)     = {}", self.maker_orders);
        let _ = writeln!(s, "taker_orders (Ioc)        = {}", self.taker_orders);
        let _ = writeln!(
            s,
            "taker_orders_MATCHED      = {}  <- orders that produced >=1 fill",
            self.taker_orders_matched
        );
        let _ = writeln!(s, "matched_base_qty          = {}", self.matched_base_qty);
        let _ = writeln!(
            s,
            "orders_submitted_per_sec  = {:.0}",
            self.orders_submitted_per_sec
        );
        let _ = writeln!(
            s,
            "orders_applied_per_sec    = {:.0}  <- all PlaceOrder txs that landed in a block",
            self.orders_applied_per_sec
        );
        let _ = writeln!(
            s,
            "MATCHED_orders_per_sec    = {:.0}  <- the number that matters",
            self.matched_orders_per_sec
        );
        let _ = writeln!(
            s,
            "fill_latency_us           = p50 {} / p99 {} / p99.9 {} / max {} (n={})",
            self.fill_latency.p50_us,
            self.fill_latency.p99_us,
            self.fill_latency.p999_us,
            self.fill_latency.max_us,
            self.fill_latency.count
        );
        let _ = writeln!(
            s,
            "inclusion_latency_us      = p50 {} / p99 {} / p99.9 {} / max {} (n={})",
            self.inclusion_latency.p50_us,
            self.inclusion_latency.p99_us,
            self.inclusion_latency.p999_us,
            self.inclusion_latency.max_us,
            self.inclusion_latency.count
        );
        let _ = writeln!(
            s,
            "block_time_us             = p50 {} / p99 {} / max {} (target interval {} ms)",
            self.block_time.p50_us,
            self.block_time.p99_us,
            self.block_time.max_us,
            cfg.block_interval_ms
        );
        let _ = writeln!(
            s,
            "orders_per_block          = min {} / mean {:.0} / max {}",
            self.block_orders_min, self.block_orders_mean, self.block_orders_max
        );
        let _ = writeln!(s, "final_book_orders         = {}", self.final_book_orders);
        let _ = writeln!(s, "final_state_root          = {:?}", self.final_state_root);
        if self.phases.total_us() > 0 {
            let _ = writeln!(s, "where the time goes (block loop, measured window):");
            for (label, us, pct) in self.phases.breakdown() {
                let _ = writeln!(s, "  {label:<24} {us:>12} us  {pct:>6.2}%");
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Address set and chain setup
// ---------------------------------------------------------------------------

/// One synthetic account: key, address and locally tracked nonce.
pub struct BenchAccount {
    /// Signing key derived from `keccak256("{seed_prefix}{role}{i}")`.
    pub sk: SecretKey,
    /// Address recovered from the derived public key.
    pub addr: Address,
    /// Next nonce this account will sign with.
    pub nonce: u64,
}

fn derive_accounts(seed_prefix: &str, role: &str, n: u32) -> Vec<BenchAccount> {
    (0..n)
        .map(|i| {
            let (sk, pk) = keypair_from_seed(format!("{seed_prefix}{role}{i}").as_bytes());
            BenchAccount {
                sk,
                addr: address_from_pubkey(&pk),
                nonce: 0,
            }
        })
        .collect()
}

/// Open a fresh RocksDB at `path`, register the benchmark's spot market and
/// fund every maker and taker address far past what the run can spend.
///
/// Funding is deliberately generous rather than exact: an `Insufficient`
/// rejection aborts the whole block in `build_and_apply`, so the benchmark must
/// make it unreachable rather than merely unlikely.
pub fn setup_chain(
    db: &Database,
    cfg: &OrderBenchConfig,
    makers: &[BenchAccount],
    takers: &[BenchAccount],
) {
    Meta::new(db).set_chain_id(cfg.chain_id);
    Markets::new(db).put(&MarketConfig {
        id: cfg.market,
        base: cfg.market.base(),
        quote: cfg.market.quote(),
        tick_size: cfg.tick_size,
        min_qty: cfg.min_qty,
        fee_bps: cfg.fee_bps,
    });

    // Head-room: every address could conceivably place every order in the run.
    let max_orders_each = (cfg.blocks + 1) * (cfg.takers_per_block + cfg.makers_per_block) as u64
        + cfg.book_depth as u64 * 2;
    let base_need = cfg
        .order_qty
        .saturating_mul(max_orders_each as u128)
        .saturating_mul(4);
    let quote_need = cfg
        .crossing_price(Side::Buy)
        .saturating_mul(cfg.order_qty)
        .saturating_div(100_000_000)
        .saturating_mul(max_orders_each as u128)
        .saturating_mul(4);

    let accts = Accounts::new(db);
    for a in makers.iter().chain(takers.iter()) {
        let mut s = AccountState::default();
        s.add(cfg.market.base(), base_need.max(1_000_000_000_000))
            .expect("seed balance");
        s.add(cfg.market.quote(), quote_need.max(1_000_000_000_000))
            .expect("seed balance");
        accts.put(&a.addr, &s);
    }
}

// ---------------------------------------------------------------------------
// Order generation
// ---------------------------------------------------------------------------

/// What a generated order is for. The block loop uses this to attribute
/// applied orders to the maker or taker bucket and to decide whether to check
/// for a fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderRole {
    /// `Gtc`, priced inside the ladder, always rests.
    MakerRest,
    /// `Ioc`, priced through the book, expected to fill.
    TakerCross,
    /// `Ioc`, priced away from the book, expected to match nothing.
    TakerNoCross,
}

/// One client's record that it submitted a transaction: the hash to match a
/// block against, the instant just before the Cap'n Proto write, and whether
/// the order was a taker (so a fill latency can be separated from a plain
/// inclusion latency).
#[derive(Clone, Copy, Debug)]
pub struct SubmitStamp {
    /// `SignedTx::hash`, which is what a block carries.
    pub hash: H256,
    /// When the client began the submit.
    pub at: Instant,
    /// `true` for an `Ioc` taker order.
    pub is_taker: bool,
}

/// A generated order on its way to a block, carrying the instant it was
/// handed off so the block loop can compute submit-to-fill latency.
pub struct PendingOrder {
    /// The signed, sender-resolved transaction.
    pub tx: VerifiedTx,
    /// When the producer finished signing and handed the order off.
    pub submitted_at: Instant,
    /// Maker, crossing taker, or non-crossing taker.
    pub role: OrderRole,
}

/// Build one `PlaceOrder` transaction for `acct` in `role`, advancing its nonce.
fn make_order(
    cfg: &OrderBenchConfig,
    acct: &mut BenchAccount,
    role: OrderRole,
    side: Side,
    level: u32,
    client_order_id: H256,
) -> anyhow::Result<VerifiedTx> {
    let (price, tif) = match role {
        OrderRole::MakerRest => (
            match side {
                Side::Buy => cfg.bid_at(level),
                Side::Sell => cfg.ask_at(level),
            },
            Tif::Gtc,
        ),
        OrderRole::TakerCross => (cfg.crossing_price(side), Tif::Ioc),
        OrderRole::TakerNoCross => (cfg.non_crossing_price(side), Tif::Ioc),
    };
    let raw = Transaction {
        nonce: acct.nonce,
        chain_id: cfg.chain_id,
        body: TxBody::PlaceOrder {
            market: cfg.market,
            side,
            price,
            qty: cfg.order_qty,
            tif,
            post_only: false,
            client_order_id,
        },
    };
    let signed = SignedTx::sign_with(&acct.sk, raw)?;
    acct.nonce += 1;
    Ok(VerifiedTx::from_parts_unchecked(signed, acct.addr))
}

/// Deterministic per-order client id, so two runs with the same seed produce
/// byte-identical transactions.
fn client_id(counter: u64) -> H256 {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&counter.to_be_bytes());
    H256(b)
}

/// State a producer carries between orders: which role comes next and how
/// many it has emitted.
struct ProducerState {
    makers: Vec<BenchAccount>,
    takers: Vec<BenchAccount>,
    rng: StdRng,
    counter: u64,
    maker_cursor: usize,
    taker_cursor: usize,
    /// Emit this many makers, then this many takers, then repeat. Keeps the
    /// configured mix stable at every timescale rather than only on average.
    makers_per_round: u32,
    takers_per_round: u32,
    in_round: u32,
}

impl ProducerState {
    fn next_order(&mut self, cfg: &OrderBenchConfig) -> anyhow::Result<PendingOrder> {
        let round_len = self.makers_per_round + self.takers_per_round;
        let is_maker = round_len > 0 && self.in_round < self.makers_per_round;
        self.in_round = (self.in_round + 1) % round_len.max(1);

        let cid = client_id(self.counter);
        self.counter += 1;
        let side = if self.rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };

        let (role, acct) = if is_maker {
            let ix = self.maker_cursor % self.makers.len();
            self.maker_cursor += 1;
            (OrderRole::MakerRest, &mut self.makers[ix])
        } else {
            let ix = self.taker_cursor % self.takers.len();
            self.taker_cursor += 1;
            let crosses =
                cfg.cross_rate_bps >= BPS || self.rng.gen_ratio(cfg.cross_rate_bps.min(BPS), BPS);
            let role = if crosses {
                OrderRole::TakerCross
            } else {
                OrderRole::TakerNoCross
            };
            (role, &mut self.takers[ix])
        };
        let level = self.rng.gen_range(0..cfg.price_levels);
        let tx = make_order(cfg, acct, role, side, level, cid)?;
        Ok(PendingOrder {
            tx,
            submitted_at: Instant::now(),
            role,
        })
    }
}

// ---------------------------------------------------------------------------
// Block loop shared between warmup and the measured window
// ---------------------------------------------------------------------------

/// Running tallies the block loop accumulates.
#[derive(Default)]
struct Tally {
    applied: u64,
    rejected: u64,
    taker: u64,
    taker_matched: u64,
    maker: u64,
    matched_base_qty: u128,
    fill_latencies: Vec<u64>,
    inclusion_latencies: Vec<u64>,
    block_times: Vec<u64>,
    block_orders: Vec<u64>,
    phases: PhaseTotals,
}

/// Apply one block: order the transactions, run the executor, commit the
/// tries, sign and persist the header, advance the head.
///
/// This reimplements `cb_block::build_and_apply` for one reason only:
/// `build_and_apply` owns the `BlockState` privately, so a fill cannot be
/// observed per transaction from outside it and per-phase timing cannot be
/// collected.
///
/// It is not a byte-for-byte copy. It omits `enforce_required_funding_ticks`
/// and `enforce_required_futures_settle`, which are no-ops for a chain with no
/// registered derivative markets, and it stamps the header timestamp from the
/// height rather than the wall clock — a field outside the state-root preimage.
/// Everything that feeds the state root is the same call in the same order.
///
/// `instrumented_block_loop_matches_build_and_apply` is the check: it feeds the
/// same transactions through this loop on one database and through the real
/// `build_and_apply` on another, and asserts the state roots agree block for
/// block. Without that test the "it mirrors the real thing" claim would be an
/// assertion about code rather than a measurement.
#[allow(clippy::too_many_arguments)]
fn apply_block(
    db: &Database,
    cfg: &OrderBenchConfig,
    proposer_sk: &SecretKey,
    proposer_addr: Address,
    batch: Vec<PendingOrder>,
    tally: &mut Tally,
    record_latency: bool,
    chain: &std::sync::Arc<std::sync::Mutex<cb_block::SealChain>>,
) -> anyhow::Result<H256> {
    use cb_crypto::keccak256;

    let base = cfg.market.base();

    // Keep role and submit time alongside each tx across the sort. Keyed by
    // `(sender, nonce)`, which is unique per transaction and costs nothing to
    // form: keying by `SignedTx::hash` re-encoded and keccak'd every
    // transaction twice, and was about a seventh of the block thread's time,
    // harness work the node never does.
    let mut meta: std::collections::HashMap<(Address, u64), (OrderRole, Instant)> =
        std::collections::HashMap::with_capacity(batch.len());
    let mut txs = Vec::with_capacity(batch.len());
    for p in batch {
        meta.insert(
            (p.tx.sender, p.tx.signed.tx.nonce),
            (p.role, p.submitted_at),
        );
        txs.push(p.tx);
    }
    let height = Meta::new(db).head_height() + 1;
    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr,
    };

    // Everything below the observer is the node's own block path,
    // `execute_and_hand_off`: ordering, execution with a savepoint per
    // transaction, and handing the rows to the writer thread to be sealed.
    // The benchmark used to carry a hand-copied block loop, and twice that
    // copy measured work the node does not do. It now only watches.
    let mut applied_here = 0u64;
    let mut fill_submits: Vec<Instant> = Vec::new();
    let mut inclusion_submits: Vec<Instant> = Vec::new();
    let mut before = 0u128;
    let observe = |state: &mut BlockState,
                   v: &VerifiedTx,
                   outcome: Option<&Result<(), cb_block::ExecError>>| {
        let (role, submitted_at) = meta
            .get(&(v.sender, v.signed.tx.nonce))
            .copied()
            .unwrap_or((OrderRole::MakerRest, Instant::now()));
        let watch = matches!(role, OrderRole::TakerCross | OrderRole::TakerNoCross);
        match outcome {
            None => {
                if watch {
                    before = state.load(&v.sender).balance(base);
                }
            }
            Some(Ok(())) => {
                tally.applied += 1;
                applied_here += 1;
                if watch {
                    tally.taker += 1;
                    let after = state.load(&v.sender).balance(base);
                    if after != before {
                        tally.taker_matched += 1;
                        tally.matched_base_qty += after.abs_diff(before);
                        if record_latency {
                            fill_submits.push(submitted_at);
                        }
                    }
                } else {
                    tally.maker += 1;
                }
                if record_latency {
                    inclusion_submits.push(submitted_at);
                }
            }
            Some(Err(e)) => {
                tally.rejected += 1;
                tracing::warn!(
                    ?e,
                    height,
                    "order rejected — the workload should make this unreachable"
                );
            }
        }
    };
    let sk = *proposer_sk;
    let chain = std::sync::Arc::clone(chain);
    let out = cb_block::execute_and_hand_off(
        db,
        &ctx,
        txs,
        height,
        0,
        Vec::new(),
        chain,
        move |preimage: &[u8]| {
            sign(&sk, keccak256(preimage))
                .expect("proposer sign")
                .to_vec()
        },
        |_, _| {},
        observe,
    )
    .map_err(|e| anyhow::anyhow!("block {height}: {e:?}"))?;

    if record_latency {
        // Latencies are taken against the moment this block was handed off,
        // not a clock read per transaction: a block's transactions land
        // together, and two clock reads per order were a measurable share of
        // the thread being measured.
        let executed_at = Instant::now();
        let us = |t: &Instant| executed_at.saturating_duration_since(*t).as_micros() as u64;
        tally.fill_latencies.extend(fill_submits.iter().map(us));
        tally
            .inclusion_latencies
            .extend(inclusion_submits.iter().map(us));
    }
    if cfg.phase_timing {
        tally.phases.execute_us += out.execute_us;
        tally.phases.commit_us += out.handoff_us;
    }
    tally.block_orders.push(applied_here);
    // The sealed root when writes are synchronous; with a writer thread it is
    // the latest sealed one, and the caller flushes before reporting a root.
    Ok(Meta::new(db).state_root())
}

// ---------------------------------------------------------------------------
// In-process path
// ---------------------------------------------------------------------------

/// Run the in-process order benchmark.
///
/// Producer threads sign orders and push them into a bounded channel; the block
/// loop drains up to `max_block_txs` per block and applies them. Signature
/// recovery is not on this path, the mempool is not on this path, and there is
/// no network. Everything else — ordering, matching, account mutation, book
/// column-family writes, trie hashing, the batched RocksDB commit and block
/// persistence — is.
pub fn run_inprocess(db: &Database, cfg: &OrderBenchConfig) -> anyhow::Result<OrderBenchReport> {
    cfg.validate()?;
    if !cfg.sync_writes {
        db.enable_async_writes()?;
    }
    let mut makers = derive_accounts(&cfg.seed_prefix, "maker-", cfg.num_makers);
    let takers = derive_accounts(&cfg.seed_prefix, "taker-", cfg.num_takers);
    setup_chain(db, cfg, &makers, &takers);

    let (proposer_sk, proposer_pk) = keypair_from_seed(b"cb-order-bench-proposer");
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(db).set_proposer_addr(proposer_addr);

    // ---- warmup: build the resting book ----
    let seal_chain = std::sync::Arc::new(std::sync::Mutex::new(cb_block::SealChain::at_head(db)));

    warmup_book(
        db,
        cfg,
        &mut makers,
        &proposer_sk,
        proposer_addr,
        &seal_chain,
    )?;

    // ---- producers ----
    let stop = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicU64::new(0));
    let (tx_chan, rx_chan) = bounded::<PendingOrder>(cfg.channel_capacity);
    let handles = spawn_producers(cfg, makers, takers, &tx_chan, &stop, &submitted)?;
    drop(tx_chan);

    // ---- startup load: every account and book into memory ----
    //
    // The node does this before its first block (ADR 0007), so the measured
    // window starts where a running sequencer is: nothing on the block path
    // reads an account or a book from RocksDB. Timed and reported, but outside
    // the window.
    let preload_start = Instant::now();
    let preloaded = cb_block::preload_state(db)?;
    tracing::info!(
        accounts = preloaded.accounts,
        markets = preloaded.markets,
        resting_orders = preloaded.resting_orders,
        ms = preload_start.elapsed().as_millis() as u64,
        "state loaded into memory before the measured window"
    );
    eprintln!(
        "preload                   = {} accounts, {} markets, {} resting orders in {} ms (not timed)",
        preloaded.accounts,
        preloaded.markets,
        preloaded.resting_orders,
        preload_start.elapsed().as_millis()
    );

    if cfg.prefill {
        let fill_start = Instant::now();
        while rx_chan.len() < cfg.channel_capacity {
            anyhow::ensure!(
                fill_start.elapsed() < Duration::from_secs(600),
                "producers did not fill the channel within ten minutes"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!(
            "prefill                   = {} signed orders queued in {} ms (not timed)",
            rx_chan.len(),
            fill_start.elapsed().as_millis()
        );
    }

    // ---- measured window ----
    let mut tally = Tally::default();
    let start = Instant::now();
    let mut last_block_start = start;
    for _ in 0..cfg.blocks {
        let drain_start = cfg.phase_timing.then(Instant::now);
        let batch = drain_channel(&rx_chan, cfg.max_block_txs, cfg.block_interval_ms);
        if let (Some(s), true) = (drain_start, cfg.phase_timing) {
            tally.phases.drain_us += s.elapsed().as_micros() as u64;
        }
        if batch.is_empty() {
            continue;
        }
        let block_start = Instant::now();
        tally
            .block_times
            .push(block_start.duration_since(last_block_start).as_micros() as u64);
        last_block_start = block_start;
        apply_block(
            db,
            cfg,
            &proposer_sk,
            proposer_addr,
            batch,
            &mut tally,
            true,
            &seal_chain,
        )?;
    }
    // The window closes when the last block is on disk, not when the last
    // block was built: a writer thread that is still catching up is work this
    // run caused.
    db.flush_pending()?;
    let wall_secs = start.elapsed().as_secs_f64();
    // Every block is sealed now, so the head's root is the last block's.
    let final_root = Meta::new(db).state_root();
    stop.store(true, Ordering::Relaxed);
    // Unblock producers parked on a full channel, then join.
    while rx_chan.try_recv().is_ok() {}
    for h in handles {
        let _ = h.join();
    }

    Ok(finish(
        "in-process (no signature recovery, no mempool, no network)",
        db,
        cfg,
        tally,
        submitted.load(Ordering::Relaxed),
        wall_secs,
        final_root,
    ))
}

/// Seed the resting book to `book_depth` orders per side before the measured
/// window opens. Without this the first blocks measure an empty book and the
/// match rate is meaningless.
fn warmup_book(
    db: &Database,
    cfg: &OrderBenchConfig,
    makers: &mut [BenchAccount],
    proposer_sk: &SecretKey,
    proposer_addr: Address,
    chain: &std::sync::Arc<std::sync::Mutex<cb_block::SealChain>>,
) -> anyhow::Result<()> {
    let mut counter = u64::MAX / 2; // disjoint from the measured window's ids
    let mut cursor = 0usize;
    let mut remaining_per_side = cfg.book_depth;
    let chunk = cfg.max_block_txs.max(1);
    let mut warm_cfg = cfg.clone();
    warm_cfg.phase_timing = false;
    let mut tally = Tally::default();
    while remaining_per_side > 0 {
        let this_round = remaining_per_side.min((chunk / 2) as u32);
        let mut batch = Vec::with_capacity(this_round as usize * 2);
        for k in 0..this_round {
            for side in [Side::Buy, Side::Sell] {
                let ix = cursor % makers.len();
                cursor += 1;
                let level = k % cfg.price_levels;
                let tx = make_order(
                    cfg,
                    &mut makers[ix],
                    OrderRole::MakerRest,
                    side,
                    level,
                    client_id(counter),
                )?;
                counter += 1;
                batch.push(PendingOrder {
                    tx,
                    submitted_at: Instant::now(),
                    role: OrderRole::MakerRest,
                });
            }
        }
        apply_block(
            db,
            &warm_cfg,
            proposer_sk,
            proposer_addr,
            batch,
            &mut tally,
            false,
            chain,
        )?;
        remaining_per_side -= this_round;
    }
    anyhow::ensure!(
        tally.rejected == 0,
        "warmup produced {} rejected orders — the book fixture is wrong",
        tally.rejected
    );
    Ok(())
}

/// Partition the address sets across producer threads and start them. Each
/// thread is the sole author of its slice's nonces, so no cross-thread
/// coordination is needed and the channel stays FIFO per address.
fn spawn_producers(
    cfg: &OrderBenchConfig,
    makers: Vec<BenchAccount>,
    takers: Vec<BenchAccount>,
    tx_chan: &Sender<PendingOrder>,
    stop: &Arc<AtomicBool>,
    submitted: &Arc<AtomicU64>,
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    let mut maker_chunks: Vec<Vec<BenchAccount>> = (0..cfg.producers).map(|_| Vec::new()).collect();
    for (i, m) in makers.into_iter().enumerate() {
        maker_chunks[i % cfg.producers].push(m);
    }
    let mut taker_chunks: Vec<Vec<BenchAccount>> = (0..cfg.producers).map(|_| Vec::new()).collect();
    for (i, t) in takers.into_iter().enumerate() {
        taker_chunks[i % cfg.producers].push(t);
    }

    let mut handles = Vec::with_capacity(cfg.producers);
    for (ix, (my_makers, my_takers)) in maker_chunks.into_iter().zip(taker_chunks).enumerate() {
        let chan = tx_chan.clone();
        let stop = stop.clone();
        let submitted = submitted.clone();
        let cfg = cfg.clone();
        // Split the per-block mix evenly across producers, rounding up so the
        // aggregate is never short of the configured rate.
        let per = |total: u32| total.div_ceil(cfg.producers as u32).max(1);
        let makers_per_round = per(cfg.makers_per_block);
        let takers_per_round = per(cfg.takers_per_block);
        handles.push(
            std::thread::Builder::new()
                .name(format!("cb-order-bench-{ix}"))
                .spawn(move || {
                    let mut st = ProducerState {
                        makers: my_makers,
                        takers: my_takers,
                        rng: StdRng::seed_from_u64(
                            cfg.rng_seed.wrapping_add(ix as u64 * 0x9E37_79B9),
                        ),
                        counter: (ix as u64) << 40,
                        maker_cursor: 0,
                        taker_cursor: 0,
                        makers_per_round,
                        takers_per_round,
                        in_round: 0,
                    };
                    // A generated order has already consumed its account's
                    // nonce, so it can never be dropped: dropping one leaves a
                    // permanent hole that makes every later order from that
                    // account `BadNonce`. On a full channel the producer parks
                    // holding the order and retries it, which is also the
                    // backpressure that keeps submitted ~= applied.
                    let mut pending: Option<PendingOrder> = None;
                    let mut pacer = Pacer::new(cfg.target_orders_per_sec, cfg.producers);
                    while !stop.load(Ordering::Relaxed) {
                        pacer.wait();
                        let order = match pending.take() {
                            Some(o) => o,
                            None => match st.next_order(&cfg) {
                                Ok(o) => o,
                                Err(e) => {
                                    tracing::error!(?e, "order generation failed");
                                    return;
                                }
                            },
                        };
                        match chan.send_timeout(order, Duration::from_millis(20)) {
                            Ok(()) => {
                                submitted.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(crossbeam_channel::SendTimeoutError::Timeout(o)) => {
                                pending = Some(o);
                            }
                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return,
                        }
                    }
                })?,
        );
    }
    Ok(handles)
}

/// Drain up to `cap` orders from the producer channel, waiting up to
/// `interval_ms` for the first one so an empty channel does not spin.
fn drain_channel(rx: &Receiver<PendingOrder>, cap: usize, interval_ms: u64) -> Vec<PendingOrder> {
    let mut out = Vec::with_capacity(cap.min(4096));
    match rx.recv_timeout(Duration::from_millis(interval_ms.max(1))) {
        Ok(v) => out.push(v),
        Err(_) => return out,
    }
    while out.len() < cap {
        match rx.try_recv() {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    out
}

/// Turn the accumulated tallies into a report.
fn finish(
    path: &'static str,
    db: &Database,
    cfg: &OrderBenchConfig,
    mut tally: Tally,
    orders_submitted: u64,
    wall_secs: f64,
    final_state_root: H256,
) -> OrderBenchReport {
    let per_sec = |n: u64| {
        if wall_secs > 0.0 {
            n as f64 / wall_secs
        } else {
            0.0
        }
    };
    let blocks = tally.block_orders.len() as u64;
    let block_orders_min = tally.block_orders.iter().copied().min().unwrap_or(0);
    let block_orders_max = tally.block_orders.iter().copied().max().unwrap_or(0);
    let block_orders_mean = if blocks > 0 {
        tally.block_orders.iter().sum::<u64>() as f64 / blocks as f64
    } else {
        0.0
    };
    OrderBenchReport {
        path,
        blocks,
        wall_secs,
        orders_submitted,
        orders_applied: tally.applied,
        orders_rejected: tally.rejected,
        orders_unaccounted: orders_submitted.saturating_sub(tally.applied + tally.rejected),
        taker_orders: tally.taker,
        taker_orders_matched: tally.taker_matched,
        maker_orders: tally.maker,
        matched_base_qty: tally.matched_base_qty,
        orders_submitted_per_sec: per_sec(orders_submitted),
        orders_applied_per_sec: per_sec(tally.applied),
        matched_orders_per_sec: per_sec(tally.taker_matched),
        fill_latency: Percentiles::from_samples(&mut tally.fill_latencies),
        inclusion_latency: Percentiles::from_samples(&mut tally.inclusion_latencies),
        block_time: Percentiles::from_samples(&mut tally.block_times),
        block_orders_min,
        block_orders_max,
        block_orders_mean,
        phases: tally.phases,
        final_book_orders: OrdersCf::new(db).list_for_market(cfg.market).len() as u64,
        final_state_root,
    }
}

// ---------------------------------------------------------------------------
// Deterministic replay mode
// ---------------------------------------------------------------------------

/// Run a fixed, single-threaded order stream through **`build_and_apply`** —
/// the function the real sequencer calls — and return the final state root.
///
/// This is the fidelity check for the instrumented block loop above and the
/// determinism check for the workload: two runs with the same config must
/// produce the same root, and the root must match what [`run_inprocess`]'s loop
/// produces for the same transactions. It reports no rate; it is a correctness
/// harness, not a benchmark.
pub fn run_deterministic(db: &Database, cfg: &OrderBenchConfig) -> anyhow::Result<H256> {
    cfg.validate()?;
    let mut makers = derive_accounts(&cfg.seed_prefix, "maker-", cfg.num_makers);
    let takers = derive_accounts(&cfg.seed_prefix, "taker-", cfg.num_takers);
    setup_chain(db, cfg, &makers, &takers);
    let (proposer_sk, proposer_pk) = keypair_from_seed(b"cb-order-bench-proposer");
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(db).set_proposer_addr(proposer_addr);
    let seal_chain = std::sync::Arc::new(std::sync::Mutex::new(cb_block::SealChain::at_head(db)));

    warmup_book(
        db,
        cfg,
        &mut makers,
        &proposer_sk,
        proposer_addr,
        &seal_chain,
    )?;

    let mut st = ProducerState {
        makers,
        takers,
        rng: StdRng::seed_from_u64(cfg.rng_seed),
        counter: 0,
        maker_cursor: 0,
        taker_cursor: 0,
        makers_per_round: cfg.makers_per_block.max(1),
        takers_per_round: cfg.takers_per_block.max(1),
        in_round: 0,
    };
    let per_block = (cfg.makers_per_block + cfg.takers_per_block) as usize;
    let mut root = H256::ZERO;
    for h in 0..cfg.blocks {
        let mut batch = Vec::with_capacity(per_block);
        for _ in 0..per_block {
            batch.push(st.next_order(cfg)?.tx);
        }
        let signer = |preimage: &[u8]| {
            sign(&proposer_sk, cb_crypto::keccak256(preimage))
                .expect("proposer sign")
                .to_vec()
        };
        let ctx = ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        };
        let res = build_and_apply(db, &ctx, batch, h, 0, Vec::new(), signer)
            .map_err(|e| anyhow::anyhow!("build_and_apply failed: {e:?}"))?;
        root = res.state_root;
    }
    Ok(root)
}

// ---------------------------------------------------------------------------
// RPC path
// ---------------------------------------------------------------------------

/// Run the order benchmark over the real Cap'n Proto RPC server and the real
/// sequencer loop.
///
/// This path pays everything the in-process path skips: Cap'n Proto encode on
/// the client, TCP loopback, Cap'n Proto decode on the server, **ECDSA sender
/// recovery in `submitTx`**, a RocksDB `CF_MEMPOOL` put per transaction, and
/// the sequencer's hash-ordered mempool drain. It is what a real client
/// experiences, minus consensus voting.
///
/// Latency is measured as: the client's `Instant` immediately before the capnp
/// `submitTx` write, to the instant a local watcher observes the head advance
/// past the block containing that transaction. The watcher polls
/// `Meta::head_height()` directly rather than over RPC, so the reported figure
/// excludes the propagation of the block back out to a subscriber; it is
/// submit-to-commit, and the watcher's poll interval bounds the error.
pub fn run_rpc(cfg: &OrderBenchConfig, listen_port: u16) -> anyhow::Result<OrderBenchReport> {
    cfg.validate()?;
    let dir = tempfile::tempdir()?;
    let db = Database::open(dir.path())?;
    let mut makers = derive_accounts(&cfg.seed_prefix, "maker-", cfg.num_makers);
    let takers = derive_accounts(&cfg.seed_prefix, "taker-", cfg.num_takers);
    setup_chain(&db, cfg, &makers, &takers);
    if !cfg.sync_writes {
        db.enable_async_writes()?;
    }

    let (proposer_sk, proposer_pk) = keypair_from_seed(b"cb-order-bench-proposer");
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(&db).set_proposer_addr(proposer_addr);
    let seal_chain = std::sync::Arc::new(std::sync::Mutex::new(cb_block::SealChain::at_head(&db)));

    warmup_book(
        &db,
        cfg,
        &mut makers,
        &proposer_sk,
        proposer_addr,
        &seal_chain,
    )?;
    let head_after_warmup = Meta::new(&db).head_height();

    // ---- server side: real RPC server + real sequencer loop ----
    let notify = Arc::new(tokio::sync::Notify::new());
    let metrics = Arc::new(Metrics::new());
    let addr: std::net::SocketAddr = format!("127.0.0.1:{listen_port}").parse()?;
    let server = cb_rpc::SequencerServer::without_voting(db.clone(), notify.clone());
    let _rpc_thread = cb_rpc::run_local_serve(addr, server);

    let seq_db = db.clone();
    let seq_notify = notify.clone();
    let seq_metrics = metrics.clone();
    let seq_cfg = cfg.clone();
    let seq_key = Arc::new(proposer_sk);
    let seq_stop = Arc::new(AtomicBool::new(false));
    let seq_stop_inner = seq_stop.clone();
    let sequencer = std::thread::Builder::new()
        .name("cb-order-bench-sequencer".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("sequencer runtime");
            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                let task = tokio::task::spawn_local(crate::sequencer::run_sequencer(
                    seq_db,
                    ExecCtx {
                        treasury_signer: Address::ZERO,
                        ..Default::default()
                    },
                    seq_notify,
                    seq_cfg.block_interval_ms,
                    seq_cfg.max_block_txs,
                    seq_metrics,
                    None,
                    PruningCfg::disabled(),
                    seq_key,
                    None,
                    // The benchmark signs its own nonces and drives the
                    // mempool directly; the admission gate is off for it.
                    None,
                    None,
                ));
                while !seq_stop_inner.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                task.abort();
                let _ = task.await;
            }));
        })?;

    // Give the listener a moment to bind before the clients dial it.
    std::thread::sleep(Duration::from_millis(200));

    // ---- client side ----
    let stop = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicU64::new(0));
    let accepted = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    // Latency plumbing. Each client announces `(tx hash, the instant just
    // before the capnp write, whether it was a taker)` down an unbounded
    // channel; the watcher drains it into a map and, when a block appears,
    // subtracts. A channel rather than a shared mutex keeps the clients off a
    // lock on their hot path, so the measurement does not perturb the thing
    // being measured.
    let (sub_tx, sub_rx) = crossbeam_channel::unbounded::<SubmitStamp>();
    let clients = spawn_rpc_clients(
        cfg, makers, takers, addr, &stop, &submitted, &accepted, &rejected, sub_tx,
    )?;

    // ---- watcher: time each block's commit and attribute orders to it ----
    let start = Instant::now();
    let mut tally = Tally::default();
    let mut last_seen = head_after_warmup;
    let mut last_block_at = start;
    let mut stamps: std::collections::HashMap<H256, SubmitStamp> = std::collections::HashMap::new();
    let deadline = start + Duration::from_millis(cfg.block_interval_ms.max(1) * cfg.blocks * 4);
    while tally.block_orders.len() < cfg.blocks as usize && Instant::now() < deadline {
        while let Ok(stamp) = sub_rx.try_recv() {
            stamps.insert(stamp.hash, stamp);
        }
        let head = Meta::new(&db).head_height();
        if head <= last_seen {
            std::thread::sleep(Duration::from_micros(100));
            continue;
        }
        let now = Instant::now();
        for h in (last_seen + 1)..=head {
            // The head height is published when a block is handed off, and
            // its bytes only when the writer thread seals it (ADR 0007). An
            // unsealed height is not skipped: stop here and retry it next poll.
            let Some(bytes) = cb_state::Blocks::new(&db).get(h) else {
                break;
            };
            last_seen = h;
            let Ok(block) = cb_block::Block::decode(&bytes) else {
                continue;
            };
            let n = block.txs.len() as u64;
            tally.applied += n;
            tally.block_orders.push(n);
            tally
                .block_times
                .push(now.duration_since(last_block_at).as_micros() as u64);
            last_block_at = now;
            // A transaction can reach a block before its stamp has been
            // pulled off the channel, so drain once more before matching.
            while let Ok(stamp) = sub_rx.try_recv() {
                stamps.insert(stamp.hash, stamp);
            }
            for t in &block.txs {
                if let TxBody::PlaceOrder { tif, .. } = &t.tx.body {
                    match tif {
                        Tif::Ioc => tally.taker += 1,
                        _ => tally.maker += 1,
                    }
                }
                if let Some(stamp) = stamps.remove(&t.hash()) {
                    let us = now.duration_since(stamp.at).as_micros() as u64;
                    tally.inclusion_latencies.push(us);
                    if stamp.is_taker {
                        tally.fill_latencies.push(us);
                    }
                }
            }
        }
        if last_seen < head {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
    let wall_secs = start.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for c in clients {
        let _ = c.join();
    }
    seq_stop.store(true, Ordering::Relaxed);
    let _ = sequencer.join();

    // Matched taker orders: takers never rest, so a taker whose base balance
    // moved across the measured window filled at least once. Counting matched
    // *orders* exactly needs a per-tx observation the RPC path cannot make
    // from outside the executor, so this path reports matched taker orders as
    // the count of Ioc orders that consumed book depth, derived from the
    // book's resting-order delta. See `docs/performance.md`.
    let final_book = OrdersCf::new(&db).list_for_market(cfg.market).len() as u64;
    let consumed = (cfg.book_depth as u64 * 2 + tally.maker).saturating_sub(final_book);
    tally.taker_matched = consumed.min(tally.taker);
    tally.matched_base_qty = consumed as u128 * cfg.order_qty;

    let mut report = finish(
        "RPC (Cap'n Proto + TCP + ECDSA recovery + mempool)",
        &db,
        cfg,
        tally,
        submitted.load(Ordering::Relaxed),
        wall_secs,
        Meta::new(&db).state_root(),
    );
    report.orders_rejected = rejected.load(Ordering::Relaxed);
    report.orders_unaccounted = accepted
        .load(Ordering::Relaxed)
        .saturating_sub(report.orders_applied);
    Ok(report)
}

/// Start one client thread per producer, each with its own single-threaded
/// tokio runtime, `LocalSet` and TCP connection (capnp-rpc futures are `!Send`).
/// Submits are pipelined: the next order is written without awaiting the
/// previous reply, so the measurement is of the server, not of round-trip
/// latency.
#[allow(clippy::too_many_arguments)]
fn spawn_rpc_clients(
    cfg: &OrderBenchConfig,
    makers: Vec<BenchAccount>,
    takers: Vec<BenchAccount>,
    addr: std::net::SocketAddr,
    stop: &Arc<AtomicBool>,
    submitted: &Arc<AtomicU64>,
    accepted: &Arc<AtomicU64>,
    rejected: &Arc<AtomicU64>,
    stamps: Sender<SubmitStamp>,
) -> anyhow::Result<Vec<std::thread::JoinHandle<()>>> {
    let mut maker_chunks: Vec<Vec<BenchAccount>> = (0..cfg.producers).map(|_| Vec::new()).collect();
    for (i, m) in makers.into_iter().enumerate() {
        maker_chunks[i % cfg.producers].push(m);
    }
    let mut taker_chunks: Vec<Vec<BenchAccount>> = (0..cfg.producers).map(|_| Vec::new()).collect();
    for (i, t) in takers.into_iter().enumerate() {
        taker_chunks[i % cfg.producers].push(t);
    }

    let mut handles = Vec::with_capacity(cfg.producers);
    for (ix, (my_makers, my_takers)) in maker_chunks.into_iter().zip(taker_chunks).enumerate() {
        let cfg = cfg.clone();
        let stop = stop.clone();
        let submitted = submitted.clone();
        let accepted = accepted.clone();
        let rejected = rejected.clone();
        let stamps = stamps.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("cb-order-bench-client-{ix}"))
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!(?e, "client runtime");
                            return;
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    rt.block_on(local.run_until(rpc_client_loop(
                        cfg, my_makers, my_takers, addr, ix, stop, submitted, accepted, rejected,
                        stamps,
                    )));
                })?,
        );
    }
    Ok(handles)
}

#[allow(clippy::too_many_arguments)]
async fn rpc_client_loop(
    cfg: OrderBenchConfig,
    makers: Vec<BenchAccount>,
    takers: Vec<BenchAccount>,
    addr: std::net::SocketAddr,
    ix: usize,
    stop: Arc<AtomicBool>,
    submitted: Arc<AtomicU64>,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
    stamps: Sender<SubmitStamp>,
) {
    use cb_rpc::conv::signed_tx_to_capnp;
    use cb_schema::rpc_capnp::submit_result;

    let client = match cb_rpc::connect(addr).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(?e, "client connect");
            return;
        }
    };
    let per = |total: u32| total.div_ceil(cfg.producers as u32).max(1);
    let mut st = ProducerState {
        makers,
        takers,
        rng: StdRng::seed_from_u64(cfg.rng_seed.wrapping_add(ix as u64 * 0x9E37_79B9)),
        counter: (ix as u64) << 40,
        maker_cursor: 0,
        taker_cursor: 0,
        makers_per_round: per(cfg.makers_per_block),
        takers_per_round: per(cfg.takers_per_block),
        in_round: 0,
    };
    // Cap the number of promises in flight so a slow server cannot make the
    // client's own queue the thing being measured.
    let inflight = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let max_inflight = 2_048usize;
    let mut pacer = AsyncPacer::new(cfg.target_orders_per_sec, cfg.producers);
    while !stop.load(Ordering::Relaxed) {
        pacer.wait().await;
        if inflight.get() >= max_inflight {
            tokio::task::yield_now().await;
            continue;
        }
        let order = match st.next_order(&cfg) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(?e, "order generation failed");
                return;
            }
        };
        let hash = order.tx.signed.hash();
        let is_taker = matches!(order.role, OrderRole::TakerCross | OrderRole::TakerNoCross);
        let mut req = client.cap.submit_tx_request();
        signed_tx_to_capnp(req.get().init_tx(), &order.tx.signed);
        // Stamp immediately before the write, so the measurement starts where
        // a real client's request starts rather than where it was generated.
        let _ = stamps.send(SubmitStamp {
            hash,
            at: Instant::now(),
            is_taker,
        });
        let promise = req.send().promise;
        submitted.fetch_add(1, Ordering::Relaxed);
        inflight.set(inflight.get() + 1);
        // Hand control back to the `RpcSystem` task on **every** iteration.
        // `req.send()` only queues into capnp's outgoing buffer; nothing
        // reaches the socket until the RPC system is polled, and the RPC
        // system is a `spawn_local` task on this same single-threaded
        // `LocalSet`. Yielding only when `inflight` filled up meant a
        // rate-limited client sat on up to `max_inflight` stamped-but-unsent
        // orders — at 500 orders/sec per client that is four seconds of
        // fabricated latency, which is exactly the "stall" an earlier run of
        // this benchmark reported and wrongly blamed on the mempool. The
        // stamp-to-write gap is now one scheduler turn.
        tokio::task::yield_now().await;
        let accepted = accepted.clone();
        let rejected = rejected.clone();
        let inflight2 = inflight.clone();
        tokio::task::spawn_local(async move {
            match promise.await {
                Ok(resp) => match resp.get().and_then(|g| g.get_result()) {
                    Ok(r) => match r.which() {
                        Ok(submit_result::Ok(())) => {
                            accepted.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    Err(_) => {
                        rejected.fetch_add(1, Ordering::Relaxed);
                    }
                },
                Err(_) => {
                    rejected.fetch_add(1, Ordering::Relaxed);
                }
            }
            inflight2.set(inflight2.get().saturating_sub(1));
        });
    }
    // Let the last promises settle so `accepted` is not short of reality.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

// ---------------------------------------------------------------------------
// Signature-recovery micro-measurement
// ---------------------------------------------------------------------------

/// Cost of ECDSA sender recovery for `n` benchmark-shaped orders, serially and
/// across the rayon pool. Returned as `(serial_per_sec, parallel_per_sec)`.
///
/// This exists because the RPC path's dominant suspect is signature recovery,
/// and a suspicion is not a measurement. The orders are the same shape the
/// benchmark submits, so the per-signature cost is directly comparable against
/// the per-order cost the RPC path reports.
pub fn measure_recovery(cfg: &OrderBenchConfig, n: usize) -> anyhow::Result<(f64, f64)> {
    use rayon::prelude::*;
    let mut acct = derive_accounts(&cfg.seed_prefix, "recovery-", 1);
    let mut signed: Vec<SignedTx> = Vec::with_capacity(n);
    for i in 0..n {
        let v = make_order(
            cfg,
            &mut acct[0],
            OrderRole::TakerCross,
            Side::Buy,
            (i as u32) % cfg.price_levels,
            client_id(i as u64),
        )?;
        signed.push(v.signed);
    }

    let t0 = Instant::now();
    let mut sink = 0u8;
    for s in &signed {
        sink ^= s.sender().map(|a| a.0[0]).unwrap_or(0);
    }
    let serial = t0.elapsed().as_secs_f64();
    std::hint::black_box(sink);

    let t1 = Instant::now();
    let recovered: Vec<Address> = signed
        .par_iter()
        .map(|s| s.sender().unwrap_or(Address::ZERO))
        .collect();
    let parallel = t1.elapsed().as_secs_f64();
    std::hint::black_box(recovered.len());

    Ok((
        if serial > 0.0 { n as f64 / serial } else { 0.0 },
        if parallel > 0.0 {
            n as f64 / parallel
        } else {
            0.0
        },
    ))
}

/// Mempool round-trip cost: `n` `MempoolRecord` puts followed by one drain of
/// the same size, on a throwaway database. Returns `(put_per_sec,
/// drain_per_sec)`.
///
/// The benchmark module header for the transfer generator claims the
/// hash-ordered drain caps pipeline depth. This measures what the mempool
/// itself costs, so that claim can be weighed against the other phases.
pub fn measure_mempool(cfg: &OrderBenchConfig, n: usize) -> anyhow::Result<(f64, f64)> {
    let dir = tempfile::tempdir()?;
    let db = Database::open(dir.path())?;
    let mut acct = derive_accounts(&cfg.seed_prefix, "mempool-", 1);
    let mut records = Vec::with_capacity(n);
    for i in 0..n {
        let v = make_order(
            cfg,
            &mut acct[0],
            OrderRole::TakerCross,
            Side::Buy,
            (i as u32) % cfg.price_levels,
            client_id(i as u64),
        )?;
        let hash = v.signed.hash();
        let sender = v.sender;
        let nonce = v.signed.tx.nonce;
        let rec = cb_tx::MempoolRecord {
            signed: v.signed,
            sender: v.sender,
        };
        records.push((hash, sender, nonce, bincode::serialize(&rec)?));
    }

    let t0 = Instant::now();
    let pool = Mempool::new(&db);
    for (h, sender, nonce, bytes) in &records {
        pool.put(*h, *sender, *nonce, bytes);
    }
    let put = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    let drained = pool.drain(n);
    let verified =
        cb_tx::decode_mempool_batch(&drained.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>());
    let drain = t1.elapsed().as_secs_f64();
    std::hint::black_box(verified.len());

    Ok((
        if put > 0.0 { n as f64 / put } else { 0.0 },
        if drain > 0.0 { n as f64 / drain } else { 0.0 },
    ))
}

/// Re-export so the binary can name the key type without depending on secp256k1.
pub type BenchPublicKey = PublicKey;

/// Paces one producer to its share of a global submission-rate target.
///
/// Each producer gets `target / producers` orders per second and sleeps to its
/// next slot. A producer that falls behind does not try to catch up by bursting
/// — the schedule is reset to now — so an overloaded run degrades into
/// open-loop saturation rather than into a sawtooth.
struct Pacer {
    interval: Option<Duration>,
    next: Instant,
}

impl Pacer {
    /// Shortest sleep worth asking for.
    ///
    /// Both `std::thread::sleep` and `tokio::time::sleep_until` round up to
    /// roughly a millisecond, so asking for a 200 µs gap between orders yields
    /// a 1 ms one and silently caps the achievable rate at about 1,000/sec per
    /// producer. Below this threshold the pacer does not sleep at all; the
    /// deadline still advances by the exact interval, so the schedule debt
    /// accumulates until one sleep of at least a tick pays it off. The rate
    /// averages out; orders arrive in bursts of at most one tick's worth.
    const MIN_SLEEP: Duration = Duration::from_millis(1);

    /// One producer's share of a global rate, as a per-order interval.
    fn interval_for(target_per_sec: Option<u64>, producers: usize) -> Option<Duration> {
        target_per_sec.and_then(|t| {
            let share = t / producers.max(1) as u64;
            1_000_000_000u64
                .checked_div(share)
                .map(Duration::from_nanos)
        })
    }

    fn new(target_per_sec: Option<u64>, producers: usize) -> Self {
        Self {
            interval: Self::interval_for(target_per_sec, producers),
            next: Instant::now(),
        }
    }

    fn wait(&mut self) {
        let Some(interval) = self.interval else {
            return;
        };
        let now = Instant::now();
        if self.next > now {
            if self.next - now >= Pacer::MIN_SLEEP {
                std::thread::sleep(self.next - now);
            }
        } else {
            // Behind schedule: run flat out and re-base rather than burst.
            self.next = now;
        }
        self.next += interval;
    }
}

/// The same pacing as [`Pacer`], for a client running on a `LocalSet`.
///
/// It must not use `std::thread::sleep`: that blocks the whole single-threaded
/// runtime, including the `RpcSystem` task that actually writes bytes to the
/// socket, so the pacing gap would also be a gap in I/O.
struct AsyncPacer {
    interval: Option<Duration>,
    next: tokio::time::Instant,
}

impl AsyncPacer {
    fn new(target_per_sec: Option<u64>, producers: usize) -> Self {
        Self {
            interval: Pacer::interval_for(target_per_sec, producers),
            next: tokio::time::Instant::now(),
        }
    }

    async fn wait(&mut self) {
        let Some(interval) = self.interval else {
            return;
        };
        let now = tokio::time::Instant::now();
        if self.next > now {
            if self.next - now >= Pacer::MIN_SLEEP {
                tokio::time::sleep_until(self.next).await;
            }
        } else {
            self.next = now;
        }
        self.next += interval;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> OrderBenchConfig {
        OrderBenchConfig {
            blocks: 6,
            book_depth: 40,
            price_levels: 4,
            takers_per_block: 8,
            makers_per_block: 8,
            num_makers: 8,
            num_takers: 8,
            producers: 2,
            max_block_txs: 256,
            channel_capacity: 1_024,
            // The block loop is fast enough to cut a block before a producer
            // has signed its first taker; prefill so every block has both.
            prefill: true,
            block_interval_ms: 5,
            ..Default::default()
        }
    }

    #[test]
    fn inprocess_run_matches_orders_and_rejects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let cfg = tiny();
        let r = run_inprocess(&db, &cfg).unwrap();
        assert_eq!(r.orders_rejected, 0, "workload must not produce rejects");
        assert!(r.orders_applied > 0, "no orders applied");
        assert!(
            r.taker_orders_matched > 0,
            "no taker order matched — the benchmark would be measuring the wrong thing"
        );
        assert!(r.matched_base_qty > 0);
    }

    #[test]
    fn deterministic_replay_reproduces_the_state_root() {
        let cfg = OrderBenchConfig {
            blocks: 4,
            book_depth: 20,
            price_levels: 4,
            takers_per_block: 4,
            makers_per_block: 4,
            num_makers: 4,
            num_takers: 4,
            producers: 1,
            max_block_txs: 128,
            ..tiny()
        };
        let d1 = tempfile::tempdir().unwrap();
        let a = run_deterministic(&Database::open(d1.path()).unwrap(), &cfg).unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let b = run_deterministic(&Database::open(d2.path()).unwrap(), &cfg).unwrap();
        assert_eq!(a, b, "same config must reproduce the same state root");
        assert_ne!(a, H256::ZERO);
    }

    #[test]
    fn non_crossing_takers_do_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let cfg = OrderBenchConfig {
            cross_rate_bps: 0,
            ..tiny()
        };
        let r = run_inprocess(&db, &cfg).unwrap();
        assert_eq!(r.orders_rejected, 0);
        assert_eq!(
            r.taker_orders_matched, 0,
            "cross_rate_bps=0 must produce no fills"
        );
    }

    /// The instrumented block loop must agree with the real one.
    ///
    /// `apply_block` exists so a fill can be observed per transaction and each
    /// phase timed, neither of which `build_and_apply` permits from outside.
    /// That is only acceptable if the two produce the same chain. This feeds
    /// one fixed order stream through `apply_block` on one database and the
    /// real `build_and_apply` on another, and compares state roots block for
    /// block — so a future edit to either path that changes what lands in the
    /// state root fails here rather than quietly making the benchmark measure
    /// something the sequencer does not do.
    #[test]
    fn instrumented_block_loop_matches_build_and_apply() {
        let cfg = OrderBenchConfig {
            blocks: 6,
            book_depth: 24,
            price_levels: 3,
            takers_per_block: 6,
            makers_per_block: 6,
            num_makers: 6,
            num_takers: 6,
            producers: 1,
            max_block_txs: 128,
            phase_timing: false,
            ..tiny()
        };

        let dir_a = tempfile::tempdir().unwrap();
        let db_a = Database::open(dir_a.path()).unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let db_b = Database::open(dir_b.path()).unwrap();

        let (proposer_sk, proposer_pk) = keypair_from_seed(b"cb-order-bench-proposer");
        let proposer_addr = address_from_pubkey(&proposer_pk);

        // Identical genesis and identical warmup on both databases.
        let mut warm_makers: Vec<Vec<BenchAccount>> = Vec::new();
        for db in [&db_a, &db_b] {
            let mut makers = derive_accounts(&cfg.seed_prefix, "maker-", cfg.num_makers);
            let takers = derive_accounts(&cfg.seed_prefix, "taker-", cfg.num_takers);
            setup_chain(db, &cfg, &makers, &takers);
            Meta::new(db).set_proposer_addr(proposer_addr);
            let seal_chain =
                std::sync::Arc::new(std::sync::Mutex::new(cb_block::SealChain::at_head(db)));
            warmup_book(
                db,
                &cfg,
                &mut makers,
                &proposer_sk,
                proposer_addr,
                &seal_chain,
            )
            .unwrap();
            warm_makers.push(makers);
        }
        assert_eq!(
            Meta::new(&db_a).state_root(),
            Meta::new(&db_b).state_root(),
            "warmup must leave both databases identical"
        );
        // The intra-block ordering is seeded by the parent block's hash, and
        // the two databases were warmed by two independent signings whose
        // headers need not hash alike. State is what this test compares, so
        // give both sides the same hash chain to seed from.
        let align_heads = |a: &Database, b: &Database| {
            let m = Meta::new(a);
            Meta::new(b).set_head(m.head_height(), m.head_hash(), m.state_root());
        };
        align_heads(&db_a, &db_b);
        let chain_a =
            std::sync::Arc::new(std::sync::Mutex::new(cb_block::SealChain::at_head(&db_a)));

        // One order stream, replayed from the same seed on both sides.
        let producer = |makers: Vec<BenchAccount>| ProducerState {
            makers,
            takers: derive_accounts(&cfg.seed_prefix, "taker-", cfg.num_takers),
            rng: StdRng::seed_from_u64(cfg.rng_seed),
            counter: 0,
            maker_cursor: 0,
            taker_cursor: 0,
            makers_per_round: cfg.makers_per_block,
            takers_per_round: cfg.takers_per_block,
            in_round: 0,
        };
        let mut st_a = producer(warm_makers.remove(0));
        let mut st_b = producer(warm_makers.remove(0));

        let per_block = (cfg.makers_per_block + cfg.takers_per_block) as usize;
        let mut tally = Tally::default();
        for h in 0..cfg.blocks {
            let batch_a: Vec<PendingOrder> = (0..per_block)
                .map(|_| st_a.next_order(&cfg).unwrap())
                .collect();
            let batch_b: Vec<VerifiedTx> = (0..per_block)
                .map(|_| st_b.next_order(&cfg).unwrap().tx)
                .collect();
            // The two streams must be the same transactions, or the comparison
            // below would be vacuous.
            assert_eq!(
                batch_a
                    .iter()
                    .map(|p| p.tx.signed.hash())
                    .collect::<Vec<_>>(),
                batch_b.iter().map(|v| v.signed.hash()).collect::<Vec<_>>(),
                "the two replays diverged at block {h}"
            );

            let root_a = apply_block(
                &db_a,
                &cfg,
                &proposer_sk,
                proposer_addr,
                batch_a,
                &mut tally,
                false,
                &chain_a,
            )
            .unwrap();

            let signer = |preimage: &[u8]| {
                sign(&proposer_sk, cb_crypto::keccak256(preimage))
                    .expect("sign")
                    .to_vec()
            };
            // Same timestamp as the instrumented loop stamps (`height`), so
            // the two headers — and therefore the head hashes that seed the
            // next block's intra-block ordering — are identical. With a
            // hash-seeded permutation, "the same transactions" is only half
            // of what makes two blocks execute identically.
            let res = build_and_apply(
                &db_b,
                &ExecCtx {
                    treasury_signer: Address::ZERO,
                    ..Default::default()
                },
                batch_b,
                h + 1,
                0,
                Vec::new(),
                signer,
            )
            .unwrap();

            assert_eq!(
                root_a, res.state_root,
                "instrumented loop and build_and_apply disagreed at block {h}"
            );
            align_heads(&db_a, &db_b);
        }
        assert_eq!(tally.rejected, 0);
        assert!(tally.taker_matched > 0);
    }

    #[test]
    fn percentiles_are_monotonic() {
        let mut s: Vec<u64> = (1..=1000).collect();
        let p = Percentiles::from_samples(&mut s);
        assert!(p.p50_us <= p.p99_us);
        assert!(p.p99_us <= p.p999_us);
        assert!(p.p999_us <= p.max_us);
        assert_eq!(p.count, 1000);
    }
}
