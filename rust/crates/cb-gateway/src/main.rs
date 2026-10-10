//! The gateway binary: the one place that names a concrete backend.
//!
//! Everything above this file takes `Arc<dyn Trait>`. This is where SQLite,
//! the in-memory bus, the simulated feed and a TCP socket to the sequencer are
//! chosen, and it is the only place any of them may be named. If a handler
//! ever needs to know which it got, the abstraction was wrong.
//!
//! ## Startup does not require the sequencer
//!
//! The gateway binds its port and serves `/health` whether or not the node is
//! up. A gateway that refused to start without its dependency would be
//! impossible to diagnose from the outside: the operator would have a dead
//! port and no page to read. Instead, the RPC bridge reconnects in the
//! background, `/health` says which dependency is down, and every endpoint
//! that needs the chain answers `503 NODE_UNAVAILABLE` until it is back.
//!
//! ## Shutdown drains
//!
//! On a signal the gateway stops accepting connections, tells every WebSocket
//! client it is closing, lets in-flight requests finish, and only then stops
//! the RPC thread. Dropping a browser mid-order is avoidable, so it is
//! avoided.

#![forbid(unsafe_code)]

use cb_gateway::auth::AuthService;
use cb_gateway::candles::CandleAggregator;
use cb_gateway::config::{Config, StoreKind};
use cb_gateway::directory::Directory;
use cb_gateway::faucet::FaucetService;
use cb_gateway::http;
use cb_gateway::leaderboard::LeaderboardService;
use cb_gateway::marketdata::MarketDataCache;
use cb_gateway::markets::{MarketCatalog, SpotDefaults};
use cb_gateway::memory::{InMemoryDatabase, InMemoryKv};
use cb_gateway::node::bridge::{BridgeConfig, NodeBridge};
use cb_gateway::node::{NodeClient, TcpConnector};
use cb_gateway::ratelimit::RateLimiter;
use cb_gateway::signer::TxSubmitter;
use cb_gateway::sim::{NoSimulation, SimCommand, SimControl, SimMode};
use cb_gateway::sim_process::{ChainFacts, ProcessSimConfig, ProcessSimulation};
use cb_gateway::state::{AppState, Settings};

