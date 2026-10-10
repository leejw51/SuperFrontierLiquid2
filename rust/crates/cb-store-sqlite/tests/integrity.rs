//! The store under the conditions that corrupt stores: a schema that
//! disagrees with the binary, transactions that fail halfway, writers racing
//! for one key, and values at the edge of what the encodings can hold.
//!
//! `repositories.rs` proves each method round-trips; `schema.rs` proves the
//! ordinary migration paths. This suite proves the failure paths leave the
//! database exactly as it was, and that a race or an extreme value produces
//! an honest error rather than a wrong answer.

mod common;

use cb_ports::{AppDatabase, LeaderboardWindow, PortError};
use cb_store_sqlite::SqliteDb;
use common::{claim, d, db, entry, perp, rank, seed_user, session, spot, t, user};

/// A migrated database in a file, for the tests that need a real pool or
/// need to reach the file behind the store's back.
async fn file_db(dir: &tempfile::TempDir, name: &str) -> (SqliteDb, std::path::PathBuf) {
    let path = dir.path().join(name);
    let db = SqliteDb::open(&path).await.expect("opens");
    db.migrate().await.expect("migrates");
    (db, path)
}

/// Run one statement against the file directly, as an operator with a
/// `sqlite3` shell could.
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

// ---------------------------------------------------------------------------
// Strict migrations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_half_applied_migration_is_refused_not_waved_through() {
    // sqlx marks a migration that failed partway with `success = 0` ("dirty").
    // Its version number is on record, so the old postcondition check would
    // have taken the schema as current.
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, path) = file_db(&dir, "dirty.sqlite").await;
    seed_user(&db, &user("u1", "alice")).await;
    drop(db);
    tamper(
        &path,
        "UPDATE _sqlx_migrations SET success = 0 WHERE version = 1",
    )
    .await;

    let db = SqliteDb::open(&path).await.expect("reopens");
    let error = db.migrate().await.expect_err("a dirty schema is refused");
    assert!(matches!(error, PortError::Internal(_)), "{error:?}");
    assert!(
        error.to_string().contains("migration"),
        "the error says what disagreed: {error}"
    );
}

#[tokio::test]
async fn a_schema_whose_bookkeeping_was_lost_is_refused_and_its_data_survives() {
    // The tables exist but `_sqlx_migrations` says nothing was applied — a
    // restored backup of the data without its bookkeeping, say. Re-applying
    // the first migration onto live tables fails; the store must report that
    // rather than claim success, and must not have dropped anything trying.
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, path) = file_db(&dir, "lost.sqlite").await;
    seed_user(&db, &user("u1", "alice")).await;
    drop(db);
    tamper(&path, "DELETE FROM _sqlx_migrations").await;

    let db = SqliteDb::open(&path).await.expect("reopens");
    assert_eq!(db.schema_version().await.expect("version"), 0);
    assert!(
        db.migrate().await.is_err(),
        "the schema cannot be re-applied over itself"
    );
    assert!(
        db.users().by_id("u1").await.expect("reads").is_some(),
        "the data is untouched by the refused migration"
    );
}

#[tokio::test]
async fn a_refused_migration_is_refused_every_time_not_just_the_first() {
    // A strict check that could be retried past would be no check at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, path) = file_db(&dir, "edited.sqlite").await;
    drop(db);
    tamper(
        &path,
        "UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1",
    )
    .await;
    let db = SqliteDb::open(&path).await.expect("reopens");
    for _ in 0..3 {
        assert!(db.migrate().await.is_err());
    }
    // Concurrent callers are serialised and all refused.
    let (a, b) = tokio::join!(db.migrate(), db.migrate());
    assert!(a.is_err() && b.is_err());
}

#[tokio::test]
async fn opening_does_not_migrate_so_an_operator_can_inspect_a_suspect_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fresh.sqlite");
    let db = SqliteDb::open(&path).await.expect("opens");
    assert_eq!(db.schema_version().await.expect("version"), 0);
    assert!(
        db.users().by_id("u1").await.is_err(),
        "no schema was applied behind the operator's back"
    );
}

