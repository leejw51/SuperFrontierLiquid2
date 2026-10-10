//! Fixtures shared by the store's integration tests.
//!
//! Every instant here is built from an RFC 3339 literal rather than
//! `Utc::now()`. Fixed instants make a failure reproducible, and they let a
//! test assert on an exact boundary — which is where the interesting bugs in
//! a time-range query live.

#![allow(dead_code)]

use cb_ports::{
    AppDatabase, AuditRecord, Candle, FaucetClaim, LeaderboardEntry, LedgerEntry, MarketMeta,
    Session, User,
};
use cb_store_sqlite::SqliteDb;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// Parse an RFC 3339 literal written by a test.
pub fn t(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("test fixture timestamps are valid RFC 3339")
        .with_timezone(&Utc)
}

/// Parse a decimal literal written by a test.
pub fn d(text: &str) -> Decimal {
    Decimal::from_str_exact(text).expect("test fixture decimals parse exactly")
}

/// A migrated in-memory database.
pub async fn db() -> SqliteDb {
    let db = SqliteDb::open_in_memory()
        .await
        .expect("an in-memory database opens");
    db.migrate()
        .await
        .expect("migrations apply to a fresh database");
    db
}

/// A user whose fields are all distinct, so a mis-mapped column shows up.
pub fn user(id: &str, handle: &str) -> User {
    User {
        user_id: id.to_string(),
        handle: handle.to_string(),
        address: format!("0x{id}"),
        recovery_hash: format!("argon2:{id}"),
        is_bot: false,
        created_at: t("2026-01-01T00:00:00Z"),
        last_seen_at: t("2026-01-01T00:00:00Z"),
    }
}

/// A user created at a chosen instant, for ordering tests.
pub fn user_at(id: &str, handle: &str, created_at: DateTime<Utc>) -> User {
    User {
        created_at,
        last_seen_at: created_at,
        ..user(id, handle)
    }
}

/// A bot account.
pub fn bot(id: &str, handle: &str) -> User {
    User {
        is_bot: true,
        ..user(id, handle)
    }
}

/// Insert a user and fail the test loudly if the store rejects it.
pub async fn seed_user(db: &SqliteDb, u: &User) {
    db.users()
        .create(u)
        .await
        .unwrap_or_else(|e| panic!("seeding user `{}` must succeed: {e}", u.handle));
}

/// A session for `user_id`.
pub fn session(token: &str, user_id: &str, expires_at: DateTime<Utc>) -> Session {
    Session {
        token_hash: token.to_string(),
        user_id: user_id.to_string(),
        created_at: t("2026-01-01T00:00:00Z"),
        expires_at,
        revoked: false,
    }
}

/// A faucet grant.
pub fn claim(id: &str, user_id: &str, asset: &str, amount: &str, at: &str) -> FaucetClaim {
    FaucetClaim {
        claim_id: id.to_string(),
        user_id: user_id.to_string(),
        asset: asset.to_string(),
        amount: d(amount),
        tx_hash: format!("0xtx{id}"),
        claimed_at: t(at),
    }
}

/// A bar with distinct OHLCV values.
pub fn candle(symbol: &str, interval: &str, open_time: &str, close: &str) -> Candle {
    Candle {
        symbol: symbol.to_string(),
        interval: interval.to_string(),
        open_time: t(open_time),
        open: d("100.5"),
        high: d("110.25"),
        low: d("99.75"),
        close: d(close),
        volume: d("12.34567890"),
        trade_count: 42,
    }
}

/// A ledger movement.
pub fn entry(
    id: &str,
    user_id: &str,
    kind: &str,
    asset: &str,
    amount: &str,
    at: &str,
) -> LedgerEntry {
    LedgerEntry {
        entry_id: id.to_string(),
        user_id: user_id.to_string(),
        kind: kind.to_string(),
        asset: asset.to_string(),
        amount: d(amount),
        balance_after: d("1000"),
        memo: Some(format!("memo for {id}")),
        tx_hash: Some(format!("0xtx{id}")),
        height: Some(99),
        at: t(at),
    }
}

/// One ranking row.
pub fn rank(rank: u32, handle: &str, equity: &str) -> LeaderboardEntry {
    LeaderboardEntry {
        rank,
        handle: handle.to_string(),
        address: format!("0x{handle}"),
        equity: d(equity),
        pnl: d("-12.5"),
        pnl_pct: d("-1.234567"),
        volume: d("98765.4321"),
        trades: 17,
        is_bot: false,
    }
}

/// A perpetual market, which exercises every nullable column.
pub fn perp(symbol: &str) -> MarketMeta {
    MarketMeta {
        symbol: symbol.to_string(),
        kind: "perp".into(),
        asset_class: "crypto".into(),
        base: "BTC".into(),
        quote: "USD".into(),
        display_name: format!("{symbol} Perpetual"),
        status: "trading".into(),
        tick_size: d("0.5"),
        lot_size: d("0.0001"),
        min_notional: d("10"),
        price_decimals: 1,
        size_decimals: 4,
        max_leverage: Some(d("20")),
        initial_margin_bps: Some(500),
        maintenance_margin_bps: Some(250),
        funding_interval_seconds: Some(3_600),
        expiry: None,
        maker_fee_bps: -1,
        taker_fee_bps: 5,
    }
}

/// A spot market, whose derivative-only columns are all NULL.
pub fn spot(symbol: &str) -> MarketMeta {
    MarketMeta {
        kind: "spot".into(),
        max_leverage: None,
        initial_margin_bps: None,
        maintenance_margin_bps: None,
        funding_interval_seconds: None,
        expiry: None,
        ..perp(symbol)
    }
}

/// An audit record.
pub fn audit(id: &str, actor: Option<&str>, action: &str, subject: &str, at: &str) -> AuditRecord {
    AuditRecord {
        record_id: id.to_string(),
        actor: actor.map(str::to_string),
        action: action.to_string(),
        subject: subject.to_string(),
        detail: serde_json::json!({ "nested": { "qty": "1.5" }, "ok": true, "n": 7 }),
        source_ip: Some("198.51.100.7".into()),
        at: t(at),
    }
}
