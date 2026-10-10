//! What one tick produces, and what it refuses to produce.
//!
//! These are the tests that would catch the two failures nobody notices in
//! time: a price published at the wrong scale, and a stale price republished
//! as though it were new. Both reach margin and liquidation before they reach
//! a human.

use cb_feed::{FaultyFeed, FeedFaults, Scenario, SimulatedFeed, UNIVERSE};
use cb_oracle::markets::OracleMarket;
use cb_oracle::publisher::{Publisher, SkipReason};
use cb_ports::clock::{Clock, FixedClock};
use cb_ports::feed::MarketDataFeed;
use cb_tx::TxBody;
use cb_types::{symbols, Address, DerivId};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const CHAIN_ID: u32 = 1000;

/// The dev authority key published in `rust/.env.example`.
const AUTHORITY_KEY: &str = "0x943f7fb2f31e4f9a82252e317c82fb35408191017f64100b7ce1485d21e56730";

fn key() -> cb_crypto::SecretKey {
    cb_oracle::config::parse_secret_key(AUTHORITY_KEY).expect("dev key parses")
}

fn authority() -> Address {
    let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key());
    cb_crypto::address_from_pubkey(&public)
}

fn fixed_clock() -> Arc<FixedClock> {
    Arc::new(FixedClock::new(
        chrono::DateTime::from_str("2026-01-01T00:00:00Z").expect("a valid instant"),
    ))
}

/// Every derivative market `genesis-full.jsonl` lists, as the node would
/// report them. Built from `cb_types::symbols`, which `cb-genesis`'s
/// `full_genesis` test pins against the genesis file itself.
fn genesis_full_markets() -> Vec<OracleMarket> {
    symbols::PERPS
        .iter()
        .map(|p| OracleMarket::new(p.deriv_id(), p.underlying, authority()))
        .chain(
            symbols::FUTURES
                .iter()
                .map(|f| OracleMarket::new(f.deriv_id(), f.underlying, authority())),
        )
        .collect()
}

fn simulated(seed: u64, clock: Arc<FixedClock>) -> Arc<SimulatedFeed> {
    Arc::new(SimulatedFeed::with_config(
        seed,
        UNIVERSE,
        Duration::from_secs(1),
        clock,
    ))
}

#[tokio::test]
async fn one_update_per_market_per_tick_signed_by_the_authority() {
    let clock = fixed_clock();
    let feed = simulated(7, clock.clone());
    let publisher = Publisher::new(feed, clock, key(), Duration::from_secs(3));
    let markets = genesis_full_markets();
    assert_eq!(
        markets.len(),
        10,
        "seven perpetuals and three dated futures"
    );

    let plan = publisher
        .plan(&markets, CHAIN_ID, 512, 9)
        .await
        .expect("a plan");

    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert_eq!(plan.updates.len(), markets.len());

    for (i, (update, market)) in plan.updates.iter().zip(&markets).enumerate() {
        assert_eq!(update.market, market.market, "markets keep their order");
        assert_eq!(
            update.nonce,
            9 + i as u64,
            "nonces are consecutive, which is what admission control demands"
        );

        // Signed by the configured authority — the executor rejects anything
        // else with `notAuthorized`, and that rejection is silent from here.
        let verified = update.tx.clone().verify().expect("a valid signature");
        assert_eq!(verified.sender, authority());

        match &verified.signed.tx.body {
            TxBody::OracleUpdate {
                market: m,
                px_q8,
                submitted_height,
            } => {
                assert_eq!(*m, market.market);
                assert_eq!(*px_q8, update.px_q8);
                assert_eq!(*submitted_height, 512);
                assert!(*px_q8 > 0);
            }
            other => panic!("expected an OracleUpdate, got {other:?}"),
        }
        assert_eq!(verified.signed.tx.chain_id, CHAIN_ID);
    }

    assert_eq!(plan.next_nonce(9), 9 + markets.len() as u64);
}

