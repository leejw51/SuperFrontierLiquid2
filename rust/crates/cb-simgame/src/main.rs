//! `cb-simgame` — a bot-driven exchange that checks itself.
//!
//! Two subcommands: `run` drives the block executor in process (fast,
//! hermetic, deterministic — this is the mode that proves correctness), and
//! `live` drives a running node over Cap'n Proto RPC (this is the mode that
//! makes the frontend feel alive).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use cb_simgame::invariants::CheckerConfig;
use cb_simgame::net::{run as run_net, NetConfig};
use cb_simgame::scenario::ScenarioTimeline;
use cb_simgame::world::{BotCounts, World, WorldConfig};

#[derive(Parser)]
#[command(
    name = "cb-simgame",
    about = "Bot-driven exchange simulation with continuous invariant assertions"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in process against the block executor, checking every invariant at
    /// every block boundary.
    Run(RunArgs),
    /// Drive a running node over Cap'n Proto RPC.
    Live(LiveArgs),
}

#[derive(clap::Args, Clone)]
struct PopulationArgs {
    /// Market makers: the bots that give a lone human a counterparty.
    #[arg(long, default_value_t = 3)]
    market_makers: usize,
    /// Leveraged trend followers. Expected to get liquidated in a crash.
    #[arg(long, default_value_t = 6)]
    momentum: usize,
    /// Random traders, including deliberately invalid orders.
    #[arg(long, default_value_t = 4)]
    noise: usize,
    /// Occasional large market orders.
    #[arg(long, default_value_t = 2)]
    whales: usize,
    /// Keepers that trigger liquidations.
    #[arg(long, default_value_t = 2)]
    liquidators: usize,
    /// Bots that fade the book-versus-index premium.
    #[arg(long, default_value_t = 2)]
    arbitrageurs: usize,
}

impl From<&PopulationArgs> for BotCounts {
    fn from(a: &PopulationArgs) -> Self {
        BotCounts {
            market_makers: a.market_makers,
            momentum: a.momentum,
            noise: a.noise,
            whales: a.whales,
            liquidators: a.liquidators,
            arbitrageurs: a.arbitrageurs,
        }
    }
}

#[derive(clap::Args)]
struct RunArgs {
    /// Master seed. The same seed reproduces the run exactly.
    #[arg(long, default_value_t = 0xC0FFEE)]
    seed: u64,
    /// Scenario timeline, e.g. `calm:200,crash:80,calm`. The last segment
    /// runs to the end of the run.
    #[arg(long, default_value = "calm")]
    scenario: String,
    /// Blocks to produce.
    #[arg(long, default_value_t = 500)]
    blocks: u64,
    /// Funding cadence in blocks.
    #[arg(long, default_value_t = 25)]
    funding_interval: u32,
    /// Height at which the dated future expires. `0` lists no future.
    #[arg(long, default_value_t = 0)]
    future_expiry: u64,
    /// Stop at the first invariant violation.
    #[arg(long, default_value_t = false)]
    fail_fast: bool,
    /// Blocks an account may sit below maintenance margin before it must be
    /// in the liquidation queue.
    #[arg(long, default_value_t = 8)]
    liquidation_grace: u64,
    /// Reproduce shipped behaviour exactly by leaving the funding premium at
    /// zero. The chain never derives one (see `docs/qa-report.md`), so with
    /// this flag every funding tick moves nothing.
    #[arg(long, default_value_t = false)]
    no_premium: bool,
    /// List a spot market and let the bots trade it. Off by default: spot
    /// has two confirmed lock-accounting defects (see `docs/qa-report.md`),
    /// one of which panics the executor within a few hundred blocks and
    /// would end every run before the derivative invariants were exercised.
    #[arg(long, default_value_t = false)]
    spot: bool,
    /// Report violations but still exit zero. The default is to exit
    /// non-zero when any invariant was violated, which is what CI wants.
    #[arg(long, default_value_t = false)]
    lenient: bool,
    #[command(flatten)]
    population: PopulationArgs,
}

#[derive(clap::Args)]
struct LiveArgs {
    /// Node to connect to.
    #[arg(long, default_value = "127.0.0.1:7700")]
    sequencer: SocketAddr,
    /// Chain id for signing.
    #[arg(long, default_value_t = 1)]
    chain_id: u32,
    /// Master seed.
    #[arg(long, default_value_t = 0xC0FFEE)]
    seed: u64,
    /// Scenario timeline driving the oracle, when `--drive-oracle` is set.
    #[arg(long, default_value = "calm")]
    scenario: String,
    /// Stop after this many ticks. Omit to run until interrupted.
    #[arg(long)]
    ticks: Option<u64>,
    /// Milliseconds between ticks.
    #[arg(long, default_value_t = 250)]
    tick_ms: u64,
    /// Post oracle prices from the scenario engine. Only works if the seed
    /// below matches the deployment's genesis oracle authority.
    #[arg(long, default_value_t = false)]
    drive_oracle: bool,
    /// Seed phrase for the oracle key.
    #[arg(long, default_value = "cb-simgame-oracle")]
    oracle_seed: String,
    #[command(flatten)]
    population: PopulationArgs,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Run(args) => run_in_process(args),
        Cmd::Live(args) => run_live(args),
    }
}

fn run_in_process(args: RunArgs) -> ExitCode {
    let timeline = match ScenarioTimeline::parse(&args.scenario) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bad --scenario: {e}");
            return ExitCode::from(2);
        }
    };
    let config = WorldConfig {
        seed: args.seed,
        bots: (&args.population).into(),
        timeline,
        blocks: args.blocks,
        funding_interval_blocks: args.funding_interval.max(1),
        future_expiry_height: args.future_expiry,
        enable_spot: args.spot,
        inject_premium: !args.no_premium,
        checker: CheckerConfig {
            liquidation_grace_blocks: args.liquidation_grace,
            fail_fast: args.fail_fast,
            ..CheckerConfig::default()
        },
        ..WorldConfig::default()
    };
    let mut world = match World::new(config) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to build world: {e}");
            return ExitCode::FAILURE;
        }
    };
    let report = match world.run() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("run failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    print!("{}", report.render());
    if !args.lenient && !report.all_invariants_held() {
        eprintln!(
            "\n{} invariant violation(s); see the detail above",
            report.violations.len()
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_live(args: LiveArgs) -> ExitCode {
    let timeline = match ScenarioTimeline::parse(&args.scenario) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bad --scenario: {e}");
            return ExitCode::from(2);
        }
    };
    let cfg = NetConfig {
        sequencer: args.sequencer,
        chain_id: args.chain_id,
        seed: args.seed,
        timeline,
        bots: (&args.population).into(),
        ticks: args.ticks,
        tick_interval: Duration::from_millis(args.tick_ms),
        drive_oracle: args.drive_oracle,
        oracle_seed: args.oracle_seed,
        ..NetConfig::default()
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The Cap'n Proto client is `!Send`, so everything runs on a LocalSet.
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, run_net(cfg));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("live session failed: {e}");
            ExitCode::FAILURE
        }
    }
}
