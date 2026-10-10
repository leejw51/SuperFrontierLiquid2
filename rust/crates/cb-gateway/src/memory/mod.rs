//! In-memory implementations of the ports the gateway consumes.
//!
//! ## Why these exist outside `#[cfg(test)]`
//!
//! Two reasons, and the first is the important one.
//!
//! * **Integration tests are separate crates.** A `#[cfg(test)]` module is
//!   invisible to `tests/*.rs`, so a test double that only exists under
//!   `cfg(test)` cannot be used by the tests that matter most — the ones that
//!   drive the real router end to end.
//! * **The composition root needs a default.** `cb-store-sqlite` is being
//!   written in parallel. Until it lands, `--store memory` gives a gateway you
//!   can actually run, and afterwards it remains the fastest way to start a
//!   throwaway instance. Nothing above `main.rs` knows which it got.
//!
//! These are honest implementations of the traits, not stubs: handle
//! uniqueness is enforced case-insensitively *inside* the repository, exactly
//! as `UserRepository::create` requires, because a check-then-insert in the
//! gateway would race two registrations of the same handle through the gap.
//!
//! Nothing here is durable. Restarting loses every user, which is correct for
//! a test double and obvious enough that no one will ship it by accident.

use async_trait::async_trait;
use cb_ports::{
    AppDatabase, AuditRecord, AuditRepository, BusSubscription, Candle, CandleInterval,
    CandleRepository, EventBus, EventEnvelope, FaucetClaim, FaucetRepository, KvBatch, KvStore,
    LeaderboardEntry, LeaderboardRepository, LeaderboardWindow, LedgerEntry, LedgerRepository,
    MarketMeta, MarketMetaRepository, PortError, PortResult, Session, SessionRepository, Topic,
    User, UserRepository, WriteOp,
};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Everything the in-memory database holds.
#[derive(Default)]
struct Tables {
    users: Vec<User>,
    sessions: HashMap<String, Session>,
    faucet_claims: Vec<FaucetClaim>,
    candles: BTreeMap<(String, String, i64), Candle>,
    ledger: Vec<LedgerEntry>,
    leaderboard: HashMap<String, (DateTime<Utc>, Vec<LeaderboardEntry>)>,
    markets: Vec<MarketMeta>,
    audit: Vec<AuditRecord>,
}

/// An [`AppDatabase`] that lives in a mutex.
#[derive(Default)]
pub struct InMemoryDatabase {
    tables: Mutex<Tables>,
}

impl InMemoryDatabase {
    /// An empty database.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty database behind an `Arc`, ready for the composition root.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

#[async_trait]
impl UserRepository for InMemoryDatabase {
    async fn create(&self, user: &User) -> PortResult<()> {
        let mut tables = self.tables.lock();
        // Uniqueness is enforced under the lock. A caller that checked first
        // and inserted second would let two registrations of the same handle
        // through the gap between the two calls.
        if tables
            .users
            .iter()
            .any(|u| u.handle.eq_ignore_ascii_case(&user.handle))
        {
            return Err(PortError::Conflict(format!("handle {}", user.handle)));
        }
        if tables.users.iter().any(|u| u.user_id == user.user_id) {
            return Err(PortError::Conflict(format!("user_id {}", user.user_id)));
        }
        tables.users.push(user.clone());
        Ok(())
    }

    async fn by_id(&self, user_id: &str) -> PortResult<Option<User>> {
        let tables = self.tables.lock();
        Ok(tables.users.iter().find(|u| u.user_id == user_id).cloned())
    }

    async fn by_handle(&self, handle: &str) -> PortResult<Option<User>> {
        let tables = self.tables.lock();
        Ok(tables
            .users
            .iter()
            .find(|u| u.handle.eq_ignore_ascii_case(handle))
            .cloned())
    }

    async fn by_address(&self, address: &str) -> PortResult<Option<User>> {
        let tables = self.tables.lock();
        Ok(tables
            .users
            .iter()
            .find(|u| u.address.eq_ignore_ascii_case(address))
            .cloned())
    }

