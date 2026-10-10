//! Trading-session policy for equity markets.
//!
//! # The decision: equities trade around the clock
//!
//! [`EQUITY_TRADING_POLICY`] is [`TradingPolicy::AroundTheClock`], and that is a
//! deliberate product decision, not an oversight.
//!
//! SuperFrontierLiquid is an exchange **simulator**. Its equity markets are
//! cash-settled synthetics priced off a mocked reference feed — no share is ever
//! custodied, no transfer agent is involved, and no venue is closed because
//! nobody is there to clear. The reasons a real equity market keeps a session
//! calendar (settlement windows, market-maker obligations, auction mechanics,
//! human oversight) do not apply to any of this. What a calendar *would* do is
//! make four of the seven listed underlyings untradable for sixteen hours a day
//! plus weekends, which in a toy exchange people join to play with is simply a
//! worse toy.
//!
//! The crypto markets have no session concept at all, so a calendar would also
//! split the venue into two classes of market with different availability — more
//! surface area, for no simulation fidelity gained.
//!
//! # Flipping the decision
//!
//! The policy is a decision, so it is represented as one. [`SessionCalendar`] is
//! implemented, tested, and exported; [`SessionCalendar::US_EQUITY_REGULAR_HOURS`]
//! describes a conventional 09:30–16:00 weekday session. To switch policy, change
//! [`EQUITY_TRADING_POLICY`] to [`TradingPolicy::SessionCalendar`] and the
//! [`is_tradable`] helper starts refusing out-of-session timestamps. Nothing else
//! needs to move.
//!
//! # Determinism: this module is *not* on the consensus path
//!
//! **No state-transition code may call anything in this module.** A block's
//! result must be a pure function of its inputs (`docs/PLAN.md` §6, invariant 8);
//! a handler that consulted the wall clock would let two replicas replaying the
//! same block reach different state roots, and consensus could never converge.
//!
//! That is why every function here takes an explicit `unix_ts: i64` rather than
//! reading a clock, and why the policy is enforced at the *gateway* — reject the
//! order before it is sequenced — rather than in the executor. If a session
//! policy ever needs to be consensus-enforced, the gate has to key off block
//! height or an on-chain timestamp, not off this module.
//!
//! # Fidelity limits, stated plainly
//!
//! [`SessionCalendar::US_EQUITY_REGULAR_HOURS`] models a fixed UTC window. It
//! does **not** model daylight saving, exchange holidays, half-days, or
//! auction periods. Encoding those would mean shipping a timezone and holiday
//! database for a policy that is switched off by default. If the policy is ever
//! turned on for something that matters, that data is the first thing to add.

use crate::ids::{AssetClass, AssetId};

/// Seconds in a day.
const SECS_PER_DAY: i64 = 86_400;

/// Which availability rule applies to a class of market.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TradingPolicy {
    /// The market never closes. The default for every market on this chain.
    AroundTheClock,
    /// The market is open only during the sessions of a [`SessionCalendar`].
    SessionCalendar(SessionCalendar),
}

/// The policy applied to equity markets. See the module docs for the reasoning.
///
/// Crypto markets are always [`TradingPolicy::AroundTheClock`] and have no
/// switch; a 24/7 crypto venue is not a decision anyone needs to revisit.
pub const EQUITY_TRADING_POLICY: TradingPolicy = TradingPolicy::AroundTheClock;

/// A weekly trading session: an open/close time-of-day plus the days it applies.
///
/// Times are seconds from UTC midnight. Days are a bitmask with bit 0 = Sunday
/// through bit 6 = Saturday, which is the same numbering [`weekday`] returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionCalendar {
    /// Session open, in seconds after UTC midnight. Inclusive.
    pub open_secs_utc: i64,
    /// Session close, in seconds after UTC midnight. Exclusive.
    pub close_secs_utc: i64,
    /// Bitmask of weekdays the session runs on; bit 0 = Sunday.
    pub weekday_mask: u8,
}

/// Monday through Friday: bits 1..=5 set, Sunday (bit 0) and Saturday (bit 6) clear.
const WEEKDAYS: u8 = 0b0011_1110;

