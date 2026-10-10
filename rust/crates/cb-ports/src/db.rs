//! The application database, as a set of repositories.
//!
//! Repositories are split by aggregate rather than exposed as one wide trait,
//! so a caller that only reads candles cannot accidentally write users, and a
//! test double only has to implement what the code under test actually uses.
//!
//! [`AppDatabase`] composes them and owns the concerns that cross aggregates:
//! migrations and health.
//!
//! ## What does *not* belong here
//!
//! Balances, orders and positions are consensus state. They live on chain and
//! are read through the node's RPC. Any repository method returning them would
//! be a second source of truth, and the two would drift. The records in this
//! module are account plumbing, derived views and audit trails.

use crate::error::PortResult;
use crate::models::{
    AuditRecord, Candle, CandleInterval, FaucetClaim, LeaderboardEntry, LeaderboardWindow,
    LedgerEntry, MarketMeta, Session, User,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// Player accounts.
#[async_trait]
pub trait UserRepository: Send + Sync {
    /// Insert a new user.
    ///
    /// Returns [`crate::PortError::Conflict`] when the handle is taken. Handle
    /// uniqueness is case-insensitive, and enforcing it is the
    /// implementation's job, not the caller's — a check-then-insert in the
    /// gateway would race.
    async fn create(&self, user: &User) -> PortResult<()>;

    /// Look up by internal id.
    async fn by_id(&self, user_id: &str) -> PortResult<Option<User>>;

    /// Look up by handle, case-insensitively.
    async fn by_handle(&self, handle: &str) -> PortResult<Option<User>>;

    /// Look up by on-chain address.
    async fn by_address(&self, address: &str) -> PortResult<Option<User>>;

    /// Record that the user was active at `at`.
    async fn touch_last_seen(&self, user_id: &str, at: DateTime<Utc>) -> PortResult<()>;

    /// Every user, oldest first. Bots included only when asked for.
    async fn list(&self, include_bots: bool, limit: u32) -> PortResult<Vec<User>>;

    /// How many accounts exist.
    async fn count(&self, include_bots: bool) -> PortResult<u64>;
}

/// Bearer sessions.
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Store a session.
    async fn create(&self, session: &Session) -> PortResult<()>;

    /// Fetch by token hash. Expired and revoked rows are still returned; the
    /// caller decides, so it can distinguish "expired" from "never existed".
    async fn by_token_hash(&self, token_hash: &str) -> PortResult<Option<Session>>;

    /// Mark one session revoked.
    async fn revoke(&self, token_hash: &str) -> PortResult<()>;

    /// Revoke every session belonging to a user. Returns how many.
    async fn revoke_all_for_user(&self, user_id: &str) -> PortResult<u64>;

    /// Delete sessions that expired before `before`. Returns how many.
    async fn delete_expired(&self, before: DateTime<Utc>) -> PortResult<u64>;
}

/// Faucet grants.
#[async_trait]
pub trait FaucetRepository: Send + Sync {
    /// Record a grant.
    async fn record_claim(&self, claim: &FaucetClaim) -> PortResult<()>;

    /// When the user last claimed, if ever.
    async fn last_claim_at(&self, user_id: &str) -> PortResult<Option<DateTime<Utc>>>;

    /// How many grants the user has taken.
    async fn claim_count(&self, user_id: &str) -> PortResult<u64>;

    /// A user's grant history, newest first.
    async fn history(&self, user_id: &str, limit: u32) -> PortResult<Vec<FaucetClaim>>;

    /// Total granted per asset since genesis.
    ///
    /// The faucet is the only mint in the system, so this is what conservation
    /// checks reconcile against.
    async fn total_granted(&self) -> PortResult<Vec<(String, rust_decimal::Decimal)>>;
}

/// OHLCV aggregation.
#[async_trait]
pub trait CandleRepository: Send + Sync {
    /// Insert or replace a bar.
    async fn upsert(&self, candle: &Candle) -> PortResult<()>;

    /// Insert or replace many bars in one transaction.
    async fn upsert_many(&self, candles: &[Candle]) -> PortResult<()>;

