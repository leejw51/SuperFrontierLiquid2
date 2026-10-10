//! Money survives SQLite unchanged.
//!
//! These are the tests that would catch the single worst bug this crate could
//! have: a ledger that quietly rounds. They go through the real schema and the
//! real driver rather than the codec alone, because the failure modes worth
//! fearing — a column typed REAL, a `SUM` that coerces — live in SQL, not in
//! Rust.

mod common;

use cb_ports::{AppDatabase, CandleInterval};
use common::{candle, claim, d, db, entry, perp, seed_user, t, user};
use rust_decimal::Decimal;

/// Values chosen to break a careless encoding: eight-decimal crypto sizes,
/// negatives, a value whose binary-float nearest neighbour is visibly wrong,
/// and the extremes of the type.
const AWKWARD: &[&str] = &[
    "0",
    "0.1",
    "-0.1",
    "0.00000001",
    "-0.00000001",
    "1.00000001",
    "123456789.12345678",
    "-987654321.87654321",
    "0.123456789012345678901234567",
    "-0.123456789012345678901234567",
    "79228162514264337593543950335",
    "-79228162514264337593543950335",
];

#[tokio::test]
async fn ledger_amounts_round_trip_bit_for_bit() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let mut expected = Vec::new();
    for (i, text) in AWKWARD.iter().enumerate() {
        let mut e = entry(
            &format!("e{i}"),
            "u1",
            "fee",
            "USD",
            text,
            "2026-01-01T00:00:00Z",
        );
        // balance_after gets an awkward value too, so a column that was
        // wired to the wrong bind index cannot hide.
        e.balance_after = d(AWKWARD[AWKWARD.len() - 1 - i]);
        e.at = t("2026-01-01T00:00:00Z") + chrono::Duration::seconds(i as i64);
        db.ledger().append(&e).await.expect("append");
        expected.push(e);
    }

    let mut stored = db.ledger().for_user("u1", None, 100).await.expect("read");
    stored.reverse(); // newest-first back to insertion order

    for (want, got) in expected.iter().zip(stored.iter()) {
        assert_eq!(
            got.amount, want.amount,
            "amount {} did not survive storage",
            want.amount
        );
        assert_eq!(
            got.balance_after, want.balance_after,
            "balance_after {} did not survive storage",
            want.balance_after
        );
    }
    assert_eq!(stored.len(), AWKWARD.len());
}

#[tokio::test]
async fn candle_prices_and_volumes_round_trip_at_eight_decimal_places() {
    let db = db().await;
    let mut bar = candle("BTC-PERP", "1m", "2026-01-01T00:00:00Z", "0");
    bar.open = d("64123.87654321");
    bar.high = d("64999.00000001");
    bar.low = d("63000.99999999");
    bar.close = d("64500.12345678");
    bar.volume = d("0.00000001");
    bar.trade_count = u64::from(u32::MAX) + 1; // wider than a u32, still valid

    db.candles().upsert(&bar).await.expect("upsert");

    let read = db
        .candles()
        .latest("BTC-PERP", CandleInterval::M1)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read, bar, "every OHLCV column round-trips exactly");
}

#[tokio::test]
async fn market_tick_and_lot_sizes_round_trip_including_the_optional_leverage() {
    let db = db().await;
    let mut market = perp("BTC-PERP");
    market.tick_size = d("0.00000001");
    market.lot_size = d("0.000000001");
    market.min_notional = d("10.005");
    market.max_leverage = Some(d("33.333333"));

    db.markets()
        .replace_all(&[market.clone()])
        .await
        .expect("replace");
    assert_eq!(
        db.markets().by_symbol("BTC-PERP").await.expect("read"),
        Some(market)
    );
}

/// The reason `total_granted` sums in Rust instead of in SQL.
///
/// Three tenths do not sum to nine tenths in binary floating point. If this
/// aggregation ever moves into a SQL `SUM` over the TEXT column, SQLite will
/// coerce to REAL and this test will fail with `0.8999999999999999`.
#[tokio::test]
async fn faucet_totals_are_exact_where_floating_point_would_drift() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    for i in 0..3 {
        db.faucet()
            .record_claim(&claim(
                &format!("c{i}"),
                "u1",
                "USD",
                "0.3",
                &format!("2026-01-01T00:0{i}:00Z"),
            ))
            .await
            .expect("record");
    }
    // Ten satoshis, which a float cannot represent at all.
    for i in 0..10 {
        db.faucet()
            .record_claim(&claim(
                &format!("btc{i}"),
                "u1",
                "BTC",
                "0.00000001",
                &format!("2026-01-02T00:0{i}:00Z"),
            ))
            .await
            .expect("record");
    }

    let totals = db.faucet().total_granted().await.expect("read");
    assert_eq!(
        totals,
        vec![
            ("BTC".to_string(), d("0.0000001")),
            ("USD".to_string(), d("0.9")),
        ]
    );
}

/// A negative net is as important as a positive one: fees and funding are
/// where the sign of a stored decimal actually matters.
#[tokio::test]
async fn a_net_movement_can_be_negative_and_stays_exact() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    db.ledger()
        .append_many(&[
            entry(
                "e1",
                "u1",
                "fee",
                "USD",
                "-0.30000001",
                "2026-01-01T00:00:00Z",
            ),
            entry(
                "e2",
                "u1",
                "funding",
                "USD",
                "-0.20000002",
                "2026-01-01T00:01:00Z",
            ),
            entry(
                "e3",
                "u1",
                "realized_pnl",
                "USD",
                "0.1",
                "2026-01-01T00:02:00Z",
            ),
        ])
        .await
        .expect("seed");

    let net = db
        .ledger()
        .net_by_asset("u1", t("2026-01-01T00:00:00Z"), t("2026-01-02T00:00:00Z"))
        .await
        .expect("read");
    assert_eq!(net, vec![("USD".to_string(), d("-0.40000003"))]);
}

/// Trailing zeros carry no value, so the encoding drops them — and the value
/// is unchanged, which is the property that actually matters.
#[tokio::test]
async fn trailing_zeros_do_not_change_the_value_that_comes_back() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let padded = entry("e1", "u1", "fee", "USD", "1.5000", "2026-01-01T00:00:00Z");
    db.ledger().append(&padded).await.expect("append");

    let read = &db.ledger().for_user("u1", None, 1).await.expect("read")[0];
    assert_eq!(read.amount, Decimal::from_str_exact("1.5").expect("parses"));
    assert_eq!(read.amount, padded.amount, "the quantity is unchanged");
}
