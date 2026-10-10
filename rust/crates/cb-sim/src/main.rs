//! `cb-sim` — SuperFrontierLiquid stress-test simulator binary.

use cb_sim::{
    run, run_fee_stress, run_market_collapse, run_perp_shock, run_perps_load, FeeStressConfig,
    MarketCollapseConfig, PerpShockConfig, PerpsLoadConfig, RunConfig, TraderSet,
};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::task::LocalSet;

#[derive(Parser)]
#[command(name = "cb-sim", about = "SuperFrontierLiquid stress-test simulator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate synthetic traffic against a running sequencer: spot orders from
    /// buyers and sellers on BTC/USD and ETH/USD, transfers, or a mix.
    Run {
        #[arg(long, default_value = "127.0.0.1:7700")]
        sequencer: SocketAddr,
        #[arg(long, default_value_t = 1)]
        chain_id: u32,
        #[arg(long, default_value_t = 8)]
        num_traders: u32,
        /// First trader index. Parallel workers each take a disjoint range.
        #[arg(long, default_value_t = 0)]
        trader_offset: u32,
        /// Share of submissions that are spot orders (buyers and sellers,
        /// makers and takers), 0..=100. The rest are transfers.
        #[arg(long, default_value_t = 0)]
        orders_pct: u8,
        /// Ceiling on submits awaiting a reply; the loop waits when it is
        /// reached. Also caps how many of a trader's pipelined transactions
        /// one rejection can strand on `BadNonce`.
        #[arg(long, default_value_t = 1_500)]
        max_in_flight: usize,
        #[arg(long, default_value = "cb-sim-trader-")]
        seed_prefix: String,
        /// Target tx submission rate in txs/sec.
        #[arg(long, default_value_t = 50)]
        tx_per_sec: u64,
        /// Stop after submitting this many txs (omit to run forever).
        #[arg(long)]
        max_txs: Option<u64>,
        #[arg(long, default_value_t = 0xC0FFEE)]
        rng_seed: u64,
    },
    /// Print the deterministic trader addresses (useful for verifying genesis funding).
    Traders {
        #[arg(long, default_value_t = 8)]
        num_traders: u32,
        #[arg(long, default_value = "cb-sim-trader-")]
        seed_prefix: String,
    },
    /// In-process perps load simulator (M7). Runs against an in-memory
    /// `cb-block` executor — no RPC, no sequencer process. Reports metrics:
    /// fills/block, liquidations triggered, insurance trajectory,
    /// mark-vs-oracle drift.
    PerpsLoad {
        #[arg(long, default_value_t = 8)]
        num_traders: u32,
        #[arg(long, default_value_t = 1)]
        num_markets: u32,
        #[arg(long, default_value_t = 100)]
        blocks: u64,
        #[arg(long, default_value_t = 0xC0FFEEDEADBEEFu64)]
        rng_seed: u64,
        /// Funding cadence (in blocks). 5 keeps boundaries frequent.
        #[arg(long, default_value_t = 5)]
        funding_interval_blocks: u32,
    },
    /// M15: oracle-shock cascade. 100 traders, 2 perp markets, 500 blocks.
    /// At `--shock-block` the oracle drops `--shock-drop-bps` (default 30%)
    /// in one tick. Reports liquidation count, peak underwater notional,
    /// insurance drain depth, and time-to-clear.
    PerpShock {
        #[arg(long, default_value_t = 100)]
        num_traders: u32,
        #[arg(long, default_value_t = 2)]
        num_markets: u32,
        #[arg(long, default_value_t = 500)]
        blocks: u64,
        #[arg(long, default_value_t = 200)]
        shock_block: u64,
        #[arg(long, default_value_t = 3_000)]
        shock_drop_bps: u32,
        #[arg(long, default_value_t = 0x50CC_5EED_u64)]
        rng_seed: u64,
        #[arg(long, default_value_t = 50)]
        funding_interval_blocks: u32,
        /// Optional output dir to write a `perp-shock.json` metrics blob.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// M15: long-tail bear market. 60 traders, 1 market, 1050 blocks (50
    /// warmup + 10 epochs of 100). Each epoch starts with a -5% oracle step;
    /// keeper sweeps every block. Reports per-epoch liquidation rate +
    /// insurance trajectory.
    MarketCollapse {
        #[arg(long, default_value_t = 60)]
        num_traders: u32,
        #[arg(long, default_value_t = 50)]
        warmup_blocks: u64,
        #[arg(long, default_value_t = 100)]
        epoch_blocks: u64,
        #[arg(long, default_value_t = 10)]
        num_epochs: u32,
        #[arg(long, default_value_t = 500)]
        epoch_drop_bps: u32,
        #[arg(long, default_value_t = 0xC011A5EC0DE_u64)]
        rng_seed: u64,
        #[arg(long, default_value_t = 50)]
        funding_interval_blocks: u32,
        /// Optional output dir to write a `market-collapse.json` metrics blob.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// M15: fee revenue stress. 50 traders, 1 market, 1000 blocks, ~100
    /// orders/block. Periodic small oracle moves trigger sparse solvent
    /// liquidations, depositing fees into insurance. Asserts insurance is
    /// monotonically non-decreasing across all blocks.
    FeeStress {
        #[arg(long, default_value_t = 50)]
        num_traders: u32,
        #[arg(long, default_value_t = 1_000)]
        blocks: u64,
        #[arg(long, default_value_t = 50)]
        orders_per_block: u32,
        #[arg(long, default_value_t = 0xFEE5_5E55_u64)]
        rng_seed: u64,
        #[arg(long, default_value_t = 100)]
        funding_interval_blocks: u32,
        #[arg(long, default_value_t = 50)]
        oracle_move_interval: u64,
        #[arg(long, default_value_t = 75)]
        oracle_move_bps: u32,
        /// Optional output dir to write a `fee-stress.json` metrics blob.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = LocalSet::new();

    runtime.block_on(local.run_until(async move {
        match cli.cmd {
            Cmd::Run {
                sequencer,
                chain_id,
                num_traders,
                trader_offset,
                orders_pct,
                max_in_flight,
                seed_prefix,
                tx_per_sec,
                max_txs,
                rng_seed,
            } => {
                let cfg = RunConfig {
                    sequencer,
                    chain_id,
                    num_traders,
                    trader_offset,
                    seed_prefix,
                    tx_per_sec,
                    max_txs,
                    rng_seed,
                    orders_pct,
                    max_in_flight,
                };
                run(cfg).await?;
            }
            Cmd::Traders {
                num_traders,
                seed_prefix,
            } => {
                let set = TraderSet::derive(&seed_prefix, num_traders);
                for t in &set.traders {
                    println!("{:>3}  {}", t.index, t.addr);
                }
            }
            Cmd::PerpsLoad {
                num_traders,
                num_markets,
                blocks,
                rng_seed,
                funding_interval_blocks,
            } => {
                let cfg = PerpsLoadConfig {
                    num_traders: num_traders as usize,
                    num_markets: num_markets as usize,
                    blocks,
                    rng_seed,
                    starting_collateral: 1_000_000 * 1_000_000, // $1M each
                    insurance_seed: 100_000 * 1_000_000,
                    funding_interval_blocks,
                };
                let m = run_perps_load(&cfg);
                tracing::info!(
                    blocks = m.blocks,
                    submitted = m.txs_submitted,
                    accepted = m.txs_accepted,
                    rejected = m.txs_rejected,
                    liquidations_triggered = m.liquidations_triggered,
                    liquidations_succeeded = m.liquidations_succeeded,
                    insurance_start = m.insurance_start_micro_usd,
                    insurance_end = m.insurance_end_micro_usd,
                    insurance_min = m.insurance_min_micro_usd,
                    max_mark_drift_bps = m.max_mark_drift_bps,
                    final_state_root = ?m.final_state_root,
                    "perps_load done"
                );
                let avg_tx_per_block = if m.blocks > 0 {
                    m.txs_submitted as f64 / m.blocks as f64
                } else {
                    0.0
                };
                println!("blocks                 = {}", m.blocks);
                println!("submitted              = {}", m.txs_submitted);
                println!("accepted               = {}", m.txs_accepted);
                println!("rejected               = {}", m.txs_rejected);
                println!("avg_tx_per_block       = {:.2}", avg_tx_per_block);
                println!("liquidations_triggered = {}", m.liquidations_triggered);
                println!("liquidations_succeeded = {}", m.liquidations_succeeded);
                println!(
                    "insurance              = start {} → end {} (min {})",
                    m.insurance_start_micro_usd,
                    m.insurance_end_micro_usd,
                    m.insurance_min_micro_usd
                );
                println!("max_mark_drift_bps     = {}", m.max_mark_drift_bps);
                println!("final_state_root    = {:?}", m.final_state_root);
            }
            Cmd::PerpShock {
                num_traders,
                num_markets,
                blocks,
                shock_block,
                shock_drop_bps,
                rng_seed,
                funding_interval_blocks,
                out,
            } => {
                let cfg = PerpShockConfig {
                    num_traders: num_traders as usize,
                    num_markets: num_markets as usize,
                    blocks,
                    shock_block,
                    shock_drop_bps,
                    rng_seed,
                    starting_collateral: 10_000 * 1_000_000,
                    insurance_seed: 5_000_000 * 1_000_000,
                    funding_interval_blocks,
                };
                let m = run_perp_shock(&cfg);
                tracing::info!(
                    blocks = m.blocks,
                    shock_block = m.shock_block,
                    submitted = m.txs_submitted,
                    accepted = m.txs_accepted,
                    rejected = m.txs_rejected,
                    liquidations_triggered = m.liquidations_triggered,
                    liquidations_succeeded = m.liquidations_succeeded,
                    insurance_start = m.insurance_start_micro_usd,
                    insurance_end = m.insurance_end_micro_usd,
                    insurance_min = m.insurance_min_micro_usd,
                    insurance_drain = m.insurance_drain_micro_usd,
                    peak_underwater_notional = m.peak_underwater_notional_micro_usd,
                    time_to_clear_blocks = ?m.time_to_clear_blocks,
                    final_state_root = ?m.final_state_root,
                    "perp_shock done"
                );
                println!("blocks                  = {}", m.blocks);
                println!("shock_block             = {}", m.shock_block);
                println!("submitted               = {}", m.txs_submitted);
                println!("accepted                = {}", m.txs_accepted);
                println!("rejected                = {}", m.txs_rejected);
                println!("liquidations_triggered  = {}", m.liquidations_triggered);
                println!("liquidations_succeeded  = {}", m.liquidations_succeeded);
                println!(
                    "insurance               = start {} → end {} (min {})",
                    m.insurance_start_micro_usd,
                    m.insurance_end_micro_usd,
                    m.insurance_min_micro_usd
                );
                println!(
                    "insurance_drain         = {} micro_usd",
                    m.insurance_drain_micro_usd
                );
                println!(
                    "peak_underwater         = {} micro_usd",
                    m.peak_underwater_notional_micro_usd
                );
                println!("time_to_clear_blocks    = {:?}", m.time_to_clear_blocks);
                println!("final_state_root     = {:?}", m.final_state_root);
                if let Some(dir) = out {
                    write_metrics_json(&dir, "perp-shock.json", &perp_shock_json(&m))?;
                }
            }
            Cmd::MarketCollapse {
                num_traders,
                warmup_blocks,
                epoch_blocks,
                num_epochs,
                epoch_drop_bps,
                rng_seed,
                funding_interval_blocks,
                out,
            } => {
                let cfg = MarketCollapseConfig {
                    num_traders: num_traders as usize,
                    blocks: 0,
                    warmup_blocks,
                    epoch_blocks,
                    num_epochs,
                    epoch_drop_bps,
                    rng_seed,
                    starting_collateral: 5_000 * 1_000_000,
                    insurance_seed: 1_000_000 * 1_000_000,
                    funding_interval_blocks,
                };
                let m = run_market_collapse(&cfg);
                tracing::info!(
                    blocks = m.blocks,
                    submitted = m.txs_submitted,
                    accepted = m.txs_accepted,
                    rejected = m.txs_rejected,
                    liquidations_triggered = m.liquidations_triggered,
                    liquidations_succeeded = m.liquidations_succeeded,
                    insurance_start = m.insurance_start_micro_usd,
                    insurance_end = m.insurance_end_micro_usd,
                    insurance_min = m.insurance_min_micro_usd,
                    insurance_breached = m.insurance_breached,
                    epochs = m.epochs.len(),
                    final_state_root = ?m.final_state_root,
                    "market_collapse done"
                );
                println!("blocks                  = {}", m.blocks);
                println!("submitted               = {}", m.txs_submitted);
                println!("accepted                = {}", m.txs_accepted);
                println!("rejected                = {}", m.txs_rejected);
                println!("liquidations_triggered  = {}", m.liquidations_triggered);
                println!("liquidations_succeeded  = {}", m.liquidations_succeeded);
                println!(
                    "insurance               = start {} → end {} (min {})",
                    m.insurance_start_micro_usd,
                    m.insurance_end_micro_usd,
                    m.insurance_min_micro_usd
                );
                println!("insurance_breached      = {}", m.insurance_breached);
                println!("per-epoch:");
                for e in &m.epochs {
                    let rate = if e.triggered > 0 {
                        e.succeeded as f64 / e.triggered as f64
                    } else {
                        0.0
                    };
                    println!(
                        "  epoch {:>2} block_end {:>5} triggered {:>6} succeeded {:>6} rate {:.3} insurance {} px_q8 {}",
                        e.epoch,
                        e.block_at_end,
                        e.triggered,
                        e.succeeded,
                        rate,
                        e.insurance_after_micro_usd,
                        e.oracle_px_q8
                    );
                }
                println!("final_state_root     = {:?}", m.final_state_root);
                if let Some(dir) = out {
                    write_metrics_json(
                        &dir,
                        "market-collapse.json",
                        &market_collapse_json(&m),
                    )?;
                }
            }
            Cmd::FeeStress {
                num_traders,
                blocks,
                orders_per_block,
                rng_seed,
                funding_interval_blocks,
                oracle_move_interval,
                oracle_move_bps,
                out,
            } => {
                let cfg = FeeStressConfig {
                    num_traders: num_traders as usize,
                    blocks,
                    orders_per_block,
                    rng_seed,
                    starting_collateral: 100_000 * 1_000_000,
                    insurance_seed: 50_000 * 1_000_000,
                    funding_interval_blocks,
                    oracle_move_interval,
                    oracle_move_bps,
                };
                let m = run_fee_stress(&cfg);
                tracing::info!(
                    blocks = m.blocks,
                    submitted = m.txs_submitted,
                    accepted = m.txs_accepted,
                    rejected = m.txs_rejected,
                    liquidations_triggered = m.liquidations_triggered,
                    liquidations_succeeded = m.liquidations_succeeded,
                    insurance_start = m.insurance_start_micro_usd,
                    insurance_end = m.insurance_end_micro_usd,
                    insurance_growth = m.insurance_growth_micro_usd,
                    keeper_balance_end = m.keeper_balance_end_micro_usd,
                    final_state_root = ?m.final_state_root,
                    "fee_stress done"
                );
                println!("blocks                  = {}", m.blocks);
                println!("submitted               = {}", m.txs_submitted);
                println!("accepted                = {}", m.txs_accepted);
                println!("rejected                = {}", m.txs_rejected);
                println!("liquidations_triggered  = {}", m.liquidations_triggered);
                println!("liquidations_succeeded  = {}", m.liquidations_succeeded);
                println!(
                    "insurance               = start {} → end {} (Δ {})",
                    m.insurance_start_micro_usd,
                    m.insurance_end_micro_usd,
                    m.insurance_growth_micro_usd
                );
                println!(
                    "keeper_balance_end      = {} micro_usd",
                    m.keeper_balance_end_micro_usd
                );
                println!("final_state_root     = {:?}", m.final_state_root);
                if let Some(dir) = out {
                    write_metrics_json(&dir, "fee-stress.json", &fee_stress_json(&m))?;
                }
            }
        }
        anyhow::Ok(())
    }))
}

/// Tiny hand-rolled JSON writer — avoids dragging serde_json in as a new
/// runtime dep just for metrics dumps. Each scenario formats its own struct
/// into a flat object below.
fn write_metrics_json(dir: &std::path::Path, filename: &str, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, body)?;
    eprintln!("wrote metrics to {}", path.display());
    Ok(())
}

fn perp_shock_json(m: &cb_sim::PerpShockMetrics) -> String {
    format!(
        concat!(
            "{{\n",
            "  \"blocks\": {},\n",
            "  \"shock_block\": {},\n",
            "  \"txs_submitted\": {},\n",
            "  \"txs_accepted\": {},\n",
            "  \"txs_rejected\": {},\n",
            "  \"liquidations_triggered\": {},\n",
            "  \"liquidations_succeeded\": {},\n",
            "  \"insurance_start_micro_usd\": {},\n",
            "  \"insurance_end_micro_usd\": {},\n",
            "  \"insurance_min_micro_usd\": {},\n",
            "  \"insurance_drain_micro_usd\": {},\n",
            "  \"peak_underwater_notional_micro_usd\": {},\n",
            "  \"time_to_clear_blocks\": {},\n",
            "  \"final_state_root\": \"{:?}\"\n",
            "}}\n"
        ),
        m.blocks,
        m.shock_block,
        m.txs_submitted,
        m.txs_accepted,
        m.txs_rejected,
        m.liquidations_triggered,
        m.liquidations_succeeded,
        m.insurance_start_micro_usd,
        m.insurance_end_micro_usd,
        m.insurance_min_micro_usd,
        m.insurance_drain_micro_usd,
        m.peak_underwater_notional_micro_usd,
        match m.time_to_clear_blocks {
            Some(v) => v.to_string(),
            None => "null".to_string(),
        },
        m.final_state_root
    )
}

fn market_collapse_json(m: &cb_sim::MarketCollapseMetrics) -> String {
    let mut epochs = String::new();
    for (i, e) in m.epochs.iter().enumerate() {
        if i > 0 {
            epochs.push_str(",\n");
        }
        epochs.push_str(&format!(
            "    {{\"epoch\": {}, \"block_at_end\": {}, \"triggered\": {}, \"succeeded\": {}, \"insurance_after_micro_usd\": {}, \"oracle_px_q8\": {}}}",
            e.epoch,
            e.block_at_end,
            e.triggered,
            e.succeeded,
            e.insurance_after_micro_usd,
            e.oracle_px_q8
        ));
    }
    format!(
        concat!(
            "{{\n",
            "  \"blocks\": {},\n",
            "  \"txs_submitted\": {},\n",
            "  \"txs_accepted\": {},\n",
            "  \"txs_rejected\": {},\n",
            "  \"liquidations_triggered\": {},\n",
            "  \"liquidations_succeeded\": {},\n",
            "  \"insurance_start_micro_usd\": {},\n",
            "  \"insurance_end_micro_usd\": {},\n",
            "  \"insurance_min_micro_usd\": {},\n",
            "  \"insurance_breached\": {},\n",
            "  \"final_state_root\": \"{:?}\",\n",
            "  \"epochs\": [\n{}\n  ]\n",
            "}}\n"
        ),
        m.blocks,
        m.txs_submitted,
        m.txs_accepted,
        m.txs_rejected,
        m.liquidations_triggered,
        m.liquidations_succeeded,
        m.insurance_start_micro_usd,
        m.insurance_end_micro_usd,
        m.insurance_min_micro_usd,
        m.insurance_breached,
        m.final_state_root,
        epochs,
    )
}

fn fee_stress_json(m: &cb_sim::FeeStressMetrics) -> String {
    let mut snaps = String::new();
    for (i, s) in m.block_insurance_snapshots.iter().enumerate() {
        if i > 0 {
            snaps.push_str(", ");
        }
        snaps.push_str(&s.to_string());
    }
    format!(
        concat!(
            "{{\n",
            "  \"blocks\": {},\n",
            "  \"txs_submitted\": {},\n",
            "  \"txs_accepted\": {},\n",
            "  \"txs_rejected\": {},\n",
            "  \"liquidations_triggered\": {},\n",
            "  \"liquidations_succeeded\": {},\n",
            "  \"insurance_start_micro_usd\": {},\n",
            "  \"insurance_end_micro_usd\": {},\n",
            "  \"insurance_growth_micro_usd\": {},\n",
            "  \"keeper_balance_end_micro_usd\": {},\n",
            "  \"final_state_root\": \"{:?}\",\n",
            "  \"block_insurance_snapshots\": [{}]\n",
            "}}\n"
        ),
        m.blocks,
        m.txs_submitted,
        m.txs_accepted,
        m.txs_rejected,
        m.liquidations_triggered,
        m.liquidations_succeeded,
        m.insurance_start_micro_usd,
        m.insurance_end_micro_usd,
        m.insurance_growth_micro_usd,
        m.keeper_balance_end_micro_usd,
        m.final_state_root,
        snaps,
    )
}
