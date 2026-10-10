//! Rankings.

use crate::codec::{
    decode_decimal, decode_time, decode_u32, decode_u64, encode_decimal, encode_time, encode_u32,
    encode_u64,
};
use crate::error::map_sqlx;
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::LeaderboardRepository;
use cb_ports::{LeaderboardEntry, LeaderboardWindow, PortResult};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;

const COLUMNS: &str = "rank, handle, address, equity, pnl, pnl_pct, volume, trades, is_bot";

fn from_row(row: &SqliteRow) -> PortResult<LeaderboardEntry> {
    let read = |name: &str| -> PortResult<rust_decimal::Decimal> {
        let raw: String = col(row, name)?;
        decode_decimal(name, &raw)
    };

    Ok(LeaderboardEntry {
        rank: decode_u32("rank", col(row, "rank")?)?,
        handle: col(row, "handle")?,
        address: col(row, "address")?,
        equity: read("equity")?,
        pnl: read("pnl")?,
        pnl_pct: read("pnl_pct")?,
        volume: read("volume")?,
        trades: decode_u64("trades", col(row, "trades")?)?,
        is_bot: col(row, "is_bot")?,
    })
}

#[async_trait]
impl LeaderboardRepository for SqliteDb {
    /// Delete-then-insert inside one transaction.
    ///
    /// Ranks are positions, not properties: one player's equity moving shifts
    /// every rank below them. An incremental update would have to renumber the
    /// tail anyway, and a partial renumbering is a table with two players at
    /// rank 4 — which the primary key would reject, leaving the window
    /// half-updated. Wholesale replacement has none of those states.
    ///
    /// `computed_at` is written in the same transaction, so a reader can never
    /// see a fresh timestamp over stale rows.
    async fn replace_window(
        &self,
        window: LeaderboardWindow,
        computed_at: DateTime<Utc>,
        entries: &[LeaderboardEntry],
    ) -> PortResult<()> {
        let window_key = window.as_str();

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_sqlx("beginning a leaderboard rebuild", e))?;

        sqlx::query("DELETE FROM leaderboard WHERE window_key = ?")
            .bind(window_key)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx("clearing the leaderboard window", e))?;

        for entry in entries {
            let trades = encode_u64("trades", entry.trades)?;
            sqlx::query(
                "INSERT INTO leaderboard \
                 (window_key, rank, handle, address, equity, pnl, pnl_pct, volume, trades, is_bot) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(window_key)
            .bind(encode_u32(entry.rank))
            .bind(&entry.handle)
            .bind(&entry.address)
            .bind(encode_decimal(&entry.equity))
            .bind(encode_decimal(&entry.pnl))
            .bind(encode_decimal(&entry.pnl_pct))
            .bind(encode_decimal(&entry.volume))
            .bind(trades)
            .bind(entry.is_bot)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx(&format!("inserting leaderboard rank {}", entry.rank), e))?;
        }

        sqlx::query(
            "INSERT INTO leaderboard_meta (window_key, computed_at) VALUES (?, ?) \
             ON CONFLICT (window_key) DO UPDATE SET computed_at = excluded.computed_at",
        )
        .bind(window_key)
        .bind(encode_time(computed_at))
        .execute(&mut *tx)
        .await
        .map_err(|e| map_sqlx("stamping the leaderboard window", e))?;

        tx.commit()
            .await
            .map_err(|e| map_sqlx("committing a leaderboard rebuild", e))
    }

    async fn top(
        &self,
        window: LeaderboardWindow,
        limit: u32,
    ) -> PortResult<Vec<LeaderboardEntry>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM leaderboard WHERE window_key = ? ORDER BY rank ASC LIMIT ?"
        ))
        .bind(window.as_str())
        .bind(sql_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx("reading the leaderboard", e))?;

        rows.iter().map(from_row).collect()
    }

    /// Matches the handle exactly as `replace_window` stored it — which is
    /// `users.handle`, the player's own capitalisation. Handle *uniqueness* is
    /// case-insensitive, so there is only ever one stored spelling; a caller
    /// holding a handle from anywhere else in the system will match. Folding
    /// case here would cost the index seek for no reachable case.
    async fn for_handle(
        &self,
        window: LeaderboardWindow,
        handle: &str,
    ) -> PortResult<Option<LeaderboardEntry>> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM leaderboard WHERE window_key = ? AND handle = ?"
        ))
        .bind(window.as_str())
        .bind(handle)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading the leaderboard row for `{handle}`"), e))?;

        row.as_ref().map(from_row).transpose()
    }

    async fn computed_at(&self, window: LeaderboardWindow) -> PortResult<Option<DateTime<Utc>>> {
        let raw: Option<i64> =
            sqlx::query_scalar("SELECT computed_at FROM leaderboard_meta WHERE window_key = ?")
                .bind(window.as_str())
                .fetch_optional(self.pool())
                .await
                .map_err(|e| map_sqlx("reading the leaderboard timestamp", e))?;

        raw.map(|v| decode_time("computed_at", v)).transpose()
    }
}
