//! The scenario engine: what the outside world's price does, and when.
//!
//! The price path is the simulation's only exogenous input, so it is also the
//! only place adversarial conditions can be *scripted* rather than hoped for.
//! Each [`Scenario`] is a drift-plus-noise regime; a [`ScenarioTimeline`]
//! schedules them so a run can be calm, then crash, then recover — which is
//! the sequence that actually surfaces conservation bugs, because a crash
//! creates bad debt and a recovery creates realised profit on the other side
//! of it.
//!
//! Everything here is a pure function of `(seed, height)`. The generator is
//! re-seeded per block from the run seed rather than carried as mutable
//! state, so a price path can be recomputed for any height without replaying
//! the run — and two runs with the same seed cannot diverge through it.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::fmt;
use std::str::FromStr;

use crate::universe::Underlying;

/// A market regime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Flat drift, small noise. The control case.
    Calm,
    /// Steady upward drift.
    TrendUp,
    /// Steady downward drift.
    TrendDown,
    /// Violent downward gaps. Deliberately harsh enough to cascade
    /// liquidations — see [`Scenario::drift_bps`] for why the numbers are
    /// what they are.
    Crash,
    /// Violent upward gaps: the mirror of a crash, which liquidates shorts.
    Squeeze,
    /// No drift, large two-sided noise.
    Chop,
}

impl Scenario {
    /// Per-block drift in basis points of price.
    ///
    /// The crash and squeeze numbers are set so that the regime moves price
    /// by roughly 1% per block. Maintenance margin on the default tier table
    /// is 1% of notional, so a trader at even 5x leverage is underwater
    /// within a handful of blocks and a trader at 20x is gone almost at once.
    /// A gentler crash produces a pretty chart and zero liquidations, which
    /// tests nothing.
    pub fn drift_bps(self) -> i64 {
        match self {
            Scenario::Calm => 0,
            Scenario::TrendUp => 12,
            Scenario::TrendDown => -12,
            Scenario::Crash => -110,
            Scenario::Squeeze => 110,
            Scenario::Chop => 0,
        }
    }

    /// A one-off gap applied on the first block of this regime, in basis
    /// points.
    ///
    /// A crash that only grinds downward lets a trend follower flip sides
    /// and profit, so nobody ends up on the wrong side and nothing is ever
    /// liquidated. Real crashes gap: the price is somewhere else before
    /// anyone can react, and the leveraged longs built during the calm are
    /// already underwater. That gap is what makes the liquidation cascade
    /// happen, so it is part of the scenario rather than an accident of the
    /// noise draw.
    pub fn entry_gap_bps(self) -> i64 {
        match self {
            // A quarter of the price, at once. Smaller gaps liquidate the
            // leveraged cohort one account at a time as the price grinds
            // down, which exercises the liquidation path serially but never
            // puts several accounts under water simultaneously — and a thin
            // book with several forced sellers in it is exactly where the
            // loss-absorption arithmetic is hardest.
            Scenario::Crash => -2_500,
            Scenario::Squeeze => 2_500,
            _ => 0,
        }
    }

    /// Half-width of the per-block uniform noise, in basis points.
    pub fn noise_bps(self) -> i64 {
        match self {
            Scenario::Calm => 6,
            Scenario::TrendUp | Scenario::TrendDown => 10,
            Scenario::Crash | Scenario::Squeeze => 45,
            Scenario::Chop => 60,
        }
    }

    /// Lower-case name, as accepted on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Calm => "calm",
            Scenario::TrendUp => "trend_up",
            Scenario::TrendDown => "trend_down",
            Scenario::Crash => "crash",
            Scenario::Squeeze => "squeeze",
            Scenario::Chop => "chop",
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Scenario {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "calm" => Ok(Scenario::Calm),
            "trend_up" | "trend-up" | "up" => Ok(Scenario::TrendUp),
            "trend_down" | "trend-down" | "down" => Ok(Scenario::TrendDown),
            "crash" => Ok(Scenario::Crash),
            "squeeze" => Ok(Scenario::Squeeze),
            "chop" => Ok(Scenario::Chop),
            other => Err(format!(
                "unknown scenario {other:?}: expected one of \
                 calm, trend_up, trend_down, crash, squeeze, chop"
            )),
        }
    }
}

