//! The pool, the connection settings, and migrations.

use crate::error::map_sqlx;
use async_trait::async_trait;
use cb_ports::db::{
    AppDatabase, AuditRepository, CandleRepository, FaucetRepository, LeaderboardRepository,
    LedgerRepository, MarketMetaRepository, SessionRepository, UserRepository,
};
use cb_ports::{PortError, PortResult};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::Row;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Migrations are embedded at compile time, so a deployed binary carries its
/// own schema and cannot be pointed at the wrong `migrations/` directory.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// How long a writer waits for the lock before SQLite gives up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pool size for a file-backed database.
///
/// SQLite permits one writer regardless, so this bounds concurrent *readers*.
/// Eight is comfortably above the gateway's request concurrency and well
/// below any file-descriptor pressure.
const FILE_POOL_CONNECTIONS: u32 = 8;

/// How many times [`SqliteDb::open`] retries a database that is busy.
const OPEN_ATTEMPTS: u32 = 6;

/// Backoff before the first retry; doubled each attempt.
const OPEN_RETRY_BACKOFF: Duration = Duration::from_millis(20);

/// The application database, backed by SQLite.
///
/// Cheap to clone: clones share one pool. [`AppDatabase`] requires
/// `Send + Sync + 'static`, and every repository trait is implemented directly
/// on this type, so the accessor methods hand out `&dyn Repository` borrows of
/// `self` rather than allocating per-repository objects.
#[derive(Clone)]
pub struct SqliteDb {
    inner: Arc<Inner>,
}

struct Inner {
    pool: SqlitePool,
    /// Serialises `migrate()` within this process.
    ///
    /// sqlx's migrator takes an advisory lock on backends that have one;
    /// SQLite does not, so two concurrent callers would both observe "nothing
    /// applied" and the loser would collide on `_sqlx_migrations.version`.
    /// A process-local mutex closes that window. Across processes the
    /// protection is SQLite's own file lock plus the busy timeout: the second
    /// process waits for the first one's transaction rather than interleaving
    /// with it.
    migrate_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for SqliteDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteDb")
            .field("closed", &self.inner.pool.is_closed())
            .finish()
    }
}

impl SqliteDb {
    /// Open (creating if absent) a database file.
    ///
    /// The schema is *not* applied here; call [`AppDatabase::migrate`]. Keeping
    /// them separate lets an operator open a database to inspect its
    /// [`AppDatabase::schema_version`] without mutating it.
    pub async fn open(path: impl AsRef<Path>) -> PortResult<Self> {
        let options = Self::base_options()
            .filename(path.as_ref())
            .create_if_missing(true)
            // WAL is a persistent property of the file, not the connection,
            // so setting it once on open is enough; later connections find the
            // file already in WAL. It is set explicitly here rather than left
            // to sqlx's default so the intent is visible. SQLite ignores the
            // request for an in-memory database, which is why nothing asserts
            // on the journal mode in those tests.
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);

        // Retried, because the very first connection to a brand-new file has to
        // switch its journal mode to WAL, and that switch takes an exclusive
        // lock that SQLite's busy handler does not cover. Two replicas starting
        // against the same fresh database will collide there exactly once; a
        // process that gave up would be a start-up failure caused by a race it
        // would win on the next attempt.
        let mut backoff = OPEN_RETRY_BACKOFF;
        let mut last = None;
        for attempt in 1..=OPEN_ATTEMPTS {
            let result = SqlitePoolOptions::new()
                .max_connections(FILE_POOL_CONNECTIONS)
                .acquire_timeout(Duration::from_secs(10))
                .connect_with(options.clone())
                .await;

            match result {
                Ok(pool) => return Ok(Self::from_pool(pool)),
                Err(e) => {
                    let mapped = map_sqlx("opening the database file", e);
                    if !mapped.is_retryable() || attempt == OPEN_ATTEMPTS {
                        return Err(mapped);
                    }
                    last = Some(mapped);
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
            }
        }

        Err(last.unwrap_or_else(|| {
            PortError::Internal("open retry loop exited without a result".into())
        }))
    }

