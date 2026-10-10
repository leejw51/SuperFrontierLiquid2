//! Bearer sessions.

use crate::codec::{decode_time, encode_time};
use crate::error::{map_sqlx, require_one_row};
use crate::repos::col;
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::SessionRepository;
use cb_ports::{PortResult, Session};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;

const COLUMNS: &str = "token_hash, user_id, created_at, expires_at, revoked";

fn from_row(row: &SqliteRow) -> PortResult<Session> {
    Ok(Session {
        token_hash: col(row, "token_hash")?,
        user_id: col(row, "user_id")?,
        created_at: decode_time("created_at", col(row, "created_at")?)?,
        expires_at: decode_time("expires_at", col(row, "expires_at")?)?,
        revoked: col(row, "revoked")?,
    })
}

#[async_trait]
impl SessionRepository for SqliteDb {
    /// A session for a user who does not exist is a foreign-key violation,
    /// which maps to [`cb_ports::PortError::InvalidArgument`] — the caller
    /// asked for something impossible, not something contended.
    async fn create(&self, session: &Session) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at, revoked) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&session.token_hash)
        .bind(&session.user_id)
        .bind(encode_time(session.created_at))
        .bind(encode_time(session.expires_at))
        .bind(session.revoked)
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| map_sqlx("creating a session", e))
    }

    /// Returns expired and revoked rows too, deliberately: the caller needs to
    /// tell "your session expired, sign in again" from "that token was never
    /// issued", and filtering here would collapse the two.
    async fn by_token_hash(&self, token_hash: &str) -> PortResult<Option<Session>> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM sessions WHERE token_hash = ?"
        ))
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx("loading a session", e))?;
        row.as_ref().map(from_row).transpose()
    }

    async fn revoke(&self, token_hash: &str) -> PortResult<()> {
        let result = sqlx::query("UPDATE sessions SET revoked = 1 WHERE token_hash = ?")
            .bind(token_hash)
            .execute(self.pool())
            .await
            .map_err(|e| map_sqlx("revoking a session", e))?;
        require_one_row("session", result.rows_affected())
    }

    /// Counts sessions this call actually revoked, so revoking twice reports
    /// zero the second time rather than claiming to have done the work again.
    async fn revoke_all_for_user(&self, user_id: &str) -> PortResult<u64> {
        let result =
            sqlx::query("UPDATE sessions SET revoked = 1 WHERE user_id = ? AND revoked = 0")
                .bind(user_id)
                .execute(self.pool())
                .await
                .map_err(|e| map_sqlx(&format!("revoking sessions for `{user_id}`"), e))?;
        Ok(result.rows_affected())
    }

    /// Deletes rows whose `expires_at` is strictly before `before`, so a
    /// session expiring exactly at the cutoff survives. Half-open windows are
    /// used consistently across this crate; see `CandleRepository::range`.
    async fn delete_expired(&self, before: DateTime<Utc>) -> PortResult<u64> {
        let result = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
            .bind(encode_time(before))
            .execute(self.pool())
            .await
            .map_err(|e| map_sqlx("deleting expired sessions", e))?;
        Ok(result.rows_affected())
    }
}
