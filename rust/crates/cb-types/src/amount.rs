//! Integer-amount helpers. All on-chain amounts are `u128` in the asset's base units.
//!
//! This module holds the *scale-agnostic* primitives: checked arithmetic, and
//! the decimal-string codec used to cross the boundary between what a human
//! types and what the chain stores. The asset-aware wrappers that know which
//! scale applies where live in [`crate::scales`].
//!
//! # Why the codec refuses to round
//!
//! [`parse_scaled`] rejects a string with more fractional digits than the target
//! scale can hold, instead of truncating or rounding it. That is the single most
//! important behaviour in this file. An input like `"1.12345678"` against a
//! 6-decimal asset is not a value the chain can represent; the only honest
//! answers are "reject" or "silently change the number the user typed". Every
//! silent-rounding bug in a trading system starts with a codec that chose the
//! second one.

use thiserror::Error;

/// Overflow / underflow on checked base-unit arithmetic.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AmountError {
    /// The sum exceeded `u128::MAX`.
    #[error("amount overflow")]
    Overflow,
    /// The difference went below zero.
    #[error("amount underflow")]
    Underflow,
}

/// Checked addition of two base-unit amounts.
pub fn checked_add(a: u128, b: u128) -> Result<u128, AmountError> {
    a.checked_add(b).ok_or(AmountError::Overflow)
}

/// Checked subtraction of two base-unit amounts.
pub fn checked_sub(a: u128, b: u128) -> Result<u128, AmountError> {
    a.checked_sub(b).ok_or(AmountError::Underflow)
}

/// Why a decimal string could not be converted to base units.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecimalError {
    /// The string was empty, or was just a sign or a bare `"."`.
    #[error("empty decimal string")]
    Empty,
    /// A character other than an ASCII digit or a single `.` appeared.
    #[error("invalid character `{0}` in decimal string")]
    InvalidChar(char),
    /// More than one decimal point.
    #[error("decimal string has more than one `.`")]
    MultiplePoints,
    /// A negative value. On-chain amounts are unsigned.
    #[error("decimal string must not be negative")]
    Negative,
    /// **The wrong-decimals guard.** The value has finer precision than the
    /// target scale can represent, so converting it would silently change it.
    #[error(
        "`{value}` has {given} decimal places but only {allowed} are representable at this scale"
    )]
    TooManyDecimals {
        /// The offending input, echoed back so the error message is actionable.
        value: String,
        /// How many fractional digits the caller supplied.
        given: usize,
        /// How many fractional digits the scale allows.
        allowed: u8,
    },
    /// The scaled value does not fit in a `u128`.
    #[error("decimal value `{0}` overflows u128 at this scale")]
    Overflow(String),
    /// The requested scale is beyond what `u128` can express (`10^39` and up).
    #[error("scale of 10^{0} is not representable in u128")]
    ScaleTooLarge(u8),
}

/// Largest `d` for which `10^d` fits in a `u128`. `10^38 < u128::MAX < 10^39`.
const MAX_REPRESENTABLE_DECIMALS: u8 = 38;

/// `10^decimals` as a `u128`.
///
/// Returns [`DecimalError::ScaleTooLarge`] rather than panicking for a scale no
/// `u128` can hold. Every asset listed today is far below the limit (ETH's 18 is
/// the largest), but this is a public entry point and a caller may pass a scale
/// derived from arithmetic.
pub fn pow10(decimals: u8) -> Result<u128, DecimalError> {
    if decimals > MAX_REPRESENTABLE_DECIMALS {
        return Err(DecimalError::ScaleTooLarge(decimals));
    }
    Ok(10u128.pow(decimals as u32))
}

