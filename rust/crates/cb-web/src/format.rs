//! Number, time and identifier formatting.
//!
//! Why this module exists at all: a trading interface is judged on whether two
//! renderings of the same quantity are visibly the same quantity. The design
//! system fixes that by rule — trailing zeros are never trimmed, every P&L
//! carries an explicit sign, prices are grouped and right-aligned so the decimal
//! point forms a column — and those rules only hold if exactly one place in the
//! codebase implements them.
//!
//! Every function here takes [`Decimal`]. **No money value in this crate is ever
//! an `f64`.** The gateway sends decimal strings precisely so that the binary
//! floating-point round trip never happens; parsing one into an `f64` here would
//! throw that away silently, which is the single worst class of bug an exchange
//! front end can ship. The only permitted `Decimal`-to-`f64` conversion in the
//! whole crate lives in the canvas geometry code, is documented there, and never
//! flows back into anything the user reads as a number.
//!
//! This module deliberately has no Leptos and no `web_sys` imports, so it is
//! testable on the host as well as under `wasm-bindgen-test`.

use rust_decimal::Decimal;

/// U+2212 MINUS SIGN. A hyphen is not a minus: it is narrower than the digits
/// beside it, which breaks the tabular column the design system depends on.
pub const MINUS: char = '\u{2212}';

/// U+2026 HORIZONTAL ELLIPSIS, used in truncated hashes and addresses.
pub const ELLIPSIS: char = '\u{2026}';

/// The up glyph. Direction is never carried by colour alone.
pub const UP: char = '\u{25B2}';

/// The down glyph. Direction is never carried by colour alone.
pub const DOWN: char = '\u{25BC}';

