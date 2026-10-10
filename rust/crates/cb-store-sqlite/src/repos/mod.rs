//! Repository implementations, one module per aggregate.
//!
//! Every trait is implemented on [`crate::SqliteDb`] itself rather than on a
//! per-repository struct. The port hands out `&dyn UserRepository` from
//! `AppDatabase::users()`, and a borrow of `self` satisfies that without an
//! allocation or a second `Arc` hop.
//!
//! Two conventions run through all eight modules.
//!
//! * **`limit` is applied in SQL, never in Rust.** A repository that fetched
//!   everything and truncated would be a denial-of-service waiting for the
//!   first popular market.
//! * **Ordering always has a tie-break on the primary key.** Two rows can
//!   share a microsecond; without a tie-break, paging through them can show
//!   one row twice and skip another.

mod audit;
mod candles;
mod faucet;
mod leaderboard;
mod ledger;
mod markets;
mod sessions;
mod users;

use crate::error::map_sqlx;
use cb_ports::PortResult;
use sqlx::sqlite::{Sqlite, SqliteRow};
use sqlx::Row;

/// Read one column, reporting a decode failure as a port error rather than
/// panicking. A mismatch here means the schema and this code disagree, which
/// is a deployment fault worth a clear message.
pub(crate) fn col<'r, T>(row: &'r SqliteRow, name: &str) -> PortResult<T>
where
    T: sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite>,
{
    row.try_get(name)
        .map_err(|e| map_sqlx(&format!("decoding column `{name}`"), e))
}

/// Clamp a caller-supplied `limit` into SQLite's `LIMIT` parameter.
///
/// `u32` always fits an `i64`, so this cannot fail; it exists so that the
/// intent is named at each call site rather than a bare `i64::from`.
pub(crate) fn sql_limit(limit: u32) -> i64 {
    i64::from(limit)
}