    /// Open a private in-memory database, for tests.
    ///
    /// The pool is pinned to a single connection that is never recycled.
    /// `sqlite::memory:` gives every *connection* its own empty database, so a
    /// multi-connection pool here would migrate one database and then query a
    /// different, empty one — a failure that shows up as "no such table" long
    /// after the mistake. Disabling the idle and lifetime reapers matters for
    /// the same reason: if the one connection were ever closed, the database
    /// would cease to exist.
    ///
    /// The consequence is that an in-memory database serialises everything, so
    /// it cannot be used to test concurrent write behaviour. Those tests use a
    /// temporary file.
    pub async fn open_in_memory() -> PortResult<Self> {
        let options = Self::base_options().filename(":memory:");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await
            .map_err(|e| map_sqlx("opening an in-memory database", e))?;

        Ok(Self::from_pool(pool))
    }

    /// Settings that apply to every connection regardless of backing store.
    fn base_options() -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            // Off by default, per connection. Without this the schema's
            // REFERENCES clauses would document intent and enforce nothing.
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT)
    }

    fn from_pool(pool: SqlitePool) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool,
                migrate_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The underlying pool, for repository implementations in this crate.
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.inner.pool
    }

    /// Close the pool and wait for in-flight statements to finish.
    ///
    /// After this every operation returns [`PortError::Unavailable`]. Tests
    /// use it to exercise that path without unplugging a disk.
    pub async fn close(&self) {
        self.inner.pool.close().await;
    }
}

#[async_trait]
impl AppDatabase for SqliteDb {
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

    /// Idempotent, and safe to call from several places at once.
    ///
    /// Two layers of protection, because there are two races.
    ///
    /// *Within this process*, `migrate_lock` serialises callers. sqlx's
    /// migrator takes an advisory lock on backends that offer one, but SQLite
    /// does not, so without this both callers would read "nothing applied" and
    /// the loser would collide on `_sqlx_migrations.version`.
    ///
    /// *Across processes*, the mutex is no help. The fallback is to check what
    /// actually happened: if the run failed but the schema has nonetheless
    /// reached the version this binary embeds, somebody else applied it and
    /// the postcondition holds. Reporting an error there would make a second
    /// gateway replica fail to start for having lost a harmless race.
    async fn migrate(&self) -> PortResult<()> {
        let _guard = self.inner.migrate_lock.lock().await;

        match MIGRATOR.run(&self.inner.pool).await {
            Ok(()) => Ok(()),
            // These say the database and this binary disagree about what the
            // schema *is* — an applied migration edited after the fact, one
            // this binary does not know (a newer binary migrated it), or one
            // left half-applied. None of them is the harmless race below, and
            // each leaves the version number looking reached, so the
            // postcondition check would wave them through: the checksum
            // protection sqlx keeps would be switched off, and an older
            // binary would run against a schema it has never seen.
            Err(
                run_error @ (sqlx::migrate::MigrateError::VersionMismatch(_)
                | sqlx::migrate::MigrateError::VersionMissing(_)
                | sqlx::migrate::MigrateError::Dirty(_)),
            ) => Err(map_sqlx(
                "applying migrations",
                sqlx::Error::Migrate(Box::new(run_error)),
            )),
            Err(run_error) => {
                let target = MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0);
                match self.schema_version().await {
                    Ok(applied) if applied >= target => Ok(()),
                    _ => Err(map_sqlx(
                        "applying migrations",
                        sqlx::Error::Migrate(Box::new(run_error)),
                    )),
                }
            }
        }
    }

    async fn schema_version(&self) -> PortResult<i64> {
        // A database that has never been migrated has no bookkeeping table,
        // and asking for its version is a fair question with the answer
        // "zero" rather than an error. Probing sqlite_master first keeps that
        // distinct from a genuine failure.
        let migrated: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
        )
        .fetch_one(&self.inner.pool)
        .await
        .map_err(|e| map_sqlx("checking for the migrations table", e))?;

        if migrated == 0 {
            return Ok(0);
        }

        let row =
            sqlx::query("SELECT MAX(version) AS version FROM _sqlx_migrations WHERE success = 1")
                .fetch_one(&self.inner.pool)
                .await
                .map_err(|e| map_sqlx("reading the schema version", e))?;

        // MAX over zero rows is NULL, which is still "version 0".
        let version: Option<i64> = row
            .try_get("version")
            .map_err(|e| map_sqlx("reading the schema version", e))?;
        Ok(version.unwrap_or(0))
    }

    async fn health_check(&self) -> PortResult<()> {
        if self.inner.pool.is_closed() {
            return Err(PortError::Unavailable(
                "the SQLite connection pool is closed".to_string(),
            ));
        }
        sqlx::query("SELECT 1")
            .fetch_one(&self.inner.pool)
            .await
            .map(|_| ())
            .map_err(|e| map_sqlx("health check", e))
    }
}