/// Groups the integer part of an already-formatted magnitude with commas.
fn group_int(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let n = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Splits a `Decimal` rendered at `dp` places into `(is_negative, integer, fraction)`.
fn parts(value: Decimal, dp: u32) -> (bool, String, String) {
    let rendered = format!("{:.*}", dp as usize, value);
    let negative = rendered.starts_with('-');
    let magnitude = rendered.trim_start_matches('-');
    match magnitude.split_once('.') {
        Some((int, frac)) => (negative, int.to_string(), frac.to_string()),
        None => (negative, magnitude.to_string(), String::new()),
    }
}

/// Renders a quantity at a fixed number of decimal places, thousands separated,
/// with a leading minus sign only when the value is negative.
///
/// Trailing zeros are kept: the gateway sends `"64250.50"` and the interface
/// shows `64,250.50`, because a price that drops a digit reads as a different
/// price.
#[must_use]
pub fn fixed(value: Decimal, dp: u32) -> String {
    let (negative, int, frac) = parts(value, dp);
    let mut out = String::new();
    if negative {
        out.push(MINUS);
    }
    out.push_str(&group_int(&int));
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// Renders a quantity with an **explicit** sign, positive or negative.
///
/// Used for every profit-and-loss figure and every percentage change. The `+` is
/// rendered, never implied, because sign is the redundant encoding that lets the
/// figure be read without relying on jade and vermilion.
#[must_use]
pub fn signed(value: Decimal, dp: u32) -> String {
    let (negative, int, frac) = parts(value, dp);
    let mut out = String::new();
    // A zero has no direction, so it carries no sign — and no `\u{2212}0.00`
    // either, which is what a small negative rounding to nothing used to print
    // on a row whose own label read `+0.03%`. §3 makes the sign a redundant
    // encoding of direction; there is no direction to encode here.
    let zero = int.chars().all(|ch| ch == '0' || ch == ',') && frac.chars().all(|ch| ch == '0');
    if !zero {
        out.push(if negative { MINUS } else { '+' });
    }
    out.push_str(&group_int(&int));
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// A price, at the market's own `price_decimals`.
#[must_use]
pub fn price(value: Decimal, price_decimals: u32) -> String {
    fixed(value, price_decimals)
}

/// A size, at the market's own `size_decimals`.
#[must_use]
pub fn size(value: Decimal, size_decimals: u32) -> String {
    fixed(value, size_decimals)
}

/// A profit-and-loss amount: explicit sign, two decimals.
#[must_use]
pub fn pnl(value: Decimal) -> String {
    signed(value, 2)
}

/// A percentage: explicit sign, two decimals, with the sign glyph attached.
///
/// The argument is already a percentage, matching the contract's
/// `change_24h_pct: "2.41"`.
#[must_use]
pub fn pct(value: Decimal) -> String {
    format!("{}%", signed(value, 2))
}

/// A funding rate, which crosses the wire as a fraction per interval
/// (`"0.000125"`) and is read as a percentage per interval (`0.0125%`).
#[must_use]
pub fn funding_pct(rate: Decimal) -> String {
    format!("{}%", signed(rate * Decimal::from(100), 4))
}

/// A large notional: abbreviated above a million, to one decimal.
///
/// `128450000.00` becomes `128.4M`. Below a million nothing is abbreviated,
/// because the exact figure still fits and an abbreviation would lose it.
#[must_use]
pub fn notional(value: Decimal) -> String {
    let million = Decimal::from(1_000_000);
    let billion = Decimal::from(1_000_000_000);
    let magnitude = value.abs();
    let negative = value < Decimal::ZERO;
    let (scaled, suffix) = if magnitude >= billion {
        (value / billion, "B")
    } else if magnitude >= million {
        (value / million, "M")
    } else {
        return fixed(value, 2);
    };
    let _ = negative;
    format!("{}{suffix}", fixed(scaled, 1))
}

/// An integer count, thousands separated and never abbreviated.
///
/// Block heights use this: `184,203`, not `184.2k`. A block height that has been
/// rounded is not a block height.
#[must_use]
pub fn count(value: u64) -> String {
    group_int(&value.to_string())
}

/// Truncates an address or a transaction hash to its first six and last four
/// characters, joined by an ellipsis: `0x8f3b…c21d`.
///
/// Short identifiers are returned untouched rather than padded, so the function
/// never invents characters that are not in the value.
#[must_use]
pub fn hash(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 12 {
        return value.to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}{ELLIPSIS}{tail}")
}

/// The direction glyph for a signed quantity, or `None` when it is exactly zero.
#[must_use]
pub fn direction_glyph(value: Decimal) -> Option<char> {
    if value.is_zero() {
        None
    } else if value > Decimal::ZERO {
        Some(UP)
    } else {
        Some(DOWN)
    }
}

/// Renders a whole number of seconds as `mm:ss`, or `h:mm:ss` beyond an hour.
///
/// Used by the faucet cooldown and the funding countdown. Negative inputs clamp
/// to zero, because a countdown that has run out reads `00:00`, not a negative.
#[must_use]
pub fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

/// Renders an age in whole and fractional seconds the way the chain screen does:
/// `just now`, `0.4s`, `12s`, `4m`, `2h`.
#[must_use]
pub fn age(millis: i64) -> String {
    let ms = millis.max(0);
    match ms {
        0..=99 => "just now".to_string(),
        100..=999 => format!("{}.{}s", ms / 1000, (ms % 1000) / 100),
        1_000..=59_999 => format!("{}s", ms / 1000),
        60_000..=3_599_999 => format!("{}m", ms / 60_000),
        _ => format!("{}h", ms / 3_600_000),
    }
}

/// Parses an RFC 3339 UTC timestamp into whole seconds since the Unix epoch.
///
/// The gateway sends `"2026-09-18T12:00:00Z"`. Only the UTC form the contract
/// specifies is accepted; a fractional part is read to millisecond precision and
/// anything else returns `None` rather than guessing. A dedicated date-time
/// crate would cost tens of kilobytes of wasm to parse one fixed shape.
#[must_use]
pub fn parse_rfc3339(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }
    let num = |from: usize, to: usize| value.get(from..to)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s)
}

/// Howard Hinnant's `days_from_civil`: civil date to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`]: days since the epoch to a civil date.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Renders epoch seconds as a UTC clock time, `12:41:19`.
#[must_use]
pub fn clock(epoch_seconds: i64) -> String {
    let secs = epoch_seconds.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Renders epoch seconds as a UTC clock time without seconds, `12:41`.
#[must_use]
pub fn clock_hm(epoch_seconds: i64) -> String {
    let secs = epoch_seconds.rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

/// Renders epoch seconds as a short calendar date and time, `19 Sep, 12:00 UTC`.
#[must_use]
pub fn calendar(epoch_seconds: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = epoch_seconds.div_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    let name = MONTHS
        .get((month - 1).clamp(0, 11) as usize)
        .copied()
        .unwrap_or("Jan");
    format!("{day} {name}, {} UTC", clock_hm(epoch_seconds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap_or_default()
    }

    #[test]
    fn trailing_zeros_are_never_trimmed() {
        assert_eq!(fixed(d("64250.50"), 2), "64,250.50");
        assert_eq!(fixed(d("1.25"), 4), "1.2500");
        assert_eq!(fixed(d("100000"), 2), "100,000.00");
    }

    #[test]
    fn negatives_use_a_minus_sign_not_a_hyphen() {
        assert_eq!(fixed(d("-4.82"), 2), "\u{2212}4.82");
        assert!(!fixed(d("-4.82"), 2).contains('-'));
    }

    #[test]
    fn profit_and_loss_carries_its_sign_when_it_has_one() {
        assert_eq!(pnl(d("1872.15")), "+1,872.15");
        assert_eq!(pnl(d("-2500")), "\u{2212}2,500.00");
    }

    /// A zero is not a direction, so it is neither a gain nor a loss of
    /// nothing. Sixteen markets that had never traded each read `+0.00` in
    /// jade, which says every one of them is up.
    #[test]
    fn a_zero_carries_no_sign() {
        assert_eq!(pnl(Decimal::ZERO), "0.00");
        assert_eq!(pnl(d("-0.001")), "0.00", "and no negative zero either");
        assert_eq!(pct(Decimal::ZERO), "0.00%");
        assert_eq!(signed(Decimal::ZERO, 4), "0.0000");
        assert_eq!(funding_pct(Decimal::ZERO), "0.0000%");
    }

    #[test]
    fn percentages_and_funding_match_the_reference() {
        assert_eq!(pct(d("2.41")), "+2.41%");
        assert_eq!(pct(d("-3.42")), "\u{2212}3.42%");
        assert_eq!(funding_pct(d("0.000125")), "+0.0125%");
    }

    #[test]
    fn grouping_handles_every_boundary() {
        assert_eq!(count(184_203), "184,203");
        assert_eq!(count(9_128_401), "9,128,401");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(0), "0");
    }

    #[test]
    fn large_notionals_abbreviate_only_above_a_million() {
        assert_eq!(notional(d("128450000.00")), "128.4M");
        assert_eq!(notional(d("999999.00")), "999,999.00");
        assert_eq!(notional(d("2840000")), "2.8M");
    }

    #[test]
    fn hashes_show_six_and_four() {
        assert_eq!(hash("0x8f3bc4d19a77c21d"), "0x8f3b\u{2026}c21d");
        assert_eq!(hash("0x8f3b"), "0x8f3b");
    }

    #[test]
    fn timestamps_round_trip_through_the_epoch() {
        let t = parse_rfc3339("2026-09-18T12:41:19Z").unwrap_or(0);
        assert_eq!(clock(t), "12:41:19");
        assert_eq!(calendar(t), "18 Sep, 12:41 UTC");
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("not a timestamp"), None);
    }

    #[test]
    fn durations_and_ages_read_the_way_the_screens_do() {
        assert_eq!(duration(1122), "18:42");
        assert_eq!(duration(-5), "00:00");
        assert_eq!(duration(3661), "1:01:01");
        assert_eq!(age(40), "just now");
        assert_eq!(age(400), "0.4s");
        assert_eq!(age(12_000), "12s");
    }
}
