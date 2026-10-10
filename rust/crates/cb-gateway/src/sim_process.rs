//! The simulation harness that answers `/admin/sim` when one is attached:
//! `cb-sim` workers as child processes, and the oracle's scenario switch.
//!
//! ## Why child processes
//!
//! The load simulator already exists, is measured, and is what `make start`
//! ran by hand before the operator could reach it from the admin page. It
//! signs real transactions and submits them over the same Cap'n Proto RPC a
//! wallet uses, so what the dashboard shows when the switch is on is the
//! whole path. Re-implementing that inside the gateway would have produced a
//! second, unmeasured simulator and made the gateway a participant in the
//! thing it fronts. Spawning the measured one keeps the gateway a control
//! plane.
//!
//! ## What is and is not reported
//!
//! `cb-sim` prints its counters to its log and nowhere else, so the status
//! here does not know how many orders the workers submitted. `orders_submitted`
//! is therefore the number of transactions the **chain applied** since the
//! run started, taken from the gateway's own block stream. That is the
//! honest neighbour of the figure the contract asks for, and it is labelled
//! as such in the contract. `bots` reports the maker and taker halves of the
//! trader population under the contract's `market_maker` and `noise` names,
//! because that is what the even and odd trader indices are.
//!
//! ## Only one population may exist
//!
//! Two simulators sharing a trader range share its nonces and strand each
//! other on `BadNonce`. This harness is therefore the only thing that should
//! launch `cb-sim` against a stack the gateway fronts; `make start` hands the
//! trickle to it rather than launching one itself.

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::sim::{Preset, SimCommand, SimControl, SimMode, SimStatus};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

/// Where the workers come from and what they talk to.
#[derive(Debug, Clone)]
pub struct ProcessSimConfig {
    /// The `cb-sim` binary.
    pub sim_bin: PathBuf,
    /// The sequencer the workers submit to.
    pub sequencer: SocketAddr,
    /// Directory the workers' logs are written to.
    pub log_dir: PathBuf,
    /// The oracle's scenario-control API, when the oracle runs one.
    pub oracle_control: Option<String>,
    /// The seed the first worker signs with; each further worker adds one.
    pub seed: u64,
}

/// Something that knows the chain id and how many transactions have landed.
///
/// A trait rather than `Arc<AppState>` so the harness is testable without a
/// gateway around it, and so `sim_process` does not reach into `http::chain`.
pub trait ChainFacts: Send + Sync + 'static {
    /// The chain id the workers must sign for, once the node has said.
    fn chain_id(&self) -> Option<u32>;
    /// Transactions the gateway has watched the chain apply so far.
    fn transactions_applied(&self) -> u64;
}

struct Run {
    mode: SimMode,
    preset: Preset,
    started_at: Instant,
    txs_at_start: u64,
    children: Vec<Child>,
}

struct Inner {
    run: Option<Run>,
    scenario: String,
    seed: u64,
    /// Why the last run ended, when it ended on its own.
    last_failure: Option<String>,
}

/// `cb-sim` workers under the operator's switch.
pub struct ProcessSimulation {
    config: ProcessSimConfig,
    facts: Arc<dyn ChainFacts>,
    inner: Mutex<Inner>,
    http: reqwest::Client,
}

