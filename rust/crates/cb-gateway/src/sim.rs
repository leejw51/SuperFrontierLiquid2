//! The simulation-control seam.
//!
//! `docs/api-contract.md` §10 exposes `GET`/`POST /admin/sim`, and says
//! plainly that a production deployment does not mount them. The bot
//! population that would answer them lives in `cb-simgame`, which the gateway
//! does not and should not depend on: in that arrangement the gateway is the
//! thing under test, not a participant.
//!
//! So the endpoints are served through a trait. The default implementation
//! reports a stopped simulation and refuses to start one, which is the
//! truthful answer for a gateway running on its own. A deployment that also
//! runs the bot harness supplies a real one at the composition root, and the
//! handlers never learn which they got.

use crate::error::{ApiError, ApiResult, ErrorCode};
use async_trait::async_trait;
use std::collections::BTreeMap;

/// The scenarios the contract defines.
pub const SCENARIOS: [&str; 6] = ["calm", "trend_up", "trend_down", "crash", "squeeze", "chop"];

/// The three positions of the operator's simulation switch.
///
/// This is a simulator, so the operator may change the rules of the game
/// while it runs. The modes are deliberately few and named for what a
/// person sees rather than for a worker count:
///
/// | Mode | What trades |
/// |---|---|
/// | `off` | Nobody. A lone human finds an empty book. |
/// | `on`  | A default buyer and seller: one worker, a handful of traders, a trickle of orders so the books move. |
/// | `max` | The stress test: every funded trader, every core, as many signed orders a second as the node will take. |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimMode {
    /// No default buyer or seller.
    #[default]
    Off,
    /// A default buyer and seller.
    On,
    /// Maximum stress.
    Max,
}

impl SimMode {
    /// Every mode, in switch order.
    pub const ALL: [Self; 3] = [Self::Off, Self::On, Self::Max];

    /// The wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Max => "max",
        }
    }

    /// Parse a wire value.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// The load each mode puts on the chain.
    ///
    /// `on` is the trickle `make start` used to launch by hand; `max` is the
    /// `make start3` stress profile. The genesis funds sixty-four simulator
    /// traders, so no preset asks for more.
    pub const fn preset(self) -> Option<Preset> {
        match self {
            Self::Off => None,
            Self::On => Some(Preset {
                workers: 1,
                traders: 8,
                tx_per_sec: 20,
                orders_pct: 90,
            }),
            Self::Max => Some(Preset {
                workers: 6,
                traders: 64,
                tx_per_sec: 40_000,
                orders_pct: 90,
            }),
        }
    }
}

impl std::fmt::Display for SimMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many workers, traders and orders a mode runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// Simulator processes to run side by side.
    pub workers: u32,
    /// Funded trader accounts shared between them, in disjoint ranges.
    pub traders: u32,
    /// Combined target submission rate.
    pub tx_per_sec: u64,
    /// Share of submissions that are spot orders; the rest are transfers.
    pub orders_pct: u8,
}

impl Preset {
    /// Traders each worker takes. Even indices are makers, odd are takers, so
    /// a worker needs at least two.
    pub const fn traders_per_worker(&self) -> u32 {
        let per = self.traders / self.workers;
        if per < 2 {
            2
        } else {
            per
        }
    }

    /// The rate one worker aims for.
    pub const fn tps_per_worker(&self) -> u64 {
        let tps = self.tx_per_sec / self.workers as u64;
        if tps < 1 {
            1
        } else {
            tps
        }
    }
}

/// The bot kinds the contract names.
pub const BOT_KINDS: [&str; 6] = [
    "market_maker",
    "momentum",
    "noise",
    "whale",
    "liquidator",
    "arbitrageur",
];

/// What `GET /admin/sim` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimStatus {
    /// Whether the bot population is running.
    pub running: bool,
    /// Which position the operator's switch is in.
    pub mode: SimMode,
    /// The active scenario.
    pub scenario: String,
    /// The seed the run was started with, so it can be reproduced.
    pub seed: u64,
    /// How many of each kind of bot are trading.
    pub bots: BTreeMap<String, u32>,
    /// How long the run has been going.
    pub elapsed_seconds: u64,
    /// Orders the bots have submitted.
    pub orders_submitted: u64,
    /// Invariant violations observed. Anything but zero is a bug.
    pub invariant_violations: u64,
}

impl Default for SimStatus {
    fn default() -> Self {
        Self {
            running: false,
            mode: SimMode::Off,
            scenario: "calm".into(),
            seed: 0,
            bots: BTreeMap::new(),
            elapsed_seconds: 0,
            orders_submitted: 0,
            invariant_violations: 0,
        }
    }
}

/// What `POST /admin/sim` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimCommand {
    /// `start`, `stop`, `scenario` or `mode`.
    pub action: String,
    /// The mode to switch to. Required when the action is `mode`; optional
    /// on `start`, where it defaults to `on`.
    pub mode: Option<String>,
    /// The scenario to run.
    pub scenario: Option<String>,
    /// The seed to run with.
    pub seed: Option<u64>,
    /// How many of each kind of bot to run.
    pub bots: BTreeMap<String, u32>,
}