/// One leg of a timeline: a regime held for a number of blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScenarioSegment {
    /// Which regime.
    pub scenario: Scenario,
    /// How many blocks it lasts.
    pub blocks: u64,
}

/// An ordered schedule of regimes. The last segment extends forever, so a
/// timeline can never run out underneath a longer run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScenarioTimeline {
    segments: Vec<ScenarioSegment>,
}

impl ScenarioTimeline {
    /// Build a timeline from segments. An empty list becomes a calm run.
    pub fn new(segments: Vec<ScenarioSegment>) -> Self {
        if segments.is_empty() {
            return Self::single(Scenario::Calm);
        }
        Self { segments }
    }

    /// A timeline that is one regime from start to finish.
    pub fn single(scenario: Scenario) -> Self {
        Self {
            segments: vec![ScenarioSegment {
                scenario,
                blocks: u64::MAX,
            }],
        }
    }

    /// The segments, in order.
    pub fn segments(&self) -> &[ScenarioSegment] {
        &self.segments
    }

    /// Which regime is in force at `block_offset` blocks into the run.
    pub fn at(&self, block_offset: u64) -> Scenario {
        let mut cursor = 0u64;
        for seg in &self.segments {
            let end = cursor.saturating_add(seg.blocks);
            if block_offset < end {
                return seg.scenario;
            }
            cursor = end;
        }
        // Unreachable while the last segment is unbounded; fall back rather
        // than panic if a caller hand-built a finite timeline.
        self.segments
            .last()
            .map(|s| s.scenario)
            .unwrap_or(Scenario::Calm)
    }

    /// Parse `calm:200,crash:80,calm:200`. A bare name means "for the rest of
    /// the run".
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        for (i, raw) in spec.split(',').filter(|s| !s.trim().is_empty()).enumerate() {
            let mut parts = raw.splitn(2, ':');
            let name = parts.next().unwrap_or("");
            let scenario: Scenario = name.parse()?;
            let blocks = match parts.next() {
                Some(n) => n
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| format!("segment {i}: bad block count {n:?}: {e}"))?,
                None => u64::MAX,
            };
            if blocks == 0 {
                return Err(format!("segment {i}: block count must be > 0"));
            }
            segments.push(ScenarioSegment { scenario, blocks });
        }
        if segments.is_empty() {
            return Err("empty scenario timeline".to_string());
        }
        // The final segment always extends to the end of the run, otherwise a
        // longer `--blocks` than the timeline would silently fall off the end.
        if let Some(last) = segments.last_mut() {
            last.blocks = u64::MAX;
        }
        Ok(Self::new(segments))
    }
}

impl fmt::Display for ScenarioTimeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.segments.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            if seg.blocks == u64::MAX {
                write!(f, "{}", seg.scenario)?;
            } else {
                write!(f, "{}:{}", seg.scenario, seg.blocks)?;
            }
        }
        Ok(())
    }
}

/// The price path for every underlying, advanced one block at a time.
///
/// Prices are stored in q8. The walk is multiplicative so a crash of a given
/// percentage costs the same fraction of equity at any price level.
#[derive(Clone, Debug)]
pub struct PriceEngine {
    seed: u64,
    timeline: ScenarioTimeline,
    prices_q8: Vec<u128>,
}

impl PriceEngine {
    /// Start every underlying at its listed price.
    pub fn new(seed: u64, timeline: ScenarioTimeline) -> Self {
        Self {
            seed,
            timeline,
            prices_q8: Underlying::ALL.iter().map(|u| u.start_price_q8()).collect(),
        }
    }

    /// Current index price for `u`, in q8.
    pub fn price_q8(&self, u: Underlying) -> u128 {
        self.prices_q8[Self::index(u)]
    }

    /// The regime in force `block_offset` blocks into the run.
    pub fn scenario_at(&self, block_offset: u64) -> Scenario {
        self.timeline.at(block_offset)
    }

