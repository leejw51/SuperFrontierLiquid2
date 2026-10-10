//! Migrations, health, and behaviour under concurrency.

mod common;

use cb_ports::{AppDatabase, PortError};
use cb_store_sqlite::SqliteDb;
use common::{db, user};

/// The version the migrations in this repository add up to. Bumping a
/// migration without bumping this is the point of the assertion.
const EXPECTED_SCHEMA_VERSION: i64 = 1;

#[tokio::test]
async fn an_unmigrated_database_reports_version_zero_rather_than_failing() {
    let db = SqliteDb::open_in_memory().await.expect("opens");
    assert_eq!(
        db.schema_version().await.expect("version is answerable"),
        0,
        "a database with no _sqlx_migrations table is at version 0"
    );
}

#[tokio::test]
async fn migrate_is_idempotent_and_leaves_a_stable_version() {
    let db = SqliteDb::open_in_memory().await.expect("opens");

    db.migrate().await.expect("first migration applies");
    let first = db.schema_version().await.expect("version readable");
    assert_eq!(first, EXPECTED_SCHEMA_VERSION);

    db.migrate().await.expect("second migration is a no-op");
    let second = db.schema_version().await.expect("version readable");
    assert_eq!(
        second, first,
        "re-running migrations must not change the schema version"
    );

    // And the database still works afterwards, which a corrupted re-run would
    // not.
    db.users()
        .create(&user("u1", "alice"))
        .await
        .expect("writes");
}

#[tokio::test]
async fn concurrent_migrate_calls_on_one_handle_all_succeed() {
    let db = SqliteDb::open_in_memory().await.expect("opens");

    let (a, b, c) = tokio::join!(db.migrate(), db.migrate(), db.migrate());
    for (i, result) in [a, b, c].into_iter().enumerate() {
        result.unwrap_or_else(|e| panic!("concurrent migrate #{i} must succeed: {e}"));
    }
    assert_eq!(
        db.schema_version().await.expect("version readable"),
        EXPECTED_SCHEMA_VERSION
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_migrate_from_separate_handles_on_one_file_all_succeed() {
    // Separate `SqliteDb` values do not share the in-process mutex, so this is
    // the cross-process race in miniature: whoever loses must still observe a
    // fully migrated schema rather than an error.
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("race.db");

    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        handles.push(tokio::spawn(async move {
            let db = SqliteDb::open(&path).await.expect("opens");
            db.migrate().await
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .expect("the migrating task does not panic")
            .unwrap_or_else(|e| panic!("racing migrator #{i} must succeed: {e}"));
    }

    let db = SqliteDb::open(&path).await.expect("opens");
    assert_eq!(
        db.schema_version().await.expect("version readable"),
        EXPECTED_SCHEMA_VERSION
    );
}

#[tokio::test]
async fn health_check_passes_while_open_and_reports_unavailable_once_closed() {
    let db = db().await;
    db.health_check().await.expect("a live database is healthy");

    db.close().await;

    let err = db
        .health_check()
        .await
        .expect_err("a closed pool is not healthy");
    assert!(
        matches!(err, PortError::Unavailable(_)),
        "a closed pool must be Unavailable (retryable), got {err:?}"
    );
    assert!(err.is_retryable(), "the caller should be told to retry");
}

#[tokio::test]
async fn operations_against_a_closed_pool_are_unavailable_not_internal() {
    let db = db().await;
    db.close().await;

    let err = db
        .users()
        .by_id("anyone")
        .await
        .expect_err("a closed pool cannot serve reads");
    assert!(
        matches!(err, PortError::Unavailable(_)),
        "a closed pool must map to Unavailable, got {err:?}"
    );
}

/// The one test that must not use an in-memory database.
///
/// `open_in_memory` pins the pool to a single connection, which would
/// serialise these writers and prove nothing. A file-backed database with a
/// real pool is where SQLite's single-writer rule actually bites, and where a
/// missing busy timeout shows up as "database is locked".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_do_not_hit_database_is_locked() {
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 25;

    let dir = tempfile::tempdir().expect("a temp dir");
    let db = SqliteDb::open(dir.path().join("concurrent.db"))
        .await
        .expect("opens");
    db.migrate().await.expect("migrates");

    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            for n in 0..PER_WRITER {
                let id = format!("w{writer}-{n}");
                db.users()
                    .create(&user(&id, &format!("player-{id}")))
                    .await
                    .unwrap_or_else(|e| panic!("writer {writer} insert {n} must not fail: {e:?}"));
            }
        }));
    }

    for task in tasks {
        task.await.expect("no writer task panics");
    }

    assert_eq!(
        db.users().count(true).await.expect("counts"),
        (WRITERS * PER_WRITER) as u64,
        "every concurrent insert must have landed exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_are_not_blocked_by_a_writer_under_wal() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let db = SqliteDb::open(dir.path().join("wal.db"))
        .await
        .expect("opens");
    db.migrate().await.expect("migrates");
    db.users()
        .create(&user("u1", "alice"))
        .await
        .expect("writes");

    let writer = {
        let db = db.clone();
        tokio::spawn(async move {
            for n in 0..50 {
                db.users()
                    .create(&user(&format!("w{n}"), &format!("bot{n}")))
                    .await
                    .expect("write succeeds");
            }
        })
    };

    let reader = {
        let db = db.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                let found = db.users().by_handle("alice").await.expect("read succeeds");
                assert!(found.is_some(), "the seeded user stays visible mid-write");
            }
        })
    };

    writer.await.expect("writer finishes");
    reader.await.expect("reader finishes");
}

