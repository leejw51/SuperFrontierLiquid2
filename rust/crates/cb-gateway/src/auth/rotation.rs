//! The guard against a silently rotated server secret.
//!
//! Every account's trading key is `HKDF-SHA256(server_secret, login_address)`
//! and is never stored, because it can always be recomputed. The consequence is
//! that changing the secret changes *which account each user owns*, and nothing
//! in the system notices: `GET /account` derives the new address, finds a fresh
//! empty account and answers `200` with a zero balance. The money is still on
//! chain under the old address, reachable only by restoring the old secret, and
//! every order resting under the old address becomes uncancellable because the
//! cancel is signed by a key the engine sees as a stranger's (§21 of the flow
//! review).
//!
//! So the gateway records a **fingerprint of its own derivation** the first
//! time it starts against a database, and refuses to start when that
//! fingerprint changes. The fingerprint is the hash of the trading address the
//! zero login address derives to: it is not the secret, it reveals nothing
//! about the secret, and it changes exactly when derived accounts would.
//!
//! It is stored as an audit record rather than in the key-value store, because
//! the audit trail is the durable store this gateway actually has — the
//! key-value store is in-memory today, and a check that forgets on every
//! restart is not a check. It also puts "the server secret changed" in the one
//! place an investigation looks.

use crate::auth::eth;
use cb_ports::{AppDatabase, AuditRecord, Clock};
use cb_types::Address;
use std::sync::Arc;

/// The audit subject every fingerprint record is filed under.
pub const SUBJECT: &str = "gateway.server_secret";

/// The audit action a fingerprint record carries.
pub const ACTION: &str = "gateway.server_secret.fingerprint";

/// What a check of the recorded fingerprint found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretCheck {
    /// Nothing was recorded before; this is a first start.
    FirstStart {
        /// The fingerprint that was recorded.
        fingerprint: String,
    },
    /// The recorded fingerprint matches the configured secret.
    Unchanged,
    /// The recorded fingerprint does not match, and accounts exist.
    Rotated {
        /// What was recorded before.
        recorded: String,
        /// What the configured secret derives now.
        current: String,
        /// How many accounts the change orphans.
        accounts: u64,
    },
}

