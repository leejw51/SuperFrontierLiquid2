//! A US equity trading calendar.
//!
//! `is_open` has to mean something, or an equity instrument in this simulator
//! is just a crypto instrument with a different name. That requires two facts
//! nobody can derive from a price: the exchange session, and the timezone it
//! is expressed in — including daylight saving, which moves the session
//! relative to UTC twice a year.
//!
//! # Why the timezone rules are hand-rolled
//!
//! The obvious tool is `chrono-tz`, and it is not in the workspace. Adding a
//! timezone database — several hundred kilobytes of IANA data, updated on its
//! own schedule — to model one exchange's hours is a poor trade when the rule
//! in question has been stable US federal law since 2007 and fits in twenty
//! lines: daylight time runs from the second Sunday in March to the first
//! Sunday in November.
//!
//! # What is deliberately not modelled
//!
//! **Market holidays.** Thanksgiving, Independence Day and the rest are a
//! maintained list that drifts every year, and a stale list is worse than no
//! list because it is wrong in a way nobody notices. A simulator that thinks
//! the market is open on Christmas is a smaller problem than one whose
//! calendar silently rots. **Early closes** (the 13:00 sessions around some
//! holidays) are excluded for the same reason. **Pre- and post-market
//! sessions** are excluded because this project's equity instruments are
//! cash-settled derivatives, and extended-hours liquidity is not something a
//! mocked reference price can represent honestly.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc, Weekday};

/// Regular session open, in minutes from Eastern midnight (09:30).
const SESSION_OPEN_MINUTES: u32 = 9 * 60 + 30;
/// Regular session close, in minutes from Eastern midnight (16:00).
const SESSION_CLOSE_MINUTES: u32 = 16 * 60;

/// UTC offset during Eastern Standard Time.
const EST_OFFSET_HOURS: i64 = -5;
/// UTC offset during Eastern Daylight Time.
const EDT_OFFSET_HOURS: i64 = -4;

/// Whether the US regular equity session is running at `at`.
///
/// Monday to Friday, 09:30 to 16:00 Eastern, close exclusive: an order at
/// exactly 16:00:00 is after the bell.
pub fn is_us_equity_session_open(at: DateTime<Utc>) -> bool {
    let local = to_eastern_naive(at);

    if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }

    let minutes = local.hour() * 60 + local.minute();
    (SESSION_OPEN_MINUTES..SESSION_CLOSE_MINUTES).contains(&minutes)
}

/// Whether `at` falls in Eastern Daylight Time.
///
/// Daylight time begins at 02:00 local standard on the second Sunday in March
/// — 07:00 UTC — and ends at 02:00 local daylight on the first Sunday in
/// November, which is 06:00 UTC.
pub fn is_eastern_daylight(at: DateTime<Utc>) -> bool {
    let year = at.year();

    let Some(start) = nth_weekday_of_month(year, 3, Weekday::Sun, 2)
        .and_then(|d| d.and_hms_opt(7, 0, 0))
        .map(|dt| Utc.from_utc_datetime(&dt))
    else {
        // Unreachable for any year chrono can represent; falling back to
        // standard time is the conservative answer rather than a panic.
        return false;
    };
    let Some(end) = nth_weekday_of_month(year, 11, Weekday::Sun, 1)
        .and_then(|d| d.and_hms_opt(6, 0, 0))
        .map(|dt| Utc.from_utc_datetime(&dt))
    else {
        return false;
    };

    at >= start && at < end
}

/// `at` expressed as naive Eastern wall-clock time.
fn to_eastern_naive(at: DateTime<Utc>) -> chrono::NaiveDateTime {
    let offset = if is_eastern_daylight(at) {
        EDT_OFFSET_HOURS
    } else {
        EST_OFFSET_HOURS
    };
    (at + chrono::Duration::hours(offset)).naive_utc()
}

/// The `n`th `weekday` of a month, 1-based.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let shift = (weekday.num_days_from_sunday() + 7 - first.weekday().num_days_from_sunday()) % 7;
    let day = 1 + shift + (n - 1) * 7;
    NaiveDate::from_ymd_opt(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("test literals are valid RFC 3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn daylight_saving_boundaries_land_on_the_right_sundays() {
        assert_eq!(
            nth_weekday_of_month(2026, 3, Weekday::Sun, 2),
            NaiveDate::from_ymd_opt(2026, 3, 8),
            "the second Sunday of March 2026"
        );
        assert_eq!(
            nth_weekday_of_month(2026, 11, Weekday::Sun, 1),
            NaiveDate::from_ymd_opt(2026, 11, 1),
            "the first Sunday of November 2026"
        );

        assert!(!is_eastern_daylight(t("2026-03-08T06:59:59Z")));
        assert!(is_eastern_daylight(t("2026-03-08T07:00:00Z")));
        assert!(is_eastern_daylight(t("2026-11-01T05:59:59Z")));
        assert!(!is_eastern_daylight(t("2026-11-01T06:00:00Z")));
    }

    /// The test that proves the offset is applied and not merely computed: the
    /// same UTC instant is outside the session in winter and inside it in
    /// summer would be the bug, so pin both halves of the year.
    #[test]
    fn the_session_moves_with_daylight_saving() {
        // January, EST (-5): 14:00Z is 09:00 local, half an hour early.
        assert!(!is_us_equity_session_open(t("2026-01-14T14:00:00Z")));
        assert!(is_us_equity_session_open(t("2026-01-14T14:30:00Z")));
        assert!(is_us_equity_session_open(t("2026-01-14T20:59:59Z")));
        assert!(!is_us_equity_session_open(t("2026-01-14T21:00:00Z")));

        // September, EDT (-4): the same wall-clock session is an hour earlier
        // in UTC.
        assert!(!is_us_equity_session_open(t("2026-09-16T13:29:59Z")));
        assert!(is_us_equity_session_open(t("2026-09-16T13:30:00Z")));
        assert!(is_us_equity_session_open(t("2026-09-16T19:59:59Z")));
        assert!(!is_us_equity_session_open(t("2026-09-16T20:00:00Z")));
    }
}
