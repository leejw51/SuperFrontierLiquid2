//! What one tick publishes, over generated ages, windows, seeds and nonces.
//!
//! `publishing.rs` checks the freshness boundary at a couple of chosen ages.
//! Here the feed's staleness and the publisher's window are both generated,
//! and the whole plan is checked against what the inputs imply.
//!
//! What the group proves:
//! - a price is published exactly when its age is within the window, and a
//!   refused one names its real age and the window it exceeded;
//! - the published price is the feed's price at the q8 scale, exactly;
//! - nonces are consecutive from the first one given, with no gaps for the
//!   markets that were skipped, and `next_nonce` continues the sequence;
//! - every update is signed by the authority, for the chain and height asked.

use cb_feed::{FaultyFeed, FeedFaults, SimulatedFeed, UNIVERSE};
use cb_oracle::markets::OracleMarket;
use cb_oracle::publisher::{Publisher, SkipReason};
use cb_ports::clock::FixedClock;
use cb_ports::feed::MarketDataFeed;
use cb_tx::TxBody;
use cb_types::{symbols, Address};
use proptest::prelude::*;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

const AUTHORITY_KEY: &str = "0x943f7fb2f31e4f9a82252e317c82fb35408191017f64100b7ce1485d21e56730";

fn key() -> cb_crypto::SecretKey {
    cb_oracle::config::parse_secret_key(AUTHORITY_KEY).unwrap()
}

fn authority() -> Address {
    let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key());
    cb_crypto::address_from_pubkey(&public)
}

fn markets() -> Vec<OracleMarket> {
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn a_tick_publishes_exactly_the_fresh_prices_with_consecutive_nonces(
        seed in any::<u64>(),
        steps in 0u32..50,
        stale_ms in prop_oneof![Just(0u64), 0u64..20_000],
        window_ms in 1u64..20_000,
        chain_id in any::<u32>(),
        height in any::<u64>(),
        first_nonce in 0u64..(u64::MAX / 2),
    ) {
        let clock = Arc::new(FixedClock::new(
            chrono::DateTime::from_str("2026-01-01T00:00:00Z").unwrap(),
        ));
        let sim = SimulatedFeed::with_config(seed, UNIVERSE, Duration::from_secs(60), clock.clone());
        sim.advance(steps);
        let faults = FeedFaults::healthy();
        faults.set_staleness(Duration::from_millis(stale_ms));
        let feed = Arc::new(FaultyFeed::new(sim, faults));
        let publisher = Publisher::new(feed.clone(), clock, key(), Duration::from_millis(window_ms));
        let markets = markets();

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let plan = rt.block_on(publisher.plan(&markets, chain_id, height, first_nonce)).unwrap();

        let fresh = stale_ms <= window_ms;
        prop_assert_eq!(plan.updates.len() + plan.skipped.len(), markets.len());
        if fresh {
            prop_assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        } else {
            prop_assert!(plan.updates.is_empty());
            for s in &plan.skipped {
                match &s.reason {
                    SkipReason::Stale { age, limit } => {
                        prop_assert_eq!(*age, Duration::from_millis(stale_ms));
                        prop_assert_eq!(*limit, Duration::from_millis(window_ms));
                    }
                    other => prop_assert!(false, "expected Stale, got {:?}", other),
                }
            }
        }

        for (i, u) in plan.updates.iter().enumerate() {
            prop_assert_eq!(u.nonce, first_nonce + i as u64, "nonces are consecutive");
            let tick = rt.block_on(feed.latest(&u.symbol)).unwrap();
            let expected = tick.price * rust_decimal::Decimal::from(100_000_000u64);
            prop_assert_eq!(rust_decimal::Decimal::from(u.px_q8 as u64), expected.normalize());
            prop_assert_eq!(u.tx.sender().unwrap(), authority());
            prop_assert_eq!(u.tx.tx.chain_id, chain_id);
            prop_assert_eq!(u.tx.tx.nonce, u.nonce);
            match &u.tx.tx.body {
                TxBody::OracleUpdate { market, px_q8, submitted_height } => {
                    prop_assert_eq!(*market, u.market);
                    prop_assert_eq!(*px_q8, u.px_q8);
                    prop_assert_eq!(*submitted_height, height);
                }
                other => prop_assert!(false, "not an oracle update: {:?}", other),
            }
        }
        prop_assert_eq!(plan.next_nonce(first_nonce), first_nonce + plan.updates.len() as u64);
    }
}