/// Parse a decimal string into base units at the given scale.
///
/// `parse_scaled("1.5", 6) == Ok(1_500_000)`. Fewer fractional digits than the
/// scale are zero-padded; **more are rejected** — see the module docs.
///
/// Accepts an optional leading `+`, a digit sequence, and an optional `.` with a
/// digit sequence. Deliberately rejects everything else: no exponents, no
/// thousands separators, no whitespace, no locale-dependent forms. A trading
/// system's amount parser should have exactly one accepted grammar.
///
/// ```
/// use cb_types::amount::{parse_scaled, DecimalError};
/// assert_eq!(parse_scaled("1.5", 6).unwrap(), 1_500_000);
/// assert_eq!(parse_scaled("2", 6).unwrap(), 2_000_000);
/// assert_eq!(parse_scaled(".25", 6).unwrap(), 250_000);
/// // Finer than the scale can hold: rejected, never rounded.
/// assert!(matches!(
///     parse_scaled("1.1234567", 6),
///     Err(DecimalError::TooManyDecimals { .. })
/// ));
/// ```
pub fn parse_scaled(s: &str, decimals: u8) -> Result<u128, DecimalError> {
    let scale = pow10(decimals)?;

    let body = match s.strip_prefix('+') {
        Some(rest) => rest,
        None => {
            if s.starts_with('-') {
                // Reject `-0` too. "It's zero anyway" is how a sign bug hides.
                return Err(DecimalError::Negative);
            }
            s
        }
    };
    if body.is_empty() {
        return Err(DecimalError::Empty);
    }

    let mut parts = body.split('.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(DecimalError::MultiplePoints);
    }
    if int_part.is_empty() && frac_part.is_empty() {
        // A bare "." (or "+.").
        return Err(DecimalError::Empty);
    }
    for c in int_part.chars().chain(frac_part.chars()) {
        if !c.is_ascii_digit() {
            return Err(DecimalError::InvalidChar(c));
        }
    }

    if frac_part.len() > decimals as usize {
        // The guard. Note we compare the *written* digits, not the value: a
        // trailing-zero-only excess like "1.5000000" against 6 decimals is still
        // refused, because accepting it would mean deciding on the user's behalf
        // which extra digits are safe to drop.
        return Err(DecimalError::TooManyDecimals {
            value: s.to_string(),
            given: frac_part.len(),
            allowed: decimals,
        });
    }

    let whole: u128 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| DecimalError::Overflow(s.to_string()))?
    };
    let frac: u128 = if frac_part.is_empty() {
        0
    } else {
        frac_part
            .parse()
            .map_err(|_| DecimalError::Overflow(s.to_string()))?
    };
    // Pad the fraction out to the full scale: "1.5" at 6 decimals is 500000,
    // not 5.
    let frac_scaled = frac
        .checked_mul(pow10(decimals - frac_part.len() as u8)?)
        .ok_or_else(|| DecimalError::Overflow(s.to_string()))?;

    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(frac_scaled))
        .ok_or_else(|| DecimalError::Overflow(s.to_string()))
}