    /// Advance every price one block. `block_offset` is blocks since the run
    /// started, which is what selects the regime.
    pub fn step(&mut self, block_offset: u64) {
        let scenario = self.timeline.at(block_offset);
        // A regime change into a crash or a squeeze gaps the price once, on
        // entry. See `Scenario::entry_gap_bps`.
        let gap = if block_offset > 0 && self.timeline.at(block_offset - 1) != scenario {
            scenario.entry_gap_bps()
        } else {
            0
        };
        let drift = scenario.drift_bps() + gap;
        let noise = scenario.noise_bps();
        for (i, u) in Underlying::ALL.iter().enumerate() {
            // Re-seed per (underlying, block) so the path is a pure function
            // of the run seed and the height, independent of how many times
            // anything else drew from a generator this block.
            let mut rng = ChaCha8Rng::seed_from_u64(
                self.seed
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .wrapping_add(block_offset.wrapping_mul(31))
                    .wrapping_add(*u as u64),
            );
            let shock = if noise > 0 {
                rng.gen_range(-noise..=noise)
            } else {
                0
            };
            let bps = drift + shock;
            let cur = self.prices_q8[i];
            let next = crate::universe::bump_bps(cur, bps);
            // Floor at a cent so a long crash can never reach zero and turn
            // every notional into nonsense.
            self.prices_q8[i] = next.max(crate::universe::Q8 / 100);
        }
    }
}

impl PriceEngine {
    fn index(u: Underlying) -> usize {
        match u {
            Underlying::Btc => 0,
            Underlying::Eth => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_parses_and_last_segment_is_unbounded() {
        let t = ScenarioTimeline::parse("calm:10,crash:5,calm:3").unwrap();
        assert_eq!(t.at(0), Scenario::Calm);
        assert_eq!(t.at(9), Scenario::Calm);
        assert_eq!(t.at(10), Scenario::Crash);
        assert_eq!(t.at(14), Scenario::Crash);
        assert_eq!(t.at(15), Scenario::Calm);
        assert_eq!(t.at(1_000_000), Scenario::Calm);
    }

    #[test]
    fn bare_name_covers_the_whole_run() {
        let t = ScenarioTimeline::parse("chop").unwrap();
        assert_eq!(t.at(0), Scenario::Chop);
        assert_eq!(t.at(10_000), Scenario::Chop);
    }

    #[test]
    fn unknown_scenario_is_an_error_not_a_default() {
        assert!(ScenarioTimeline::parse("moonshot:10").is_err());
    }

    #[test]
    fn same_seed_produces_the_same_path() {
        let t = ScenarioTimeline::parse("calm:5,crash:5").unwrap();
        let mut a = PriceEngine::new(7, t.clone());
        let mut b = PriceEngine::new(7, t);
        for h in 0..20 {
            a.step(h);
            b.step(h);
            assert_eq!(a.price_q8(Underlying::Btc), b.price_q8(Underlying::Btc));
        }
    }

    #[test]
    fn regime_change_into_a_crash_gaps_the_price() {
        let t = ScenarioTimeline::parse("calm:3,crash").unwrap();
        let mut p = PriceEngine::new(3, t);
        for h in 0..3 {
            p.step(h);
        }
        let before = p.price_q8(Underlying::Btc);
        p.step(3);
        let after = p.price_q8(Underlying::Btc);
        // The gap alone is 25%; noise cannot plausibly cancel it.
        assert!(
            after * 100 < before * 85,
            "entering a crash must gap the price down, got {before} -> {after}"
        );
    }

    #[test]
    fn crash_actually_moves_price_down_hard() {
        let mut p = PriceEngine::new(1, ScenarioTimeline::single(Scenario::Crash));
        let start = p.price_q8(Underlying::Btc);
        for h in 0..40 {
            p.step(h);
        }
        let end = p.price_q8(Underlying::Btc);
        // 40 blocks at roughly -1.1% each must lose at least a quarter of the
        // price, or the regime is too gentle to cascade liquidations.
        assert!(
            end * 4 < start * 3,
            "crash only moved {start} -> {end}; too gentle to liquidate anyone"
        );
    }
}