// ---------------------------------------------------------------------------
// Transactions roll back
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_leaderboard_rebuild_that_fails_partway_leaves_the_previous_ranking() {
    // `replace_window` deletes the window and re-inserts it in one
    // transaction. A duplicate rank in the new batch fails the second insert;
    // the delete must be undone with it, or the board goes blank.
    let db = db().await;
    let window = LeaderboardWindow::Day;
    db.leaderboard()
        .replace_window(
            window,
            t("2026-01-01T00:00:00Z"),
            &[rank(1, "alice", "100"), rank(2, "bob", "50")],
        )
        .await
        .expect("first ranking");

    let error = db
        .leaderboard()
        .replace_window(
            window,
            t("2026-01-02T00:00:00Z"),
            &[rank(1, "carol", "999"), rank(1, "dave", "998")],
        )
        .await
        .expect_err("two rows at rank 1");
    assert!(matches!(error, PortError::Conflict(_)), "{error:?}");

    let top = db.leaderboard().top(window, 10).await.expect("reads");
    let handles: Vec<&str> = top.iter().map(|e| e.handle.as_str()).collect();
    assert_eq!(handles, vec!["alice", "bob"], "the old ranking stands");
    assert_eq!(
        db.leaderboard().computed_at(window).await.expect("reads"),
        Some(t("2026-01-01T00:00:00Z")),
        "and so does its timestamp"
    );
    assert!(db
        .leaderboard()
        .for_handle(window, "carol")
        .await
        .expect("reads")
        .is_none());
}

#[tokio::test]
async fn a_market_refresh_that_fails_partway_leaves_the_previous_catalogue() {
    let db = db().await;
    db.markets()
        .replace_all(&[perp("BTC-PERP"), spot("BTC-USD")])
        .await
        .expect("first catalogue");
    let error = db
        .markets()
        .replace_all(&[perp("ETH-PERP"), perp("ETH-PERP")])
        .await
        .expect_err("a duplicate symbol");
    assert!(matches!(error, PortError::Conflict(_)), "{error:?}");

    let mut symbols: Vec<String> = db
        .markets()
        .all()
        .await
        .expect("reads")
        .into_iter()
        .map(|m| m.symbol)
        .collect();
    symbols.sort();
    assert_eq!(symbols, vec!["BTC-PERP", "BTC-USD"]);
}

#[tokio::test]
async fn a_ledger_batch_that_breaks_a_foreign_key_writes_nothing() {
    // `append_many` is atomic. A batch whose last entry names a user that
    // does not exist must not leave the entries before it behind.
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let batch = vec![
        entry("e1", "u1", "faucet", "USD", "10", "2026-01-01T00:00:00Z"),
        entry("e2", "u1", "fee", "USD", "-1", "2026-01-01T00:00:01Z"),
        entry("e3", "nobody", "fee", "USD", "-1", "2026-01-01T00:00:02Z"),
    ];
    let error = db.ledger().append_many(&batch).await.expect_err("fk");
    assert!(matches!(error, PortError::InvalidArgument(_)), "{error:?}");
    assert!(db
        .ledger()
        .for_user("u1", None, 10)
        .await
        .expect("reads")
        .is_empty());
}