use cb_gateway::tracker::OrderTracker;
use cb_gateway::watcher;
use cb_gateway::ws::Hub;
use cb_ports::{AppDatabase, Clock, SystemClock};
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    init_tracing(&config.log_filter);

    // Fail on bad configuration before anything else: a gateway that starts
    // and then cannot sign is worse than one that refuses to start.
    let treasury_key = config.treasury_key()?;
    let faucet_config = config.faucet_config()?;
    if faucet_config.grants.is_empty() {
        anyhow::bail!("the faucet must grant something; see --faucet-grant");
    }
    let dev_login = config.dev_login_allowed()?;
    let initial_sim_mode = config.initial_sim_mode()?;
    if config.sim_bin.is_none() && initial_sim_mode != SimMode::Off {
        anyhow::bail!("--sim-mode {initial_sim_mode} needs --sim-bin to name the simulator binary");
    }
    let admin_policy = config.admin_policy()?;
    let trusted_proxies = config.trusted_proxy_set()?;
    if !trusted_proxies.is_empty() {
        tracing::info!(
            proxies = %config.trusted_proxies.join(", "),
            "x-forwarded-for will be believed from these peers and from nobody else"
        );
    }
    if admin_policy.uses_handles() {
        // Loud on purpose. This is the finding-1 configuration: the operator
        // role is being granted by a self-chosen string, so on a fresh store
        // whoever logs in first holding that handle is the operator.
        tracing::warn!(
            handles = %admin_policy.handles.join(", "),
            "INSECURE: the operator role is bound to a handle. Handles are first-come and \
             chosen by the user at first login, so on a fresh store any account that registers \
             one of these handles receives every /admin route: user suspension, market halts, \
             risk configuration and arbitrary faucet grants. This is acceptable only on a \
             loopback simulator. Use --admin-address 0x<login address> instead."
        );
    }
    if admin_policy.addresses.is_empty() && !admin_policy.uses_handles() {
        tracing::info!("no operator configured; every /admin route will answer 403");
    }

    // ---- the composition root: concrete backends, chosen once ----
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let db: Arc<dyn AppDatabase> = match config.store {
        StoreKind::Memory => InMemoryDatabase::shared(),
        StoreKind::Sqlite => Arc::new(cb_store_sqlite::SqliteDb::open(&config.sqlite_path).await?),
    };
    db.migrate().await?;
    // The account directory needs an ordered key-value store. `cb-store-sqlite`
    // implements `AppDatabase`, not `KvStore`, so this is in memory for now:
    // suspensions and session metadata do not survive a restart, and that is
    // recorded as a gap rather than hidden.
    let kv = Arc::new(InMemoryKv::new());
    let directory = Arc::new(Directory::new(kv));
    let bus = Arc::new(cb_bus::InMemoryBus::new());
    let feed = Arc::new(cb_feed::SimulatedFeed::new(config.feed_seed));

    let node = NodeBridge::spawn(
        Arc::new(TcpConnector::new(config.node_rpc)),
        BridgeConfig {
            start_height: config.start_height.max(1),
            ..Default::default()
        },
    );
    let node_client: Arc<dyn NodeClient> = node.clone();

    // Before anything derives a trading key: does this secret still derive the
    // accounts this database was built with? A rotated secret orphans every
    // account in total silence, which is the failure this refuses to start on.
    match cb_gateway::auth::rotation::check(&db, &clock, config.server_secret.as_bytes()).await? {
        cb_gateway::auth::rotation::SecretCheck::FirstStart { fingerprint } => {
            tracing::info!(
                fingerprint,
                "recorded the server secret's trading-key derivation for this database"
            );
        }
        cb_gateway::auth::rotation::SecretCheck::Unchanged => {}
        cb_gateway::auth::rotation::SecretCheck::Rotated {
            recorded,
            current,
            accounts,
        } => {
            if !config.allow_server_secret_rotation {
                anyhow::bail!(cb_gateway::auth::rotation::refusal(
                    &recorded, &current, accounts
                ));
            }
            tracing::warn!(
                recorded,
                current,
                accounts,
                "STARTING WITH A ROTATED SERVER SECRET: every existing account now derives a \
                 different on-chain address. Their balances remain at the old addresses and are \
                 reachable only by restoring the previous secret, and orders resting under the \
                 old addresses can no longer be cancelled"
            );
            cb_gateway::auth::rotation::record(&db, &clock, &current, "rotation was allowed")
                .await?;
        }
    }

    let auth = Arc::new(
        AuthService::new(
            Arc::clone(&db),
            Arc::clone(&directory),
            Arc::clone(&clock),
            chrono::Duration::hours(config.session_ttl_hours.max(1)),
            &config.server_secret,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .with_admin_addresses(admin_policy.addresses.clone())
        .with_admin_handles(admin_policy.handles.clone()),
    );
    let submitter = Arc::new(TxSubmitter::new(Arc::clone(&node_client)));
    let faucet = Arc::new(FaucetService::new(
        Arc::clone(&db),
        Arc::clone(&submitter),
        Arc::clone(&clock),
        faucet_config,
        treasury_key,
    ));
    let markets = Arc::new(MarketCatalog::new(
        Arc::clone(&node_client),
        Arc::clone(&db),
        Arc::clone(&clock),
        config.block_time_ms,
        SpotDefaults::default(),
    ));
    let tracker = OrderTracker::new();
    let candles = Arc::new(CandleAggregator::new(Arc::clone(&db), Arc::clone(&clock)));
    let leaderboard = Arc::new(LeaderboardService::new(
        Arc::clone(&db),
        Arc::clone(&node_client),
        Arc::clone(&markets),
        Arc::clone(&tracker),
        Arc::clone(&clock),
        Arc::clone(&auth),
    ));

    // The simulation switch. Its facts come from the gateway's own view of
    // the chain, read through a small adapter so the harness does not need
    // the whole `AppState` to exist first.
    let chain_log = Arc::new(http::chain::ChainLog::new());
    struct Facts {
        node: Arc<dyn NodeClient>,
        chain_log: Arc<http::chain::ChainLog>,
    }
    impl ChainFacts for Facts {
        fn chain_id(&self) -> Option<u32> {
            self.node.health().chain_id
        }
        fn transactions_applied(&self) -> u64 {
            self.chain_log.total_transactions()
        }
    }
    let sim: Arc<dyn SimControl> = match &config.sim_bin {
        Some(sim_bin) => Arc::new(ProcessSimulation::new(
            ProcessSimConfig {
                sim_bin: sim_bin.clone(),
                sequencer: config.node_rpc,
                log_dir: config.sim_log_dir.clone(),
                oracle_control: config.oracle_control.clone(),
                seed: config.sim_seed,
            },
            Arc::new(Facts {
                node: Arc::clone(&node_client),
                chain_log: Arc::clone(&chain_log),
            }),
        )),
        None => Arc::new(NoSimulation),
    };

    let state = Arc::new(AppState {
        node: Arc::clone(&node_client),
        db: Arc::clone(&db),
        bus,
        feed,
        clock: Arc::clone(&clock),
        auth,
        faucet,
        submitter,
        markets: Arc::clone(&markets),
        tracker: Arc::clone(&tracker),
        candles: Arc::clone(&candles),
        leaderboard: Arc::clone(&leaderboard),
        marketdata: Arc::new(MarketDataCache::new()),
        hub: Hub::new(config.ws_send_buffer),
        mirror: Arc::new(cb_gateway::watcher::AccountMirror::new()),
        transfer_memos: Default::default(),
        chain_log,
        sim: Arc::clone(&sim),
        limits: Arc::new(RateLimiter::new(
            Arc::clone(&clock),
            config.rate_limit_per_session,
            config.rate_limit_per_ip,
        )),
        settings: Settings {
            order_confirm: Duration::from_millis(config.order_confirm_ms),
            ws_idle: Duration::from_secs(config.ws_idle_seconds),
            block_time_ms: config.block_time_ms,
            node_rpc: config.node_rpc.to_string(),
            gateway_listen: config.listen.to_string(),
            oracle_control: config.oracle_control.clone(),
            trusted_proxies: Arc::new(trusted_proxies),
            ..Default::default()
        },
        started_at: clock.now(),
    });

    tracing::info!(
        listen = %config.listen,
        node = %config.node_rpc,
        store = ?config.store,
        dev_login,
        "cb-gateway starting"
    );

    // ---- background work ----
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut tasks = vec![tokio::spawn(watcher::run(
        Arc::clone(&state),
        shutdown_rx.clone(),
    ))];
    tasks.push(tokio::spawn(refresh_markets(
        Arc::clone(&state),
        Duration::from_secs(config.market_refresh_seconds.max(1)),
        shutdown_rx.clone(),
    )));
    tasks.push(tokio::spawn(recompute_leaderboard(
        Arc::clone(&state),
        Duration::from_secs(config.leaderboard_interval_seconds.max(1)),
        shutdown_rx.clone(),
    )));
    tasks.push(tokio::spawn(janitor(
        Arc::clone(&state),
        shutdown_rx.clone(),
    )));
    if initial_sim_mode != SimMode::Off {
        tasks.push(tokio::spawn(switch_on_simulation(
            Arc::clone(&state),
            initial_sim_mode,
            shutdown_rx.clone(),
        )));
    }

    // ---- serve ----
    let app = http::router_with(Arc::clone(&state), &config.cors_origins, dev_login);
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "listening");

    let serve_state = Arc::clone(&state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        wait_for_signal().await;
        tracing::info!("shutdown requested; draining websocket connections");
        // Tell every browser before the socket goes away, so the frontend can
        // show "reconnecting" rather than a silent stall.
        serve_state.hub.close_all();
    })
    .await?;

    // ---- stop, in order ----
    let _ = shutdown_tx.send(true);
    // The workers are children of this process and die with it, but a clean
    // stop reaps them now rather than leaving a race with the pid files.
    let _ = tokio::time::timeout(
        Duration::from_secs(6),
        sim.control(SimCommand {
            action: "stop".into(),
            mode: None,
            scenario: None,
            seed: None,
            bots: Default::default(),
        }),
    )
    .await;
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    node.shutdown();
    tracing::info!("cb-gateway stopped");
    Ok(())
}

