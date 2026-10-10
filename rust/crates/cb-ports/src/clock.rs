//! Time, injectable.
//!
//! Funding accrual, faucet cooldowns, session expiry and candle bucketing all
//! turn on "what time is it". A test that cannot move time must either sleep
//! or skip the branch, so time is a port like any other.

use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// A source of wall-clock time.
pub trait Clock: Send + Sync + 'static {
    /// The current instant in UTC.
    fn now(&self) -> DateTime<Utc>;
}

/// The real system clock. Use this in production wiring.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock the test controls.
///
/// Set it to an instant, advance it deliberately, and assert on behaviour at
/// a boundary — a faucet cooldown expiring, a funding interval elapsing —
/// without sleeping.
#[derive(Debug, Clone)]
pub struct FixedClock {
    at: Arc<Mutex<DateTime<Utc>>>,
}

impl FixedClock {
    /// A clock pinned to `at`.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            at: Arc::new(Mutex::new(at)),
        }
    }

    /// Move the clock forward by `delta`.
    pub fn advance(&self, delta: chrono::Duration) {
        let mut guard = self.at.lock().expect("fixed clock poisoned");
        *guard += delta;
    }

    /// Jump the clock to an absolute instant.
    pub fn set(&self, at: DateTime<Utc>) {
        *self.at.lock().expect("fixed clock poisoned") = at;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.at.lock().expect("fixed clock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_only_moves_when_told() {
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = FixedClock::new(t0);
        assert_eq!(clock.now(), t0);
        assert_eq!(clock.now(), t0, "reading the clock must not advance it");

        clock.advance(chrono::Duration::hours(1));
        assert_eq!(clock.now(), t0 + chrono::Duration::hours(1));
    }

    #[test]
    fn fixed_clock_is_shared_across_clones() {
        let t0 = Utc::now();
        let a = FixedClock::new(t0);
        let b = a.clone();
        a.advance(chrono::Duration::seconds(5));
        assert_eq!(b.now(), t0 + chrono::Duration::seconds(5));
    }
}
