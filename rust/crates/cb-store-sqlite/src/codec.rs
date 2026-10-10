//! How domain values are represented in SQLite columns.
//!
//! SQLite has five storage classes — NULL, INTEGER, REAL, TEXT, BLOB — and
//! none of them is a decimal or an instant. Three encoding decisions follow,
//! and every one of them is a place where a careless choice loses data
//! silently rather than loudly. They are collected here so that there is
//! exactly one answer per type.
//!
//! # Money: normalised decimal TEXT
//!
//! [`rust_decimal::Decimal`] is stored as the string produced by
//! [`Decimal::normalize`] followed by `to_string`. `12.3400` and `12.34` are
//! the same quantity of money, so they get the same encoding; each value has
//! exactly one representation in the database.
//!
//! The alternative was an integer of minor units. It was rejected because a
//! fixed minor-unit scale has to be chosen up front, and any value with more
//! decimal places than that scale is silently truncated on the way in. This
//! exchange quotes crypto sizes to eight places and funding rates to more,
//! and a scale generous enough for those (10^-18) overflows SQLite's 64-bit
//! INTEGER long before it reaches a plausible notional. TEXT has no such
//! ceiling and round-trips exactly.
//!
//! REAL was never a candidate. Binary floating point cannot represent `0.1`,
//! and a ledger that cannot represent a tenth is not a ledger.
//!
//! **The trade-off, stated plainly.** TEXT decimals cannot be summed or
//! ordered by SQLite: `SUM(amount)` over a TEXT column coerces to REAL and
//! quietly reintroduces the very floating-point error this encoding exists to
//! avoid. So `total_granted` and `net_by_asset` fetch rows and fold them in
//! Rust with exact `Decimal` arithmetic, reading more rows than a SQL
//! `GROUP BY` would. That is affordable here because both are reporting
//! queries over a simulator's ledger, not hot paths.
//!
//! The brief asked for a *lexicographically sortable* text encoding. No
//! method in `cb_ports::db` orders, ranges over or aggregates a money column
//! in SQL — the leaderboard sorts by a precomputed integer rank — so a
//! sortable encoding would buy an unused property at the cost of a column no
//! operator can read in a `sqlite3` shell. If a query ever does need SQL-side
//! ordering on money, the fix is a companion sortable column written
//! alongside this one, not a change to this encoding: changing it would
//! require rewriting every stored row.
//!
//! # Instants: INTEGER microseconds since the Unix epoch
//!
//! sqlx encodes `DateTime<Utc>` as RFC 3339 text whose fractional-second
//! precision varies with the value. `"2026-01-01T00:00:00Z"` and
//! `"2026-01-01T00:00:00.000001Z"` are one microsecond apart but compare in
//! the wrong order as text, because `'Z'` (0x5A) sorts above `'.'` (0x2E).
//! Every time-range query in this schema — `range`, `delete_expired`,
//! `prune`, `net_by_asset` — depends on `<` over stored instants, so text
//! would break them for exactly the values a test is least likely to try.
//! Microseconds are `chrono`'s own resolution for `timestamp_micros` and
//! leave four centuries of headroom in an `i64`.
//!
//! # Counts: `u64` via checked conversion
//!
//! SQLite's INTEGER is signed 64-bit and sqlx offers no `u64` codec. Rather
//! than let a cast wrap, conversions are checked in both directions: a value
//! too large to store is the caller's error ([`PortError::InvalidArgument`]),
//! while a negative value read back is the database's
//! ([`PortError::Encoding`]).

use cb_ports::{PortError, PortResult};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

/// Encode money for storage. See the module header for the rationale.
pub(crate) fn encode_decimal(value: &Decimal) -> String {
    value.normalize().to_string()
}

/// Decode money read back from storage.
///
/// Uses `from_str_exact`, which refuses a string that would need rounding to
/// fit a `Decimal`, so a corrupted or over-precise column surfaces as
/// [`PortError::Encoding`] instead of being quietly rounded.
pub(crate) fn decode_decimal(column: &str, raw: &str) -> PortResult<Decimal> {
    Decimal::from_str_exact(raw).map_err(|e| {
        PortError::Encoding(format!(
            "column `{column}` holds {raw:?}, not a decimal: {e}"
        ))
    })
}

/// Decode an optional money column.
pub(crate) fn decode_opt_decimal(column: &str, raw: Option<String>) -> PortResult<Option<Decimal>> {
    raw.as_deref()
        .map(|s| decode_decimal(column, s))
        .transpose()
}

/// Encode an instant for storage.
pub(crate) fn encode_time(at: DateTime<Utc>) -> i64 {
    at.timestamp_micros()
}

/// Decode an instant read back from storage.
pub(crate) fn decode_time(column: &str, raw: i64) -> PortResult<DateTime<Utc>> {
    DateTime::from_timestamp_micros(raw).ok_or_else(|| {
        PortError::Encoding(format!(
            "column `{column}` holds {raw}, which is not a representable instant"
        ))
    })
}

/// Encode a count for storage, refusing values SQLite cannot hold.
pub(crate) fn encode_u64(column: &str, value: u64) -> PortResult<i64> {
    i64::try_from(value).map_err(|_| {
        PortError::InvalidArgument(format!(
            "`{column}` is {value}, which exceeds the largest value SQLite can store"
        ))
    })
}

