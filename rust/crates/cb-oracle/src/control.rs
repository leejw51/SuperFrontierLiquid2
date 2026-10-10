//! An HTTP surface for changing market conditions while the oracle runs.
//!
//! Restarting the oracle to switch scenario would restart the price paths with
//! it — the simulator's whole point is that a seed replays a market, and a
//! restart mid-experiment throws that away. So the scenario is changeable in
//! place: `POST /scenario {"scenario":"crash"}` and the next tick is priced
//! from a crashing market, on the same paths, with the same seed.
//!
//! It is off unless `--control-listen` is given, and it is a plain unauthenticated
//! HTTP endpoint. That is acceptable for a loopback simulator control plane and
//! nowhere else, which is why it has to be asked for.
//!
//! The endpoints are deliberately few:
//!
//! | Method | Path | Purpose |
//! |--------|------|---------|
//! | `GET`  | `/health`   | liveness, counters, and whether the node link is up |
//! | `GET`  | `/scenario` | the scenario each symbol is in |
//! | `POST` | `/scenario` | set one symbol's scenario, or all of them |

use crate::feed::FeedDriver;
use crate::supervisor::Stats;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cb_feed::Scenario;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tracing::info;

/// What the control API can see and touch.
#[derive(Clone)]
pub struct ControlState {
    /// The feed to retune, absent when the oracle is reading a real source.
    pub driver: Option<Arc<dyn FeedDriver>>,
    /// Counters from the run loop.
    pub stats: Arc<Stats>,
}

/// A scenario change.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioRequest {
    /// One of `calm`, `trend_up`, `trend_down`, `crash`, `squeeze`, `chop`.
    pub scenario: String,
    /// Restrict the change to one feed symbol, e.g. `BTC-USD`. Omitted means
    /// every symbol.
    #[serde(default)]
    pub symbol: Option<String>,
}

/// The scenario each symbol is in.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    /// Symbol to scenario name.
    pub scenarios: BTreeMap<String, String>,
}

/// Liveness and counters.
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    /// Whether a node session is live right now.
    pub connected: bool,
    /// Publish ticks attempted.
    pub ticks: u64,
    /// Oracle updates the node accepted.
    pub published: u64,
    /// Market-ticks deliberately not published.
    pub skipped: u64,
    /// Submissions the node refused.
    pub rejected: u64,
    /// Node sessions established; more than one means it reconnected.
    pub sessions: u64,
    /// The `submitted_height` most recently published.
    pub last_height: u64,
}

/// The control API's routes.
pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/scenario", get(scenario_report).post(set_scenario))
        .with_state(state)
}

async fn health(State(state): State<ControlState>) -> Json<Health> {
    let s = &state.stats;
    Json(Health {
        connected: s.connected(),
        ticks: s.ticks(),
        published: s.published(),
        skipped: s.skipped(),
        rejected: s.rejected(),
        sessions: s.sessions(),
        last_height: s.last_height(),
    })
}

async fn scenario_report(
    State(state): State<ControlState>,
) -> Result<Json<ScenarioReport>, (StatusCode, String)> {
    let driver = state.driver.as_ref().ok_or_else(no_driver)?;
    let scenarios = driver
        .symbols()
        .into_iter()
        .filter_map(|s| driver.scenario(&s).map(|sc| (s, sc.as_str().to_string())))
        .collect();
    Ok(Json(ScenarioReport { scenarios }))
}

async fn set_scenario(
    State(state): State<ControlState>,
    Json(req): Json<ScenarioRequest>,
) -> Result<Json<ScenarioReport>, (StatusCode, String)> {
    let driver = state.driver.as_ref().ok_or_else(no_driver)?;
    let scenario = Scenario::parse(&req.scenario).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "unknown scenario `{}` (expected one of: {})",
                req.scenario,
                Scenario::ALL
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;

    match &req.symbol {
        Some(symbol) => {
            if !driver.set_scenario(symbol, scenario) {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("the feed does not carry `{symbol}`"),
                ));
            }
            info!(symbol, scenario = scenario.as_str(), "scenario changed");
        }
        None => {
            driver.set_scenario_all(scenario);
            info!(
                scenario = scenario.as_str(),
                "scenario changed for every symbol"
            );
        }
    }

    scenario_report(State(state)).await
}

fn no_driver() -> (StatusCode, String) {
    (
        StatusCode::CONFLICT,
        "this oracle is reading a market data source it does not drive, so there \
         is no scenario to set"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_feed::SimulatedFeed;

    fn state() -> (ControlState, Arc<SimulatedFeed>) {
        let feed = Arc::new(SimulatedFeed::new(1));
        (
            ControlState {
                driver: Some(feed.clone()),
                stats: Arc::new(Stats::default()),
            },
            feed,
        )
    }

    #[tokio::test]
    async fn setting_every_symbol_is_reported_back() {
        let (state, feed) = state();
        let report = set_scenario(
            State(state),
            Json(ScenarioRequest {
                scenario: "crash".into(),
                symbol: None,
            }),
        )
        .await
        .expect("accepted");
        assert_eq!(report.scenarios["BTC-USD"], "crash");
        assert_eq!(report.scenarios["AAPL-USD"], "crash");
        assert_eq!(feed.scenario("BTC-USD").expect("carried"), Scenario::Crash);
    }

    #[tokio::test]
    async fn setting_one_symbol_leaves_the_others_alone() {
        let (state, feed) = state();
        let report = set_scenario(
            State(state),
            Json(ScenarioRequest {
                scenario: "squeeze".into(),
                symbol: Some("ETH-USD".into()),
            }),
        )
        .await
        .expect("accepted");
        assert_eq!(report.scenarios["ETH-USD"], "squeeze");
        assert_eq!(report.scenarios["BTC-USD"], "calm");
        assert_eq!(
            feed.scenario("ETH-USD").expect("carried"),
            Scenario::Squeeze
        );
        assert_eq!(feed.scenario("BTC-USD").expect("carried"), Scenario::Calm);
    }

    #[tokio::test]
    async fn an_unknown_scenario_or_symbol_is_a_client_error() {
        let (state, _) = state();
        let err = set_scenario(
            State(state.clone()),
            Json(ScenarioRequest {
                scenario: "moon".into(),
                symbol: None,
            }),
        )
        .await
        .expect_err("rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = set_scenario(
            State(state),
            Json(ScenarioRequest {
                scenario: "calm".into(),
                symbol: Some("DOGE-USD".into()),
            }),
        )
        .await
        .expect_err("rejected");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn without_a_driver_the_api_says_so_instead_of_pretending() {
        let state = ControlState {
            driver: None,
            stats: Arc::new(Stats::default()),
        };
        let err = scenario_report(State(state)).await.expect_err("no driver");
        assert_eq!(err.0, StatusCode::CONFLICT);
    }
}