/// Open a file database with plain sqlx, to do to it what a person with a
/// shell and the file could.
async fn tamper(path: &std::path::Path, statement: &str) {
    use sqlx::ConnectOptions;
    let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .connect()
        .await
        .expect("opens the file directly");
    sqlx::query(statement)
        .execute(&mut conn)
        .await
        .expect("tampers");
}

#[tokio::test]
async fn an_applied_migration_edited_after_the_fact_is_refused() {
    // sqlx checksums every applied migration. `migrate()` used to catch any
    // run error and succeed if the version number had been reached, which a
    // checksum mismatch always has: the protection was switched off.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gateway.sqlite");
    let db = SqliteDb::open(&path).await.expect("opens");
    db.migrate().await.expect("first migration applies");
    drop(db);

    tamper(
        &path,
        "UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1",
    )
    .await;

    let db = SqliteDb::open(&path).await.expect("reopens");
    assert!(
        db.migrate().await.is_err(),
        "a migration whose applied checksum does not match the binary's is a schema disagreement"
    );
}

#[tokio::test]
async fn a_database_migrated_by_a_newer_binary_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gateway.sqlite");
    let db = SqliteDb::open(&path).await.expect("opens");
    db.migrate().await.expect("first migration applies");
    drop(db);

    // A version this binary does not embed, as a newer build would have left.
    tamper(
        &path,
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) \
         VALUES (999, 'from the future', CURRENT_TIMESTAMP, 1, X'00', 0)",
    )
    .await;

    let db = SqliteDb::open(&path).await.expect("reopens");
    assert!(
        db.migrate().await.is_err(),
        "an older binary must not run against a schema it has never seen"
    );
}

#[tokio::test]
async fn reopening_a_migrated_file_and_migrating_again_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gateway.sqlite");
    let db = SqliteDb::open(&path).await.expect("opens");
    db.migrate().await.expect("first migration applies");
    drop(db);
    let db = SqliteDb::open(&path).await.expect("reopens");
    db.migrate()
        .await
        .expect("an unchanged schema migrates cleanly");
    assert_eq!(
        db.schema_version().await.expect("version"),
        EXPECTED_SCHEMA_VERSION
    );
}