    /// Bars for a symbol and interval, oldest first, at most `limit`.
    async fn range(
        &self,
        symbol: &str,
        interval: CandleInterval,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> PortResult<Vec<Candle>>;

    /// The most recent bar, if any.
    async fn latest(&self, symbol: &str, interval: CandleInterval) -> PortResult<Option<Candle>>;

    /// Drop bars older than `before` for one interval. Returns how many.
    async fn prune(&self, interval: CandleInterval, before: DateTime<Utc>) -> PortResult<u64>;
}

/// The off-chain view of value movements.
#[async_trait]
pub trait LedgerRepository: Send + Sync {
    /// Append an entry.
    async fn append(&self, entry: &LedgerEntry) -> PortResult<()>;

    /// Append several entries atomically.
    async fn append_many(&self, entries: &[LedgerEntry]) -> PortResult<()>;

    /// A user's entries, newest first.
    async fn for_user(
        &self,
        user_id: &str,
        kinds: Option<&[String]>,
        limit: u32,
    ) -> PortResult<Vec<LedgerEntry>>;

    /// Net movement per asset for a user over a window.
    async fn net_by_asset(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> PortResult<Vec<(String, rust_decimal::Decimal)>>;
}

/// Ranking.
#[async_trait]
pub trait LeaderboardRepository: Send + Sync {
    /// Replace the computed ranking for a window.
    ///
    /// Recomputation is wholesale rather than incremental: ranks shift when
    /// any player's equity moves, so a partial update would leave the table
    /// inconsistent.
    async fn replace_window(
        &self,
        window: LeaderboardWindow,
        computed_at: DateTime<Utc>,
        entries: &[LeaderboardEntry],
    ) -> PortResult<()>;

    /// The top `limit` entries for a window.
    async fn top(&self, window: LeaderboardWindow, limit: u32)
        -> PortResult<Vec<LeaderboardEntry>>;

    /// One player's entry, if they are ranked.
    async fn for_handle(
        &self,
        window: LeaderboardWindow,
        handle: &str,
    ) -> PortResult<Option<LeaderboardEntry>>;

    /// When the window was last recomputed.
    async fn computed_at(&self, window: LeaderboardWindow) -> PortResult<Option<DateTime<Utc>>>;
}

/// Cached market descriptions.
#[async_trait]
pub trait MarketMetaRepository: Send + Sync {
    /// Insert or replace the full market list.
    async fn replace_all(&self, markets: &[MarketMeta]) -> PortResult<()>;

    /// Every market.
    async fn all(&self) -> PortResult<Vec<MarketMeta>>;

    /// One market by symbol.
    async fn by_symbol(&self, symbol: &str) -> PortResult<Option<MarketMeta>>;

    /// Change one market's status.
    async fn set_status(&self, symbol: &str, status: &str) -> PortResult<()>;
}

/// Append-only audit trail.
#[async_trait]
pub trait AuditRepository: Send + Sync {
    /// Append a record.
    async fn append(&self, record: &AuditRecord) -> PortResult<()>;

    /// Records for one subject, newest first.
    async fn for_subject(&self, subject: &str, limit: u32) -> PortResult<Vec<AuditRecord>>;

    /// Records by one actor, newest first.
    async fn for_actor(&self, actor: &str, limit: u32) -> PortResult<Vec<AuditRecord>>;
}

/// The application database as a whole.
///
/// A composition root builds one of these and hands out repository handles.
/// Implementations are expected to be cheap to clone, sharing a pool.
#[async_trait]
pub trait AppDatabase: Send + Sync + 'static {
    /// Player accounts.
    fn users(&self) -> &dyn UserRepository;
    /// Bearer sessions.
    fn sessions(&self) -> &dyn SessionRepository;
    /// Faucet grants.
    fn faucet(&self) -> &dyn FaucetRepository;
    /// OHLCV bars.
    fn candles(&self) -> &dyn CandleRepository;
    /// Value movements.
    fn ledger(&self) -> &dyn LedgerRepository;
    /// Rankings.
    fn leaderboard(&self) -> &dyn LeaderboardRepository;
    /// Market descriptions.
    fn markets(&self) -> &dyn MarketMetaRepository;
    /// Audit trail.
    fn audit(&self) -> &dyn AuditRepository;

    /// Bring the schema up to the latest version.
    ///
    /// Must be idempotent: running it against an already-current database is a
    /// no-op, and running it twice concurrently must not corrupt the schema.
    async fn migrate(&self) -> PortResult<()>;

    /// The schema version currently applied.
    async fn schema_version(&self) -> PortResult<i64>;

    /// Whether the database is reachable.
    async fn health_check(&self) -> PortResult<()>;
}