// ---------------------------------------------------------------------------
// Concurrent writers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_writers_of_one_ledger_entry_id_produce_one_row_and_honest_conflicts() {
    // The watcher and `POST /orders` can fold the same block at once, and
    // both derive the same entry id. Exactly one write may land; every other
    // must be a `Conflict` the caller can treat as a replay — never a lock
    // error, never a second row.
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, _path) = file_db(&dir, "race.sqlite").await;
    seed_user(&db, &user("u1", "alice")).await;

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            db.ledger()
                .append(&entry(
                    "same",
                    "u1",
                    "transfer_out",
                    "USD",
                    "-5",
                    "2026-01-01T00:00:00Z",
                ))
                .await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.expect("joins") {
            Ok(()) => ok += 1,
            Err(PortError::Conflict(_)) => {}
            Err(other) => panic!("a racing duplicate must be a conflict, got {other:?}"),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(
        db.ledger()
            .for_user("u1", None, 100)
            .await
            .expect("reads")
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_registrations_of_one_handle_in_different_cases_admit_exactly_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, _path) = file_db(&dir, "handles.sqlite").await;
    let spellings = [
        "Satoshi", "satoshi", "SATOSHI", "sAtOsHi", "satoshI", "SAToshi",
    ];
    let mut tasks = Vec::new();
    for (i, handle) in spellings.iter().enumerate() {
        let db = db.clone();
        let handle = handle.to_string();
        tasks.push(tokio::spawn(async move {
            db.users().create(&user(&format!("u{i}"), &handle)).await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.expect("joins") {
            Ok(()) => ok += 1,
            Err(PortError::Conflict(_)) => {}
            Err(other) => panic!("expected a conflict, got {other:?}"),
        }
    }
    assert_eq!(ok, 1, "one account per handle, whatever the case");
    assert_eq!(db.users().count(true).await.expect("counts"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_faucet_claims_are_all_counted_and_summed_exactly() {
    // The conservation figure is a sum over every claim row. Under concurrent
    // writers every row must land and the total must be exact to the last
    // decimal place — a lost row or a float sum is a hole in the audit.
    let dir = tempfile::tempdir().expect("tempdir");
    let (db, _path) = file_db(&dir, "faucet.sqlite").await;
    for u in 0..4 {
        seed_user(&db, &user(&format!("u{u}"), &format!("player{u}"))).await;
    }
    let mut tasks = Vec::new();
    for u in 0..4 {
        for n in 0..20 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                let at = t("2026-01-01T00:00:00Z") + chrono::Duration::seconds(n);
                let mut c = claim(
                    &format!("c{u}-{n}"),
                    &format!("u{u}"),
                    "BTC",
                    "0.00000001",
                    "2026-01-01T00:00:00Z",
                );
                c.claimed_at = at;
                db.faucet().record_claim(&c).await
            }));
        }
    }
    for task in tasks {
        task.await.expect("joins").expect("every claim lands");
    }
    for u in 0..4 {
        assert_eq!(
            db.faucet()
                .claim_count(&format!("u{u}"))
                .await
                .expect("count"),
            20
        );
        assert_eq!(
            db.faucet()
                .last_claim_at(&format!("u{u}"))
                .await
                .expect("last"),
            Some(t("2026-01-01T00:00:19Z"))
        );
        let history = db
            .faucet()
            .history(&format!("u{u}"), 5)
            .await
            .expect("history");
        assert_eq!(history.len(), 5, "the limit is honoured");
        assert!(
            history
                .windows(2)
                .all(|w| w[0].claimed_at >= w[1].claimed_at),
            "newest first"
        );
    }
    assert_eq!(
        db.faucet().total_granted().await.expect("total"),
        vec![("BTC".to_string(), d("0.0000008"))]
    );
}

// ---------------------------------------------------------------------------
// Extremes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_conservation_total_that_overflows_is_an_error_not_a_wrong_number() {
    // Two claims at the top of Decimal's range. A float sum would return a
    // plausible, wrong figure; a saturating one would under-report. The store
    // must say it cannot represent the answer.
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let max = "79228162514264337593543950335";
    db.faucet()
        .record_claim(&claim("c1", "u1", "USD", max, "2026-01-01T00:00:00Z"))
        .await
        .expect("stored");
    db.faucet()
        .record_claim(&claim("c2", "u1", "USD", max, "2026-01-01T00:00:01Z"))
        .await
        .expect("stored");
    let error = db.faucet().total_granted().await.expect_err("overflow");
    assert!(matches!(error, PortError::Internal(_)), "{error:?}");
}

#[tokio::test]
async fn a_net_movement_that_overflows_is_an_error_not_a_wrong_number() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let max = "79228162514264337593543950335";
    for (id, at) in [
        ("e1", "2026-01-01T00:00:00Z"),
        ("e2", "2026-01-01T00:00:01Z"),
    ] {
        db.ledger()
            .append(&entry(id, "u1", "faucet", "USD", max, at))
            .await
            .expect("stored");
    }
    let error = db
        .ledger()
        .net_by_asset("u1", t("2026-01-01T00:00:00Z"), t("2026-01-02T00:00:00Z"))
        .await
        .expect_err("overflow");
    assert!(matches!(error, PortError::Internal(_)), "{error:?}");
}

#[tokio::test]
async fn the_finest_and_the_largest_decimals_and_opposite_signs_net_exactly() {
    // 28 fractional digits (Decimal's finest scale) netted against the
    // largest magnitude, and a value and its negation cancelling to a zero
    // that reads back as zero, not as "-0" or "0E-28".
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let rows = [
        ("e1", "BTC", "0.0000000000000000000000000001"),
        ("e2", "BTC", "0.0000000000000000000000000002"),
        ("e3", "USD", "12345678901234567890.123456789"),
        ("e4", "USD", "-12345678901234567890.123456789"),
    ];
    for (i, (id, asset, amount)) in rows.iter().enumerate() {
        let at = t("2026-01-01T00:00:00Z") + chrono::Duration::seconds(i as i64);
        let mut e = entry(id, "u1", "fee", asset, amount, "2026-01-01T00:00:00Z");
        e.at = at;
        db.ledger().append(&e).await.expect("stored");
    }
    let net = db
        .ledger()
        .net_by_asset("u1", t("2026-01-01T00:00:00Z"), t("2026-01-02T00:00:00Z"))
        .await
        .expect("net");
    let get = |asset: &str| net.iter().find(|(a, _)| a == asset).map(|(_, v)| *v);
    assert_eq!(get("BTC"), Some(d("0.0000000000000000000000000003")));
    assert!(
        get("USD").is_none_or(|v| v.is_zero()),
        "cancelled exactly: {net:?}"
    );
    let stored = db.ledger().for_user("u1", None, 10).await.expect("reads");
    let finest = stored.iter().find(|e| e.entry_id == "e1").expect("e1");
    assert_eq!(finest.amount, d("0.0000000000000000000000000001"));
    assert_eq!(finest.amount.scale(), 28);
}