impl std::fmt::Debug for ProcessSimulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessSimulation")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProcessSimulation {
    /// A harness that has not started anything yet.
    pub fn new(config: ProcessSimConfig, facts: Arc<dyn ChainFacts>) -> Self {
        let seed = config.seed;
        Self {
            config,
            facts,
            inner: Mutex::new(Inner {
                run: None,
                scenario: "calm".into(),
                seed,
                last_failure: None,
            }),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Switch to `mode`, stopping whatever runs now.
    ///
    /// Returns the resulting status. A switch to the mode already running is
    /// a no-op rather than a restart, so an operator pressing the button
    /// twice does not lose the population's warmed-up books.
    pub async fn set_mode(&self, mode: SimMode) -> ApiResult<SimStatus> {
        let already = {
            let inner = self.inner.lock();
            inner.run.as_ref().map(|run| run.mode)
        };
        if already == Some(mode) {
            return self.status().await;
        }
        let had_population = already.is_some();
        self.stop_children().await;
        if let Some(preset) = mode.preset() {
            if had_population {
                // The population that just died had transactions in flight:
                // up to `max_in_flight` per worker, queued in the node's
                // mempool under nonces the chain has not reached yet. A new
                // worker that reads its nonce from state and submits at once
                // collides with those and is refused. A moment for the
                // mempool to drain is what makes a switch a switch and not a
                // restart that fails.
                tokio::time::sleep(Duration::from_millis(1_500)).await;
            }
            self.spawn(mode, preset)?;
        }
        self.status().await
    }

    fn spawn(&self, mode: SimMode, preset: Preset) -> ApiResult<()> {
        let chain_id = self.facts.chain_id().ok_or_else(|| {
            ApiError::new(
                ErrorCode::NodeUnavailable,
                "the gateway has not learned the chain id from the sequencer yet; try again in a moment",
            )
        })?;
        if !self.config.sim_bin.is_file() {
            return Err(ApiError::new(
                ErrorCode::Internal,
                format!(
                    "the simulator binary is missing at {}; build it with `make -C rust build-sim`",
                    self.config.sim_bin.display()
                ),
            ));
        }
        std::fs::create_dir_all(&self.config.log_dir).map_err(|e| {
            ApiError::new(
                ErrorCode::Internal,
                format!("cannot create {}: {e}", self.config.log_dir.display()),
            )
        })?;
        let seed = self.inner.lock().seed;
        let per = preset.traders_per_worker();
        let tps = preset.tps_per_worker();
        let mut children = Vec::with_capacity(preset.workers as usize);
        for i in 0..preset.workers {
            let log_path = self.config.log_dir.join(format!("sim-gateway-{i}.log"));
            let log = std::fs::File::create(&log_path).map_err(|e| {
                ApiError::new(
                    ErrorCode::Internal,
                    format!("cannot open {}: {e}", log_path.display()),
                )
            })?;
            let stderr = log.try_clone().map_err(|e| {
                ApiError::new(ErrorCode::Internal, format!("cannot clone log handle: {e}"))
            })?;
            let child = Command::new(&self.config.sim_bin)
                .arg("run")
                .arg("--sequencer")
                .arg(self.config.sequencer.to_string())
                .arg("--chain-id")
                .arg(chain_id.to_string())
                .arg("--num-traders")
                .arg(per.to_string())
                .arg("--trader-offset")
                .arg((i * per).to_string())
                .arg("--orders-pct")
                .arg(preset.orders_pct.to_string())
                .arg("--tx-per-sec")
                .arg(tps.to_string())
                .arg("--rng-seed")
                .arg((seed + u64::from(i)).to_string())
                .stdin(std::process::Stdio::null())
                .stdout(log)
                .stderr(stderr)
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| {
                    ApiError::new(
                        ErrorCode::Internal,
                        format!("cannot start {}: {e}", self.config.sim_bin.display()),
                    )
                });
            match child {
                Ok(child) => children.push(child),
                Err(error) => {
                    // Leave nothing half-started: the children spawned so
                    // far die with the Vec (`kill_on_drop`).
                    drop(children);
                    return Err(error);
                }
            }
        }
        tracing::info!(
            target: "cb_gateway::sim",
            mode = %mode,
            workers = preset.workers,
            traders = per * preset.workers,
            tps = preset.tx_per_sec,
            "simulation switched on"
        );
        let mut inner = self.inner.lock();
        inner.last_failure = None;
        inner.run = Some(Run {
            mode,
            preset,
            started_at: Instant::now(),
            txs_at_start: self.facts.transactions_applied(),
            children,
        });
        Ok(())
    }

    async fn stop_children(&self) {
        let run = self.inner.lock().run.take();
        let Some(mut run) = run else { return };
        for child in &mut run.children {
            let _ = child.start_kill();
        }
        for mut child in run.children {
            // A worker that ignores SIGKILL does not exist; the timeout is
            // belt and braces so a stuck reap can never wedge the admin page.
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        }
        tracing::info!(target: "cb_gateway::sim", mode = %run.mode, "simulation switched off");
    }

    /// Reap workers that died on their own. A run with no live worker is over,
    /// and the status says so instead of reporting a population that is not
    /// there.
    fn reap(&self) {
        let mut inner = self.inner.lock();
        let Some(run) = inner.run.as_mut() else {
            return;
        };
        let mut exited = Vec::new();
        let mut alive = 0usize;
        for (i, child) in run.children.iter_mut().enumerate() {
            match child.try_wait() {
                Ok(Some(status)) => exited.push(format!("worker {i} exited with {status}")),
                Ok(None) => alive += 1,
                Err(e) => exited.push(format!("worker {i} could not be polled: {e}")),
            }
        }
        if !exited.is_empty() {
            let detail = exited.join("; ");
            tracing::warn!(target: "cb_gateway::sim", alive, %detail, "simulation worker died");
            if alive == 0 {
                inner.run = None;
                inner.last_failure = Some(format!(
                    "{detail}. The log in {} says why.",
                    self.config.log_dir.display()
                ));
            }
        }
    }

    async fn set_scenario(&self, scenario: &str) -> ApiResult<()> {
        if let Some(base) = &self.config.oracle_control {
            let url = format!("{}/scenario", base.trim_end_matches('/'));
            let response = self
                .http
                .post(&url)
                .json(&serde_json::json!({ "scenario": scenario }))
                .send()
                .await
                .map_err(|e| {
                    ApiError::new(
                        ErrorCode::NodeUnavailable,
                        format!("the oracle's scenario control at {url} did not answer: {e}"),
                    )
                })?;
            if !response.status().is_success() {
                return Err(ApiError::new(
                    ErrorCode::Internal,
                    format!(
                        "the oracle refused scenario `{scenario}`: HTTP {}",
                        response.status()
                    ),
                ));
            }
        }
        self.inner.lock().scenario = scenario.to_string();
        Ok(())
    }

    /// Ask the oracle which scenario it is actually in, when it has a control
    /// API. Falls back to what this process last set.
    async fn oracle_scenario(&self) -> Option<String> {
        let base = self.config.oracle_control.as_ref()?;
        let url = format!("{}/scenario", base.trim_end_matches('/'));
        let body: serde_json::Value = self.http.get(&url).send().await.ok()?.json().await.ok()?;
        let scenarios = body.get("scenarios")?.as_object()?;
        // The switch sets every symbol at once, so any one of them is the
        // answer; when they disagree, the most common wins.
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for value in scenarios.values() {
            if let Some(name) = value.as_str() {
                *counts.entry(name).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(name, _)| name.to_string())
    }
}

#[async_trait]
impl SimControl for ProcessSimulation {
    async fn mode(&self) -> SimMode {
        self.reap();
        self.inner
            .lock()
            .run
            .as_ref()
            .map_or(SimMode::Off, |run| run.mode)
    }

    async fn status(&self) -> ApiResult<SimStatus> {
        self.reap();
        let live_scenario = self.oracle_scenario().await;
        let inner = self.inner.lock();
        let scenario = live_scenario.unwrap_or_else(|| inner.scenario.clone());
        let mut status = SimStatus {
            running: false,
            mode: SimMode::Off,
            scenario,
            seed: inner.seed,
            bots: BTreeMap::new(),
            elapsed_seconds: 0,
            orders_submitted: 0,
            invariant_violations: 0,
        };
        if let Some(run) = &inner.run {
            let per = run.preset.traders_per_worker();
            let traders = per * run.preset.workers;
            status.running = true;
            status.mode = run.mode;
            status.elapsed_seconds = run.started_at.elapsed().as_secs();
            status.orders_submitted = self
                .facts
                .transactions_applied()
                .saturating_sub(run.txs_at_start);
            // Even trader indices rest orders, odd ones cross them.
            status
                .bots
                .insert("market_maker".into(), traders.div_ceil(2));
            status.bots.insert("noise".into(), traders / 2);
        }
        Ok(status)
    }

    async fn control(&self, command: SimCommand) -> ApiResult<SimStatus> {
        command.validate()?;
        if let Some(seed) = command.seed {
            self.inner.lock().seed = seed;
        }
        if let Some(scenario) = &command.scenario {
            self.set_scenario(scenario).await?;
        }
        match command.action.as_str() {
            "stop" => self.set_mode(SimMode::Off).await,
            "start" => {
                let mode = command
                    .mode
                    .as_deref()
                    .and_then(SimMode::parse)
                    .unwrap_or(SimMode::On);
                self.set_mode(mode).await
            }
            "mode" => {
                let mode = command
                    .mode
                    .as_deref()
                    .and_then(SimMode::parse)
                    .unwrap_or(SimMode::Off);
                self.set_mode(mode).await
            }
            // `scenario`: already applied above.
            _ => self.status().await,
        }
    }
}

impl ProcessSimulation {
    /// Why the last run ended by itself, if it did.
    pub fn last_failure(&self) -> Option<String> {
        self.inner.lock().last_failure.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Facts {
        txs: AtomicU64,
    }

    impl ChainFacts for Facts {
        fn chain_id(&self) -> Option<u32> {
            Some(1000)
        }
        fn transactions_applied(&self) -> u64 {
            self.txs.load(Ordering::SeqCst)
        }
    }

    /// A stand-in for `cb-sim` that records its arguments and sleeps, so the
    /// tests exercise the real spawn, kill and reap paths without a chain.
    fn fake_sim(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("fake-sim.sh");
        std::fs::write(
            &path,
            // Exits at once when a `die` marker sits beside it, so one test can
            // ask for a dying population without an environment variable
            // that would leak into the tests running alongside it.
            "#!/bin/sh\necho \"$@\"\nif [ -f \"$(dirname \"$0\")/die\" ]; then exit 3; fi\nsleep 60\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn harness(dir: &std::path::Path) -> ProcessSimulation {
        ProcessSimulation::new(
            ProcessSimConfig {
                sim_bin: fake_sim(dir),
                sequencer: "127.0.0.1:7700".parse().unwrap(),
                log_dir: dir.join("logs"),
                oracle_control: None,
                seed: 0xC0FFEE,
            },
            Arc::new(Facts {
                txs: AtomicU64::new(10),
            }),
        )
    }

    fn command(action: &str, mode: Option<&str>) -> SimCommand {
        SimCommand {
            action: action.into(),
            mode: mode.map(str::to_string),
            scenario: None,
            seed: None,
            bots: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn the_switch_starts_stops_and_reports_the_population() {
        let dir = tempfile::tempdir().unwrap();
        let sim = harness(dir.path());

        let off = sim.status().await.unwrap();
        assert!(!off.running);
        assert_eq!(off.mode, SimMode::Off);

        let on = sim.control(command("mode", Some("on"))).await.unwrap();
        assert!(on.running);
        assert_eq!(on.mode, SimMode::On);
        assert_eq!(on.bots.values().sum::<u32>(), 8);

        // The worker was given the preset, the chain id and a disjoint range.
        // The fake echoes its arguments as its first act; under a loaded test
        // run that can take a moment, so poll rather than sleep once.
        let log_path = dir.path().join("logs/sim-gateway-0.log");
        let mut log = String::new();
        for _ in 0..50 {
            log = std::fs::read_to_string(&log_path).unwrap_or_default();
            if log.contains("--tx-per-sec") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(log.contains("--chain-id 1000"), "{log}");
        assert!(log.contains("--num-traders 8"), "{log}");
        assert!(log.contains("--tx-per-sec 20"), "{log}");

        let max = sim.control(command("mode", Some("max"))).await.unwrap();
        assert_eq!(max.mode, SimMode::Max);
        assert_eq!(max.bots.values().sum::<u32>(), 60);
        assert_eq!(sim.inner.lock().run.as_ref().unwrap().children.len(), 6);

        let off = sim.control(command("stop", None)).await.unwrap();
        assert!(!off.running);
        assert!(sim.inner.lock().run.is_none());
    }

    #[tokio::test]
    async fn start_without_a_mode_means_on_and_repeating_it_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let sim = harness(dir.path());
        let first = sim.control(command("start", None)).await.unwrap();
        assert_eq!(first.mode, SimMode::On);
        let pid = sim.inner.lock().run.as_ref().unwrap().children[0].id();
        let again = sim.control(command("start", None)).await.unwrap();
        assert_eq!(again.mode, SimMode::On);
        assert_eq!(
            sim.inner.lock().run.as_ref().unwrap().children[0].id(),
            pid,
            "the same worker keeps running"
        );
        sim.control(command("stop", None)).await.unwrap();
    }

    #[tokio::test]
    async fn a_population_that_dies_is_reported_as_stopped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("die"), b"").unwrap();
        let sim = harness(dir.path());
        let started = sim.control(command("mode", Some("on"))).await.unwrap();
        assert!(
            started.running,
            "reported at spawn; the exit is not known yet"
        );
        // Polled rather than slept for a fixed 300 ms: the worker exits at
        // once, but under a loaded test run the exit can take longer than
        // that to be observed, and a timing assumption makes a real check
        // flaky. Five seconds is the ceiling, not the expectation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let later = loop {
            let status = sim.status().await.unwrap();
            if !status.running || tokio::time::Instant::now() >= deadline {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert!(!later.running, "a dead worker is not a running simulation");
        assert!(sim.last_failure().is_some());
    }

    #[tokio::test]
    async fn orders_submitted_counts_transactions_since_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let facts = Arc::new(Facts {
            txs: AtomicU64::new(10),
        });
        let sim = ProcessSimulation::new(
            ProcessSimConfig {
                sim_bin: fake_sim(dir.path()),
                sequencer: "127.0.0.1:7700".parse().unwrap(),
                log_dir: dir.path().join("logs"),
                oracle_control: None,
                seed: 1,
            },
            facts.clone(),
        );
        sim.control(command("mode", Some("on"))).await.unwrap();
        facts.txs.store(35, Ordering::SeqCst);
        assert_eq!(sim.status().await.unwrap().orders_submitted, 25);
        sim.control(command("stop", None)).await.unwrap();
    }
}
