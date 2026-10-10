//! Money is a decimal string, everywhere, on every endpoint.
//!
//! The contract opens with this rule and gives the reason: "JavaScript numbers
//! cannot represent the on-chain integer range, and silent precision loss in a
//! trading UI is unacceptable." The failure it prevents is quiet. A serialiser
//! change that emits `64250.5` instead of `"64250.50"` breaks no test that
//! reads the value back with `serde_json`, breaks no Rust client, and starts
//! rounding a user's balance in the browser. Nothing goes red; the numbers just
//! stop being right.
//!
//! So the check is structural rather than per-endpoint: walk every response,
//! and for every field the contract types as money, assert it is a JSON string
//! that parses as a decimal — and never a JSON number.
//!
//! The converse matters too. `height`, `tx_count` and the `*_bps` risk
//! parameters are documented as JSON numbers, and a client that does
//! `height + 1` would break if they became strings. A blanket "no numbers in
//! any response" would be the wrong rule and would fail on correct output, so
//! both lists are explicit and anything in neither is left alone.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde_json::Value;

/// Field names the contract types as a decimal string.
///
/// Read off `docs/api-contract.md` section by section. A name appears once even
/// when several endpoints use it, because the rule is about the name.
const MONEY_FIELDS: &[&str] = &[
    "amount",
    "available_margin",
    "avg_fill_price",
    "avg_price",
    "balance_after",
    "c",
    "change_24h_pct",
    "closed_qty",
    "equity",
    "fee",
    "fees_24h",
    "filled_qty",
    "free",
    "funding_paid",
    "funding_rate",
    "granted_24h",
    "h",
    "high_24h",
    "index_price",
    "insurance_fund",
    "l",
    "last_price",
    "leverage",
    "liquidation_price",
    "locked",
    "loss",
    "low_24h",
    "margin",
    "margin_ratio",
    "margin_ratio_after",
    "margin_ratio_before",
    "mark_price",
    "max_leverage",
    "min_notional",
    "notional",
    "o",
    "open_interest",
    "pnl",
    "pnl_pct",
    "price",
    "qty",
    "rate",
    "realized_pnl",
    "realized_pnl_24h",
    "realized_pnl_30d",
    "settlement_price",
    "tick_size",
    "lot_size",
    "tps_1m",
    "unrealized_pnl",
    "unrealized_pnl_pct",
    "v",
    "volume",
    "volume_24h",
    "volume_30d",
    "used_margin",
];

/// Field names the contract types as a JSON number.
///
/// Asserted as firmly as the money fields, and for the same reason in reverse:
/// a client that arithmetics on `height` breaks the day it becomes a string.
const INTEGER_FIELDS: &[&str] = &[
    "chain_id",
    "claims_24h",
    "claims_made",
    "cooldown_seconds",
    "funding_interval_seconds",
    "height",
    "initial_margin_bps",
    "liquidations",
    "liquidations_24h",
    "maintenance_margin_bps",
    "maker_fee_bps",
    "markets_traded",
    "mempool_depth",
    "price_decimals",
    "rank",
    "size_decimals",
    "taker_fee_bps",
    "trades_24h",
    "tx_count",
    "uptime_seconds",
    "validators",
];

/// Keys whose value is a list of `[price, quantity]` string pairs.
const LEVEL_LISTS: &[&str] = &["bids", "asks"];

/// `total` is money inside a balance entry and a count everywhere else.
///
/// `{"asset":"USD","free":"...","locked":"...","total":"..."}` is money;
/// `{"total":16,"trading":16,...}` in the admin overview's market summary is a
/// count of markets. The name alone cannot decide it, so the enclosing object
/// does: a balance entry is the one that names an asset.
fn total_is_money(parent: &serde_json::Map<String, Value>) -> bool {
    parent.contains_key("asset") && parent.contains_key("free")
}

/// Walks a response and asserts the contract's number conventions.
///
/// `context` names what is being checked — it is prefixed to any failure, so
/// write it as the request ("GET /markets/BTC-USD/ticker").
pub fn assert_number_conventions(value: &Value, context: &str) {
    walk(value, context, &mut String::new());
}

fn walk(value: &Value, context: &str, path: &mut String) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let mark = path.len();
                path.push('.');
                path.push_str(key);
                let money_here =
                    MONEY_FIELDS.contains(&key.as_str()) || (key == "total" && total_is_money(map));
                if LEVEL_LISTS.contains(&key.as_str()) {
                    assert_level_list(child, context, path);
                } else if money_here {
                    assert_money(child, context, path);
                } else if INTEGER_FIELDS.contains(&key.as_str()) {
                    assert!(
                        child.is_number() || child.is_null(),
                        "{context}: {path} is documented as a JSON number, got {child}"
                    );
                } else {
                    walk(child, context, path);
                }
                path.truncate(mark);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let mark = path.len();
                path.push_str(&format!("[{index}]"));
                walk(item, context, path);
                path.truncate(mark);
            }
        }
        _ => {}
    }
}

/// A money field: a decimal string, or a null.
///
/// Two shapes are containers rather than values and are walked into instead.
/// The admin overview aggregates money as a list of `{asset, amount}` entries,
/// and it groups counters under a `volume` object even though `volume` is a
/// money field on a leaderboard row. A name is only ever money when it holds a
/// scalar, so a list or an object is descended into rather than rejected.
fn assert_money(value: &Value, context: &str, path: &str) {
    match value {
        Value::Null => {}
        Value::String(text) => {
            assert!(
                Decimal::from_str(text).is_ok(),
                "{context}: {path} = {text:?} is not a decimal"
            );
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let mut child = format!("{path}[{index}]");
                walk(item, context, &mut child);
            }
        }
        Value::Object(_) => {
            let mut child = path.to_string();
            walk(value, context, &mut child);
        }
        Value::Number(_) => panic!(
            "{context}: {path} is a JSON number ({value}). The contract requires money as a \
             decimal string — a JavaScript client silently loses precision on a number, and \
             nothing in the UI reports that it happened."
        ),
        other => panic!("{context}: {path} should be a decimal string, got {other}"),
    }
}

/// An order-book side: `[[price, qty], ...]`, both strings.
fn assert_level_list(value: &Value, context: &str, path: &str) {
    let Some(levels) = value.as_array() else {
        // `bids` also names a plain number in no documented shape, so anything
        // that is not a list is simply not a book side.
        return;
    };
    for (index, level) in levels.iter().enumerate() {
        let pair = level.as_array().unwrap_or_else(|| {
            panic!("{context}: {path}[{index}] should be [price, qty], got {level}")
        });
        assert_eq!(
            pair.len(),
            2,
            "{context}: {path}[{index}] should be [price, qty], got {level}"
        );
        for (which, entry) in ["price", "qty"].iter().zip(pair) {
            assert_money(entry, context, &format!("{path}[{index}].{which}"));
        }
    }
}

/// Reads a field the contract types as money, failing with the field name.
pub fn as_decimal(value: &Value, what: &str) -> Decimal {
    match value {
        Value::String(text) => Decimal::from_str(text)
            .unwrap_or_else(|err| panic!("{what} = {text:?} is not a decimal: {err}")),
        Value::Null => Decimal::ZERO,
        other => panic!(
            "{what} = {other} should be a decimal string; a JSON number here loses precision \
             in every JavaScript client"
        ),
    }
}

/// A decimal literal, for comparisons in tests.
pub fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap_or_else(|err| panic!("{text:?} is not a decimal: {err}"))
}
