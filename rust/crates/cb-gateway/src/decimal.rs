//! The wire codec for money: decimal strings in, fixed-point integers out.
//!
//! ## Where the rules live
//!
//! Parsing — including the rule that excess precision is an *error* rather
//! than a truncation — is delegated to [`cb_types::amount::parse_scaled`].
//! That is deliberate. The chain, the CLI and the gateway must agree on what
//! `"0.123456789"` means for a six-decimal asset, and the only way to keep
//! three implementations in agreement is to have one. A user who types more
//! precision than an asset can hold has made a mistake; filling their order at
//! a size they did not ask for turns that mistake into a bug that looks like a
//! rounding artefact.
//!
//! ## What this module adds
//!
//! Formatting. `cb_types::amount::format_scaled` trims every trailing zero,
//! which is right for a CLI and wrong for the API contract, where a USD
//! balance reads `"100000.00"` and a BTC balance reads `"1.00000000"`.
//! [`format_scaled`] takes a minimum number of decimal places and pads to it,
//! while never dropping precision the value actually carries.
//!
//! ## Why not `rust_decimal`
//!
//! `Decimal` carries about 28 significant digits; on-chain amounts are `u128`,
//! which is 39. An ETH balance in wei leaves `Decimal`'s range long before it
//! leaves the chain's. Everything here is integer arithmetic on digits, so it
//! is exact across the whole domain. [`to_port_decimal`] is the one crossing
//! point, and it reports failure rather than rounding.

pub use cb_types::amount::DecimalError;

/// Parse a non-negative decimal string into an integer scaled by
/// `10^decimals`, rejecting more decimal places than the scale can hold.
///
/// ```
/// use cb_gateway::decimal::parse_scaled;
/// assert_eq!(parse_scaled("1.5", 6).unwrap(), 1_500_000);
/// assert_eq!(parse_scaled("0.000001", 6).unwrap(), 1);
/// assert!(parse_scaled("0.0000001", 6).is_err());
/// ```
pub fn parse_scaled(input: &str, decimals: u32) -> Result<u128, DecimalError> {
    let decimals = u8::try_from(decimals).map_err(|_| DecimalError::ScaleTooLarge(u8::MAX))?;
    cb_types::amount::parse_scaled(&trim_insignificant_zeros(input), decimals)
}

/// Drop fractional zeros that carry no precision.
///
/// `cb_types::amount::parse_scaled` counts *written* digits, so `"1.5000000"`
/// against a six-decimal asset is refused. That is right for a CLI, where the
/// operator typed those digits, and wrong at an HTTP boundary: a browser
/// formatting with `toFixed(8)` writes them for display consistency and means
/// nothing by them.
///
/// Only trailing zeros go. `"1.0000001"` keeps its significant ninth digit and
/// is still refused, which is the guard that matters — the gateway must not
/// truncate a size the user did ask for.
fn trim_insignificant_zeros(input: &str) -> String {
    let trimmed = input.trim();
    let Some((integer, fraction)) = trimmed.split_once('.') else {
        return trimmed.to_string();
    };
    // Only touch something that is plainly a decimal number; anything else is
    // handed through unchanged so the parser rejects it with its own message.
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return trimmed.to_string();
    }
    let significant = fraction.trim_end_matches('0');
    if significant.is_empty() {
        integer.to_string()
    } else {
        format!("{integer}.{significant}")
    }
}

/// Render an integer scaled by `10^decimals` as a decimal string.
///
/// Trailing zeros are trimmed but never below `min_places`, so a value that
/// happens to be round still prints with the places the contract's examples
/// show, and a value with genuine sub-unit precision prints all of it.
pub fn format_scaled(value: u128, decimals: u32, min_places: u32) -> String {
    let min_places = min_places.min(decimals) as usize;
    if decimals == 0 {
        return value.to_string();
    }
    let width = decimals as usize;
    let raw = value.to_string();
    let (int_part, frac_part) = if raw.len() > width {
        let split = raw.len() - width;
        (raw[..split].to_string(), raw[split..].to_string())
    } else {
        ("0".to_string(), format!("{raw:0>width$}"))
    };

    let keep = frac_part.trim_end_matches('0').len().max(min_places);
    if keep == 0 {
        return int_part;
    }
    format!("{int_part}.{}", &frac_part[..keep])
}

/// Parse a signed decimal string into an integer scaled by `10^decimals`.
pub fn parse_scaled_signed(input: &str, decimals: u32) -> Result<i128, DecimalError> {
    let trimmed = input.trim();
    match trimmed.strip_prefix('-') {
        Some(rest) => {
            let magnitude = parse_scaled(rest, decimals)?;
            i128::try_from(magnitude)
                .map(|v| -v)
                .map_err(|_| DecimalError::Overflow(input.to_string()))
        }
        None => {
            let magnitude = parse_scaled(trimmed, decimals)?;
            i128::try_from(magnitude).map_err(|_| DecimalError::Overflow(input.to_string()))
        }
    }
}