impl SessionCalendar {
    /// A conventional US equity regular session: 09:30–16:00 US Eastern,
    /// Monday to Friday, expressed here as 13:30–20:00 UTC.
    ///
    /// **That UTC window is Eastern *daylight* time (UTC-4).** A fixed-offset
    /// calendar cannot be right all year: for the roughly four months the US is
    /// on standard time (UTC-5) this window sits an hour earlier than the real
    /// session, opening at 08:30 Eastern rather than 09:30. Daylight saving,
    /// exchange holidays and half-days are **not** modelled — see the module
    /// docs. This constant exists so the policy can be flipped and exercised,
    /// not so it can be relied on for anything real.
    pub const US_EQUITY_REGULAR_HOURS: SessionCalendar = SessionCalendar {
        open_secs_utc: 13 * 3_600 + 30 * 60,
        close_secs_utc: 20 * 3_600,
        weekday_mask: WEEKDAYS,
    };

    /// Is the session open at this UTC timestamp?
    ///
    /// Takes the timestamp explicitly; this module never reads a clock. See the
    /// determinism note in the module docs.
    pub fn is_open(&self, unix_ts: i64) -> bool {
        let dow = weekday(unix_ts);
        if self.weekday_mask & (1 << dow) == 0 {
            return false;
        }
        let secs = unix_ts.rem_euclid(SECS_PER_DAY);
        secs >= self.open_secs_utc && secs < self.close_secs_utc
    }
}

/// Day of week for a Unix timestamp: 0 = Sunday … 6 = Saturday.
///
/// 1970-01-01 was a Thursday, hence the `+ 4`. Uses `div_euclid` so timestamps
/// before the epoch floor correctly instead of truncating toward zero.
pub fn weekday(unix_ts: i64) -> u8 {
    let days = unix_ts.div_euclid(SECS_PER_DAY);
    (days + 4).rem_euclid(7) as u8
}

