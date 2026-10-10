//! Account facts the port models have no column for.
//!
//! Three things the API contract requires are not in `cb-ports`:
//! **suspensions** (§10), **session metadata** — a session id, the user agent,
//! a truncated client address, a last-used time (§9) — and nothing else fits
//! in `UserRepository` or `SessionRepository` without changing a frozen trait.
//!
//! They live here, behind [`cb_ports::KvStore`], which is a port the gateway
//! already depends on and which a deployment can back with RocksDB or a
//! SQLite table. That keeps the composition-root rule intact: this module
//! names no backend either.
//!
//! ## Why the address is truncated
//!
//! A session list is a security feature — "is that me?" — and answering that
//! needs a rough location, not an exact one. Storing `203.0.113.7` turns the
//! feature into a movement log that an attacker who reaches the account also
//! reaches. `203.0.113.0/24` answers the question and does not.

use cb_ports::{KvBatch, KvStore, PortResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Key-value namespace for suspensions.
const SUSPENSIONS: &str = "gateway_suspensions";
/// Key-value namespace for session metadata.
const SESSION_META: &str = "gateway_sessions";
/// Key-value namespace for handle changes.
const HANDLES: &str = "gateway_handles";

/// An operator's suspension of an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suspension {
    /// Who was suspended.
    pub user_id: String,
    /// Why, in the operator's words.
    pub reason: String,
    /// The acting operator's login address.
    pub by: String,
    /// When.
    pub at: DateTime<Utc>,
}

/// What `GET /user/sessions` shows about one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Short, non-secret identifier. Derived from the token hash, so it is
    /// stable and reveals nothing about the token.
    pub session_id: String,
    /// The hash the session is stored under, for revocation.
    pub token_hash: String,
    /// Whose session.
    pub user_id: String,
    /// When it was issued.
    pub issued_at: DateTime<Utc>,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
    /// When it was last used.
    pub last_used_at: DateTime<Utc>,
    /// The browser that created it, as it described itself.
    pub user_agent: Option<String>,
    /// A truncated client address. See the module docs.
    pub ip_prefix: Option<String>,
}

/// Suspensions and session metadata, over a key-value port.
pub struct Directory {
    kv: Arc<dyn KvStore>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Directory { .. }")
    }
}

impl Directory {
    /// Build a directory over a key-value store.
    pub fn new(kv: Arc<dyn KvStore>) -> Self {
        Self { kv }
    }

    /// The short, non-secret id for a session token hash.
    pub fn session_id_for(token_hash: &str) -> String {
        format!("ses_{}", &token_hash[..8.min(token_hash.len())])
    }

    /// Truncate a client address to a prefix.
    ///
    /// IPv4 keeps three octets; IPv6 keeps the routing prefix. Anything
    /// unrecognisable is dropped rather than stored verbatim.
    pub fn truncate_address(address: &str) -> Option<String> {
        let address = address.trim();
        if address.is_empty() || address == "unknown" {
            return None;
        }
        if let Ok(ip) = address.parse::<std::net::IpAddr>() {
            return Some(match ip {
                std::net::IpAddr::V4(v4) => {
                    let o = v4.octets();
                    format!("{}.{}.{}.0/24", o[0], o[1], o[2])
                }
                std::net::IpAddr::V6(v6) => {
                    let s = v6.segments();
                    format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
                }
            });
        }
        None
    }

    // ---- suspensions ----

    /// Suspend an account.
    pub fn suspend(&self, suspension: &Suspension) -> PortResult<()> {
        let encoded = serde_json::to_vec(suspension)
            .map_err(|e| cb_ports::PortError::Encoding(e.to_string()))?;
        let mut batch = KvBatch::new();
        batch.put(suspension.user_id.as_bytes().to_vec(), encoded);
        self.kv.write_batch(SUSPENSIONS, batch)
    }

    /// Lift a suspension. Idempotent.
    pub fn unsuspend(&self, user_id: &str) -> PortResult<()> {
        let mut batch = KvBatch::new();
        batch.delete(user_id.as_bytes().to_vec());
        self.kv.write_batch(SUSPENSIONS, batch)
    }

    /// The suspension on an account, if any.
    pub fn suspension(&self, user_id: &str) -> PortResult<Option<Suspension>> {
        let Some(raw) = self.kv.get(SUSPENSIONS, user_id.as_bytes())? else {
            return Ok(None);
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| cb_ports::PortError::Encoding(e.to_string()))
    }

    /// Whether an account is suspended.
    pub fn is_suspended(&self, user_id: &str) -> bool {
        // A storage failure must not silently un-suspend somebody, so an error
        // is read as "suspended" and logged.
        match self.suspension(user_id) {
            Ok(found) => found.is_some(),
            Err(e) => {
                tracing::warn!(
                    target: "cb_gateway::directory",
                    error = %e,
                    user = user_id,
                    "suspension lookup failed; treating the account as suspended"
                );
                true
            }
        }
    }

    // ---- session metadata ----

