//! Translating sqlx failures into the port's closed error enum.
//!
//! This is the load-bearing part of the adapter. A caller decides whether to
//! retry, to show the player "that handle is taken", or to fail the request,
//! and that decision must read the same whether the store is SQLite today or
//! PostgreSQL later. If a unique violation arrives as
//! [`PortError::Internal`], every caller above has to learn SQLite's error
//! codes — and the port has bought nothing.
//!
//! The mapping, and why each one:
//!
//! | sqlx failure | [`PortError`] | Caller's response |
//! |---|---|---|
//! | unique constraint | `Conflict` | the value is taken; ask for another |
//! | foreign key constraint | `InvalidArgument` | the referenced row does not exist |
//! | check / not-null constraint | `InvalidArgument` | the caller sent a bad row |
//! | `SQLITE_BUSY` / `SQLITE_LOCKED` | `Contention` | retryable; the busy timeout already elapsed |
//! | `RowNotFound` | `NotFound` | the row is gone |
//! | `PoolClosed`, `PoolTimedOut`, `Io` | `Unavailable` | retryable; the backend is unhealthy |
//! | `Decode`, `ColumnDecode`, `ColumnNotFound` | `Encoding` | schema and code disagree |
//! | anything else | `Internal` | unclassified |
//!
//! `SQLITE_BUSY` deserves a note. With a busy timeout configured, SQLite only
//! raises it after waiting out the timeout, so by the time it reaches here
//! the writer really has been blocked for seconds. `Contention` says exactly
//! that: retryable, but the caller should back off rather than spin.

use cb_ports::PortError;

/// Map a sqlx error onto the port's error enum.
///
/// `context` names the operation, so that a `NotFound` or `Conflict` is
/// actionable in a log without a stack trace.
pub(crate) fn map_sqlx(context: &str, err: sqlx::Error) -> PortError {
    match err {
        sqlx::Error::RowNotFound => PortError::NotFound(context.to_string()),

        sqlx::Error::Database(db) => map_database(context, db.as_ref()),

        // The pool is shut down, or every connection is busy for longer than
        // the acquire timeout. Both mean "come back later", not "you asked
        // for something impossible".
        sqlx::Error::PoolClosed => {
            PortError::Unavailable(format!("{context}: connection pool is closed"))
        }
        sqlx::Error::PoolTimedOut => {
            PortError::Unavailable(format!("{context}: timed out acquiring a connection"))
        }
        sqlx::Error::Io(e) => PortError::Unavailable(format!("{context}: io error: {e}")),

        // Code and schema disagree about a column's name or type. This is a
        // deployment error (a migration that did not run), not a user error.
        sqlx::Error::ColumnNotFound(col) => {
            PortError::Encoding(format!("{context}: no column `{col}` in the result"))
        }
        sqlx::Error::ColumnDecode { index, source } => PortError::Encoding(format!(
            "{context}: column `{index}` failed to decode: {source}"
        )),
        sqlx::Error::Decode(e) => PortError::Encoding(format!("{context}: decode failed: {e}")),

        sqlx::Error::Migrate(e) => PortError::Internal(format!("{context}: migration failed: {e}")),

        other => PortError::Internal(format!("{context}: {other}")),
    }
}

fn map_database(context: &str, db: &dyn sqlx::error::DatabaseError) -> PortError {
    let message = db.message().to_string();

    if db.is_unique_violation() {
        return PortError::Conflict(format!("{context}: {message}"));
    }
    if db.is_foreign_key_violation() {
        return PortError::InvalidArgument(format!(
            "{context}: references a row that does not exist: {message}"
        ));
    }
    if db.is_check_violation() {
        return PortError::InvalidArgument(format!("{context}: {message}"));
    }

    // SQLite primary result codes: 5 = SQLITE_BUSY, 6 = SQLITE_LOCKED. sqlx
    // surfaces extended codes too (e.g. 261 = SQLITE_BUSY_SNAPSHOT), so match
    // on the low byte, which is where SQLite keeps the primary code.
    if let Some(code) = db.code().and_then(|c| c.parse::<i32>().ok()) {
        if matches!(code & 0xff, 5 | 6) {
            return PortError::Contention(format!(
                "{context}: database is locked after the busy timeout elapsed: {message}"
            ));
        }
    }

    // A NOT NULL violation means the caller handed us an incomplete row.
    if message.contains("NOT NULL constraint failed") {
        return PortError::InvalidArgument(format!("{context}: {message}"));
    }

    PortError::Internal(format!("{context}: {message}"))
}

/// Turn "this UPDATE or DELETE matched nothing" into [`PortError::NotFound`].
///
/// Methods such as `revoke` and `set_status` return `PortResult<()>`, so the
/// only way for a caller to learn that the row was absent is for the absence
/// to be an error. Methods that return a count report zero instead, and do
/// not use this.
pub(crate) fn require_one_row(context: &str, rows_affected: u64) -> Result<(), PortError> {
    if rows_affected == 0 {
        Err(PortError::NotFound(context.to_string()))
    } else {
        Ok(())
    }
}
