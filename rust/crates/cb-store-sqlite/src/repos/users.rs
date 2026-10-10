//! Player accounts.

use crate::codec::{decode_time, decode_u64, encode_time};
use crate::error::{map_sqlx, require_one_row};
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::UserRepository;
use cb_ports::{PortResult, User};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;

const COLUMNS: &str = "user_id, handle, address, recovery_hash, is_bot, created_at, last_seen_at";

fn from_row(row: &SqliteRow) -> PortResult<User> {
    Ok(User {
        user_id: col(row, "user_id")?,
        handle: col(row, "handle")?,
        address: col(row, "address")?,
        recovery_hash: col(row, "recovery_hash")?,
        is_bot: col(row, "is_bot")?,
        created_at: decode_time("created_at", col(row, "created_at")?)?,
        last_seen_at: decode_time("last_seen_at", col(row, "last_seen_at")?)?,
    })
}

#[async_trait]
impl UserRepository for SqliteDb {
    /// The lowercased handle is written in the *same statement* as the handle
    /// itself, and `ux_users_handle_lower` rejects a collision. There is no
    /// window in which two registrations for `Alice` and `alice` can both
    /// observe the handle as free, because neither ever reads before writing.
    ///
    /// `to_lowercase` rather than SQLite's `lower()` or a `NOCASE` collation:
    /// both of those fold ASCII only, so `Ω` and `ω` would be two accounts
    /// that look like one.
    async fn create(&self, user: &User) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO users \
             (user_id, handle, handle_lower, address, recovery_hash, is_bot, created_at, last_seen_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&user.user_id)
        .bind(&user.handle)
        .bind(user.handle.to_lowercase())
        .bind(&user.address)
        .bind(&user.recovery_hash)
        .bind(user.is_bot)
        .bind(encode_time(user.created_at))
        .bind(encode_time(user.last_seen_at))
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| map_sqlx(&format!("creating user `{}`", user.handle), e))
    }

    async fn by_id(&self, user_id: &str) -> PortResult<Option<User>> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM users WHERE user_id = ?"))
            .bind(user_id)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("loading user `{user_id}`"), e))?;
        row.as_ref().map(from_row).transpose()
    }

    async fn by_handle(&self, handle: &str) -> PortResult<Option<User>> {
        // Matching on handle_lower, not `lower(handle) = lower(?)`: the
        // former is an index seek, the latter a full scan with a function
        // call per row.
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM users WHERE handle_lower = ?"
        ))
        .bind(handle.to_lowercase())
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("loading user by handle `{handle}`"), e))?;
        row.as_ref().map(from_row).transpose()
    }

    async fn by_address(&self, address: &str) -> PortResult<Option<User>> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM users WHERE address = ?"))
            .bind(address)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("loading user by address `{address}`"), e))?;
        row.as_ref().map(from_row).transpose()
    }

    /// Absence is [`cb_ports::PortError::NotFound`] rather than a silent
    /// no-op: a session refresh for a user who no longer exists is a bug the
    /// caller should hear about.
    async fn touch_last_seen(&self, user_id: &str, at: DateTime<Utc>) -> PortResult<()> {
        let result = sqlx::query("UPDATE users SET last_seen_at = ? WHERE user_id = ?")
            .bind(encode_time(at))
            .bind(user_id)
            .execute(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("touching user `{user_id}`"), e))?;
        require_one_row(&format!("user `{user_id}`"), result.rows_affected())
    }

    async fn list(&self, include_bots: bool, limit: u32) -> PortResult<Vec<User>> {
        // Two literal statements rather than one with a bound flag: a
        // `WHERE ? OR is_bot = 0` predicate is opaque to the query planner
        // and would give up the partial index.
        let sql = if include_bots {
            format!("SELECT {COLUMNS} FROM users ORDER BY created_at, user_id LIMIT ?")
        } else {
            format!(
                "SELECT {COLUMNS} FROM users WHERE is_bot = 0 ORDER BY created_at, user_id LIMIT ?"
            )
        };

        let rows = sqlx::query(&sql)
            .bind(sql_limit(limit))
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_sqlx("listing users", e))?;

        rows.iter().map(from_row).collect()
    }

    async fn count(&self, include_bots: bool) -> PortResult<u64> {
        let sql = if include_bots {
            "SELECT COUNT(*) FROM users"
        } else {
            "SELECT COUNT(*) FROM users WHERE is_bot = 0"
        };

        let count: i64 = sqlx::query_scalar(sql)
            .fetch_one(self.pool())
            .await
            .map_err(|e| map_sqlx("counting users", e))?;
        decode_u64("count", count)
    }
}
