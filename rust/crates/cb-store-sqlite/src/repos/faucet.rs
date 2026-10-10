//! Faucet grants — the only mint in the system.

use crate::codec::{decode_decimal, decode_time, decode_u64, encode_decimal, encode_time};
use crate::error::map_sqlx;
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::FaucetRepository;
use cb_ports::{FaucetClaim, PortResult};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::sqlite::SqliteRow;
use std::collections::BTreeMap;

const COLUMNS: &str = "claim_id, user_id, asset, amount, tx_hash, claimed_at";

fn from_row(row: &SqliteRow) -> PortResult<FaucetClaim> {
    let amount: String = col(row, "amount")?;
    Ok(FaucetClaim {
        claim_id: col(row, "claim_id")?,
        user_id: col(row, "user_id")?,
        asset: col(row, "asset")?,
        amount: decode_decimal("amount", &amount)?,
        tx_hash: col(row, "tx_hash")?,
        claimed_at: decode_time("claimed_at", col(row, "claimed_at")?)?,
    })
}

#[async_trait]
impl FaucetRepository for SqliteDb {
    async fn record_claim(&self, claim: &FaucetClaim) -> PortResult<()> {
        sqlx::query(
            "INSERT INTO faucet_claims (claim_id, user_id, asset, amount, tx_hash, claimed_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&claim.claim_id)
        .bind(&claim.user_id)
        .bind(&claim.asset)
        .bind(encode_decimal(&claim.amount))
        .bind(&claim.tx_hash)
        .bind(encode_time(claim.claimed_at))
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| map_sqlx(&format!("recording faucet claim `{}`", claim.claim_id), e))
    }

    /// Reads the newest row rather than `MAX(claimed_at)` so the answer comes
    /// from a one-row seek on `ix_faucet_user_claimed`. This is on the faucet
    /// cooldown path and runs on every claim attempt.
    async fn last_claim_at(&self, user_id: &str) -> PortResult<Option<DateTime<Utc>>> {
        let raw: Option<i64> = sqlx::query_scalar(
            "SELECT claimed_at FROM faucet_claims WHERE user_id = ? \
             ORDER BY claimed_at DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading last faucet claim for `{user_id}`"), e))?;

        raw.map(|v| decode_time("claimed_at", v)).transpose()
    }

    async fn claim_count(&self, user_id: &str) -> PortResult<u64> {
        // COUNT(DISTINCT claim_id), not COUNT(*): one claim granting three
        // assets writes three rows but is still one claim, and the API's
        // `claims_made` means claims.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT claim_id) FROM faucet_claims WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_one(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("counting faucet claims for `{user_id}`"), e))?;
        decode_u64("count", count)
    }

    async fn history(&self, user_id: &str, limit: u32) -> PortResult<Vec<FaucetClaim>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM faucet_claims WHERE user_id = ? \
             ORDER BY claimed_at DESC, claim_id DESC LIMIT ?"
        ))
        .bind(user_id)
        .bind(sql_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading faucet history for `{user_id}`"), e))?;

        rows.iter().map(from_row).collect()
    }

    /// Summed in Rust, not in SQL.
    ///
    /// `SUM(amount)` over a TEXT column makes SQLite coerce each value to a
    /// REAL — reintroducing exactly the binary-floating-point error the TEXT
    /// encoding exists to prevent, and doing it silently. Conservation checks
    /// reconcile against this number, so it has to be exact. The cost is
    /// reading every claim row; the index is covering, so it is an index scan
    /// rather than a table scan, and this is a reporting call rather than a
    /// hot path.
    async fn total_granted(&self) -> PortResult<Vec<(String, Decimal)>> {
        let rows = sqlx::query("SELECT asset, amount FROM faucet_claims ORDER BY asset")
            .fetch_all(self.pool())
            .await
            .map_err(|e| map_sqlx("totalling faucet grants", e))?;

        // BTreeMap so the result is ordered by asset deterministically, which
        // makes a conservation report diffable between runs.
        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        for row in &rows {
            let asset: String = col(row, "asset")?;
            let amount: String = col(row, "amount")?;
            let amount = decode_decimal("amount", &amount)?;
            let slot = totals.entry(asset).or_default();
            *slot = slot.checked_add(amount).ok_or_else(|| {
                cb_ports::PortError::Internal(
                    "faucet grants for one asset overflow a Decimal; \
                     conservation accounting cannot be represented"
                        .to_string(),
                )
            })?;
        }

        Ok(totals.into_iter().collect())
    }
}
