//! `cb-oracle` — publish oracle prices for every derivative market on a node.
//!
//! # Why the runtime is built by hand
//!
//! `#[tokio::main]` gives a multi-threaded runtime, and the Cap'n Proto client
//! futures are `!Send`: they cannot be moved between worker threads, so they
//! cannot be spawned on one. `cb-client` works around this with one
//! single-thread runtime per call and `cb-gateway` with a dedicated RPC thread
//! behind a channel, because both have `Send` work of their own to keep
//! running. This binary has none: publishing prices is the entire job. So it
//! takes the simplest correct shape — one current-thread runtime, one
//! `LocalSet` wrapped around the whole program — and everything inside is free
//! to be `!Send`, including the scenario-control server.
//!
//! ```text
//!   current-thread runtime
//!   └── LocalSet
//!       ├── control API           (optional, --control-listen)
//!       └── supervisor
//!           └── session ──── Cap'n Proto RPC (!Send) ──▶ node
//! ```

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use cb_feed::SimulatedFeed;
use cb_oracle::config::{Cli, OracleConfig};
use cb_oracle::control::{router, ControlState};
use cb_oracle::feed::FeedDriver;
use cb_oracle::node::CapnpConnector;
use cb_oracle::publisher::Publisher;
use cb_oracle::supervisor::{Backoff, RunLimit, Stats, Supervisor};
use cb_ports::clock::SystemClock;
use clap::Parser;
use std::rc::Rc;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let cfg = OracleConfig::from_cli(Cli::parse())?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // One feed step per publish, so `cb-feed`'s per-step volatility stays
    // calibrated against the period this process actually runs at. See
    // `cb_oracle::config` for the full argument.
    let feed = Arc::new(SimulatedFeed::with_config(
        cfg.seed,
        cb_feed::UNIVERSE,
        cfg.feed_tick(),
        Arc::new(SystemClock),
    ));
    feed.set_scenario_all(cfg.scenario);

    info!(
        rpc = %cfg.rpc,
        authority = %format!("0x{}", hex::encode(cfg.authority.0)),
        seed = cfg.seed,
        scenario = cfg.scenario.as_str(),
        period_ms = cfg.period.as_millis() as u64,
        max_price_age_ms = cfg.max_price_age.as_millis() as u64,
        "starting the oracle"
    );

    let stats = Arc::new(Stats::default());
    let publisher = Publisher::new(
        feed.clone(),
        Arc::new(SystemClock),
        cfg.key,
        cfg.max_price_age,
    );
    let driver: Arc<dyn FeedDriver> = feed;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build the oracle runtime")?;
    let local = tokio::task::LocalSet::new();

    rt.block_on(local.run_until(async move {
        if let Some(addr) = cfg.control_listen {
            let app = router(ControlState {
                driver: Some(driver.clone()),
                stats: stats.clone(),
            });
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind the scenario-control API to {addr}"))?;
            info!(%addr, "scenario control: GET /health, GET|POST /scenario");
            tokio::task::spawn_local(async move {
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!(error = %e, "the scenario-control API stopped");
                }
            });
        }

        let mut supervisor = Supervisor {
            connector: Rc::new(CapnpConnector::new(cfg.rpc, cfg.rpc_timeout)),
            publisher,
            driver: Some(driver),
            period: cfg.period,
            chain_id: cfg.chain_id,
            backoff: Backoff::default(),
            stats,
        };
        supervisor
            .run(RunLimit {
                max_ticks: cfg.max_ticks,
            })
            .await
    }))
}
