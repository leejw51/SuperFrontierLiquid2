//! Wall-clock access, in one place.
//!
//! The interface needs "now" in several places — how old a price is, how long
//! until funding, how far a faucet cooldown has run — and the mock backend needs
//! it to generate a plausible stream. Reading it through this module rather than
//! calling `js_sys::Date` at each site means the pure-logic modules stay
//! testable on the host, where there is no `Date`.

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_millis() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        // `Date::now` returns an f64 of integral milliseconds. This is a clock
        // reading, not a monetary value; no price ever passes through here.
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    }
}

/// Whole seconds since the Unix epoch.
#[must_use]
pub fn now_seconds() -> i64 {
    now_millis().div_euclid(1_000)
}

/// Formats epoch seconds as the RFC 3339 UTC string the contract uses.
#[must_use]
pub fn rfc3339(epoch_seconds: i64) -> String {
    const MONTH_LENGTHS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let days = epoch_seconds.div_euclid(86_400);
    let secs = epoch_seconds.rem_euclid(86_400);
    let (mut year, mut remaining) = (1970_i64, days);
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if remaining < len {
            break;
        }
        remaining -= len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let mut month = 1;
    for (index, base) in MONTH_LENGTHS.iter().enumerate() {
        let len = base + i64::from(leap && index == 1);
        if remaining < len {
            break;
        }
        remaining -= len;
        month += 1;
    }
    format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
        remaining + 1,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::parse_rfc3339;

    #[test]
    fn rfc3339_round_trips_through_the_parser() {
        for epoch in [0_i64, 1_000_000_000, 1_789_000_000, 1_663_500_079] {
            let text = rfc3339(epoch);
            assert_eq!(parse_rfc3339(&text), Some(epoch), "{text}");
        }
    }

    #[test]
    fn a_leap_day_is_a_day() {
        // 2024-02-29T00:00:00Z
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
