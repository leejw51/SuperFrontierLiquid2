//! Shared fixtures for the `cb-simgame` integration tests.
//!
//! Every test here drives the in-process world, so a test is a whole
//! simulated exchange: markets, a bot population, a scripted price path, and
//! the invariant checker running at every block boundary.
//!
//! ## Why some runs set `inject_premium: false`
//!
//! When QA wrote these fixtures the chain never derived a funding premium, so
//! `inject_premium: false` reproduced shipped behaviour — an inert funding
//! path — and `inject_premium: true` was the only way to exercise funding at
//! all. Both of the findings that made that necessary are fixed:
//!
//! * F9 — `apply_oracle_update` now reads the book midpoint, folds it into
//!   `MarkPrice::premium_ema_q9` and passes it to `compute_mark_price`, so the
//!   chain derives its own premium and funding flows without any injection.
//! * F2 — a new or grown position is stamped with the market's live cumulative
//!   index, so the flows cancel and funding is a pure transfer.
//!
//! `inject_premium` is therefore no longer "off = shipped behaviour". It is a
//! knob that adds the harness's own premium *on top of* the one the chain
//! derives, useful for driving funding harder than a calm book would. Runs
//! that assert neutrality now do so with it off, against the chain's own
//! premium, which is the thing that actually ships.

#![allow(dead_code)]

use cb_simgame::invariants::{CheckerConfig, Invariant, Violation};
use cb_simgame::report::RunReport;
use cb_simgame::scenario::ScenarioTimeline;
use cb_simgame::world::{BotCounts, World, WorldConfig};

/// A population big enough to keep both books two-sided and to put real
/// leveraged risk on the table, small enough to run a few thousand blocks in
/// a test.
pub fn population() -> BotCounts {
    BotCounts {
        market_makers: 3,
        momentum: 8,
        noise: 3,
        whales: 2,
        liquidators: 2,
        arbitrageurs: 2,
    }
}

/// A configuration with the given seed, timeline and block count.
pub fn config(seed: u64, timeline: &str, blocks: u64) -> WorldConfig {
    WorldConfig {
        seed,
        bots: population(),
        timeline: ScenarioTimeline::parse(timeline).expect("valid timeline"),
        blocks,
        funding_interval_blocks: 25,
        ..WorldConfig::default()
    }
}

/// Run to completion, returning the report. Panics on a fatal error, with the
/// report rendered so a failure in CI is readable without a rerun.
pub fn run(config: WorldConfig) -> RunReport {
    let mut world = World::new(config).expect("world builds");
    let report = world.run().expect("run completes");
    if let Some(fatal) = &report.fatal {
        panic!(
            "the run stopped on a fatal error: {fatal}\n{}",
            report.render()
        );
    }
    report
}

/// Assert that every invariant held, printing the full report if not.
pub fn assert_all_invariants_held(report: &RunReport) {
    if !report.violations.is_empty() {
        panic!(
            "{} invariant violation(s) in a run that must be clean:\n{}",
            report.violations.len(),
            report.render()
        );
    }
}

/// Assert that the only violations are the two the funding-index defect (F2)
/// produced, and that they landed on funding-tick boundaries.
///
/// Kept because it is the shape of assertion a *future* funding regression
/// wants, and because the narrowness is the point: it never said "ignore
/// invariants 1 and 7", it said "the only thing wrong is the defect already
/// reduced to a unit test". No test uses it now — F2 is fixed and funding nets
/// to zero, so `assert_all_invariants_held` is what the funding tests call.
#[allow(dead_code)]
pub fn assert_only_known_funding_defect(report: &RunReport, funding_interval: u64) {
    let unexpected: Vec<&Violation> = report
        .violations
        .iter()
        .filter(|v| {
            let is_funding_shaped = matches!(
                v.invariant,
                Invariant::Conservation | Invariant::FundingNeutrality
            );
            let on_boundary = v.height % funding_interval == 0;
            !(is_funding_shaped && on_boundary)
        })
        .collect();
    if !unexpected.is_empty() {
        panic!(
            "{} violation(s) that the known funding-index defect does not explain:\n{}\n{}",
            unexpected.len(),
            unexpected
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            report.render()
        );
    }
}

/// A checker configuration that stops the run at the first violation.
pub fn fail_fast() -> CheckerConfig {
    CheckerConfig {
        fail_fast: true,
        ..CheckerConfig::default()
    }
}
