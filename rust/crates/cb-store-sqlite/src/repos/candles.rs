//! OHLCV bars.

use crate::codec::{
    decode_decimal, decode_time, decode_u64, encode_decimal, encode_time, encode_u64,
};
use crate::error::map_sqlx;
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::CandleRepository;
use cb_ports::{Candle, CandleInterval, PortResult};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Sqlite};

const COLUMNS: &str = "symbol, interval, open_time, open, high, low, close, volume, trade_count";

/// One statement serves both `upsert` and `upsert_many`.
///
/// `INSERT … ON CONFLICT DO UPDATE` rather than `INSERT OR REPLACE`: the
/// latter deletes the old row and inserts a new one, which fires delete
/// triggers and would cascade to anything referencing the bar. An upsert
/// should change a bar, not replace its identity. Re-aggregating a bucket is
/// something the gateway does on every tick within the current minute, so it
/// must be cheap and it must not duplicate.
const UPSERT: &str = "INSERT INTO candles \
     (symbol, interval, open_time, open, high, low, close, volume, trade_count) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
     ON CONFLICT (symbol, interval, open_time) DO UPDATE SET \
     open = excluded.open, high = excluded.high, low = excluded.low, \
     close = excluded.close, volume = excluded.volume, \
     trade_count = excluded.trade_count";

fn from_row(row: &SqliteRow) -> PortResult<Candle> {
    let read = |name: &str| -> PortResult<rust_decimal::Decimal> {
        let raw: String = col(row, name)?;
        decode_decimal(name, &raw)
    };

    Ok(Candle {
        symbol: col(row, "symbol")?,
        interval: col(row, "interval")?,
        open_time: decode_time("open_time", col(row, "open_time")?)?,
        open: read("open")?,
        high: read("high")?,
        low: read("low")?,
        close: read("close")?,
        volume: read("volume")?,
        trade_count: decode_u64("trade_count", col(row, "trade_count")?)?,
    })
}

/// Bind one candle to the upsert statement.
///
/// Generic over the executor so the same code runs standalone and inside the
/// `upsert_many` transaction — duplicating the bind order would be a place for
/// the two paths to drift apart.
async fn upsert_with<'e, E>(executor: E, candle: &Candle) -> PortResult<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    let trade_count = encode_u64("trade_count", candle.trade_count)?;
    sqlx::query(UPSERT)
        .bind(&candle.symbol)
        .bind(&candle.interval)
        .bind(encode_time(candle.open_time))
        .bind(encode_decimal(&candle.open))
        .bind(encode_decimal(&candle.high))
        .bind(encode_decimal(&candle.low))
        .bind(encode_decimal(&candle.close))
        .bind(encode_decimal(&candle.volume))
        .bind(trade_count)
        .execute(executor)
        .await
        .map(|_| ())
        .map_err(|e| {
            map_sqlx(
                &format!("upserting {} {} candle", candle.symbol, candle.interval),
                e,
            )
        })
}

#[async_trait]
impl CandleRepository for SqliteDb {
    async fn upsert(&self, candle: &Candle) -> PortResult<()> {
        upsert_with(self.pool(), candle).await
    }

    /// All-or-nothing. A partially applied batch would leave a chart with a
    /// hole in it that no later write repairs, because the aggregator only
    /// revisits the current bucket.
    async fn upsert_many(&self, candles: &[Candle]) -> PortResult<()> {
        if candles.is_empty() {
            return Ok(());
        }

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_sqlx("beginning a candle batch", e))?;

        for candle in candles {
            upsert_with(&mut *tx, candle).await?;
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx("committing a candle batch", e))
    }

    /// The window is half-open: `from` is inclusive, `to` is exclusive.
    ///
    /// Bars are named by the instant they open, so a `[from, to)` window over
    /// open times tiles without overlap — asking for `[09:00, 10:00)` and then
    /// `[10:00, 11:00)` returns each bar exactly once. An inclusive `to` would
    /// return the 10:00 bar in both.
    ///
    /// Results are oldest-first, as the trait specifies. Note the consequence
    /// of combining that with `limit`: when the window holds more bars than
    /// `limit`, the *oldest* survive. A caller wanting the most recent N should
    /// pass a `from` derived from the interval width, or use `latest`.
    async fn range(
        &self,
        symbol: &str,
        interval: CandleInterval,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> PortResult<Vec<Candle>> {
        let mut sql = format!("SELECT {COLUMNS} FROM candles WHERE symbol = ? AND interval = ?");
        if from.is_some() {
            sql.push_str(" AND open_time >= ?");
        }
        if to.is_some() {
            sql.push_str(" AND open_time < ?");
        }
        sql.push_str(" ORDER BY open_time ASC LIMIT ?");

        let mut query = sqlx::query(&sql).bind(symbol).bind(interval.as_str());
        if let Some(from) = from {
            query = query.bind(encode_time(from));
        }
        if let Some(to) = to {
            query = query.bind(encode_time(to));
        }
        let rows = query
            .bind(sql_limit(limit))
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("reading {symbol} candles"), e))?;

        rows.iter().map(from_row).collect()
    }

    async fn latest(&self, symbol: &str, interval: CandleInterval) -> PortResult<Option<Candle>> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM candles WHERE symbol = ? AND interval = ? \
             ORDER BY open_time DESC LIMIT 1"
        ))
        .bind(symbol)
        .bind(interval.as_str())
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading the latest {symbol} candle"), e))?;

        row.as_ref().map(from_row).transpose()
    }

    /// Drops bars that opened strictly before `before`, matching the
    /// half-open convention of `range`: the bar at exactly `before` is the
    /// first one kept.
    async fn prune(&self, interval: CandleInterval, before: DateTime<Utc>) -> PortResult<u64> {
        let result = sqlx::query("DELETE FROM candles WHERE interval = ? AND open_time < ?")
            .bind(interval.as_str())
            .bind(encode_time(before))
            .execute(self.pool())
            .await
            .map_err(|e| map_sqlx("pruning candles", e))?;
        Ok(result.rows_affected())
    }
}