#[tokio::test]
async fn prices_are_published_at_the_chains_q8_scale() {
    let clock = fixed_clock();
    let feed = simulated(7, clock.clone());
    let publisher = Publisher::new(feed.clone(), clock, key(), Duration::from_secs(3));
    let markets = genesis_full_markets();

    let plan = publisher
        .plan(&markets, CHAIN_ID, 1, 0)
        .await
        .expect("a plan");

    // The scale check, done two ways so a change to either side is caught.
    //
    // 1. Against the feed's own decimal: `px_q8` must be the price times 1e8
    //    exactly, which is what `cb_types::scales::format_price_q8` inverts.
    for (update, market) in plan.updates.iter().zip(&markets) {
        let tick = feed
            .latest(&market.feed_symbol)
            .await
            .expect("the feed carries it");
        let rendered =
            cb_types::scales::format_price_q8(update.px_q8).expect("a representable price");
        assert_eq!(
            rust_decimal::Decimal::from_str(&rendered).expect("a decimal"),
            tick.price.normalize(),
            "{} published {} for a feed price of {}",
            market.market,
            update.px_q8,
            tick.price
        );
    }

    // 2. Against a number a human can check: the simulator starts BTC at
    //    $64,000, which is 6_400_000_000_000 at q8. One step of calm drift
    //    cannot move it by a factor of ten, so a factor-of-ten error in the
    //    conversion fails here rather than in a liquidation.
    let btc = plan
        .updates
        .iter()
        .find(|u| u.market == DerivId::perp(0))
        .expect("BTC-PERP is perp 0");
    assert!(
        (6_000_000_000_000..=6_800_000_000_000).contains(&btc.px_q8),
        "BTC-PERP published {} — expected about 6.4e12 at the q8 scale",
        btc.px_q8
    );

    // Two markets on one underlying get one price: BTC-PERP and BTC-261225.
    let btc_future = plan
        .updates
        .iter()
        .find(|u| u.market == DerivId::future(1))
        .expect("BTC-261225 is future 1");
    assert_eq!(btc.px_q8, btc_future.px_q8);
}

#[tokio::test]
async fn a_stale_feed_publishes_nothing() {
    let clock = fixed_clock();
    let faults = FeedFaults::healthy();
    let feed = Arc::new(FaultyFeed::new(
        SimulatedFeed::with_config(7, UNIVERSE, Duration::from_secs(1), clock.clone()),
        faults.clone(),
    ));
    let publisher = Publisher::new(feed, clock, key(), Duration::from_secs(3));
    let markets = genesis_full_markets();

    // Healthy first, so the test is measuring the staleness and not a typo.
    let plan = publisher
        .plan(&markets, CHAIN_ID, 1, 0)
        .await
        .expect("plan");
    assert_eq!(plan.updates.len(), markets.len());

    // The connection is up and the data has stopped moving — the case where
    // nothing errors and a careless oracle republishes yesterday's price as
    // today's.
    faults.set_staleness(Duration::from_secs(30));
    let plan = publisher
        .plan(&markets, CHAIN_ID, 2, 0)
        .await
        .expect("plan");

    assert!(
        plan.is_empty(),
        "a stale feed must publish nothing, got {} updates",
        plan.updates.len()
    );
    assert_eq!(plan.skipped.len(), markets.len());
    for skip in &plan.skipped {
        match skip.reason {
            SkipReason::Stale { age, limit } => {
                assert_eq!(age, Duration::from_secs(30));
                assert_eq!(limit, Duration::from_secs(3));
            }
            ref other => panic!("expected staleness, got {other:?}"),
        }
    }

    // And it recovers rather than latching: a feed that starts moving again is
    // published again.
    faults.reset();
    let plan = publisher
        .plan(&markets, CHAIN_ID, 3, 0)
        .await
        .expect("plan");
    assert_eq!(plan.updates.len(), markets.len());
}

#[tokio::test]
async fn a_price_just_inside_the_window_is_still_published() {
    let clock = fixed_clock();
    let faults = FeedFaults::healthy();
    let feed = Arc::new(FaultyFeed::new(
        SimulatedFeed::with_config(7, UNIVERSE, Duration::from_secs(1), clock.clone()),
        faults.clone(),
    ));
    let publisher = Publisher::new(feed, clock, key(), Duration::from_secs(3));
    let markets = genesis_full_markets();

    // Exactly at the limit is fresh; one millisecond past it is not. Pinning
    // the boundary keeps a later refactor from turning "three periods" into
    // "two and a bit" without anyone noticing.
    faults.set_staleness(Duration::from_secs(3));
    assert_eq!(
        publisher
            .plan(&markets, CHAIN_ID, 1, 0)
            .await
            .expect("plan")
            .updates
            .len(),
        markets.len()
    );

    faults.set_staleness(Duration::from_millis(3_001));
    assert!(publisher
        .plan(&markets, CHAIN_ID, 2, 0)
        .await
        .expect("plan")
        .is_empty());
}

