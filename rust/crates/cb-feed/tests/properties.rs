//! Price-path properties of the simulated feed, over generated seeds,
//! scenarios and step counts.
//!
//! `feed.rs` checks each scenario's character with one seed. These check the
//! bounds every tick must respect whatever the seed and whatever sequence of
//! scenario changes the simulation control API throws at it, because the
//! oracle signs whatever this feed prints and a single malformed tick becomes
//! an on-chain mark price.
//!
//! What the group proves, for every carried symbol at every step:
//! - the price is positive and finite, and carries no more decimals than the
//!   instrument quotes;
//! - the quote is two-sided and uncrossed: `0 < bid < ask`, and
//!   `bid <= price <= ask`;
//! - the tick's timestamp is the clock's time at the last step, so a clock
//!   that only moves forward yields timestamps that only move forward;
//! - the whole path is a pure function of the seed and the operations applied;
//! - arbitrary symbol and scenario names are refused, never a panic.

use cb_feed::{Scenario, SimulatedFeed, UNIVERSE};
use cb_ports::{FixedClock, MarketDataFeed, PortError};
use chrono::{DateTime, Duration as Chrono, Utc};
use proptest::prelude::*;
use std::sync::Arc;
use std::time::Duration;

fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-02T15:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

#[derive(Debug, Clone)]
enum Op {
    /// Advance the clock by this many seconds, then step the feed this many
    /// times.
    Step { secs: u16, steps: u8 },
    /// Put a symbol (index into the universe) into a scenario.
    Scenario { symbol: usize, scenario: usize },
    /// Put every symbol into a scenario.
    All { scenario: usize },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u16..3_600, 1u8..20).prop_map(|(secs, steps)| Op::Step { secs, steps }),
        2 => (0..UNIVERSE.len(), 0..Scenario::ALL.len())
            .prop_map(|(symbol, scenario)| Op::Scenario { symbol, scenario }),
        1 => (0..Scenario::ALL.len()).prop_map(|scenario| Op::All { scenario }),
    ]
}

/// Replay `ops` on a fresh feed, checking every tick after every operation,
/// and return the observed `(symbol, price, bid, ask)` trace.
fn replay(seed: u64, tick_secs: u64, ops: &[Op]) -> Result<Vec<String>, TestCaseError> {
    let clock = Arc::new(FixedClock::new(start()));
    let feed = SimulatedFeed::with_config(
        seed,
        UNIVERSE,
        Duration::from_secs(tick_secs),
        clock.clone(),
    );
    let symbols: Vec<String> = UNIVERSE.iter().map(|s| s.symbol.to_string()).collect();
    let rt = runtime();
    let mut trace = Vec::new();
    let mut last_at = start();
    for op in ops {
        match *op {
            Op::Step { secs, steps } => {
                clock.advance(Chrono::seconds(secs as i64));
                feed.advance(steps as u32);
            }
            Op::Scenario { symbol, scenario } => {
                feed.set_scenario(UNIVERSE[symbol].symbol, Scenario::ALL[scenario])
                    .unwrap();
            }
            Op::All { scenario } => feed.set_scenario_all(Scenario::ALL[scenario]),
        }
        let ticks = rt.block_on(feed.latest_batch(&symbols)).unwrap();
        prop_assert_eq!(ticks.len(), symbols.len(), "every carried symbol is served");
        for (tick, spec) in ticks.iter().zip(UNIVERSE) {
            prop_assert_eq!(&tick.symbol, spec.symbol);
            let (bid, ask) = (tick.bid.unwrap(), tick.ask.unwrap());
            prop_assert!(
                tick.price > rust_decimal::Decimal::ZERO,
                "{} price {}",
                spec.symbol,
                tick.price
            );
            prop_assert!(
                bid > rust_decimal::Decimal::ZERO,
                "{} bid {}",
                spec.symbol,
                bid
            );
            prop_assert!(
                bid < ask,
                "{} crossed or locked: {} / {}",
                spec.symbol,
                bid,
                ask
            );
            prop_assert!(
                bid <= tick.price && tick.price <= ask,
                "{} price {} outside {} / {}",
                spec.symbol,
                tick.price,
                bid,
                ask
            );
            for v in [tick.price, bid, ask] {
                prop_assert!(
                    v.scale() <= spec.price_decimals,
                    "{} {} has more than {} decimals",
                    spec.symbol,
                    v,
                    spec.price_decimals
                );
            }
            prop_assert!(tick.at >= last_at, "{} went back in time", spec.symbol);
            prop_assert_eq!(tick.at, clock_now(&clock));
            trace.push(format!("{} {} {} {}", tick.symbol, tick.price, bid, ask));
        }
        last_at = clock_now(&clock);
    }
    Ok(trace)
}

fn clock_now(c: &FixedClock) -> DateTime<Utc> {
    cb_ports::Clock::now(c)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Minute ticks: the regime the oracle runs in.
    #[test]
    fn every_tick_is_positive_uncrossed_and_in_time_order(
        seed in any::<u64>(), ops in proptest::collection::vec(op(), 1..40),
    ) {
        replay(seed, 60, &ops)?;
    }

    /// Day-long ticks under repeated crashes: the regime that drives prices
    /// towards their floor, where a spread wider than the price would make
    /// the bid negative.
    #[test]
    fn bounds_hold_when_crashes_drive_prices_to_the_floor(
        seed in any::<u64>(), crashes in 1usize..6,
    ) {
        let mut ops = Vec::new();
        for _ in 0..crashes {
            ops.push(Op::All { scenario: 3 }); // Crash
            ops.push(Op::Step { secs: 60, steps: 200 });
        }
        replay(seed, 86_400, &ops)?;
    }

    /// The same seed and operations give the same trace, tick for tick.
    #[test]
    fn a_path_is_a_function_of_seed_and_operations(
        seed in any::<u64>(), ops in proptest::collection::vec(op(), 1..20),
    ) {
        prop_assert_eq!(replay(seed, 60, &ops)?, replay(seed, 60, &ops)?);
    }

    #[test]
    fn unknown_symbols_and_scenarios_are_refused_not_panicked_on(name in "\\PC{0,16}") {
        let feed = SimulatedFeed::new(1);
        let carried = UNIVERSE.iter().any(|s| s.symbol == name);
        match runtime().block_on(feed.latest(&name)) {
            Ok(t) => prop_assert!(carried && t.symbol == name),
            Err(PortError::NotFound(_)) => prop_assert!(!carried),
            Err(e) => prop_assert!(false, "unexpected {:?}", e),
        }
        prop_assert_eq!(feed.set_scenario(&name, Scenario::Calm).is_ok(), carried);
        if let Some(s) = Scenario::parse(&name) {
            prop_assert_eq!(s.as_str(), name.as_str());
        }
    }
}

#[test]
fn every_scenario_name_round_trips() {
    for s in Scenario::ALL {
        assert_eq!(Scenario::parse(s.as_str()), Some(s));
    }
}
