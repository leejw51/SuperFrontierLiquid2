//! Rate limiting for the endpoints that change something.
//!
//! ## Two keys, because they fail differently
//!
//! A limit keyed only on the session is useless against someone registering
//! accounts in a loop — every new account arrives with a fresh key. A limit
//! keyed only on the client address punishes everyone behind one NAT for one
//! person's behaviour. Mutating requests are therefore checked against both,
//! and the tighter one wins.
//!
//! Reads are not limited. They are cheap, they are what a trading UI does
//! constantly, and throttling them would degrade the product to defend
//! against something that is not a threat.
//!
//! ## Fixed windows, deliberately
//!
//! This is a counter per key per window, not a token bucket. A fixed window
//! lets through up to twice the nominal rate across a window boundary, which
//! for a simulator is an acceptable trade for an implementation whose
//! behaviour an operator can predict from two numbers.

use cb_ports::Clock;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;

/// The outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Within the limit.
    Allow,
    /// Over the limit; retry after this many seconds.
    Deny {
        /// Seconds until the window rolls over.
        retry_after_seconds: u64,
    },
}

impl Decision {
    /// Whether the request may proceed.
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started_at_ms: i64,
    count: u32,
}

/// Most keys the limiter will track at once.
///
/// The map is attacker-influenced — a new session or a new peer address is a
/// new key — so it needs a ceiling as well as a janitor. Fifty thousand
/// windows is far more than a simulator will ever have live callers and costs
/// a few megabytes at most.
const MAX_TRACKED_KEYS: usize = 50_000;

/// A fixed-window counter over two key spaces.
pub struct RateLimiter {
    clock: Arc<dyn Clock>,
    windows: DashMap<String, Window>,
    window: Duration,
    per_session: u32,
    per_ip: u32,
    max_keys: usize,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("per_session", &self.per_session)
            .field("per_ip", &self.per_ip)
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    /// A limiter over a one-minute window.
    pub fn new(clock: Arc<dyn Clock>, per_session: u32, per_ip: u32) -> Self {
        Self {
            clock,
            windows: DashMap::new(),
            window: Duration::from_secs(60),
            per_session,
            per_ip,
            max_keys: MAX_TRACKED_KEYS,
        }
    }

    /// A limiter that tracks at most `max_keys` windows. For tests.
    #[cfg(test)]
    fn with_capacity(
        clock: Arc<dyn Clock>,
        per_session: u32,
        per_ip: u32,
        max_keys: usize,
    ) -> Self {
        Self {
            max_keys: max_keys.max(1),
            ..Self::new(clock, per_session, per_ip)
        }
    }

    /// Check and count one mutating request.
    ///
    /// Both keys are counted even when the first one denies, so a caller
    /// cannot use a denied session to probe without touching their address
    /// budget.
    pub fn check(&self, session: Option<&str>, client: &str) -> Decision {
        let ip = self.bump(&format!("ip:{client}"), self.per_ip);
        let session = match session {
            Some(id) => self.bump(&format!("session:{id}"), self.per_session),
            None => Decision::Allow,
        };
        match (ip, session) {
            (Decision::Allow, Decision::Allow) => Decision::Allow,
            // The tighter of the two, so neither limit can be evaded.
            (
                Decision::Deny {
                    retry_after_seconds: a,
                },
                Decision::Deny {
                    retry_after_seconds: b,
                },
            ) => Decision::Deny {
                retry_after_seconds: a.max(b),
            },
            (deny, Decision::Allow) | (Decision::Allow, deny) => deny,
        }
    }