/// Put the simulation switch in its configured start-up position.
///
/// The workers sign for a chain id the gateway learns from the sequencer, so
/// this waits for the link before switching. A stack that comes up in the
/// wrong order — gateway first, node later — still ends in the right state.
async fn switch_on_simulation(
    state: Arc<AppState>,
    mode: SimMode,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return },
            _ = ticker.tick() => {
                if tokio::time::Instant::now() > deadline {
                    tracing::warn!(target: "cb_gateway::sim", %mode, "the sequencer never came up; the simulation switch stays off");
                    return;
                }
                if state.node.health().chain_id.is_none() {
                    continue;
                }
                let command = SimCommand {
                    action: "mode".into(),
                    mode: Some(mode.as_str().into()),
                    scenario: None,
                    seed: None,
                    bots: Default::default(),
                };
                match state.sim.control(command).await {
                    Ok(status) => {
                        tracing::info!(target: "cb_gateway::sim", mode = %status.mode, running = status.running, "simulation switch set from configuration");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(target: "cb_gateway::sim", error = %e, "could not switch the simulation on; retrying");
                    }
                }
            }
        }
    }
}

/// Install the tracing subscriber.
fn init_tracing(filter: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .try_init();
}

/// Re-read the market universe periodically.
async fn refresh_markets(
    state: Arc<AppState>,
    period: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return },
            _ = ticker.tick() => {
                let head = state.head_height();
                if let Err(e) = state.markets.refresh(head).await {
                    tracing::debug!(error = %e, "market refresh failed; keeping the previous catalogue");
                }
            }
        }
    }
}

/// Recompute the leaderboard periodically.
async fn recompute_leaderboard(
    state: Arc<AppState>,
    period: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return },
            _ = ticker.tick() => {
                if let Err(e) = state.leaderboard.recompute(10_000).await {
                    tracing::debug!(error = %e, "leaderboard recompute failed");
                }
            }
        }
    }
}

/// Expire sessions, prune orders, candles and rate-limit windows.
///
/// Everything the gateway keeps in memory is bounded by something; this is
/// where the bounds are enforced, so a gateway that runs for a month does not
/// end the month holding a month of dead orders.
async fn janitor(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(300));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return },
            _ = ticker.tick() => {
                let now = state.clock.now();
                state.tracker.prune(now);
                state.submitter.prune(now);
                state.limits.prune();
                if let Err(e) = state.auth.purge_expired_sessions().await {
                    tracing::debug!(error = %e, "session purge failed");
                }
                // Keep a thousand buckets of each interval: a day and a half
                // of minutes, and nearly three years of days.
                if let Err(e) = state.candles.prune(1_000).await {
                    tracing::debug!(error = %e, "candle prune failed");
                }
            }
        }
    }
}

/// Wait for `SIGINT` or `SIGTERM`.
async fn wait_for_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {},
        _ = terminate => {},
    }
}
