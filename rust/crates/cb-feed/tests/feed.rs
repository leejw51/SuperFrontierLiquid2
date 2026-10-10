//! The properties the simulator's usefulness rests on.

use cb_feed::universe::AssetSpec;
use cb_feed::{FaultyFeed, FeedFaults, Scenario, SimulatedFeed, StaticRefData, UNIVERSE};
use cb_ports::{EquityReferenceData, FixedClock, MarketDataFeed, PortError, PriceTick};
use chrono::{DateTime, Datelike, Utc, Weekday};
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use std::time::Duration;

/// A single-instrument universe, for the test that a symbol's path does not
/// depend on which other symbols are loaded.
static BTC_ONLY: [AssetSpec; 1] = [UNIVERSE[0]];

const SEED: u64 = 0xC0FFEE;

fn t(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("test literals are valid RFC 3339")
        .with_timezone(&Utc)
}

/// A feed on a one-hour tick and a frozen clock, for scenario tests that need
/// a horizon long enough for drift to be visible above noise.
fn hourly(seed: u64) -> SimulatedFeed {
    SimulatedFeed::with_config(
        seed,
        UNIVERSE,
        Duration::from_secs(3_600),
        Arc::new(FixedClock::new(t("2026-01-05T15:00:00Z"))),
    )
}

fn path(feed: &SimulatedFeed, symbol: &str, steps: u32) -> Vec<f64> {
    (0..steps)
        .map(|_| {
            feed.step();
            feed.raw_price(symbol).expect("the symbol is carried")
        })
        .collect()
}

// ---------------------------------------------------------- determinism --

/// The whole simulation's reproducibility rests on this one property.
#[tokio::test]
async fn the_same_seed_produces_the_same_price_path() {
    let a = SimulatedFeed::new(SEED);
    let b = SimulatedFeed::new(SEED);

    for symbol in ["BTC-USD", "ETH-USD", "SOL-USD", "AAPL-USD", "SPY-USD"] {
        assert_eq!(
            path(&a, symbol, 200),
            path(&b, symbol, 200),
            "two feeds with seed {SEED} must produce identical paths for {symbol}"
        );
    }
}

#[tokio::test]
async fn different_seeds_diverge() {
    let a = SimulatedFeed::new(SEED);
    let b = SimulatedFeed::new(SEED + 1);

    let pa = path(&a, "BTC-USD", 100);
    let pb = path(&b, "BTC-USD", 100);

    assert_ne!(pa, pb, "a different seed must give a different path");
    assert!(
        pa.iter().zip(&pb).all(|(x, y)| x != y),
        "the paths must differ at every step, not merely somewhere"
    );
}

/// Two symbols must not share a stream, or the market would be one instrument
/// wearing several names.
#[tokio::test]
async fn symbols_have_independent_streams() {
    let feed = SimulatedFeed::new(SEED);
    let btc = path(&feed, "BTC-USD", 50);
    let feed2 = SimulatedFeed::new(SEED);
    let eth = path(&feed2, "ETH-USD", 50);

    let btc_returns: Vec<f64> = btc.windows(2).map(|w| w[1] / w[0]).collect();
    let eth_returns: Vec<f64> = eth.windows(2).map(|w| w[1] / w[0]).collect();
    assert!(
        btc_returns
            .iter()
            .zip(&eth_returns)
            .any(|(a, b)| (a - b).abs() > 1e-12),
        "BTC and ETH must not be driven by the same random stream"
    );
}

/// Seeding from the symbol's name, not its index, is what buys this: adding an
/// instrument to the universe cannot perturb the others' paths.
#[tokio::test]
async fn a_symbols_path_does_not_depend_on_which_other_symbols_are_loaded() {
    let full = SimulatedFeed::new(SEED);
    let just_btc = SimulatedFeed::with_symbols(SEED, &BTC_ONLY);

    assert_eq!(
        path(&full, "BTC-USD", 100),
        path(&just_btc, "BTC-USD", 100),
        "BTC's path must be the same whether or not the other six are present"
    );
}

/// If reading moved the market, adding a log line would change a simulation's
/// outcome.
#[tokio::test]
async fn observing_a_price_never_advances_it() {
    let feed = SimulatedFeed::new(SEED);
    feed.advance(10);

    let before = feed.raw_price("BTC-USD").expect("carried");
    let first = feed.latest("BTC-USD").await.expect("carried");
    for _ in 0..25 {
        let again = feed.latest("BTC-USD").await.expect("carried");
        assert_eq!(again.price, first.price);
    }
    feed.latest_batch(&["BTC-USD".into(), "ETH-USD".into()])
        .await
        .expect("batch");

    assert_eq!(
        feed.raw_price("BTC-USD").expect("carried"),
        before,
        "reading must not consume randomness"
    );
}

