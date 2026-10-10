//! The append-only audit trail.

use crate::codec::{decode_json, decode_time, encode_time};
use crate::error::map_sqlx;
use crate::repos::{col, sql_limit};
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::AuditRepository;
use cb_ports::{AuditRecord, PortResult};
use sqlx::sqlite::SqliteRow;

const COLUMNS: &str = "record_id, actor, \"action\", subject, detail, source_ip, at";

fn from_row(row: &SqliteRow) -> PortResult<AuditRecord> {
    let detail: String = col(row, "detail")?;
    Ok(AuditRecord {
        record_id: col(row, "record_id")?,
        actor: col(row, "actor")?,
        action: col(row, "action")?,
        subject: col(row, "subject")?,
        detail: decode_json("detail", &detail)?,
        source_ip: col(row, "source_ip")?,
        at: decode_time("at", col(row, "at")?)?,
    })
}

#[async_trait]
impl AuditRepository for SqliteDb {
    /// `detail` is serialised to TEXT here rather than bound as JSON: the
    /// workspace's sqlx build omits the `json` feature, so there is no
    /// `Json<T>` codec to lean on. The conversion is explicit in both
    /// directions, which also means a malformed stored document surfaces as
    /// [`cb_ports::PortError::Encoding`] instead of a panic.
    ///
    /// There is deliberately no update or delete. An audit trail that can be
    /// edited proves nothing, and the absence of the method is the enforcement.
    async fn append(&self, record: &AuditRecord) -> PortResult<()> {
        let detail = serde_json::to_string(&record.detail).map_err(|e| {
            cb_ports::PortError::Encoding(format!("audit detail is not serialisable: {e}"))
        })?;

        sqlx::query(
            "INSERT INTO audit_log (record_id, actor, \"action\", subject, detail, source_ip, at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.record_id)
        .bind(&record.actor)
        .bind(&record.action)
        .bind(&record.subject)
        .bind(detail)
        .bind(&record.source_ip)
        .bind(encode_time(record.at))
        .execute(self.pool())
        .await
        .map(|_| ())
        .map_err(|e| map_sqlx(&format!("appending audit record `{}`", record.record_id), e))
    }

    async fn for_subject(&self, subject: &str, limit: u32) -> PortResult<Vec<AuditRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM audit_log WHERE subject = ? \
             ORDER BY at DESC, record_id DESC LIMIT ?"
        ))
        .bind(subject)
        .bind(sql_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading the audit trail for `{subject}`"), e))?;

        rows.iter().map(from_row).collect()
    }

    async fn for_actor(&self, actor: &str, limit: u32) -> PortResult<Vec<AuditRecord>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM audit_log WHERE actor = ? \
             ORDER BY at DESC, record_id DESC LIMIT ?"
        ))
        .bind(actor)
        .bind(sql_limit(limit))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("reading the audit trail by `{actor}`"), e))?;

        rows.iter().map(from_row).collect()
    }
}