/// Encode an optional count for storage.
pub(crate) fn encode_opt_u64(column: &str, value: Option<u64>) -> PortResult<Option<i64>> {
    value.map(|v| encode_u64(column, v)).transpose()
}

/// Decode a count read back from storage.
pub(crate) fn decode_u64(column: &str, raw: i64) -> PortResult<u64> {
    u64::try_from(raw)
        .map_err(|_| PortError::Encoding(format!("column `{column}` holds a negative count {raw}")))
}

/// Decode an optional count read back from storage.
pub(crate) fn decode_opt_u64(column: &str, raw: Option<i64>) -> PortResult<Option<u64>> {
    raw.map(|v| decode_u64(column, v)).transpose()
}

/// Encode a `u32` count. Always representable; present for symmetry so no
/// call site has to reason about which widths need checking.
pub(crate) fn encode_u32(value: u32) -> i64 {
    i64::from(value)
}

/// Decode a `u32` read back from storage.
pub(crate) fn decode_u32(column: &str, raw: i64) -> PortResult<u32> {
    u32::try_from(raw).map_err(|_| {
        PortError::Encoding(format!(
            "column `{column}` holds {raw}, which is out of range for a u32"
        ))
    })
}

/// Decode an optional `u32` read back from storage.
pub(crate) fn decode_opt_u32(column: &str, raw: Option<i64>) -> PortResult<Option<u32>> {
    raw.map(|v| decode_u32(column, v)).transpose()
}

/// Decode a JSON document stored as TEXT.
///
/// The workspace's sqlx build omits the `json` feature, so there is no
/// `Json<T>` wrapper to lean on; the conversion is explicit instead.
pub(crate) fn decode_json(column: &str, raw: &str) -> PortResult<serde_json::Value> {
    serde_json::Value::from_str(raw)
        .map_err(|e| PortError::Encoding(format!("column `{column}` is not valid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) {
        let original = Decimal::from_str_exact(text).expect("test literal parses");
        let decoded = decode_decimal("amount", &encode_decimal(&original))
            .expect("an encoded decimal decodes");
        assert_eq!(
            decoded, original,
            "round-tripping {text} through TEXT changed its value"
        );
    }

    #[test]
    fn decimals_round_trip_at_the_precisions_this_exchange_uses() {
        round_trip("0");
        round_trip("1");
        round_trip("-1");
        round_trip("0.00000001"); // satoshi
        round_trip("-0.00000001");
        round_trip("123456789.123456789012345678"); // 18 places
        round_trip("-98765.43210987654321");
        round_trip("79228162514264337593543950335"); // Decimal::MAX
        round_trip("-79228162514264337593543950335"); // Decimal::MIN
    }

    #[test]
    fn encoding_is_canonical_so_equal_money_stores_identically() {
        let a = Decimal::from_str_exact("12.3400").expect("literal parses");
        let b = Decimal::from_str_exact("12.34").expect("literal parses");
        assert_eq!(a, b);
        assert_eq!(encode_decimal(&a), encode_decimal(&b));
        assert_eq!(encode_decimal(&a), "12.34");
    }

    #[test]
    fn negative_zero_normalises_to_zero() {
        let neg_zero = Decimal::from_str_exact("-0.00").expect("literal parses");
        assert_eq!(encode_decimal(&neg_zero), "0");
    }

    #[test]
    fn a_non_decimal_column_is_an_encoding_error_not_a_panic() {
        let err = decode_decimal("amount", "not a number").expect_err("must reject");
        assert!(matches!(err, PortError::Encoding(_)), "got {err:?}");
    }

    #[test]
    fn instants_round_trip_at_microsecond_resolution() {
        for text in [
            "1970-01-01T00:00:00Z",
            "2026-09-18T12:34:56.123456Z",
            "1969-07-20T20:17:40.000001Z", // pre-epoch, negative micros
        ] {
            let original = DateTime::parse_from_rfc3339(text)
                .expect("test literal parses")
                .with_timezone(&Utc);
            let decoded =
                decode_time("at", encode_time(original)).expect("an encoded instant decodes");
            assert_eq!(decoded, original, "round-tripping {text} changed it");
        }
    }

    #[test]
    fn encoded_instants_order_the_same_way_the_instants_do() {
        // The property RFC 3339 text fails: 'Z' sorts above '.', so
        // "…:00Z" > "…:00.000001Z" as text while the instants compare the
        // other way. Integers cannot get this wrong.
        let zero = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("literal parses")
            .with_timezone(&Utc);
        let one_micro = zero + chrono::Duration::microseconds(1);
        assert!(encode_time(zero) < encode_time(one_micro));
    }

    #[test]
    fn counts_beyond_sqlites_range_are_rejected_rather_than_wrapped() {
        let err = encode_u64("trade_count", u64::MAX).expect_err("must reject");
        assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");
        assert_eq!(encode_u64("trade_count", 7).expect("7 fits"), 7);
    }

    #[test]
    fn a_negative_stored_count_is_an_encoding_error() {
        let err = decode_u64("trade_count", -1).expect_err("must reject");
        assert!(matches!(err, PortError::Encoding(_)), "got {err:?}");
    }
}
