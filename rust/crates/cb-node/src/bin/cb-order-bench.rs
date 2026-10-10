//! `cb-order-bench` — the honest order-throughput benchmark.
//!
//! Places and matches real spot CLOB orders and reports, separately, how many
//! orders were submitted, how many landed in a block, and how many **matched**.
//! See `cb_node::order_bench` for the mix and what each path includes, and
//! `docs/performance.md` for recorded results.
//!
//! ```text
//! cb-order-bench in-process --blocks 200 --takers-per-block 1000
//! cb-order-bench rpc        --blocks 200 --takers-per-block 1000
//! cb-order-bench profile
//! cb-order-bench determinism
//! ```

use cb_node::order_bench::{
    measure_mempool, measure_recovery, run_deterministic, run_inprocess, run_rpc, OrderBenchConfig,
};
use cb_state::Database;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cb-order-bench",
    about = "Order-matching throughput benchmark for SuperFrontierLiquid"
)]
struct Cli {
    #[command(flatten)]
    knobs: Knobs,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Workload knobs shared by every subcommand.
#[derive(clap::Args, Clone)]
struct Knobs {
    /// Blocks in the measured window.
    #[arg(long, default_value_t = 200)]
    blocks: u64,
    /// Resting orders held on each side of the book.
    #[arg(long, default_value_t = 2_000)]
    book_depth: u32,
    /// Resting price levels on each side.
    #[arg(long, default_value_t = 20)]
    price_levels: u32,
    /// Taker (Ioc) orders aimed at each block.
    #[arg(long, default_value_t = 1_000)]
    takers_per_block: u32,
    /// Maker (Gtc, resting) orders aimed at each block.
    #[arg(long, default_value_t = 1_000)]
    makers_per_block: u32,
    /// Share of taker orders priced to cross, in basis points (10000 = all).
    #[arg(long, default_value_t = 10_000)]
    cross_rate_bps: u32,
    /// Distinct maker addresses.
    #[arg(long, default_value_t = 64)]
    num_makers: u32,
    /// Distinct taker addresses.
    #[arg(long, default_value_t = 64)]
    num_takers: u32,
    /// Producer / client threads.
    #[arg(long, default_value_t = 4)]
    producers: usize,
    /// Ceiling on transactions drained into one block.
    #[arg(long, default_value_t = 20_000)]
    max_block_txs: usize,
    /// Bounded producer channel capacity (in-process path only). Also bounds
    /// queueing delay: the default is roughly two blocks of pipeline depth.
    #[arg(long, default_value_t = 4_000)]
    channel_capacity: usize,
    /// Fill the channel to `--channel-capacity` signed orders before the
    /// measured window opens, so the window measures the engine rather than
    /// the signers. Latency figures from such a run are meaningless.
    #[arg(long, default_value_t = false)]
    prefill: bool,
    /// Target block interval in milliseconds.
    #[arg(long, default_value_t = 50)]
    block_interval_ms: u64,
    /// PRNG seed. Same seed, same order stream.
    #[arg(long, default_value_t = 0x0_ADDE_C0DE)]
    rng_seed: u64,
    /// Key-derivation prefix for the synthetic address set.
    #[arg(long, default_value = "cb-order-bench-")]
    seed_prefix: String,
    /// Turn off the per-phase time breakdown.
    #[arg(long, default_value_t = false)]
    no_phase_timing: bool,
    /// Cap submissions at this many orders per second across all producers.
    /// Omit to run open-loop, which measures the ceiling but makes the latency
    /// percentiles a measure of queue depth rather than service time.
    #[arg(long)]
    target_orders_per_sec: Option<u64>,
    /// Write each block before building the next, as `cb-node --sync-writes`
    /// does. Default is the node's default: a writer thread takes the write
    /// while the next block is built.
    #[arg(long, default_value_t = false)]
    sync_writes: bool,
}

impl Knobs {
    fn into_config(self) -> OrderBenchConfig {
        OrderBenchConfig {
            blocks: self.blocks,
            book_depth: self.book_depth,
            price_levels: self.price_levels,
            takers_per_block: self.takers_per_block,
            makers_per_block: self.makers_per_block,
            cross_rate_bps: self.cross_rate_bps,
            num_makers: self.num_makers,
            num_takers: self.num_takers,
            producers: self.producers,
            max_block_txs: self.max_block_txs,
            channel_capacity: self.channel_capacity,
            prefill: self.prefill,
            block_interval_ms: self.block_interval_ms,
            rng_seed: self.rng_seed,
            seed_prefix: self.seed_prefix,
            phase_timing: !self.no_phase_timing,
            target_orders_per_sec: self.target_orders_per_sec,
            sync_writes: self.sync_writes,
            ..Default::default()
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// In-process path: matcher, executor, trie commit. No signature
    /// recovery, no mempool, no network.
    InProcess,
    /// RPC path: Cap'n Proto over TCP, ECDSA recovery in the server, mempool
    /// round-trip, the real sequencer loop.
    Rpc {
        /// TCP port the in-process sequencer listens on.
        #[arg(long, default_value_t = 7_811)]
        port: u16,
    },
    /// Micro-measurements that attribute the RPC path's overhead: ECDSA
    /// recovery (serial vs. rayon) and the mempool round-trip.
    Profile {
        /// Signatures / mempool records per measurement.
        #[arg(long, default_value_t = 50_000)]
        samples: usize,
    },
    /// Replay the same workload twice through `build_and_apply` and assert the
    /// state roots match. Correctness harness, reports no rate.
    Determinism,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();
    let cfg = cli.knobs.into_config();

    match cli.cmd {
        Cmd::InProcess => {
            let dir = tempfile::tempdir()?;
            let db = Database::open(dir.path())?;
            let report = run_inprocess(&db, &cfg)?;
            print!("{}", report.render(&cfg));
            fail_on_rejects(report.orders_rejected)?;
        }
        Cmd::Rpc { port } => {
            let report = run_rpc(&cfg, port)?;
            print!("{}", report.render(&cfg));
            fail_on_rejects(report.orders_rejected)?;
        }
        Cmd::Profile { samples } => {
            let (serial, parallel) = measure_recovery(&cfg, samples)?;
            let (put, drain) = measure_mempool(&cfg, samples)?;
            println!("samples                   = {samples}");
            println!("ecdsa_recover_serial/sec  = {serial:.0}");
            println!("ecdsa_recover_rayon/sec   = {parallel:.0}");
            println!(
                "ecdsa_recover_speedup     = {:.2}x",
                if serial > 0.0 { parallel / serial } else { 0.0 }
            );
            println!("mempool_put/sec           = {put:.0}  (one RocksDB put per tx)");
            println!("mempool_drain+decode/sec  = {drain:.0}  (hash-ordered iterate + parallel bincode decode)");
        }
        Cmd::Determinism => {
            let d1 = tempfile::tempdir()?;
            let a = run_deterministic(&Database::open(d1.path())?, &cfg)?;
            let d2 = tempfile::tempdir()?;
            let b = run_deterministic(&Database::open(d2.path())?, &cfg)?;
            println!("run_a_state_root = {a:?}");
            println!("run_b_state_root = {b:?}");
            anyhow::ensure!(
                a == b,
                "state roots diverged — the workload is not deterministic"
            );
            println!("deterministic    = yes");
        }
    }
    Ok(())
}

/// A benchmark whose transactions get rejected is measuring work that never
/// landed. Fail loudly rather than print a flattering rate.
fn fail_on_rejects(rejected: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        rejected == 0,
        "{rejected} orders were rejected by the executor; the reported rates are not trustworthy"
    );
    Ok(())
}