impl SimCommand {
    /// Validate the command against the contract's vocabulary.
    ///
    /// Done here rather than in the handler so every implementation gets the
    /// same check, and an implementation cannot quietly accept a scenario the
    /// contract does not define.
    pub fn validate(&self) -> ApiResult<()> {
        if !["start", "stop", "scenario", "mode"].contains(&self.action.as_str()) {
            return Err(ApiError::validation(
                "action must be `start`, `stop`, `scenario` or `mode`",
            ));
        }
        if let Some(mode) = &self.mode {
            if SimMode::parse(mode).is_none() {
                return Err(ApiError::validation(
                    "mode must be one of `off`, `on`, `max`",
                ));
            }
        }
        if self.action == "mode" && self.mode.is_none() {
            return Err(ApiError::validation(
                "`mode` is required when the action is `mode`",
            ));
        }
        if let Some(scenario) = &self.scenario {
            if !SCENARIOS.contains(&scenario.as_str()) {
                return Err(ApiError::validation(format!(
                    "scenario must be one of {}",
                    SCENARIOS.join(", ")
                )));
            }
        }
        if self.action == "scenario" && self.scenario.is_none() {
            return Err(ApiError::validation(
                "`scenario` is required when the action is `scenario`",
            ));
        }
        for kind in self.bots.keys() {
            if !BOT_KINDS.contains(&kind.as_str()) {
                return Err(ApiError::validation(format!(
                    "unknown bot kind `{kind}`; expected one of {}",
                    BOT_KINDS.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// Control over the bot-driven simulation.
#[async_trait]
pub trait SimControl: Send + Sync + 'static {
    /// The current state of the simulation.
    async fn status(&self) -> ApiResult<SimStatus>;

    /// The switch position alone, for callers on a public, hot path.
    ///
    /// `status` may ask the oracle which scenario it is in; `GET /chain/status`
    /// is fetched by every page load and must not wait on a second service.
    async fn mode(&self) -> SimMode {
        self.status().await.map(|s| s.mode).unwrap_or_default()
    }

    /// Start, stop or re-scenario the run.
    async fn control(&self, command: SimCommand) -> ApiResult<SimStatus>;
}

/// The implementation used when no bot harness is attached.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSimulation;

#[async_trait]
impl SimControl for NoSimulation {
    async fn status(&self) -> ApiResult<SimStatus> {
        Ok(SimStatus::default())
    }

    async fn control(&self, command: SimCommand) -> ApiResult<SimStatus> {
        // Validate first, so a caller with a typo hears about the typo rather
        // than about the missing harness.
        command.validate()?;
        if command.action == "stop" || command.mode.as_deref() == Some("off") {
            // Stopping something that is not running is not an error.
            return Ok(SimStatus::default());
        }
        Err(ApiError::new(
            ErrorCode::Forbidden,
            "no simulation harness is attached to this gateway",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(action: &str) -> SimCommand {
        SimCommand {
            action: action.into(),
            mode: None,
            scenario: None,
            seed: None,
            bots: BTreeMap::new(),
        }
    }

    #[test]
    fn the_mode_switch_has_three_positions_and_a_preset_for_each_live_one() {
        for mode in SimMode::ALL {
            assert_eq!(SimMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SimMode::parse("MAX"), Some(SimMode::Max));
        assert_eq!(SimMode::parse("turbo"), None);
        assert!(SimMode::Off.preset().is_none());
        let on = SimMode::On.preset().expect("on has a preset");
        let max = SimMode::Max.preset().expect("max has a preset");
        assert!(max.tx_per_sec > on.tx_per_sec * 100);
        assert!(
            max.traders <= 64,
            "the genesis funds sixty-four simulator traders"
        );
        assert_eq!(max.traders_per_worker() * max.workers, 60);
        assert_eq!(on.traders_per_worker(), 8);
    }

    #[test]
    fn a_mode_action_must_name_a_known_mode() {
        assert!(command("mode").validate().is_err());
        let mut bad = command("mode");
        bad.mode = Some("turbo".into());
        assert!(bad.validate().is_err());
        let mut good = command("mode");
        good.mode = Some("max".into());
        assert!(good.validate().is_ok());
    }

    #[tokio::test]
    async fn the_default_reports_a_stopped_simulation() {
        let status = NoSimulation.status().await.expect("status");
        assert!(!status.running);
        assert_eq!(status.invariant_violations, 0);
        assert!(status.bots.is_empty());
    }

    #[test]
    fn the_action_vocabulary_is_the_contracts() {
        assert!(command("start").validate().is_ok());
        assert!(command("stop").validate().is_ok());
        assert!(command("scenario").validate().is_err(), "needs a scenario");
        assert!(command("fly").validate().is_err());
    }

    #[test]
    fn a_scenario_action_must_name_a_scenario() {
        assert!(command("scenario").validate().is_err());
        let mut named = command("scenario");
        named.scenario = Some("crash".into());
        assert!(named.validate().is_ok());
    }

    #[test]
    fn an_unknown_scenario_or_bot_kind_is_refused() {
        let mut bad_scenario = command("start");
        bad_scenario.scenario = Some("moon".into());
        assert_eq!(
            bad_scenario.validate().expect_err("unknown").code,
            ErrorCode::ValidationFailed
        );

        let mut bad_bot = command("start");
        bad_bot.bots.insert("wizard".into(), 3);
        assert!(bad_bot.validate().is_err());

        let mut good = command("start");
        good.bots.insert("market_maker".into(), 8);
        good.bots.insert("noise".into(), 32);
        assert!(good.validate().is_ok());
    }

    #[tokio::test]
    async fn stopping_a_simulation_that_is_not_running_is_not_an_error() {
        assert!(NoSimulation.control(command("stop")).await.is_ok());
    }

    #[tokio::test]
    async fn starting_without_a_harness_says_so_rather_than_pretending() {
        let mut start = command("start");
        start.scenario = Some("crash".into());
        let err = NoSimulation.control(start).await.expect_err("no harness");
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn a_typo_is_reported_before_the_missing_harness() {
        let mut typo = command("start");
        typo.scenario = Some("crashh".into());
        assert_eq!(
            NoSimulation.control(typo).await.expect_err("typo").code,
            ErrorCode::ValidationFailed
        );
    }
}