#[tokio::test]
async fn instants_round_trip_to_the_microsecond_across_the_whole_range_the_store_uses() {
    // Instants are INTEGER microseconds. A value before the epoch, one with
    // a single microsecond, and one far in the future must all come back
    // equal and in order — an encoding with a sign or width mistake fails on
    // exactly these.
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let instants = [
        t("1969-12-31T23:59:59.999999Z"),
        t("1970-01-01T00:00:00.000001Z"),
        t("2026-09-25T12:34:56.789012Z"),
        t("9999-12-31T23:59:59.999999Z"),
    ];
    for (i, at) in instants.iter().enumerate() {
        let mut e = entry(
            &format!("e{i}"),
            "u1",
            "fee",
            "USD",
            "1",
            "2026-01-01T00:00:00Z",
        );
        e.at = *at;
        db.ledger().append(&e).await.expect("stored");
    }
    let mut back: Vec<_> = db
        .ledger()
        .for_user("u1", None, 10)
        .await
        .expect("reads")
        .into_iter()
        .map(|e| e.at)
        .collect();
    back.reverse();
    assert_eq!(
        back,
        instants.to_vec(),
        "newest-first reversed is oldest-first"
    );

    // A window ending exactly on an instant excludes it; one a microsecond
    // later includes it.
    let edge = instants[2];
    let before = db
        .ledger()
        .net_by_asset("u1", instants[1], edge)
        .await
        .expect("net");
    let through = db
        .ledger()
        .net_by_asset("u1", instants[1], edge + chrono::Duration::microseconds(1))
        .await
        .expect("net");
    assert_eq!(before, vec![("USD".to_string(), d("1"))]);
    assert_eq!(through, vec![("USD".to_string(), d("2"))]);
}

#[tokio::test]
async fn hostile_text_round_trips_verbatim_and_is_never_executed() {
    // Every query is parameterised. Text that would break a concatenated
    // query — quotes, a statement terminator, a comment, a NUL, non-ASCII —
    // must be stored and returned byte for byte, and the table must survive.
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let hostile = "'); DROP TABLE ledger_entries; --\u{0}\"\\ 🚀 ｗｉｄｅ \u{202e}";
    let mut e = entry("e1", "u1", hostile, "USD", "1", "2026-01-01T00:00:00Z");
    e.memo = Some(hostile.to_string());
    e.tx_hash = Some(hostile.to_string());
    db.ledger().append(&e).await.expect("stored");

    let kinds = vec![hostile.to_string()];
    let back = db
        .ledger()
        .for_user("u1", Some(&kinds), 10)
        .await
        .expect("the kind filter binds the hostile text too");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].kind, hostile);
    assert_eq!(back[0].memo.as_deref(), Some(hostile));
    assert_eq!(back[0].tx_hash.as_deref(), Some(hostile));
    assert!(db
        .users()
        .by_handle("' OR '1'='1")
        .await
        .expect("reads")
        .is_none());
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_session_purge_is_strictly_before_its_cutoff() {
    // `delete_expired(before)` removes `expires_at < before`: a session
    // expiring exactly at the cutoff survives this purge (the gateway already
    // refuses it, since it treats `expires_at <= now` as expired) and goes on
    // the next one. Pinned so the two halves cannot drift into a window where
    // a purged session was still considered live.
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let cutoff = t("2026-01-02T00:00:00Z");
    for (token, at) in [
        ("before", cutoff - chrono::Duration::microseconds(1)),
        ("at", cutoff),
        ("after", cutoff + chrono::Duration::microseconds(1)),
    ] {
        db.sessions()
            .create(&session(token, "u1", at))
            .await
            .expect("stored");
    }
    assert_eq!(
        db.sessions().delete_expired(cutoff).await.expect("purged"),
        1
    );
    assert!(db
        .sessions()
        .by_token_hash("before")
        .await
        .expect("reads")
        .is_none());
    assert!(db
        .sessions()
        .by_token_hash("at")
        .await
        .expect("reads")
        .is_some());
    assert!(db
        .sessions()
        .by_token_hash("after")
        .await
        .expect("reads")
        .is_some());
    assert_eq!(
        db.sessions()
            .delete_expired(cutoff + chrono::Duration::microseconds(1))
            .await
            .expect("purged"),
        1,
        "the next purge takes it"
    );
}

#[tokio::test]
async fn revoking_a_session_twice_is_harmless_and_it_stays_revoked() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    db.sessions()
        .create(&session("tok", "u1", t("2027-01-01T00:00:00Z")))
        .await
        .expect("stored");
    db.sessions().revoke("tok").await.expect("revoked");
    db.sessions()
        .revoke("tok")
        .await
        .expect("revoking again is not an error");
    assert!(
        db.sessions()
            .by_token_hash("tok")
            .await
            .expect("reads")
            .expect("row")
            .revoked
    );
    assert_eq!(
        db.sessions()
            .revoke_all_for_user("u1")
            .await
            .expect("counts"),
        0,
        "nothing left to revoke"
    );
}