    fn bump(&self, key: &str, limit: u32) -> Decision {
        if limit == 0 {
            return Decision::Allow;
        }
        let now_ms = self.clock.now().timestamp_millis();
        let window_ms = self.window.as_millis() as i64;
        if !self.windows.contains_key(key) && self.windows.len() >= self.max_keys {
            self.make_room(now_ms, window_ms);
        }
        let mut entry = self.windows.entry(key.to_string()).or_insert(Window {
            started_at_ms: now_ms,
            count: 0,
        });
        if now_ms - entry.started_at_ms >= window_ms {
            entry.started_at_ms = now_ms;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count > limit {
            let elapsed = now_ms - entry.started_at_ms;
            Decision::Deny {
                retry_after_seconds: ((window_ms - elapsed).max(0) as u64).div_ceil(1000).max(1),
            }
        } else {
            Decision::Allow
        }
    }

    /// Make space for a new key when the map is at its ceiling.
    ///
    /// Stale windows go first; if that is not enough, the oldest live windows
    /// are evicted. Evicting a live window forgives whatever that key had
    /// spent, which is the right way round: the alternative — refusing to
    /// track new keys once full — would let one flood deny service to every
    /// caller who arrived after it, turning the limiter into the weapon.
    fn make_room(&self, now_ms: i64, window_ms: i64) {
        self.windows
            .retain(|_, w| now_ms - w.started_at_ms < window_ms);
        if self.windows.len() < self.max_keys {
            return;
        }
        // Evict a tenth of the map, oldest first, so this is amortised rather
        // than a scan on every subsequent insert.
        let target = self.max_keys / 10 + 1;
        let mut oldest: Vec<(String, i64)> = self
            .windows
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().started_at_ms))
            .collect();
        oldest.sort_by_key(|(_, started)| *started);
        for (key, _) in oldest.into_iter().take(target) {
            self.windows.remove(&key);
        }
    }

    /// Drop windows that have rolled over, so keys seen once do not
    /// accumulate for the life of the process.
    pub fn prune(&self) {
        let now_ms = self.clock.now().timestamp_millis();
        let window_ms = self.window.as_millis() as i64;
        self.windows
            .retain(|_, w| now_ms - w.started_at_ms < window_ms * 2);
    }

    /// How many keys are being tracked. For `/health`.
    pub fn tracked_keys(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_ports::FixedClock;
    use chrono::{DateTime, Duration as ChronoDuration, Utc};

    fn limiter(per_session: u32, per_ip: u32) -> (FixedClock, RateLimiter) {
        let clock = FixedClock::new(
            DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let limiter = RateLimiter::new(Arc::new(clock.clone()), per_session, per_ip);
        (clock, limiter)
    }

    #[test]
    fn requests_inside_the_limit_are_allowed() {
        let (_clock, limiter) = limiter(3, 100);
        for _ in 0..3 {
            assert!(limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        }
    }

    #[test]
    fn the_session_limit_denies_and_says_when_to_retry() {
        let (_clock, limiter) = limiter(2, 100);
        limiter.check(Some("sess_a"), "1.2.3.4");
        limiter.check(Some("sess_a"), "1.2.3.4");
        match limiter.check(Some("sess_a"), "1.2.3.4") {
            Decision::Deny {
                retry_after_seconds,
            } => assert!((1..=60).contains(&retry_after_seconds)),
            Decision::Allow => panic!("the third request must be denied"),
        }
    }

    #[test]
    fn the_address_limit_catches_a_caller_who_keeps_making_new_sessions() {
        let (_clock, limiter) = limiter(1_000, 2);
        assert!(limiter.check(Some("sess_1"), "1.2.3.4").is_allowed());
        assert!(limiter.check(Some("sess_2"), "1.2.3.4").is_allowed());
        assert!(
            !limiter.check(Some("sess_3"), "1.2.3.4").is_allowed(),
            "a fresh session must not reset the address budget"
        );
    }

    #[test]
    fn one_callers_limit_does_not_affect_another() {
        let (_clock, limiter) = limiter(1, 1);
        assert!(limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        assert!(!limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        assert!(limiter.check(Some("sess_b"), "5.6.7.8").is_allowed());
    }

    #[test]
    fn the_window_rolls_over() {
        let (clock, limiter) = limiter(1, 1);
        assert!(limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        assert!(!limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        clock.advance(ChronoDuration::seconds(61));
        assert!(limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
    }

    #[test]
    fn an_unauthenticated_caller_is_still_limited_by_address() {
        let (_clock, limiter) = limiter(1, 2);
        assert!(limiter.check(None, "1.2.3.4").is_allowed());
        assert!(limiter.check(None, "1.2.3.4").is_allowed());
        assert!(!limiter.check(None, "1.2.3.4").is_allowed());
    }

    #[test]
    fn a_limit_of_zero_disables_that_key_space() {
        let (_clock, limiter) = limiter(0, 0);
        for _ in 0..1_000 {
            assert!(limiter.check(Some("sess_a"), "1.2.3.4").is_allowed());
        }
    }

    #[test]
    fn the_key_map_is_bounded_even_under_a_flood_of_new_keys() {
        // Finding 3's sub-finding: every distinct key the caller invents used
        // to persist until the 300 s janitor, so the control meant to bound
        // abuse amplified it instead.
        let clock = FixedClock::new(
            DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let limiter = RateLimiter::with_capacity(Arc::new(clock.clone()), 0, 10, 64);
        for i in 0..5_000 {
            limiter.check(None, &format!("198.51.100.{i}"));
            assert!(
                limiter.tracked_keys() <= 64,
                "the map grew past its ceiling at key {i}: {}",
                limiter.tracked_keys()
            );
        }
        assert!(limiter.tracked_keys() > 0, "it still tracks recent callers");
    }

    #[test]
    fn a_caller_at_the_ceiling_is_still_tracked_rather_than_locked_out() {
        let (_clock, limiter) = limiter(0, 2);
        let limiter = RateLimiter {
            max_keys: 4,
            ..limiter
        };
        for i in 0..100 {
            limiter.check(None, &format!("198.51.100.{i}"));
        }
        // A fresh caller arriving after the flood gets its own budget.
        assert!(limiter.check(None, "203.0.113.1").is_allowed());
        assert!(limiter.check(None, "203.0.113.1").is_allowed());
        assert!(!limiter.check(None, "203.0.113.1").is_allowed());
    }

    #[test]
    fn stale_windows_are_pruned() {
        let (clock, limiter) = limiter(10, 10);
        limiter.check(Some("sess_a"), "1.2.3.4");
        assert_eq!(limiter.tracked_keys(), 2);
        clock.advance(ChronoDuration::seconds(300));
        limiter.prune();
        assert_eq!(limiter.tracked_keys(), 0);
    }
}