/// Is an asset tradable at this timestamp under the configured policy?
///
/// Non-equity assets are always tradable. Equities follow
/// [`EQUITY_TRADING_POLICY`], which is currently
/// [`TradingPolicy::AroundTheClock`], so this returns `true` for everything —
/// by design, and the `equities_trade_around_the_clock_under_current_policy`
/// test pins that so flipping the policy is a visible, deliberate change rather
/// than a silent one.
///
/// **Call this from the gateway, never from a transaction handler.**
pub fn is_tradable(asset: AssetId, unix_ts: i64) -> bool {
    if asset.class() != AssetClass::Equity {
        return true;
    }
    match EQUITY_TRADING_POLICY {
        TradingPolicy::AroundTheClock => true,
        TradingPolicy::SessionCalendar(cal) => cal.is_open(unix_ts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-06-24 at 14:00 UTC. All these tests need from it is that it is a
    /// Wednesday — `weekday_matches_known_dates` pins the weekday arithmetic
    /// itself, and the session-boundary tests derive midnight from this value
    /// rather than assuming any particular local time.
    const WED_1400_UTC: i64 = 1_782_309_600;

    #[test]
    fn weekday_matches_known_dates() {
        // 1970-01-01, a Thursday.
        assert_eq!(weekday(0), 4);
        // 1970-01-04, a Sunday.
        assert_eq!(weekday(3 * SECS_PER_DAY), 0);
        // 1969-12-31, a Wednesday — before the epoch, so `div_euclid` matters.
        assert_eq!(weekday(-1), 3);
    }

    #[test]
    fn weekday_is_stable_across_a_full_week() {
        let mut seen = [false; 7];
        for d in 0..7 {
            seen[weekday(d * SECS_PER_DAY) as usize] = true;
        }
        assert!(
            seen.iter().all(|x| *x),
            "seven days must cover every weekday"
        );
    }

    // ---------------- the policy that is actually in force ----------------

    #[test]
    fn equities_trade_around_the_clock_under_current_policy() {
        // This is the decision. If someone flips `EQUITY_TRADING_POLICY`, this
        // test fails and they have to acknowledge it — which is the point.
        assert_eq!(EQUITY_TRADING_POLICY, TradingPolicy::AroundTheClock);

        // 3am UTC on a Sunday: no equity venue anywhere is open, and we trade.
        let sunday_0300 = 3 * SECS_PER_DAY + 3 * 3_600;
        assert_eq!(weekday(sunday_0300), 0);
        for a in AssetId::ALL {
            assert!(
                is_tradable(a, sunday_0300),
                "{a} must be tradable at all times under the current policy"
            );
        }
    }

    #[test]
    fn non_equities_are_unconditionally_tradable() {
        // True regardless of which policy is configured.
        for a in [AssetId::Btc, AssetId::Eth, AssetId::Usd, AssetId::Sol] {
            assert!(is_tradable(a, 0));
            assert!(is_tradable(a, WED_1400_UTC));
        }
    }

    // ---------------- the calendar, so flipping the policy is safe ----------------

    #[test]
    fn regular_hours_calendar_opens_and_closes_on_a_weekday() {
        let cal = SessionCalendar::US_EQUITY_REGULAR_HOURS;
        let midnight = WED_1400_UTC - 14 * 3_600;
        assert_eq!(weekday(midnight), 3, "test fixture should be a Wednesday");

        assert!(
            !cal.is_open(midnight + 13 * 3_600 + 29 * 60),
            "13:29 is pre-open"
        );
        assert!(
            cal.is_open(midnight + 13 * 3_600 + 30 * 60),
            "13:30 is the open"
        );
        assert!(cal.is_open(midnight + 14 * 3_600), "mid-session");
        assert!(
            cal.is_open(midnight + 19 * 3_600 + 59 * 60),
            "19:59 is still open"
        );
        assert!(
            !cal.is_open(midnight + 20 * 3_600),
            "20:00 is the close, exclusive"
        );
        assert!(!cal.is_open(midnight + 22 * 3_600), "after hours");
    }

    #[test]
    fn regular_hours_calendar_is_shut_at_weekends() {
        let cal = SessionCalendar::US_EQUITY_REGULAR_HOURS;
        let midnight = WED_1400_UTC - 14 * 3_600;
        // Wednesday + 3 days = Saturday, + 4 = Sunday.
        let saturday = midnight + 3 * SECS_PER_DAY + 14 * 3_600;
        let sunday = midnight + 4 * SECS_PER_DAY + 14 * 3_600;
        assert_eq!(weekday(saturday), 6);
        assert_eq!(weekday(sunday), 0);
        assert!(!cal.is_open(saturday));
        assert!(!cal.is_open(sunday));
    }

    #[test]
    fn weekday_mask_covers_monday_through_friday_only() {
        let mask = SessionCalendar::US_EQUITY_REGULAR_HOURS.weekday_mask;
        assert_eq!(mask & 1, 0, "Sunday must be excluded");
        assert_eq!(mask & (1 << 6), 0, "Saturday must be excluded");
        for d in 1..=5 {
            assert_ne!(mask & (1 << d), 0, "weekday {d} must be included");
        }
    }

    #[test]
    fn session_policy_would_gate_equities_if_enabled() {
        // Exercise the branch the default policy does not take, so the
        // alternative is known to work the day someone flips the constant.
        let policy = TradingPolicy::SessionCalendar(SessionCalendar::US_EQUITY_REGULAR_HOURS);
        let gated = |asset: AssetId, ts: i64| -> bool {
            if asset.class() != AssetClass::Equity {
                return true;
            }
            match policy {
                TradingPolicy::AroundTheClock => true,
                TradingPolicy::SessionCalendar(cal) => cal.is_open(ts),
            }
        };
        let midnight = WED_1400_UTC - 14 * 3_600;
        assert!(gated(AssetId::Aapl, midnight + 14 * 3_600), "in session");
        assert!(
            !gated(AssetId::Aapl, midnight + 2 * 3_600),
            "out of session"
        );
        assert!(
            gated(AssetId::Btc, midnight + 2 * 3_600),
            "crypto is never gated"
        );
    }
}
