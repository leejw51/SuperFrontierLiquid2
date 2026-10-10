//! Property tests for the value types every adapter shares.
//!
//! These types cross every port boundary, so their encodings and their
//! arithmetic are checked over generated inputs rather than examples.
//!
//! What the group proves:
//! - candle buckets are epoch-aligned, contain the instant they are asked
//!   about, are idempotent, and nest: every shorter bucket lies inside the
//!   longer bucket that contains it, so aggregating 1m candles into 1h
//!   candles never straddles a boundary;
//! - every wire name round-trips, arbitrary names are refused rather than
//!   mapped to a default, and the serde form round-trips too;
//! - a quote's spread fraction is `(ask - bid) / mid` exactly, non-negative
//!   for an uncrossed quote, and absent rather than a division by zero;
//! - retryability is decided by the error's kind alone.

use cb_ports::{CandleInterval, LeaderboardWindow, PortError, PriceTick, Topic};
use chrono::{DateTime, Utc};
use proptest::prelude::*;
use rust_decimal::Decimal;
use std::time::Duration;

/// Instants across a range far wider than the exchange will ever see, but
/// inside what chrono can represent after flooring to a day.
fn instant() -> impl Strategy<Value = DateTime<Utc>> {
    (-50_000_000_000i64..50_000_000_000i64, 0u32..1_000_000_000)
        .prop_map(|(s, n)| DateTime::from_timestamp(s, n).unwrap())
}

fn interval() -> impl Strategy<Value = CandleInterval> {
    prop::sample::select(CandleInterval::ALL.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn a_bucket_is_aligned_and_contains_its_instant(at in instant(), i in interval()) {
        let start = i.bucket_start(at);
        prop_assert_eq!(start.timestamp().rem_euclid(i.seconds()), 0);
        prop_assert_eq!(start.timestamp_subsec_nanos(), 0);
        prop_assert!(start <= at);
        prop_assert!(at < start + chrono::Duration::seconds(i.seconds()));
        prop_assert_eq!(i.bucket_start(start), start, "idempotent");
    }

    #[test]
    fn shorter_buckets_nest_inside_longer_ones(at in instant(), a in interval(), b in interval()) {
        let (short, long) = if a.seconds() <= b.seconds() { (a, b) } else { (b, a) };
        prop_assert_eq!(long.seconds() % short.seconds(), 0, "widths divide");
        let s = short.bucket_start(at);
        let l = long.bucket_start(at);
        prop_assert!(l <= s);
        prop_assert!(s + chrono::Duration::seconds(short.seconds())
            <= l + chrono::Duration::seconds(long.seconds()));
        prop_assert_eq!(long.bucket_start(s), l, "the short bucket's start is in the same long bucket");
    }

    #[test]
    fn wire_names_are_exact(s in "\\PC{0,6}") {
        if let Some(i) = CandleInterval::parse(&s) {
            prop_assert_eq!(i.as_str(), s.as_str());
        }
        if let Some(w) = LeaderboardWindow::parse(&s) {
            prop_assert_eq!(w.as_str(), s.as_str());
        }
    }

    #[test]
    fn the_spread_fraction_is_exact_and_signed_by_crossing(
        bid in 1i64..10_000_000_000, width in -1_000_000i64..10_000_000, dp in 0u32..8,
    ) {
        let b = Decimal::new(bid, dp);
        let a = Decimal::new(bid + width, dp);
        let tick = PriceTick {
            symbol: "X".into(), price: b, bid: Some(b), ask: Some(a),
            at: DateTime::UNIX_EPOCH, source: "p".into(),
        };
        let mid = (a + b) / Decimal::TWO;
        match tick.spread_fraction() {
            None => prop_assert!(mid.is_zero()),
            Some(f) => {
                prop_assert_eq!(f, (a - b) / mid);
                prop_assert_eq!(f >= Decimal::ZERO, a >= b, "negative exactly when crossed");
            }
        }
    }

    #[test]
    fn topics_keep_their_name(name in "\\PC{0,40}") {
        let t = Topic::new(name.clone());
        prop_assert_eq!(t.as_str(), name.as_str());
        prop_assert_eq!(t.to_string(), name.clone());
        prop_assert_eq!(Topic::from(name.as_str()), t);
    }
}

#[test]
fn every_interval_and_window_round_trips_through_serde() {
    for i in CandleInterval::ALL {
        let json = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<CandleInterval>(&json).unwrap(), i);
    }
    for w in [
        LeaderboardWindow::Day,
        LeaderboardWindow::Week,
        LeaderboardWindow::AllTime,
    ] {
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(serde_json::from_str::<LeaderboardWindow>(&json).unwrap(), w);
    }
    assert!(serde_json::from_str::<CandleInterval>("\"m2\"").is_err());
}

#[test]
fn exactly_the_transient_errors_are_retryable() {
    let cases = [
        (PortError::NotFound("x".into()), false),
        (PortError::Conflict("x".into()), false),
        (PortError::InvalidArgument("x".into()), false),
        (PortError::PermissionDenied("x".into()), false),
        (PortError::Encoding("x".into()), false),
        (PortError::Internal("x".into()), false),
        (PortError::Unavailable("x".into()), true),
        (PortError::Timeout(Duration::from_secs(1)), true),
        (PortError::RateLimited { retry_after: None }, true),
        (
            PortError::RateLimited {
                retry_after: Some(Duration::from_millis(5)),
            },
            true,
        ),
        (PortError::Contention("x".into()), true),
    ];
    for (e, retry) in cases {
        assert_eq!(e.is_retryable(), retry, "{e}");
        assert!(!e.to_string().is_empty());
    }
    // The rate-limit message carries the wait when there is one.
    let e = PortError::RateLimited {
        retry_after: Some(Duration::from_millis(5)),
    };
    assert!(e.to_string().contains("5ms"), "{e}");
}