// ---------------------------------------------------------- port surface --

#[tokio::test]
async fn the_feed_carries_the_documented_universe() {
    let feed = SimulatedFeed::new(SEED);
    let mut symbols = feed.supported_symbols().await.expect("symbols");
    symbols.sort();

    assert_eq!(
        symbols,
        vec!["AAPL-USD", "BTC-USD", "ETH-USD", "NVDA-USD", "SOL-USD", "SPY-USD", "TSLA-USD",]
    );
    assert!(feed.source_id().starts_with("sim:"));
    feed.health_check()
        .await
        .expect("a simulated source is always up");
}

#[tokio::test]
async fn an_uncarried_symbol_is_not_found_rather_than_unavailable() {
    let feed = SimulatedFeed::new(SEED);
    let err = feed.latest("DOGE-USD").await.expect_err("not carried");
    assert!(
        matches!(err, PortError::NotFound(_)),
        "a symbol this source does not carry is distinct from a source that is \
         down: {err:?}"
    );
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_batch_skips_unknown_symbols_instead_of_failing() {
    let feed = SimulatedFeed::new(SEED);
    feed.step();

    let got = feed
        .latest_batch(&["BTC-USD".into(), "DOGE-USD".into(), "AAPL-USD".into()])
        .await
        .expect("one unknown symbol must not fail the batch");

    assert_eq!(
        got.iter().map(|t| t.symbol.as_str()).collect::<Vec<_>>(),
        vec!["BTC-USD", "AAPL-USD"]
    );
}

#[tokio::test]
async fn subscribing_to_nothing_is_rejected() {
    let feed = SimulatedFeed::new(SEED);
    let err = feed
        .subscribe(&[])
        .await
        .map(|_| ())
        .expect_err("a subscription with no symbols is rejected");
    assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");

    let err = feed
        .subscribe(&["DOGE-USD".into()])
        .await
        .map(|_| ())
        .expect_err("a subscription to only uncarried symbols is rejected");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn a_subscription_streams_a_tick_per_symbol_per_step() {
    let feed = SimulatedFeed::new(SEED);
    let mut sub = feed
        .subscribe(&["BTC-USD".into(), "ETH-USD".into()])
        .await
        .expect("subscribe");

    feed.step();

    let mut seen = Vec::new();
    for _ in 0..2 {
        let tick = tokio::time::timeout(Duration::from_secs(5), sub.next())
            .await
            .expect("the feed produces a tick promptly")
            .expect("no error")
            .expect("the stream is open");
        seen.push(tick.symbol);
    }
    seen.sort();
    assert_eq!(seen, vec!["BTC-USD", "ETH-USD"]);
}

#[tokio::test]
async fn ticks_carry_the_injected_clocks_time_and_the_sources_identity() {
    let at = t("2026-05-04T12:00:00Z");
    let clock = FixedClock::new(at);
    let feed = SimulatedFeed::with_config(
        SEED,
        UNIVERSE,
        Duration::from_secs(1),
        Arc::new(clock.clone()),
    );
    feed.step();

    let tick = feed.latest("BTC-USD").await.expect("carried");
    assert_eq!(tick.at, at, "observation time comes from the clock port");
    assert_eq!(tick.source, feed.source_id());

    clock.advance(chrono::Duration::minutes(5));
    feed.step();
    assert_eq!(
        feed.latest("BTC-USD").await.expect("carried").at,
        at + chrono::Duration::minutes(5)
    );
}

// ------------------------------------------------------------ scenarios --

fn drawdown(prices: &[f64], start: f64) -> f64 {
    prices.iter().fold(f64::MAX, |a, b| a.min(*b)) / start
}

/// The assertion the brief asks for: a crash must be a genuine sharp
/// drawdown, not elevated noise. Elevated noise would fail this — a
/// three-times volatility multiplier alone moves a one-hour BTC tick by well
/// under a percent.
#[tokio::test]
async fn a_crash_produces_a_real_drawdown_where_calm_does_not() {
    let crashing = hourly(SEED);
    let start = crashing.raw_price("BTC-USD").expect("carried");
    crashing.set_scenario_all(Scenario::Crash);
    let crash_path = path(&crashing, "BTC-USD", 200);

    let calm = hourly(SEED);
    let calm_path = path(&calm, "BTC-USD", 200);

    let crash_low = drawdown(&crash_path, start);
    let calm_low = drawdown(&calm_path, start);

    assert!(
        crash_low < 0.85,
        "a crash must draw down more than 15%; got {:.1}%",
        (1.0 - crash_low) * 100.0
    );
    assert!(
        calm_low > 0.85,
        "the same seed under `calm` must not draw down like that; got {:.1}%",
        (1.0 - calm_low) * 100.0
    );
    assert!(
        crash_path[crash_path.len() - 1] < start * 0.85,
        "the slide must persist after the opening shock rather than snapping back"
    );
}

#[tokio::test]
async fn a_squeeze_produces_a_real_rally() {
    let feed = hourly(SEED);
    let start = feed.raw_price("ETH-USD").expect("carried");
    feed.set_scenario_all(Scenario::Squeeze);
    let prices = path(&feed, "ETH-USD", 200);

    let high = prices.iter().fold(f64::MIN, |a, b| a.max(*b)) / start;
    assert!(
        high > 1.15,
        "a squeeze must rally more than 15%; got {:.1}%",
        (high - 1.0) * 100.0
    );
}

/// Both feeds share a seed, so they draw the identical normal variates; the
/// only thing separating them is the scenario's drift. That makes the
/// comparison deterministic rather than a coin flip about which path happened
/// to wander further.
#[tokio::test]
async fn trends_point_the_way_they_say_they_do() {
    let up = hourly(SEED);
    up.set_scenario_all(Scenario::TrendUp);
    let down = hourly(SEED);
    down.set_scenario_all(Scenario::TrendDown);

    let up_path = path(&up, "SPY-USD", 2_000);
    let down_path = path(&down, "SPY-USD", 2_000);

    assert!(
        up_path[up_path.len() - 1] > down_path[down_path.len() - 1],
        "trend_up must finish above trend_down on the same random stream: \
         {} vs {}",
        up_path[up_path.len() - 1],
        down_path[down_path.len() - 1]
    );
}

#[tokio::test]
async fn chop_thrashes_without_going_anywhere() {
    let feed = hourly(SEED);
    let anchor = feed.raw_price("BTC-USD").expect("carried");
    feed.set_scenario_all(Scenario::Chop);
    let prices = path(&feed, "BTC-USD", 2_000);

    let worst = prices
        .iter()
        .map(|p| (p / anchor - 1.0).abs())
        .fold(0.0f64, f64::max);
    assert!(
        worst < 0.35,
        "chop must stay in a range around where it started; wandered {:.1}%",
        worst * 100.0
    );

    // And it is not merely calm: the path has to actually move.
    let motion = prices
        .windows(2)
        .map(|w| (w[1] / w[0] - 1.0).abs())
        .fold(0.0f64, f64::max);
    assert!(motion > 0.0005, "chop must be violent, not quiet: {motion}");
}

#[tokio::test]
async fn a_scenario_can_be_set_per_symbol_and_read_back() {
    let feed = SimulatedFeed::new(SEED);
    assert_eq!(feed.scenario("BTC-USD").expect("carried"), Scenario::Calm);

    feed.set_scenario("BTC-USD", Scenario::Crash)
        .expect("carried");
    assert_eq!(feed.scenario("BTC-USD").expect("carried"), Scenario::Crash);
    assert_eq!(
        feed.scenario("ETH-USD").expect("carried"),
        Scenario::Calm,
        "setting one symbol's scenario must not touch another's"
    );

    let err = feed
        .set_scenario("DOGE-USD", Scenario::Crash)
        .expect_err("a typo in a scenario script must not silently do nothing");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

// --------------------------------------------------------------- quotes --

fn spread_fraction(tick: &PriceTick) -> f64 {
    tick.spread_fraction()
        .expect("the simulated feed always quotes both sides")
        .to_f64()
        .expect("a spread fraction fits an f64")
}

#[tokio::test]
async fn quotes_straddle_the_mid_and_never_cross() {
    let feed = SimulatedFeed::new(SEED);
    for scenario in Scenario::ALL {
        feed.set_scenario_all(scenario);
        for _ in 0..50 {
            feed.step();
            for spec in UNIVERSE {
                let tick = feed.latest(spec.symbol).await.expect("carried");
                let bid = tick.bid.expect("bid quoted");
                let ask = tick.ask.expect("ask quoted");
                assert!(
                    bid < ask,
                    "{} quoted a crossed or locked market under {scenario:?}: \
                     bid {bid}, ask {ask}",
                    spec.symbol
                );
                assert!(
                    bid <= tick.price && tick.price <= ask,
                    "{} quoted a mid outside its own spread under {scenario:?}",
                    spec.symbol
                );
            }
        }
    }
}

#[tokio::test]
async fn spreads_widen_with_volatility() {
    let calm = SimulatedFeed::new(SEED);
    calm.set_scenario_all(Scenario::Calm);
    calm.step();

    let stressed = SimulatedFeed::new(SEED);
    stressed.set_scenario_all(Scenario::Crash);
    stressed.step();

    let quiet = spread_fraction(&calm.latest("BTC-USD").await.expect("carried"));
    let violent = spread_fraction(&stressed.latest("BTC-USD").await.expect("carried"));

    assert!(
        violent > quiet * 2.0,
        "a market maker quotes markedly wider in a crash: {violent} vs {quiet}"
    );
}

#[tokio::test]
async fn an_index_etf_quotes_tighter_than_a_high_beta_single_name() {
    let feed = SimulatedFeed::new(SEED);
    feed.step();

    let spy = spread_fraction(&feed.latest("SPY-USD").await.expect("carried"));
    let tsla = spread_fraction(&feed.latest("TSLA-USD").await.expect("carried"));
    assert!(spy < tsla, "SPY {spy} should quote inside TSLA {tsla}");
}

// ------------------------------------------------------- reference data --

#[tokio::test]
async fn reference_data_describes_every_instrument() {
    let refdata = StaticRefData::with_clock(&FixedClock::new(t("2026-01-01T00:00:00Z")));

    let all = refdata.all_instruments().await.expect("all");
    assert_eq!(all.len(), UNIVERSE.len());

    let aapl = refdata.instrument("AAPL").await.expect("carried");
    assert_eq!(aapl.name, "Apple Inc.");
    assert_eq!(aapl.asset_class, "equity");
    assert_eq!(aapl.venue.as_deref(), Some("NASDAQ"));
    assert_eq!(aapl.currency, "USD");
    assert!(aapl.tradable);

    let btc = refdata.instrument("BTC").await.expect("carried");
    assert_eq!(btc.asset_class, "crypto");
    assert_eq!(btc.venue, None, "crypto has no listing venue");

    assert_eq!(
        refdata.instrument("BTC-USD").await.expect("carried"),
        btc,
        "either spelling resolves to the same instrument"
    );

    let err = refdata.instrument("DOGE").await.expect_err("not carried");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");

    refdata
        .health_check()
        .await
        .expect("a fixture is always up");
}

#[tokio::test]
async fn as_of_is_stable_so_a_caller_can_cache_on_it() {
    let refdata = StaticRefData::with_clock(&FixedClock::new(t("2026-01-01T00:00:00Z")));
    let first = refdata.instrument("AAPL").await.expect("carried");
    let second = refdata.instrument("AAPL").await.expect("carried");
    assert_eq!(first.as_of, second.as_of);
}

#[tokio::test]
async fn the_equity_session_calendar_is_meaningful() {
    let refdata = StaticRefData::new();

    // A Wednesday in September, 11:00 Eastern Daylight: mid-session.
    let midsession = t("2026-09-16T15:00:00Z");
    assert_eq!(midsession.weekday(), Weekday::Wed);
    assert!(refdata.is_open("AAPL", midsession).await.expect("carried"));

    // A Saturday, same time of day.
    let weekend = t("2026-09-19T15:00:00Z");
    assert_eq!(weekend.weekday(), Weekday::Sat);
    assert!(!refdata.is_open("AAPL", weekend).await.expect("carried"));

    // A Tuesday evening, 22:00 Eastern: a weekday, and firmly shut.
    let after_hours = t("2026-09-16T02:00:00Z");
    assert_eq!(
        after_hours.weekday(),
        Weekday::Wed,
        "02:00 UTC is still Wednesday in UTC"
    );
    assert!(!refdata.is_open("AAPL", after_hours).await.expect("carried"));

    // The bell, to the minute, on both sides of daylight saving.
    assert!(!refdata
        .is_open("SPY", t("2026-01-14T14:29:59Z"))
        .await
        .expect("carried"));
    assert!(refdata
        .is_open("SPY", t("2026-01-14T14:30:00Z"))
        .await
        .expect("carried"));
    assert!(refdata
        .is_open("SPY", t("2026-09-16T13:30:00Z"))
        .await
        .expect("carried"));
    assert!(!refdata
        .is_open("SPY", t("2026-09-16T20:00:00Z"))
        .await
        .expect("carried"));
}

#[tokio::test]
async fn crypto_is_always_open() {
    let refdata = StaticRefData::new();
    for at in [
        "2026-09-19T03:00:00Z", // Saturday, small hours
        "2026-12-25T00:00:00Z", // Christmas
        "2026-09-16T15:00:00Z", // an ordinary Wednesday
    ] {
        assert!(
            refdata.is_open("BTC", t(at)).await.expect("carried"),
            "crypto must be open at {at}"
        );
    }

    let err = refdata
        .is_open("DOGE", t("2026-09-16T15:00:00Z"))
        .await
        .expect_err("not carried");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

// ----------------------------------------------------------- FaultyFeed --

#[tokio::test]
async fn injected_staleness_ages_ticks_without_changing_prices() {
    let at = t("2026-05-04T12:00:00Z");
    let inner = SimulatedFeed::with_config(
        SEED,
        UNIVERSE,
        Duration::from_secs(1),
        Arc::new(FixedClock::new(at)),
    );
    inner.step();
    let honest = inner.latest("BTC-USD").await.expect("carried");

    let faults = FeedFaults::healthy();
    let feed = FaultyFeed::new(inner, Arc::clone(&faults));
    faults.set_staleness(Duration::from_secs(90));

    let stale = feed
        .latest("BTC-USD")
        .await
        .expect("a stale feed still answers");
    assert_eq!(
        stale.price, honest.price,
        "staleness must not alter the price; that is what makes it hard to spot"
    );
    assert_eq!(
        stale.at,
        at - chrono::Duration::seconds(90),
        "the observation time must move into the past, which is the only signal \
         a freshness check has"
    );

    faults.reset();
    assert_eq!(feed.latest("BTC-USD").await.expect("healthy").at, at);
}

#[tokio::test]
async fn a_downed_source_is_unavailable_across_every_method() {
    let faults = FeedFaults::healthy();
    let feed = FaultyFeed::new(SimulatedFeed::new(SEED), Arc::clone(&faults));
    feed.health_check().await.expect("healthy to start");

    faults.take_down();
    for result in [
        feed.latest("BTC-USD").await.map(|_| ()),
        feed.latest_batch(&["BTC-USD".into()]).await.map(|_| ()),
        feed.subscribe(&["BTC-USD".into()]).await.map(|_| ()),
        feed.supported_symbols().await.map(|_| ()),
        feed.health_check().await,
    ] {
        let err = result.expect_err("a downed source answers nothing");
        assert!(matches!(err, PortError::Unavailable(_)), "got {err:?}");
        assert!(err.is_retryable());
    }

    faults.restore();
    feed.health_check().await.expect("restored");
}

#[tokio::test]
async fn rate_limiting_reports_how_long_to_wait() {
    let faults = FeedFaults::healthy();
    let feed = FaultyFeed::new(SimulatedFeed::new(SEED), Arc::clone(&faults));
    faults.rate_limit_every_nth(3, Duration::from_secs(30));

    let mut limited = 0;
    for n in 1..=9 {
        match feed.latest("BTC-USD").await {
            Ok(_) => {}
            Err(PortError::RateLimited { retry_after }) => {
                limited += 1;
                assert_eq!(
                    retry_after,
                    Some(Duration::from_secs(30)),
                    "a caller must be told how long to back off"
                );
            }
            Err(other) => panic!("unexpected error on call {n}: {other:?}"),
        }
    }
    assert_eq!(limited, 3, "calls 3, 6 and 9 are rejected");
    assert_eq!(faults.calls(), 9);
}

#[tokio::test]
async fn a_stream_can_degrade_halfway_through() {
    let at = t("2026-05-04T12:00:00Z");
    let inner = SimulatedFeed::with_config(
        SEED,
        UNIVERSE,
        Duration::from_secs(1),
        Arc::new(FixedClock::new(at)),
    );
    let faults = FeedFaults::healthy();
    let feed = FaultyFeed::new(inner.clone(), Arc::clone(&faults));

    let mut sub = feed
        .subscribe(&["BTC-USD".into()])
        .await
        .expect("subscribe");

    inner.step();
    let fresh = sub.next().await.expect("read").expect("open");
    assert_eq!(fresh.at, at);

    faults.set_staleness(Duration::from_secs(600));
    inner.step();
    let stale = sub.next().await.expect("read").expect("open");
    assert_eq!(
        stale.at,
        at - chrono::Duration::seconds(600),
        "staleness applies at delivery, so a healthy stream can rot mid-flight"
    );

    faults.take_down();
    inner.step();
    let err = sub
        .next()
        .await
        .expect_err("the source went down mid-stream");
    assert!(matches!(err, PortError::Unavailable(_)), "got {err:?}");
}