#[tokio::test]
async fn a_seeded_run_is_reproducible() {
    async fn run(seed: u64, scenario: Scenario) -> Vec<Vec<u128>> {
        let clock = fixed_clock();
        let feed = simulated(seed, clock.clone());
        feed.set_scenario_all(scenario);
        let publisher = Publisher::new(feed.clone(), clock, key(), Duration::from_secs(3));
        let markets = genesis_full_markets();
        let mut path = Vec::new();
        for tick in 0..25u64 {
            feed.step();
            let plan = publisher
                .plan(&markets, CHAIN_ID, tick + 1, tick * 10)
                .await
                .expect("plan");
            path.push(plan.updates.iter().map(|u| u.px_q8).collect());
        }
        path
    }

    // The property the whole simulator rests on: the same seed replays the
    // same market, so a liquidation cascade can be re-run rather than merely
    // reported.
    assert_eq!(run(99, Scenario::Calm).await, run(99, Scenario::Calm).await);

    // And a different seed is a different market, or "reproducible" would be
    // satisfiable by a constant.
    assert_ne!(
        run(99, Scenario::Calm).await,
        run(100, Scenario::Calm).await
    );

    // The scenario is part of the run, not decoration: a crash does not
    // produce the calm path.
    assert_ne!(
        run(99, Scenario::Calm).await,
        run(99, Scenario::Crash).await
    );
}

#[tokio::test]
async fn a_market_the_feed_cannot_price_is_skipped_not_faked() {
    let clock = fixed_clock();
    // A feed carrying BTC only, against a market list that also wants ETH.
    let narrow = Arc::new(SimulatedFeed::with_config(
        7,
        &UNIVERSE[..1],
        Duration::from_secs(1),
        clock.clone(),
    ));
    assert_eq!(
        narrow.supported_symbols().await.expect("symbols"),
        vec!["BTC-USD".to_string()]
    );

    let publisher = Publisher::new(narrow, clock, key(), Duration::from_secs(3));
    let markets = vec![
        OracleMarket::new(DerivId::perp(0), cb_types::AssetId::Btc, authority()),
        OracleMarket::new(DerivId::perp(1), cb_types::AssetId::Eth, authority()),
    ];

    let plan = publisher
        .plan(&markets, CHAIN_ID, 1, 0)
        .await
        .expect("plan");
    assert_eq!(plan.updates.len(), 1);
    assert_eq!(plan.updates[0].market, DerivId::perp(0));
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].reason, SkipReason::NoPrice);
    // The nonce sequence closes over the gap rather than leaving a hole the
    // node would reject.
    assert_eq!(plan.next_nonce(0), 1);
}

#[tokio::test]
async fn the_freshness_decision_uses_the_clock_port_not_wall_time() {
    // A replay with a fixed clock must make the same freshness decisions the
    // live run made; reading `Utc::now()` directly would make this test pass
    // today and fail whenever the machine is slow.
    let clock = fixed_clock();
    let feed = simulated(7, clock.clone());
    let publisher = Publisher::new(feed.clone(), clock.clone(), key(), Duration::from_secs(3));
    let markets = genesis_full_markets();

    feed.step();
    assert_eq!(
        publisher
            .plan(&markets, CHAIN_ID, 1, 0)
            .await
            .expect("plan")
            .updates
            .len(),
        markets.len()
    );

    // Move the clock, not the feed: the prices are now old.
    clock.advance(chrono::Duration::seconds(60));
    assert!(publisher
        .plan(&markets, CHAIN_ID, 2, 0)
        .await
        .expect("plan")
        .is_empty());

    // Stepping restamps them, and publishing resumes.
    feed.step();
    assert_eq!(
        publisher
            .plan(&markets, CHAIN_ID, 3, 0)
            .await
            .expect("plan")
            .updates
            .len(),
        markets.len()
    );
    assert_eq!(clock.now().to_rfc3339(), "2026-01-01T00:01:00+00:00");
}
