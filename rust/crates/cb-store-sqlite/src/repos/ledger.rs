//! The off-chain view of value movements.

use crate::codec::{
    decode_decimal, decode_opt_u64, decode_time, encode_decimal, encode_opt_u64, encode_time,
};
use crate::error::map_sqlx;
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::LedgerRepository;
use cb_ports::{LedgerEntry, PortError, PortResult};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::sqlite::SqliteRow;
use sqlx::{Executor, Sqlite};
use std::collections::BTreeMap;

const COLUMNS: &str =
    "entry_id, user_id, kind, asset, amount, balance_after, memo, tx_hash, height, at";

const INSERT: &str = "INSERT INTO ledger_entries \
     (entry_id, user_id, kind, asset, amount, balance_after, memo, tx_hash, height, at) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

fn from_row(row: &SqliteRow) -> PortResult<LedgerEntry> {
    let amount: String = col(row, "amount")?;
    let balance_after: String = col(row, "balance_after")?;
    Ok(LedgerEntry {
        entry_id: col(row, "entry_id")?,
        user_id: col(row, "user_id")?,
        kind: col(row, "kind")?,
        asset: col(row, "asset")?,
        amount: decode_decimal("amount", &amount)?,
        balance_after: decode_decimal("balance_after", &balance_after)?,
        memo: col(row, "memo")?,
        tx_hash: col(row, "tx_hash")?,
        height: decode_opt_u64("height", col(row, "height")?)?,
        at: decode_time("at", col(row, "at")?)?,
    })
}

async fn insert_with<'e, E>(executor: E, entry: &LedgerEntry) -> PortResult<()>
where
    E: Executor<'e, Database = Sqlite>,
{
    let height = encode_opt_u64("height", entry.height)?;
    sqlx::query(INSERT)
        .bind(&entry.entry_id)
        .bind(&entry.user_id)
        .bind(&entry.kind)
        .bind(&entry.asset)
        .bind(encode_decimal(&entry.amount))
        .bind(encode_decimal(&entry.balance_after))
        .bind(&entry.memo)
        .bind(&entry.tx_hash)
        .bind(height)
        .bind(encode_time(entry.at))
        .execute(executor)
        .await
        .map(|_| ())
        .map_err(|e| map_sqlx(&format!("appending ledger entry `{}`", entry.entry_id), e))
}

#[async_trait]
impl LedgerRepository for SqliteDb {
    async fn append(&self, entry: &LedgerEntry) -> PortResult<()> {
        insert_with(self.pool(), entry).await
    }

    /// Atomic: either every entry lands or none does.
    ///
    /// This is not a convenience. A fill produces a fee entry and a realised-PnL
    /// entry that only balance as a pair; a funding tick produces a payment and
    /// a receipt. Writing half of such a set would leave the off-chain ledger
    /// asserting that value appeared from nowhere, and nothing later would
    /// notice, because these rows are append-only and never revisited.
    ///
    /// The rollback is SQLite's, not ours: a constraint violation on the third
    /// entry aborts the transaction, and `Transaction`'s `Drop` rolls back
    /// whatever was already applied.
    async fn append_many(&self, entries: &[LedgerEntry]) -> PortResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_sqlx("beginning a ledger batch", e))?;

        for entry in entries {
            insert_with(&mut *tx, entry).await?;
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx("committing a ledger batch", e))
    }

    /// `kinds = None` means every kind; `kinds = Some(&[])` means no kind, and
    /// therefore no rows. The empty slice is answered without touching the
    /// database — an `IN ()` clause is not valid SQL, and silently turning it
    /// into "all kinds" would be a filter that does the opposite of what it
    /// says.
    async fn for_user(
        &self,
        user_id: &str,
        kinds: Option<&[String]>,
        limit: u32,
    ) -> PortResult<Vec<LedgerEntry>> {
        if kinds.is_some_and(<[String]>::is_empty) {
            return Ok(Vec::new());
        }

        let mut sql = format!("SELECT {COLUMNS} FROM ledger_entries WHERE user_id = ?");
        if let Some(kinds) = kinds {
            // One placeholder per kind, bound below. String-interpolating the
            // values would be an injection hole; `kind` is caller-supplied.
            let placeholders = vec!["?"; kinds.len()].join(", ");
            sql.push_str(&format!(" AND kind IN ({placeholders})"));
        }
        sql.push_str(" ORDER BY at DESC, entry_id DESC LIMIT ?");

        let mut query = sqlx::query(&sql).bind(user_id);
        for kind in kinds.unwrap_or(&[]) {
            query = query.bind(kind);
        }
        let rows = query
            .bind(sql_limit(limit))
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("reading the ledger for `{user_id}`"), e))?;

        rows.iter().map(from_row).collect()
    }

    /// Half-open window `[from, to)`, so consecutive windows partition time
    /// and a movement is never counted in two reports.
    ///
    /// Summed in Rust with exact `Decimal` arithmetic. `SUM` over the TEXT
    /// amount column would make SQLite coerce to REAL; a ledger that reports
    /// its net movement to seventeen significant figures and then rounds is
    /// worse than one that is slow.
    async fn net_by_asset(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> PortResult<Vec<(String, Decimal)>> {
        if to < from {
            return Err(PortError::InvalidArgument(format!(
                "net_by_asset window ends ({to}) before it starts ({from})"
            )));
        }

        let rows = sqlx::query(
            "SELECT asset, amount FROM ledger_entries \
             WHERE user_id = ? AND at >= ? AND at < ? ORDER BY asset",
        )
        .bind(user_id)
        .bind(encode_time(from))
        .bind(encode_time(to))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("netting the ledger for `{user_id}`"), e))?;

        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        for row in &rows {
            let asset: String = col(row, "asset")?;
            let amount: String = col(row, "amount")?;
            let amount = decode_decimal("amount", &amount)?;
            let slot = totals.entry(asset).or_default();
            *slot = slot.checked_add(amount).ok_or_else(|| {
                PortError::Internal("ledger movements for one asset overflow a Decimal".to_string())
            })?;
        }

        Ok(totals.into_iter().collect())
    }
}