/// A fingerprint of the trading-key derivation this secret produces.
///
/// The zero address is used as the probe because it is a valid input to the
/// derivation and belongs to nobody, so the value leaks no user's address and
/// none of the secret.
pub fn fingerprint(server_secret: &[u8]) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let (_key, probe) = eth::derive_trading_key(server_secret, &Address([0u8; 20]))
        .map_err(|e| anyhow::anyhow!("deriving the server secret fingerprint: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"cb-gateway/server-secret-fingerprint/v1");
    hasher.update(probe.0);
    Ok(hex::encode(&hasher.finalize()[..16]))
}

/// Compare the configured secret against what this database was built with.
///
/// Records the fingerprint when there is none, which is what makes a first
/// start against a fresh database silent. Never writes on a mismatch: the
/// caller decides whether the rotation is allowed, and recording it before that
/// decision would destroy the evidence of what the accounts were derived with.
pub async fn check(
    db: &Arc<dyn AppDatabase>,
    clock: &Arc<dyn Clock>,
    server_secret: &[u8],
) -> anyhow::Result<SecretCheck> {
    let current = fingerprint(server_secret)?;
    let recorded = db
        .audit()
        .for_subject(SUBJECT, 16)
        .await?
        .into_iter()
        .find(|record| record.action == ACTION)
        .and_then(|record| {
            record
                .detail
                .get("fingerprint")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    match recorded {
        None => {
            record(db, clock, &current, "first start").await?;
            Ok(SecretCheck::FirstStart {
                fingerprint: current,
            })
        }
        Some(recorded) if recorded == current => Ok(SecretCheck::Unchanged),
        Some(recorded) => Ok(SecretCheck::Rotated {
            recorded,
            current,
            accounts: db.users().count(true).await?,
        }),
    }
}

/// Write a fingerprint into the audit trail.
pub async fn record(
    db: &Arc<dyn AppDatabase>,
    clock: &Arc<dyn Clock>,
    fingerprint: &str,
    why: &str,
) -> anyhow::Result<()> {
    db.audit()
        .append(&AuditRecord {
            record_id: format!("aud_{}", uuid::Uuid::new_v4().simple()),
            actor: None,
            action: ACTION.to_string(),
            subject: SUBJECT.to_string(),
            detail: serde_json::json!({ "fingerprint": fingerprint, "why": why }),
            source_ip: None,
            at: clock.now(),
        })
        .await?;
    Ok(())
}

/// The message a refused start prints.
///
/// Spelled out at length on purpose: this is the one moment where an operator
/// can still recover the accounts, and the recovery is "put the old secret
/// back", which nothing else in the system will tell them.
pub fn refusal(recorded: &str, current: &str, accounts: u64) -> String {
    format!(
        "the server secret does not match the one this database's accounts were derived with \
         (recorded derivation {recorded}, configured derivation {current}).\n\n\
         Every trading key is HKDF-SHA256(server_secret, login_address), so starting with this \
         secret would give all {accounts} existing accounts new, empty on-chain addresses. Their \
         balances would stay where they are, reachable only with the previous secret; \
         `GET /account` would answer 200 with nothing in it; and every order resting under the \
         old addresses would become uncancellable.\n\n\
         Restore the previous CB_GATEWAY_SERVER_SECRET, or — if the orphaning is intended, for \
         instance because the chain and this database are both scratch — start once with \
         --allow-server-secret-rotation."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryDatabase;
    use cb_ports::SystemClock;

    fn parts() -> (Arc<dyn AppDatabase>, Arc<dyn Clock>) {
        (InMemoryDatabase::shared(), Arc::new(SystemClock))
    }

    #[test]
    fn the_fingerprint_follows_the_derivation_not_the_bytes() {
        let a = fingerprint(b"a server secret that is at least 32 bytes long").expect("a");
        let b = fingerprint(b"a server secret that is at least 32 bytes long").expect("b");
        let c = fingerprint(b"a different secret that is also at least 32 by").expect("c");
        assert_eq!(a, b, "the same secret must fingerprint the same");
        assert_ne!(a, c, "a different secret must fingerprint differently");
        assert_eq!(a.len(), 32, "sixteen bytes of hex");
    }

    #[tokio::test]
    async fn a_first_start_records_and_a_restart_is_silent() {
        let (db, clock) = parts();
        let secret = b"a server secret that is at least 32 bytes long";
        assert!(matches!(
            check(&db, &clock, secret).await.expect("checked"),
            SecretCheck::FirstStart { .. }
        ));
        assert_eq!(
            check(&db, &clock, secret).await.expect("checked"),
            SecretCheck::Unchanged
        );
    }

    #[tokio::test]
    async fn a_rotated_secret_is_reported_rather_than_accepted_in_silence() {
        // §21 of the flow review: rotating the secret used to be completely
        // silent — the accounts simply became somebody else's, and the only
        // symptom was every balance reading zero.
        let (db, clock) = parts();
        check(
            &db,
            &clock,
            b"a server secret that is at least 32 bytes long",
        )
        .await
        .expect("first start");

        let outcome = check(
            &db,
            &clock,
            b"a different secret that is also at least 32 by",
        )
        .await
        .expect("checked");
        match outcome {
            SecretCheck::Rotated {
                recorded, current, ..
            } => assert_ne!(recorded, current),
            other => panic!("a rotation must be reported, got {other:?}"),
        }

        // And the rotation must not have been recorded by the check itself:
        // the evidence of what the accounts were derived with is the only way
        // back.
        assert!(matches!(
            check(
                &db,
                &clock,
                b"a server secret that is at least 32 bytes long"
            )
            .await
            .expect("checked"),
            SecretCheck::Unchanged
        ));
    }
}