    /// Record a newly issued session.
    pub fn record_session(&self, meta: &SessionMeta) -> PortResult<()> {
        let encoded =
            serde_json::to_vec(meta).map_err(|e| cb_ports::PortError::Encoding(e.to_string()))?;
        let mut batch = KvBatch::new();
        batch.put(session_key(&meta.user_id, &meta.session_id), encoded);
        self.kv.write_batch(SESSION_META, batch)
    }

    /// Every session recorded for a user, newest first.
    pub fn sessions_for(&self, user_id: &str) -> PortResult<Vec<SessionMeta>> {
        let prefix = format!("{user_id}/");
        let mut found: Vec<SessionMeta> = self
            .kv
            .scan_prefix(SESSION_META, prefix.as_bytes())?
            .into_iter()
            .filter_map(|(_, raw)| serde_json::from_slice(&raw).ok())
            .collect();
        found.sort_by_key(|a| std::cmp::Reverse(a.issued_at));
        Ok(found)
    }

    /// One session of a user's, by its short id.
    pub fn session(&self, user_id: &str, session_id: &str) -> PortResult<Option<SessionMeta>> {
        let Some(raw) = self
            .kv
            .get(SESSION_META, &session_key(user_id, session_id))?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| cb_ports::PortError::Encoding(e.to_string()))
    }

    /// Note that a session was used.
    pub fn touch_session(
        &self,
        user_id: &str,
        session_id: &str,
        at: DateTime<Utc>,
    ) -> PortResult<()> {
        let Some(mut meta) = self.session(user_id, session_id)? else {
            return Ok(());
        };
        meta.last_used_at = at;
        self.record_session(&meta)
    }

    /// Forget a session's metadata.
    pub fn forget_session(&self, user_id: &str, session_id: &str) -> PortResult<()> {
        let mut batch = KvBatch::new();
        batch.delete(session_key(user_id, session_id));
        self.kv.write_batch(SESSION_META, batch)
    }

    /// Drop metadata for sessions that have expired.
    pub fn prune_sessions(&self, user_id: &str, now: DateTime<Utc>) -> PortResult<u64> {
        let mut removed = 0;
        for meta in self.sessions_for(user_id)? {
            if meta.expires_at <= now {
                self.forget_session(user_id, &meta.session_id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Handle changes.
///
/// `UserRepository` has no update method and `AppDatabase` is a frozen port,
/// so a handle change cannot be written back to the user row. The change is
/// recorded here instead, as a pair of mappings — `user_id → handle` and
/// `handle → user_id` — and [`Directory::handle_is_free`] consults both this
/// table and the repository so uniqueness still holds across the two.
///
/// This is a workaround for a missing port method, and it is worth naming as
/// one: the right fix is `UserRepository::rename`, which would make the
/// repository enforce uniqueness the way it does at creation.
impl Directory {
    /// The handle a user has changed to, if any.
    pub fn handle_of(&self, user_id: &str) -> PortResult<Option<String>> {
        let Some(raw) = self.kv.get(HANDLES, user_key(user_id).as_slice())? else {
            return Ok(None);
        };
        Ok(String::from_utf8(raw).ok())
    }

    /// Which user has claimed a handle here, if any.
    pub fn owner_of_handle(&self, handle: &str) -> PortResult<Option<String>> {
        let key = handle_key(&handle.to_ascii_lowercase());
        let Some(raw) = self.kv.get(HANDLES, key.as_slice())? else {
            return Ok(None);
        };
        Ok(String::from_utf8(raw).ok())
    }

    /// Record a handle change, releasing the previous one.
    pub fn set_handle(&self, user_id: &str, handle: &str) -> PortResult<()> {
        let handle = handle.to_ascii_lowercase();
        let mut batch = KvBatch::new();
        if let Some(previous) = self.handle_of(user_id)? {
            if previous != handle {
                batch.delete(handle_key(&previous));
            }
        }
        batch.put(user_key(user_id), handle.clone().into_bytes());
        batch.put(handle_key(&handle), user_id.as_bytes().to_vec());
        self.kv.write_batch(HANDLES, batch)
    }
}

fn user_key(user_id: &str) -> Vec<u8> {
    format!("u/{user_id}").into_bytes()
}

fn handle_key(handle: &str) -> Vec<u8> {
    format!("h/{handle}").into_bytes()
}

fn session_key(user_id: &str, session_id: &str) -> Vec<u8> {
    format!("{user_id}/{session_id}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryKv;

    fn directory() -> Directory {
        Directory::new(Arc::new(InMemoryKv::new()))
    }

    fn meta(user: &str, id: &str, issued_minutes: i64) -> SessionMeta {
        let base = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .expect("static")
            .with_timezone(&Utc);
        SessionMeta {
            session_id: id.into(),
            token_hash: format!("hash-{id}"),
            user_id: user.into(),
            issued_at: base + chrono::Duration::minutes(issued_minutes),
            expires_at: base + chrono::Duration::hours(24),
            last_used_at: base,
            user_agent: Some("Mozilla/5.0".into()),
            ip_prefix: Some("203.0.113.0/24".into()),
        }
    }

    #[test]
    fn a_suspension_round_trips_and_can_be_lifted() {
        let directory = directory();
        assert!(!directory.is_suspended("usr_a"));
        directory
            .suspend(&Suspension {
                user_id: "usr_a".into(),
                reason: "abusive order flow".into(),
                by: "0xoperator".into(),
                at: Utc::now(),
            })
            .expect("suspend");
        assert!(directory.is_suspended("usr_a"));
        assert_eq!(
            directory
                .suspension("usr_a")
                .expect("read")
                .expect("present")
                .reason,
            "abusive order flow"
        );
        directory.unsuspend("usr_a").expect("unsuspend");
        assert!(!directory.is_suspended("usr_a"));
        // Lifting a suspension that is not there is not an error.
        directory.unsuspend("usr_a").expect("idempotent");
    }

    #[test]
    fn one_users_suspension_does_not_affect_another() {
        let directory = directory();
        directory
            .suspend(&Suspension {
                user_id: "usr_a".into(),
                reason: "x".into(),
                by: "0x".into(),
                at: Utc::now(),
            })
            .expect("suspend");
        assert!(directory.is_suspended("usr_a"));
        assert!(!directory.is_suspended("usr_b"));
    }

    #[test]
    fn sessions_are_listed_newest_first_and_scoped_to_their_user() {
        let directory = directory();
        directory
            .record_session(&meta("usr_a", "ses_1", 0))
            .expect("record");
        directory
            .record_session(&meta("usr_a", "ses_2", 10))
            .expect("record");
        directory
            .record_session(&meta("usr_b", "ses_3", 5))
            .expect("record");

        let sessions = directory.sessions_for("usr_a").expect("list");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "ses_2", "newest first");
        assert_eq!(directory.sessions_for("usr_b").expect("list").len(), 1);
    }

    #[test]
    fn one_users_session_is_not_reachable_through_another() {
        let directory = directory();
        directory
            .record_session(&meta("usr_a", "ses_1", 0))
            .expect("record");
        assert!(directory.session("usr_a", "ses_1").expect("read").is_some());
        assert!(
            directory.session("usr_b", "ses_1").expect("read").is_none(),
            "a session id alone must not reach another account's session"
        );
    }

    #[test]
    fn touching_a_session_updates_only_its_last_use() {
        let directory = directory();
        directory
            .record_session(&meta("usr_a", "ses_1", 0))
            .expect("record");
        let later = Utc::now();
        directory
            .touch_session("usr_a", "ses_1", later)
            .expect("touch");
        let found = directory
            .session("usr_a", "ses_1")
            .expect("read")
            .expect("present");
        assert_eq!(found.last_used_at, later);
        assert_eq!(found.session_id, "ses_1");
    }

    #[test]
    fn expired_session_metadata_is_pruned() {
        let directory = directory();
        directory
            .record_session(&meta("usr_a", "ses_1", 0))
            .expect("record");
        let much_later = DateTime::parse_from_rfc3339("2026-09-20T12:00:00Z")
            .expect("static")
            .with_timezone(&Utc);
        assert_eq!(
            directory
                .prune_sessions("usr_a", much_later)
                .expect("prune"),
            1
        );
        assert!(directory.sessions_for("usr_a").expect("list").is_empty());
    }

    #[test]
    fn a_handle_change_is_recorded_both_ways_and_releases_the_old_name() {
        let directory = directory();
        assert!(directory.handle_of("usr_a").expect("read").is_none());

        directory.set_handle("usr_a", "satoshi").expect("set");
        assert_eq!(
            directory.handle_of("usr_a").expect("read").as_deref(),
            Some("satoshi")
        );
        assert_eq!(
            directory
                .owner_of_handle("SATOSHI")
                .expect("read")
                .as_deref(),
            Some("usr_a")
        );

        directory.set_handle("usr_a", "satoshi_v2").expect("rename");
        assert_eq!(
            directory.handle_of("usr_a").expect("read").as_deref(),
            Some("satoshi_v2")
        );
        assert!(
            directory
                .owner_of_handle("satoshi")
                .expect("read")
                .is_none(),
            "the previous handle must be released"
        );
    }

    #[test]
    fn a_session_id_is_short_and_reveals_nothing_about_the_token() {
        let hash = "a".repeat(64);
        let id = Directory::session_id_for(&hash);
        assert_eq!(id, "ses_aaaaaaaa");
        assert!(id.len() < hash.len());
    }

    #[test]
    fn addresses_are_truncated_to_a_prefix_not_stored_exactly() {
        assert_eq!(
            Directory::truncate_address("203.0.113.7").as_deref(),
            Some("203.0.113.0/24")
        );
        assert_eq!(
            Directory::truncate_address("2001:db8:85a3:1::8a2e:370:7334").as_deref(),
            Some("2001:db8:85a3:1::/64")
        );
        assert!(Directory::truncate_address("unknown").is_none());
        assert!(Directory::truncate_address("").is_none());
        assert!(Directory::truncate_address("not-an-address").is_none());
    }
}