/// Render base units as a decimal string at the given scale — the exact inverse
/// of [`parse_scaled`].
///
/// Trailing zeros in the fraction are trimmed, and a whole value renders with no
/// decimal point at all (`format_scaled(2_000_000, 6) == "2"`). Both forms
/// re-parse to the identical integer, which is what
/// `round_trip_is_exact_for_every_asset` pins.
pub fn format_scaled(units: u128, decimals: u8) -> Result<String, DecimalError> {
    let scale = pow10(decimals)?;
    let whole = units / scale;
    let frac = units % scale;
    if frac == 0 {
        return Ok(whole.to_string());
    }
    let frac_str = format!("{frac:0width$}", width = decimals as usize);
    Ok(format!("{whole}.{}", frac_str.trim_end_matches('0')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AssetId;
    use proptest::prelude::*;

    #[test]
    fn add_overflow_returns_error() {
        assert_eq!(checked_add(u128::MAX, 1), Err(AmountError::Overflow));
    }

    #[test]
    fn sub_underflow_returns_error() {
        assert_eq!(checked_sub(0, 1), Err(AmountError::Underflow));
    }

    #[test]
    fn happy_path_works() {
        assert_eq!(checked_add(2, 3), Ok(5));
        assert_eq!(checked_sub(5, 3), Ok(2));
    }

    // ---------------- parse_scaled ----------------

    #[test]
    fn parses_whole_and_fractional_values() {
        assert_eq!(parse_scaled("0", 6).unwrap(), 0);
        assert_eq!(parse_scaled("1", 6).unwrap(), 1_000_000);
        assert_eq!(parse_scaled("1.5", 6).unwrap(), 1_500_000);
        assert_eq!(parse_scaled("1.000001", 6).unwrap(), 1_000_001);
        assert_eq!(parse_scaled("0.000001", 6).unwrap(), 1);
        assert_eq!(parse_scaled("+3.25", 6).unwrap(), 3_250_000);
        assert_eq!(parse_scaled(".5", 6).unwrap(), 500_000);
        assert_eq!(parse_scaled("7.", 6).unwrap(), 7_000_000);
    }

    #[test]
    fn short_fractions_are_zero_padded_not_left_shifted() {
        // The classic off-by-10^n: "1.5" must be 1_500_000, never 1_000_005.
        assert_eq!(parse_scaled("1.5", 6).unwrap(), 1_500_000);
        assert_eq!(parse_scaled("1.05", 6).unwrap(), 1_050_000);
        assert_eq!(parse_scaled("1.000005", 6).unwrap(), 1_000_005);
    }

    #[test]
    fn scale_zero_accepts_only_integers() {
        assert_eq!(parse_scaled("42", 0).unwrap(), 42);
        assert!(matches!(
            parse_scaled("42.5", 0),
            Err(DecimalError::TooManyDecimals { .. })
        ));
    }

    #[test]
    fn excess_precision_is_rejected_not_rounded() {
        let err = parse_scaled("1.1234567", 6).unwrap_err();
        match err {
            DecimalError::TooManyDecimals { given, allowed, .. } => {
                assert_eq!(given, 7);
                assert_eq!(allowed, 6);
            }
            other => panic!("expected TooManyDecimals, got {other:?}"),
        }
    }

    #[test]
    fn excess_precision_is_rejected_even_when_the_extra_digits_are_zeros() {
        // Tempting to allow — but then the parser is deciding which of the
        // user's digits are meaningful, and the next change makes it round.
        assert!(matches!(
            parse_scaled("1.5000000", 6),
            Err(DecimalError::TooManyDecimals { .. })
        ));
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert_eq!(parse_scaled("", 6).unwrap_err(), DecimalError::Empty);
        assert_eq!(parse_scaled(".", 6).unwrap_err(), DecimalError::Empty);
        assert_eq!(parse_scaled("+", 6).unwrap_err(), DecimalError::Empty);
        assert_eq!(parse_scaled("-1", 6).unwrap_err(), DecimalError::Negative);
        assert_eq!(parse_scaled("-0", 6).unwrap_err(), DecimalError::Negative);
        assert_eq!(
            parse_scaled("1.2.3", 6).unwrap_err(),
            DecimalError::MultiplePoints
        );
        assert_eq!(
            parse_scaled("1_000", 6).unwrap_err(),
            DecimalError::InvalidChar('_')
        );
        assert_eq!(
            parse_scaled("1,5", 6).unwrap_err(),
            DecimalError::InvalidChar(',')
        );
        assert_eq!(
            parse_scaled(" 1", 6).unwrap_err(),
            DecimalError::InvalidChar(' ')
        );
        assert_eq!(
            parse_scaled("1e6", 6).unwrap_err(),
            DecimalError::InvalidChar('e')
        );
        assert_eq!(
            parse_scaled("nan", 6).unwrap_err(),
            DecimalError::InvalidChar('n')
        );
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        let huge = "9".repeat(40);
        assert!(matches!(
            parse_scaled(&huge, 18),
            Err(DecimalError::Overflow(_))
        ));
    }

    #[test]
    fn scale_beyond_u128_is_rejected() {
        assert_eq!(pow10(39).unwrap_err(), DecimalError::ScaleTooLarge(39));
        assert!(pow10(38).is_ok());
    }

    // ---------------- format_scaled ----------------

    #[test]
    fn formats_base_units_back_to_decimal() {
        assert_eq!(format_scaled(0, 6).unwrap(), "0");
        assert_eq!(format_scaled(2_000_000, 6).unwrap(), "2");
        assert_eq!(format_scaled(1_500_000, 6).unwrap(), "1.5");
        assert_eq!(format_scaled(1, 6).unwrap(), "0.000001");
        assert_eq!(format_scaled(100_000_000, 8).unwrap(), "1");
        assert_eq!(format_scaled(123_456_789, 8).unwrap(), "1.23456789");
    }

    // ---------------- the round trip that matters ----------------

    #[test]
    fn round_trip_is_exact_for_every_asset() {
        // For each listed asset, take a spread of base-unit values and assert
        // `parse(format(v)) == v` at that asset's own decimals. This is the
        // property that guarantees no display path can lose or invent value.
        for asset in AssetId::ALL {
            let d = asset.decimals();
            let one = asset.one_whole();
            let cases = [
                0u128,
                1,
                2,
                one - 1,
                one,
                one + 1,
                one * 3 / 2,
                one * 123_456,
                one * 999_999 + one / 2,
                one * 7 + 1,
            ];
            for v in cases {
                let text = format_scaled(v, d)
                    .unwrap_or_else(|e| panic!("format {v} at {d} decimals: {e}"));
                let back = parse_scaled(&text, d)
                    .unwrap_or_else(|e| panic!("reparse `{text}` at {d} decimals: {e}"));
                assert_eq!(back, v, "{asset} round trip broke via `{text}`");
            }
        }
    }

    #[test]
    fn round_trip_is_exact_at_the_contract_scale() {
        // Derivative quantities use the fixed contract scale, not an asset's
        // decimals — round-tripping must hold there too.
        for v in [
            0u128,
            1,
            99_999_999,
            100_000_000,
            100_000_001,
            12_345_678_900,
        ] {
            let text = format_scaled(v, crate::ids::CONTRACT_QTY_DECIMALS).unwrap();
            let back = parse_scaled(&text, crate::ids::CONTRACT_QTY_DECIMALS).unwrap();
            assert_eq!(back, v);
        }
    }

    // ---------------- boundaries, and the characters that only look like digits ----------------

    #[test]
    fn the_largest_u128_parses_at_scale_zero_and_one_more_overflows() {
        let max = u128::MAX.to_string();
        assert_eq!(parse_scaled(&max, 0).unwrap(), u128::MAX);
        // The same digits with the last one bumped: 2^128 exactly.
        let mut over = max.clone().into_bytes();
        *over.last_mut().unwrap() += 1;
        let over = String::from_utf8(over).unwrap();
        assert!(matches!(
            parse_scaled(&over, 0),
            Err(DecimalError::Overflow(_))
        ));
        // At any positive scale the maximum itself no longer fits either.
        assert!(matches!(
            parse_scaled(&max, 1),
            Err(DecimalError::Overflow(_))
        ));
    }

    #[test]
    fn the_largest_u128_formats_and_reparses_exactly_at_every_scale() {
        for d in [0u8, 6, 8, 18, MAX_REPRESENTABLE_DECIMALS] {
            let text = format_scaled(u128::MAX, d).unwrap();
            assert_eq!(
                parse_scaled(&text, d).unwrap(),
                u128::MAX,
                "scale {d} via `{text}`"
            );
        }
    }

    #[test]
    fn a_sign_in_the_wrong_place_is_an_invalid_character_not_a_negative() {
        // Only a single leading `+` is a sign. Anywhere else the character is
        // just not a digit, and the error says which one.
        assert_eq!(
            parse_scaled("+-1", 6).unwrap_err(),
            DecimalError::InvalidChar('-')
        );
        assert_eq!(
            parse_scaled("1.-5", 6).unwrap_err(),
            DecimalError::InvalidChar('-')
        );
        assert_eq!(
            parse_scaled("1-", 6).unwrap_err(),
            DecimalError::InvalidChar('-')
        );
        assert_eq!(
            parse_scaled("++1", 6).unwrap_err(),
            DecimalError::InvalidChar('+')
        );
    }

    #[test]
    fn only_ascii_digits_count_as_digits() {
        // A fullwidth digit, an Arabic-Indic digit and a hex prefix all look
        // numeric to a person and to `char::is_numeric`; none of them is money.
        assert_eq!(
            parse_scaled("\u{ff11}", 6).unwrap_err(),
            DecimalError::InvalidChar('\u{ff11}')
        );
        assert_eq!(
            parse_scaled("\u{663}", 6).unwrap_err(),
            DecimalError::InvalidChar('\u{663}')
        );
        assert_eq!(
            parse_scaled("0x10", 6).unwrap_err(),
            DecimalError::InvalidChar('x')
        );
        assert_eq!(
            parse_scaled("1E5", 6).unwrap_err(),
            DecimalError::InvalidChar('E')
        );
    }

    #[test]
    fn leading_zeros_are_harmless_and_carry_no_octal_meaning() {
        assert_eq!(parse_scaled("007.50", 2).unwrap(), 750);
        assert_eq!(parse_scaled("000", 6).unwrap(), 0);
        assert_eq!(parse_scaled("0.0", 1).unwrap(), 0);
    }

    proptest! {
        /// The round trip, over the whole value space rather than a hand-picked
        /// list: whatever the chain stores, the string it prints re-parses to
        /// the same integer at the same scale.
        #[test]
        fn format_then_parse_is_the_identity_for_every_value_and_scale(
            v in any::<u128>(),
            d in 0u8..=MAX_REPRESENTABLE_DECIMALS,
        ) {
            let text = format_scaled(v, d).unwrap();
            prop_assert_eq!(parse_scaled(&text, d).unwrap(), v, "via `{}`", text);
        }

        /// A parser that panics is a parser that takes the gateway down on a
        /// crafted order body. Every input, at every scale, is an `Ok` or an
        /// `Err`; the result itself is not asserted because there is nothing
        /// to say about arbitrary text except that it does not crash.
        #[test]
        fn parsing_arbitrary_text_never_panics(s in ".{0,48}", d in any::<u8>()) {
            let _ = parse_scaled(&s, d);
        }

        /// Anything the parser accepts prints back to something the parser
        /// accepts, and to the same value.
        #[test]
        fn an_accepted_amount_prints_back_to_an_equal_amount(
            whole in 0u128..1_000_000_000_000u128,
            frac in "[0-9]{0,8}",
            d in 8u8..=18,
        ) {
            let text = if frac.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{frac}")
            };
            let units = parse_scaled(&text, d).unwrap();
            let printed = format_scaled(units, d).unwrap();
            prop_assert_eq!(parse_scaled(&printed, d).unwrap(), units, "`{}` -> `{}`", text, printed);
        }
    }
}