/// Render a signed integer scaled by `10^decimals`, for PnL and funding.
pub fn format_scaled_signed(value: i128, decimals: u32, min_places: u32) -> String {
    let body = format_scaled(value.unsigned_abs(), decimals, min_places);
    if value < 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// Render a ratio as a fixed-point decimal string with `places` places.
///
/// Used for percentages and margin ratios, where the contract wants a string
/// such as `"84.25"` and the inputs are integers that must not touch a float.
pub fn format_ratio(numerator: i128, denominator: i128, places: u32) -> String {
    if denominator == 0 {
        return format_scaled_signed(0, places, places);
    }
    let scale = 10i128.saturating_pow(places);
    let scaled = numerator
        .saturating_mul(scale)
        .checked_div(denominator)
        .unwrap_or(0);
    format_scaled_signed(scaled, places, places)
}

/// Convert a scaled integer to `rust_decimal::Decimal` for the port models,
/// which use `Decimal` throughout.
///
/// Returns `None` when the value exceeds `Decimal`'s mantissa. Callers must
/// not round on its behalf: a candle that cannot be represented is a fact
/// worth logging, not one worth quietly corrupting.
pub fn to_port_decimal(value: u128, decimals: u32) -> Option<rust_decimal::Decimal> {
    use std::str::FromStr;
    let text = format_scaled(value, decimals, 0);
    let parsed = rust_decimal::Decimal::from_str(&text).ok()?;
    // `Decimal::from_str` rounds silently once the input passes its mantissa,
    // so parsing succeeding proves nothing. Round-trip and compare: if the
    // value cannot come back unchanged, it was not representable.
    (parsed.to_string() == text).then_some(parsed)
}

/// Render a `rust_decimal::Decimal` from a port model as a wire string with at
/// least `min_places` decimal places.
pub fn format_port_decimal(value: rust_decimal::Decimal, min_places: u32) -> String {
    let negative = value.is_sign_negative() && !value.is_zero();
    let text = value.abs().normalize().to_string();
    let (int_part, frac_part) = match text.split_once('.') {
        Some((i, f)) => (i.to_string(), f.to_string()),
        None => (text, String::new()),
    };
    let keep = frac_part.len().max(min_places as usize);
    let body = if keep == 0 {
        int_part
    } else {
        format!("{int_part}.{frac_part:0<keep$}")
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_representable_unit_round_trips() {
        assert_eq!(parse_scaled("0.000001", 6).expect("parse"), 1);
        assert_eq!(format_scaled(1, 6, 2), "0.000001");
        assert_eq!(parse_scaled("0.00000001", 8).expect("parse"), 1);
        assert_eq!(format_scaled(1, 8, 8), "0.00000001");
    }

    #[test]
    fn a_very_large_balance_survives_the_full_u128_range() {
        // u128::MAX has 39 digits; rust_decimal caps near 28, so this is
        // exactly the value a Decimal-based codec would corrupt.
        let text = format_scaled(u128::MAX, 18, 0);
        assert_eq!(text, "340282366920938463463.374607431768211455");
        assert_eq!(parse_scaled(&text, 18).expect("round trip"), u128::MAX);
        assert!(
            to_port_decimal(u128::MAX, 18).is_none(),
            "the port model cannot hold it, and must say so rather than round"
        );
    }

    #[test]
    fn excess_precision_is_rejected_not_truncated() {
        let err = parse_scaled("0.123456789", 8).expect_err("must reject");
        assert!(
            matches!(err, DecimalError::TooManyDecimals { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn trailing_zeros_are_formatting_and_are_accepted() {
        // A browser formatting a USD amount with `toFixed(8)` writes zeros it
        // means nothing by. Refusing those would break every order.
        assert_eq!(parse_scaled("1.50000000", 6).expect("parse"), 1_500_000);
        assert_eq!(
            parse_scaled("100000.00000000", 6).expect("parse"),
            100_000_000_000
        );
        assert_eq!(parse_scaled("1.000000000000", 0).expect("parse"), 1);
        // ...but a significant digit past the scale is still an error.
        assert!(parse_scaled("1.0000001", 6).is_err());
        assert!(parse_scaled("0.1234567890", 8).is_err());
    }

    #[test]
    fn overflowing_the_chain_is_an_error_not_a_wrap() {
        let too_big = format!("{}0", u128::MAX);
        assert!(parse_scaled(&too_big, 0).is_err());
    }

    #[test]
    fn malformed_input_is_rejected() {
        for bad in [
            "", "   ", "abc", "1.2.3", ".", "1e9", "1,000", "0x10", "-1.5",
        ] {
            assert!(parse_scaled(bad, 6).is_err(), "`{bad}` should not parse");
        }
    }

    #[test]
    fn formatting_pads_to_the_minimum_but_never_hides_precision() {
        assert_eq!(format_scaled(100_000_000_000, 6, 2), "100000.00");
        assert_eq!(format_scaled(100_000_000, 8, 8), "1.00000000");
        assert_eq!(format_scaled(6_425_050_000_000, 8, 2), "64250.50");
        assert_eq!(format_scaled(1_000_001, 6, 2), "1.000001");
        assert_eq!(format_scaled(0, 6, 2), "0.00");
        assert_eq!(format_scaled(5, 0, 0), "5");
    }

    #[test]
    fn signed_values_render_and_parse_symmetrically() {
        assert_eq!(format_scaled_signed(-7_200_000_000, 6, 2), "-7200.00");
        assert_eq!(
            parse_scaled_signed("-7200.00", 6).expect("parse"),
            -7_200_000_000
        );
        assert_eq!(format_scaled_signed(0, 6, 2), "0.00");
    }

    #[test]
    fn ratios_render_without_touching_a_float() {
        assert_eq!(format_ratio(8425, 10_000, 2), "0.84");
        assert_eq!(format_ratio(84_250, 100, 2), "842.50");
        assert_eq!(format_ratio(1, 0, 2), "0.00", "no division by zero");
        assert_eq!(format_ratio(-1150, 10_000, 3), "-0.115");
    }

    #[test]
    fn port_decimal_conversion_is_exact_inside_its_range() {
        let d = to_port_decimal(6_425_050_000_000, 8).expect("in range");
        assert_eq!(format_port_decimal(d, 2), "64250.50");
        assert_eq!(
            format_port_decimal(rust_decimal::Decimal::new(-1234, 2), 2),
            "-12.34"
        );
    }
}