    async fn touch_last_seen(&self, user_id: &str, at: DateTime<Utc>) -> PortResult<()> {
        let mut tables = self.tables.lock();
        match tables.users.iter_mut().find(|u| u.user_id == user_id) {
            Some(user) => {
                user.last_seen_at = at;
                Ok(())
            }
            None => Err(PortError::NotFound(format!("user {user_id}"))),
        }
    }

    async fn list(&self, include_bots: bool, limit: u32) -> PortResult<Vec<User>> {
        let tables = self.tables.lock();
        Ok(tables
            .users
            .iter()
            .filter(|u| include_bots || !u.is_bot)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn count(&self, include_bots: bool) -> PortResult<u64> {
        let tables = self.tables.lock();
        Ok(tables
            .users
            .iter()
            .filter(|u| include_bots || !u.is_bot)
            .count() as u64)
    }
}

#[async_trait]
impl SessionRepository for InMemoryDatabase {
    async fn create(&self, session: &Session) -> PortResult<()> {
        let mut tables = self.tables.lock();
        tables
            .sessions
            .insert(session.token_hash.clone(), session.clone());
        Ok(())
    }

    async fn by_token_hash(&self, token_hash: &str) -> PortResult<Option<Session>> {
        let tables = self.tables.lock();
        Ok(tables.sessions.get(token_hash).cloned())
    }

    async fn revoke(&self, token_hash: &str) -> PortResult<()> {
        let mut tables = self.tables.lock();
        match tables.sessions.get_mut(token_hash) {
            Some(session) => {
                session.revoked = true;
                Ok(())
            }
            None => Err(PortError::NotFound("session".into())),
        }
    }

    async fn revoke_all_for_user(&self, user_id: &str) -> PortResult<u64> {
        let mut tables = self.tables.lock();
        let mut revoked = 0;
        for session in tables.sessions.values_mut() {
            if session.user_id == user_id && !session.revoked {
                session.revoked = true;
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    async fn delete_expired(&self, before: DateTime<Utc>) -> PortResult<u64> {
        let mut tables = self.tables.lock();
        let before_count = tables.sessions.len();
        tables.sessions.retain(|_, s| s.expires_at >= before);
        Ok((before_count - tables.sessions.len()) as u64)
    }
}

#[async_trait]
impl FaucetRepository for InMemoryDatabase {
    async fn record_claim(&self, claim: &FaucetClaim) -> PortResult<()> {
        let mut tables = self.tables.lock();
        tables.faucet_claims.push(claim.clone());
        Ok(())
    }

    async fn last_claim_at(&self, user_id: &str) -> PortResult<Option<DateTime<Utc>>> {
        let tables = self.tables.lock();
        Ok(tables
            .faucet_claims
            .iter()
            .filter(|c| c.user_id == user_id)
            .map(|c| c.claimed_at)
            .max())
    }

    async fn claim_count(&self, user_id: &str) -> PortResult<u64> {
        let tables = self.tables.lock();
        // One claim grants several assets, which is several rows. The count
        // the contract shows is claims, not rows, so group by claim id.
        let ids: std::collections::HashSet<&str> = tables
            .faucet_claims
            .iter()
            .filter(|c| c.user_id == user_id)
            .map(|c| c.claim_id.as_str())
            .collect();
        Ok(ids.len() as u64)
    }

    async fn history(&self, user_id: &str, limit: u32) -> PortResult<Vec<FaucetClaim>> {
        let tables = self.tables.lock();
        let mut claims: Vec<FaucetClaim> = tables
            .faucet_claims
            .iter()
            .filter(|c| c.user_id == user_id)
            .cloned()
            .collect();
        claims.sort_by_key(|a| std::cmp::Reverse(a.claimed_at));
        claims.truncate(limit as usize);
        Ok(claims)
    }

    async fn total_granted(&self) -> PortResult<Vec<(String, Decimal)>> {
        let tables = self.tables.lock();
        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        for claim in &tables.faucet_claims {
            *totals.entry(claim.asset.clone()).or_default() += claim.amount;
        }
        Ok(totals.into_iter().collect())
    }
}

#[async_trait]
impl CandleRepository for InMemoryDatabase {
    async fn upsert(&self, candle: &Candle) -> PortResult<()> {
        let mut tables = self.tables.lock();
        tables.candles.insert(key_of(candle), candle.clone());
        Ok(())
    }

    async fn upsert_many(&self, candles: &[Candle]) -> PortResult<()> {
        let mut tables = self.tables.lock();
        for candle in candles {
            tables.candles.insert(key_of(candle), candle.clone());
        }
        Ok(())
    }

    async fn range(
        &self,
        symbol: &str,
        interval: CandleInterval,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> PortResult<Vec<Candle>> {
        let tables = self.tables.lock();
        let mut found: Vec<Candle> = tables
            .candles
            .values()
            .filter(|c| c.symbol == symbol && c.interval == interval.as_str())
            .filter(|c| from.is_none_or(|f| c.open_time >= f))
            .filter(|c| to.is_none_or(|t| c.open_time <= t))
            .cloned()
            .collect();
        found.sort_by_key(|c| c.open_time);
        // The most recent `limit` bars, still oldest-first.
        if found.len() > limit as usize {
            found.drain(..found.len() - limit as usize);
        }
        Ok(found)
    }

    async fn latest(&self, symbol: &str, interval: CandleInterval) -> PortResult<Option<Candle>> {
        let tables = self.tables.lock();
        Ok(tables
            .candles
            .values()
            .filter(|c| c.symbol == symbol && c.interval == interval.as_str())
            .max_by_key(|c| c.open_time)
            .cloned())
    }

    async fn prune(&self, interval: CandleInterval, before: DateTime<Utc>) -> PortResult<u64> {
        let mut tables = self.tables.lock();
        let before_count = tables.candles.len();
        tables
            .candles
            .retain(|_, c| !(c.interval == interval.as_str() && c.open_time < before));
        Ok((before_count - tables.candles.len()) as u64)
    }
}

fn key_of(candle: &Candle) -> (String, String, i64) {
    (
        candle.symbol.clone(),
        candle.interval.clone(),
        candle.open_time.timestamp(),
    )
}

#[async_trait]
impl LedgerRepository for InMemoryDatabase {
    /// Append one entry, refusing an id that is already present.
    ///
    /// `entry_id` is the primary key of `ledger_entries` in the SQL schema, so
    /// a duplicate is a `Conflict` there. This implementation has to agree:
    /// the watcher derives deterministic ids precisely so that replaying a
    /// block rewrites the same id and is rejected, and an in-memory store that
    /// accepted the duplicate would let a test pass while the durable store
    /// doubled every entry on restart.
    async fn append(&self, entry: &LedgerEntry) -> PortResult<()> {
        let mut tables = self.tables.lock();
        if tables.ledger.iter().any(|e| e.entry_id == entry.entry_id) {
            return Err(PortError::Conflict(format!(
                "ledger entry `{}` already exists",
                entry.entry_id
            )));
        }
        tables.ledger.push(entry.clone());
        Ok(())
    }

    /// Append a set atomically: either every entry lands or none does.
    async fn append_many(&self, entries: &[LedgerEntry]) -> PortResult<()> {
        let mut tables = self.tables.lock();
        for entry in entries {
            if tables.ledger.iter().any(|e| e.entry_id == entry.entry_id)
                || entries
                    .iter()
                    .filter(|other| other.entry_id == entry.entry_id)
                    .count()
                    > 1
            {
                return Err(PortError::Conflict(format!(
                    "ledger entry `{}` already exists",
                    entry.entry_id
                )));
            }
        }
        tables.ledger.extend_from_slice(entries);
        Ok(())
    }

    async fn for_user(
        &self,
        user_id: &str,
        kinds: Option<&[String]>,
        limit: u32,
    ) -> PortResult<Vec<LedgerEntry>> {
        let tables = self.tables.lock();
        let mut entries: Vec<LedgerEntry> = tables
            .ledger
            .iter()
            .filter(|e| e.user_id == user_id)
            .filter(|e| kinds.is_none_or(|ks| ks.contains(&e.kind)))
            .cloned()
            .collect();
        entries.sort_by_key(|a| std::cmp::Reverse(a.at));
        entries.truncate(limit as usize);
        Ok(entries)
    }

    async fn net_by_asset(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> PortResult<Vec<(String, Decimal)>> {
        let tables = self.tables.lock();
        let mut totals: BTreeMap<String, Decimal> = BTreeMap::new();
        for entry in tables
            .ledger
            .iter()
            .filter(|e| e.user_id == user_id && e.at >= from && e.at <= to)
        {
            *totals.entry(entry.asset.clone()).or_default() += entry.amount;
        }
        Ok(totals.into_iter().collect())
    }
}

#[async_trait]
impl LeaderboardRepository for InMemoryDatabase {
    async fn replace_window(
        &self,
        window: LeaderboardWindow,
        computed_at: DateTime<Utc>,
        entries: &[LeaderboardEntry],
    ) -> PortResult<()> {
        let mut tables = self.tables.lock();
        tables
            .leaderboard
            .insert(window.as_str().to_string(), (computed_at, entries.to_vec()));
        Ok(())
    }

    async fn top(
        &self,
        window: LeaderboardWindow,
        limit: u32,
    ) -> PortResult<Vec<LeaderboardEntry>> {
        let tables = self.tables.lock();
        Ok(tables
            .leaderboard
            .get(window.as_str())
            .map(|(_, entries)| entries.iter().take(limit as usize).cloned().collect())
            .unwrap_or_default())
    }

    async fn for_handle(
        &self,
        window: LeaderboardWindow,
        handle: &str,
    ) -> PortResult<Option<LeaderboardEntry>> {
        let tables = self.tables.lock();
        Ok(tables.leaderboard.get(window.as_str()).and_then(|(_, e)| {
            e.iter()
                .find(|entry| entry.handle.eq_ignore_ascii_case(handle))
                .cloned()
        }))
    }

    async fn computed_at(&self, window: LeaderboardWindow) -> PortResult<Option<DateTime<Utc>>> {
        let tables = self.tables.lock();
        Ok(tables.leaderboard.get(window.as_str()).map(|(at, _)| *at))
    }
}

#[async_trait]
impl MarketMetaRepository for InMemoryDatabase {
    async fn replace_all(&self, markets: &[MarketMeta]) -> PortResult<()> {
        self.tables.lock().markets = markets.to_vec();
        Ok(())
    }

    async fn all(&self) -> PortResult<Vec<MarketMeta>> {
        Ok(self.tables.lock().markets.clone())
    }

    async fn by_symbol(&self, symbol: &str) -> PortResult<Option<MarketMeta>> {
        let tables = self.tables.lock();
        Ok(tables
            .markets
            .iter()
            .find(|m| m.symbol.eq_ignore_ascii_case(symbol))
            .cloned())
    }

    async fn set_status(&self, symbol: &str, status: &str) -> PortResult<()> {
        let mut tables = self.tables.lock();
        match tables
            .markets
            .iter_mut()
            .find(|m| m.symbol.eq_ignore_ascii_case(symbol))
        {
            Some(market) => {
                market.status = status.to_string();
                Ok(())
            }
            None => Err(PortError::NotFound(format!("market {symbol}"))),
        }
    }
}

#[async_trait]
impl AuditRepository for InMemoryDatabase {
    async fn append(&self, record: &AuditRecord) -> PortResult<()> {
        self.tables.lock().audit.push(record.clone());
        Ok(())
    }

    async fn for_subject(&self, subject: &str, limit: u32) -> PortResult<Vec<AuditRecord>> {
        let tables = self.tables.lock();
        let mut records: Vec<AuditRecord> = tables
            .audit
            .iter()
            .filter(|r| r.subject == subject)
            .cloned()
            .collect();
        records.sort_by_key(|a| std::cmp::Reverse(a.at));
        records.truncate(limit as usize);
        Ok(records)
    }

    async fn for_actor(&self, actor: &str, limit: u32) -> PortResult<Vec<AuditRecord>> {
        let tables = self.tables.lock();
        let mut records: Vec<AuditRecord> = tables
            .audit
            .iter()
            .filter(|r| r.actor.as_deref() == Some(actor))
            .cloned()
            .collect();
        records.sort_by_key(|a| std::cmp::Reverse(a.at));
        records.truncate(limit as usize);
        Ok(records)
    }
}

#[async_trait]
impl AppDatabase for InMemoryDatabase {
    fn users(&self) -> &dyn UserRepository {
        self
    }
    fn sessions(&self) -> &dyn SessionRepository {
        self
    }
    fn faucet(&self) -> &dyn FaucetRepository {
        self
    }
    fn candles(&self) -> &dyn CandleRepository {
        self
    }
    fn ledger(&self) -> &dyn LedgerRepository {
        self
    }
    fn leaderboard(&self) -> &dyn LeaderboardRepository {
        self
    }
    fn markets(&self) -> &dyn MarketMetaRepository {
        self
    }
    fn audit(&self) -> &dyn AuditRepository {
        self
    }

    async fn migrate(&self) -> PortResult<()> {
        Ok(())
    }

    async fn schema_version(&self) -> PortResult<i64> {
        Ok(0)
    }

    async fn health_check(&self) -> PortResult<()> {
        Ok(())
    }
}

/// An in-memory [`KvStore`], used for the custodial key vault.
#[derive(Default)]
pub struct InMemoryKv {
    #[allow(clippy::type_complexity)]
    namespaces: Mutex<HashMap<String, BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl InMemoryKv {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvStore for InMemoryKv {
    fn get(&self, namespace: &str, key: &[u8]) -> PortResult<Option<Vec<u8>>> {
        let namespaces = self.namespaces.lock();
        Ok(namespaces.get(namespace).and_then(|m| m.get(key)).cloned())
    }

    fn write_batch(&self, namespace: &str, batch: KvBatch) -> PortResult<()> {
        let mut namespaces = self.namespaces.lock();
        let map = namespaces.entry(namespace.to_string()).or_default();
        for op in batch.ops() {
            match op {
                WriteOp::Put { key, value } => {
                    map.insert(key.clone(), value.clone());
                }
                WriteOp::Delete { key } => {
                    map.remove(key);
                }
            }
        }
        Ok(())
    }

    fn scan_prefix(&self, namespace: &str, prefix: &[u8]) -> PortResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let namespaces = self.namespaces.lock();
        // BTreeMap iterates in ascending key order, which `KvStore` requires.
        Ok(namespaces
            .get(namespace)
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| k.starts_with(prefix))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn flush(&self) -> PortResult<()> {
        Ok(())
    }
}

/// An in-process [`EventBus`], shaped like a partitioned log.
pub struct InMemoryBus {
    topics: Mutex<HashMap<String, tokio::sync::broadcast::Sender<EventEnvelope>>>,
    offsets: Mutex<HashMap<String, Arc<AtomicU64>>>,
    capacity: usize,
}

impl Default for InMemoryBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl InMemoryBus {
    /// A bus whose per-topic buffer holds `capacity` messages.
    pub fn new(capacity: usize) -> Self {
        Self {
            topics: Mutex::new(HashMap::new()),
            offsets: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    fn sender(&self, topic: &Topic) -> tokio::sync::broadcast::Sender<EventEnvelope> {
        let mut topics = self.topics.lock();
        topics
            .entry(topic.as_str().to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel(self.capacity).0)
            .clone()
    }

    fn offset(&self, topic: &Topic) -> Arc<AtomicU64> {
        let mut offsets = self.offsets.lock();
        Arc::clone(
            offsets
                .entry(topic.as_str().to_string())
                .or_insert_with(|| Arc::new(AtomicU64::new(0))),
        )
    }
}

struct BroadcastSubscription {
    receiver: tokio::sync::broadcast::Receiver<EventEnvelope>,
}

#[async_trait]
impl BusSubscription for BroadcastSubscription {
    async fn next(&mut self) -> PortResult<Option<EventEnvelope>> {
        use tokio::sync::broadcast::error::RecvError;
        match self.receiver.recv().await {
            Ok(envelope) => Ok(Some(envelope)),
            Err(RecvError::Closed) => Ok(None),
            // A lagging subscriber is dropped rather than buffered without
            // bound. The caller resubscribes and resynchronises.
            Err(RecvError::Lagged(n)) => Err(PortError::Contention(format!("lagged {n} messages"))),
        }
    }
}

#[async_trait]
impl EventBus for InMemoryBus {
    async fn publish(&self, topic: &Topic, key: &str, payload: Vec<u8>) -> PortResult<u64> {
        let offset = self.offset(topic).fetch_add(1, Ordering::SeqCst);
        let envelope = EventEnvelope {
            topic: topic.clone(),
            key: key.to_string(),
            payload,
            offset,
            published_at: Utc::now(),
        };
        // No subscribers is not a failure; a log with no readers still advances.
        let _ = self.sender(topic).send(envelope);
        Ok(offset)
    }

    async fn subscribe(&self, topic: &Topic) -> PortResult<Box<dyn BusSubscription>> {
        Ok(Box::new(BroadcastSubscription {
            receiver: self.sender(topic).subscribe(),
        }))
    }

    async fn subscribe_from(
        &self,
        _topic: &Topic,
        offset: u64,
    ) -> PortResult<Box<dyn BusSubscription>> {
        Err(PortError::InvalidArgument(format!(
            "this bus keeps no retention, so offset {offset} cannot be replayed"
        )))
    }

    async fn high_watermark(&self, topic: &Topic) -> PortResult<u64> {
        Ok(self.offset(topic).load(Ordering::SeqCst))
    }

    async fn health_check(&self) -> PortResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(handle: &str) -> User {
        User {
            user_id: format!("usr_{handle}"),
            handle: handle.to_string(),
            address: format!("0x{handle}"),
            recovery_hash: "hash".into(),
            is_bot: false,
            created_at: Utc::now(),
            last_seen_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn handle_uniqueness_is_case_insensitive_and_enforced_in_the_repository() {
        let db = InMemoryDatabase::new();
        db.users().create(&user("satoshi")).await.expect("first");
        let mut clash = user("SATOSHI");
        clash.user_id = "usr_other".into();
        assert!(matches!(
            db.users().create(&clash).await,
            Err(PortError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn sessions_are_returned_even_when_revoked_so_callers_can_tell_why() {
        let db = InMemoryDatabase::new();
        let session = Session {
            token_hash: "abc".into(),
            user_id: "usr_1".into(),
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            revoked: false,
        };
        db.sessions().create(&session).await.expect("create");
        db.sessions().revoke("abc").await.expect("revoke");
        let found = db
            .sessions()
            .by_token_hash("abc")
            .await
            .expect("lookup")
            .expect("still present");
        assert!(found.revoked);
    }

    #[tokio::test]
    async fn the_kv_store_scans_a_prefix_in_ascending_order() {
        let kv = InMemoryKv::new();
        let mut batch = KvBatch::new();
        batch.put(b"k/c".to_vec(), b"3".to_vec());
        batch.put(b"k/a".to_vec(), b"1".to_vec());
        batch.put(b"other".to_vec(), b"x".to_vec());
        kv.write_batch("ns", batch).expect("write");
        let found = kv.scan_prefix("ns", b"k/").expect("scan");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, b"k/a");
        assert_eq!(found[1].0, b"k/c");
    }

    #[tokio::test]
    async fn the_bus_assigns_monotonic_offsets_and_delivers() {
        let bus = InMemoryBus::new(16);
        let topic = Topic::new("market.trades");
        let mut subscription = bus.subscribe(&topic).await.expect("subscribe");
        assert_eq!(bus.publish(&topic, "BTC", b"a".to_vec()).await.unwrap(), 0);
        assert_eq!(bus.publish(&topic, "BTC", b"b".to_vec()).await.unwrap(), 1);
        let first = subscription.next().await.expect("recv").expect("message");
        assert_eq!(first.offset, 0);
        assert_eq!(bus.high_watermark(&topic).await.unwrap(), 2);
    }
}
