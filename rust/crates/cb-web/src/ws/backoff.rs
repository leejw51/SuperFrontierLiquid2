//! Reconnection timing.
//!
//! Two constraints pull against each other. A trading interface that is offline
//! is dangerous, so the first retries have to be fast. A gateway that has just
//! fallen over must not be hit by every open tab at the same instant, so the
//! delay has to grow and it has to be jittered.
//!
//! The answer is ordinary exponential backoff from half a second to a thirty
//! second ceiling, with the delay spread by a fifth either way. The jitter is
//! supplied by the caller rather than drawn here, so the schedule is a pure
//! function and can be asserted in a test.

/// The first delay, in milliseconds.
pub const BASE_MS: u32 = 500;

/// The ceiling, in milliseconds. Beyond this, waiting longer helps nobody.
pub const CAP_MS: u32 = 30_000;

/// An exponential backoff schedule with a bounded ceiling.
#[derive(Debug, Clone, Copy, Default)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// A schedule that has not yet retried.
    #[must_use]
    pub const fn new() -> Self {
        Self { attempt: 0 }
    }

    /// How many reconnection attempts have been made.
    ///
    /// The connection indicator shows this: *Reconnecting · attempt 3*. A
    /// spinner with no number tells the user nothing about whether to wait.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Clears the schedule. Called the moment a connection succeeds.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// The unjittered delay for the next attempt.
    #[must_use]
    pub const fn plain_delay_ms(&self) -> u32 {
        let shift = if self.attempt > 6 { 6 } else { self.attempt };
        let delay = BASE_MS.saturating_mul(1 << shift);
        if delay > CAP_MS {
            CAP_MS
        } else {
            delay
        }
    }

    /// Advances the schedule and returns the delay to wait, in milliseconds.
    ///
    /// `jitter_permille` is a caller-supplied value in `0..1000`, which spreads
    /// the delay by up to a fifth either way. Passing 500 gives the plain
    /// exponential delay, which is what the tests assert against.
    pub fn next_delay_ms(&mut self, jitter_permille: u32) -> u32 {
        let plain = self.plain_delay_ms();
        self.attempt = self.attempt.saturating_add(1);
        // Map 0..=999 onto -200..=+199 permille of the plain delay.
        let offset = i64::from(jitter_permille.min(999)) - 500;
        let spread = i64::from(plain) * offset * 2 / 5_000;
        let jittered = i64::from(plain) + spread;
        jittered.clamp(100, i64::from(CAP_MS)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_retry_is_fast_and_the_schedule_doubles() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay_ms(500), 500);
        assert_eq!(backoff.next_delay_ms(500), 1_000);
        assert_eq!(backoff.next_delay_ms(500), 2_000);
        assert_eq!(backoff.next_delay_ms(500), 4_000);
        assert_eq!(backoff.attempt(), 4);
    }

    #[test]
    fn the_schedule_stops_growing_at_the_ceiling() {
        let mut backoff = Backoff::new();
        for _ in 0..20 {
            let delay = backoff.next_delay_ms(500);
            assert!(delay <= CAP_MS, "{delay} exceeded the ceiling");
        }
        assert_eq!(backoff.next_delay_ms(500), CAP_MS);
    }

    #[test]
    fn jitter_spreads_the_delay_by_a_fifth_either_way() {
        let mut low = Backoff::new();
        let mut high = Backoff::new();
        // One attempt in, the plain delay is 1000ms.
        low.next_delay_ms(500);
        high.next_delay_ms(500);
        assert_eq!(low.next_delay_ms(0), 800);
        assert_eq!(high.next_delay_ms(999), 1_199);
    }

    #[test]
    fn a_successful_connection_clears_the_schedule() {
        let mut backoff = Backoff::new();
        backoff.next_delay_ms(500);
        backoff.next_delay_ms(500);
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay_ms(500), BASE_MS);
    }

    #[test]
    fn a_delay_is_never_shorter_than_a_tenth_of_a_second() {
        let mut backoff = Backoff::new();
        assert!(backoff.next_delay_ms(0) >= 100);
    }
}
