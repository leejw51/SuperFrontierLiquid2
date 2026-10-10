//! Every repository method, plus the failure paths that make the port worth
//! having.

mod common;

use cb_ports::{AppDatabase, CandleInterval, LeaderboardWindow, PortError};
use common::{
    audit, bot, candle, claim, d, db, entry, perp, rank, seed_user, session, spot, t, user, user_at,
};

// ------------------------------------------------------------------ users --

#[tokio::test]
async fn a_user_round_trips_through_every_lookup() {
    let db = db().await;
    let alice = user("u1", "Alice");
    seed_user(&db, &alice).await;

    assert_eq!(
        db.users().by_id("u1").await.expect("read"),
        Some(alice.clone())
    );
    assert_eq!(
        db.users().by_address("0xu1").await.expect("read"),
        Some(alice.clone())
    );
    assert_eq!(
        db.users().by_handle("Alice").await.expect("read"),
        Some(alice)
    );

    assert_eq!(db.users().by_id("missing").await.expect("read"), None);
    assert_eq!(db.users().by_handle("nobody").await.expect("read"), None);
    assert_eq!(db.users().by_address("0xnope").await.expect("read"), None);
}

#[tokio::test]
async fn handle_uniqueness_is_case_insensitive_and_enforced_by_the_schema() {
    let db = db().await;
    seed_user(&db, &user("u1", "Alice")).await;

    // A different id and a different address, so the only thing that can
    // collide is the handle.
    let impostor = user("u2", "ALICE");
    let err = db
        .users()
        .create(&impostor)
        .await
        .expect_err("a differently-cased duplicate handle must be rejected");

    match &err {
        PortError::Conflict(msg) => assert!(
            msg.contains("handle_lower"),
            "the conflict must come from the unique index on the lowercased \
             handle, not from some other constraint: {msg}"
        ),
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert!(!err.is_retryable(), "retrying a taken handle cannot help");

    assert_eq!(
        db.users().count(true).await.expect("count"),
        1,
        "the rejected insert must not have landed"
    );
}

#[tokio::test]
async fn a_handle_lookup_ignores_case_in_either_direction() {
    let db = db().await;
    seed_user(&db, &user("u1", "MixedCase")).await;

    for spelling in ["MixedCase", "mixedcase", "MIXEDCASE", "mIxEdCaSe"] {
        let found = db
            .users()
            .by_handle(spelling)
            .await
            .expect("read")
            .unwrap_or_else(|| panic!("`{spelling}` must find the stored handle"));
        assert_eq!(
            found.handle, "MixedCase",
            "the stored spelling is returned, not the queried one"
        );
    }
}

#[tokio::test]
async fn a_duplicate_address_is_a_conflict() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let mut clash = user("u2", "bob");
    clash.address = "0xu1".into();

    let err = db
        .users()
        .create(&clash)
        .await
        .expect_err("addresses are unique");
    assert!(matches!(err, PortError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn touch_last_seen_updates_the_row_and_reports_a_missing_user() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let later = t("2026-03-04T05:06:07.008009Z");
    db.users()
        .touch_last_seen("u1", later)
        .await
        .expect("touch");

    let reloaded = db
        .users()
        .by_id("u1")
        .await
        .expect("read")
        .expect("present");
    assert_eq!(reloaded.last_seen_at, later, "microseconds must survive");
    assert_eq!(
        reloaded.created_at,
        t("2026-01-01T00:00:00Z"),
        "touching must not disturb created_at"
    );

    let err = db
        .users()
        .touch_last_seen("ghost", later)
        .await
        .expect_err("touching a user who does not exist is an error");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn listing_users_is_oldest_first_respects_limit_and_can_exclude_bots() {
    let db = db().await;
    seed_user(&db, &user_at("u3", "carol", t("2026-01-03T00:00:00Z"))).await;
    seed_user(&db, &user_at("u1", "alice", t("2026-01-01T00:00:00Z"))).await;
    seed_user(&db, &user_at("u2", "bob", t("2026-01-02T00:00:00Z"))).await;
    db.users()
        .create(&bot("b1", "marketmaker"))
        .await
        .expect("bots insert");

    let humans: Vec<_> = db
        .users()
        .list(false, 10)
        .await
        .expect("list")
        .into_iter()
        .map(|u| u.handle)
        .collect();
    assert_eq!(
        humans,
        vec!["alice", "bob", "carol"],
        "oldest first, no bots"
    );

    let all = db.users().list(true, 10).await.expect("list");
    assert_eq!(all.len(), 4, "including bots returns the bot too");

    let capped = db.users().list(true, 2).await.expect("list");
    assert_eq!(capped.len(), 2, "limit is applied");
    assert_eq!(
        capped[0].handle, "marketmaker",
        "the bot shares the earliest instant, and user_id breaks the tie"
    );

    assert_eq!(db.users().count(true).await.expect("count"), 4);
    assert_eq!(db.users().count(false).await.expect("count"), 3);

    assert!(
        db.users().list(true, 0).await.expect("list").is_empty(),
        "a zero limit asks for nothing and gets nothing"
    );
}

// --------------------------------------------------------------- sessions --

#[tokio::test]
async fn a_session_round_trips_and_revocation_is_visible() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let s = session("hash-a", "u1", t("2026-02-01T00:00:00Z"));
    db.sessions().create(&s).await.expect("create");

    assert_eq!(
        db.sessions().by_token_hash("hash-a").await.expect("read"),
        Some(s)
    );
    assert_eq!(
        db.sessions().by_token_hash("nope").await.expect("read"),
        None,
        "an unknown token is None, not an error"
    );

    db.sessions().revoke("hash-a").await.expect("revoke");
    let after = db
        .sessions()
        .by_token_hash("hash-a")
        .await
        .expect("read")
        .expect("a revoked session is still returned");
    assert!(
        after.revoked,
        "the caller distinguishes revoked from absent"
    );
}

#[tokio::test]
async fn revoking_an_unknown_session_is_not_found() {
    let db = db().await;
    let err = db
        .sessions()
        .revoke("ghost")
        .await
        .expect_err("nothing to revoke");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn a_session_for_an_unknown_user_is_rejected_by_the_foreign_key() {
    let db = db().await;
    let err = db
        .sessions()
        .create(&session("hash", "ghost", t("2026-02-01T00:00:00Z")))
        .await
        .expect_err("sessions require a user");
    assert!(
        matches!(err, PortError::InvalidArgument(_)),
        "a dangling reference is the caller's mistake, not contention: {err:?}"
    );
}

#[tokio::test]
async fn revoke_all_for_user_counts_only_what_it_changed() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    seed_user(&db, &user("u2", "bob")).await;

    for n in 0..3 {
        db.sessions()
            .create(&session(&format!("a{n}"), "u1", t("2026-02-01T00:00:00Z")))
            .await
            .expect("create");
    }
    db.sessions()
        .create(&session("b0", "u2", t("2026-02-01T00:00:00Z")))
        .await
        .expect("create");

    assert_eq!(
        db.sessions()
            .revoke_all_for_user("u1")
            .await
            .expect("revoke"),
        3
    );
    assert_eq!(
        db.sessions()
            .revoke_all_for_user("u1")
            .await
            .expect("revoke"),
        0,
        "a second sweep has nothing left to do"
    );

    let others = db
        .sessions()
        .by_token_hash("b0")
        .await
        .expect("read")
        .expect("present");
    assert!(!others.revoked, "another user's session is untouched");
}

#[tokio::test]
async fn delete_expired_removes_only_genuinely_expired_rows() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let cutoff = t("2026-02-01T00:00:00Z");
    db.sessions()
        .create(&session(
            "long-gone",
            "u1",
            cutoff - chrono::Duration::days(1),
        ))
        .await
        .expect("create");
    db.sessions()
        .create(&session(
            "one-micro-early",
            "u1",
            cutoff - chrono::Duration::microseconds(1),
        ))
        .await
        .expect("create");
    db.sessions()
        .create(&session("exactly-at-cutoff", "u1", cutoff))
        .await
        .expect("create");
    db.sessions()
        .create(&session(
            "still-valid",
            "u1",
            cutoff + chrono::Duration::days(1),
        ))
        .await
        .expect("create");

    let removed = db.sessions().delete_expired(cutoff).await.expect("sweep");
    assert_eq!(removed, 2, "only rows strictly before the cutoff go");

    for kept in ["exactly-at-cutoff", "still-valid"] {
        assert!(
            db.sessions()
                .by_token_hash(kept)
                .await
                .expect("read")
                .is_some(),
            "`{kept}` must survive the sweep"
        );
    }
    for gone in ["long-gone", "one-micro-early"] {
        assert!(
            db.sessions()
                .by_token_hash(gone)
                .await
                .expect("read")
                .is_none(),
            "`{gone}` must have been swept"
        );
    }

    assert_eq!(
        db.sessions().delete_expired(cutoff).await.expect("sweep"),
        0,
        "sweeping twice removes nothing the second time"
    );
}

// ----------------------------------------------------------------- faucet --

#[tokio::test]
async fn faucet_claims_round_trip_and_aggregate_exactly() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    seed_user(&db, &user("u2", "bob")).await;

    assert_eq!(db.faucet().last_claim_at("u1").await.expect("read"), None);
    assert_eq!(db.faucet().claim_count("u1").await.expect("read"), 0);
    assert!(db.faucet().total_granted().await.expect("read").is_empty());

    let first = claim("c1", "u1", "USD", "1000.00", "2026-01-01T00:00:00Z");
    let second = claim("c2", "u1", "USD", "0.00000001", "2026-01-02T00:00:00Z");
    let third = claim("c3", "u2", "BTC", "0.5", "2026-01-03T00:00:00Z");
    for c in [&first, &second, &third] {
        db.faucet().record_claim(c).await.expect("record");
    }

    assert_eq!(
        db.faucet().last_claim_at("u1").await.expect("read"),
        Some(t("2026-01-02T00:00:00Z"))
    );
    assert_eq!(db.faucet().claim_count("u1").await.expect("read"), 2);

    let history = db.faucet().history("u1", 10).await.expect("read");
    assert_eq!(
        history
            .iter()
            .map(|c| c.claim_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c2", "c1"],
        "history is newest first"
    );
    assert_eq!(history[1], first, "every field survives the round trip");

    assert_eq!(db.faucet().history("u1", 1).await.expect("read").len(), 1);

    let totals = db.faucet().total_granted().await.expect("read");
    assert_eq!(
        totals,
        vec![
            ("BTC".to_string(), d("0.5")),
            ("USD".to_string(), d("1000.00000001")),
        ],
        "totals are grouped by asset, ordered by asset, and exact to the satoshi"
    );
}

#[tokio::test]
async fn a_faucet_claim_for_an_unknown_user_is_rejected() {
    let db = db().await;
    let err = db
        .faucet()
        .record_claim(&claim("c1", "ghost", "USD", "1", "2026-01-01T00:00:00Z"))
        .await
        .expect_err("claims require a user");
    assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");
}

// ---------------------------------------------------------------- candles --

#[tokio::test]
async fn upsert_replaces_a_bar_rather_than_duplicating_it() {
    let db = db().await;
    let key_time = "2026-01-01T00:01:00Z";

    db.candles()
        .upsert(&candle("BTC-PERP", "1m", key_time, "101"))
        .await
        .expect("insert");

    let mut revised = candle("BTC-PERP", "1m", key_time, "107.5");
    revised.high = d("112");
    revised.volume = d("99.99999999");
    revised.trade_count = 51;
    db.candles().upsert(&revised).await.expect("update");

    let bars = db
        .candles()
        .range("BTC-PERP", CandleInterval::M1, None, None, 100)
        .await
        .expect("read");
    assert_eq!(
        bars.len(),
        1,
        "the same (symbol, interval, open_time) is one bar"
    );
    assert_eq!(bars[0], revised, "the second write wins in every column");
}

#[tokio::test]
async fn a_different_interval_or_symbol_is_a_different_bar() {
    let db = db().await;
    let at = "2026-01-01T00:00:00Z";
    db.candles()
        .upsert(&candle("BTC-PERP", "1m", at, "1"))
        .await
        .expect("w");
    db.candles()
        .upsert(&candle("BTC-PERP", "5m", at, "2"))
        .await
        .expect("w");
    db.candles()
        .upsert(&candle("ETH-PERP", "1m", at, "3"))
        .await
        .expect("w");

    assert_eq!(
        db.candles()
            .latest("BTC-PERP", CandleInterval::M1)
            .await
            .expect("read")
            .expect("present")
            .close,
        d("1")
    );
    assert_eq!(
        db.candles()
            .latest("BTC-PERP", CandleInterval::M5)
            .await
            .expect("read")
            .expect("present")
            .close,
        d("2")
    );
    assert_eq!(
        db.candles()
            .latest("ETH-PERP", CandleInterval::M1)
            .await
            .expect("read")
            .expect("present")
            .close,
        d("3")
    );
}

#[tokio::test]
async fn upsert_many_is_one_transaction_and_is_also_an_upsert() {
    let db = db().await;
    let batch: Vec<_> = (0..5)
        .map(|m| {
            candle(
                "BTC-PERP",
                "1m",
                &format!("2026-01-01T00:0{m}:00Z"),
                &format!("10{m}"),
            )
        })
        .collect();

    db.candles().upsert_many(&batch).await.expect("first batch");
    db.candles()
        .upsert_many(&batch)
        .await
        .expect("second batch");

    let bars = db
        .candles()
        .range("BTC-PERP", CandleInterval::M1, None, None, 100)
        .await
        .expect("read");
    assert_eq!(bars.len(), 5, "replaying a batch must not duplicate bars");

    db.candles()
        .upsert_many(&[])
        .await
        .expect("an empty batch is a no-op");
}

/// The other half of `upsert_many`'s transaction guarantee: a batch that fails
/// partway must leave nothing behind.
///
/// A half-applied batch would give a chart a hole that no later write repairs,
/// because the aggregator only ever revisits the current bucket.
#[tokio::test]
async fn a_candle_batch_that_fails_partway_leaves_nothing_behind() {
    let db = db().await;

    let mut doomed = candle("BTC-PERP", "1m", "2026-01-01T00:02:00Z", "102");
    // Larger than SQLite's signed 64-bit INTEGER, so encoding it is rejected
    // after the first two bars have already been written inside the
    // transaction.
    doomed.trade_count = u64::MAX;

    let err = db
        .candles()
        .upsert_many(&[
            candle("BTC-PERP", "1m", "2026-01-01T00:00:00Z", "100"),
            candle("BTC-PERP", "1m", "2026-01-01T00:01:00Z", "101"),
            doomed,
            candle("BTC-PERP", "1m", "2026-01-01T00:03:00Z", "103"),
        ])
        .await
        .expect_err("a batch with an unstorable bar must fail");
    assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");

    assert!(
        db.candles()
            .range("BTC-PERP", CandleInterval::M1, None, None, 100)
            .await
            .expect("read")
            .is_empty(),
        "the two bars written before the failure must have rolled back"
    );

    // The store is still usable: a leaked transaction would have left the
    // connection mid-transaction and the next write would hang or fail.
    db.candles()
        .upsert(&candle("BTC-PERP", "1m", "2026-01-01T00:00:00Z", "100"))
        .await
        .expect("the store recovers");
}

#[tokio::test]
async fn range_is_oldest_first_and_honours_a_half_open_window_and_limit() {
    let db = db().await;
    let batch: Vec<_> = (0..6)
        .map(|m| {
            candle(
                "BTC-PERP",
                "1m",
                &format!("2026-01-01T00:0{m}:00Z"),
                &format!("10{m}"),
            )
        })
        .collect();
    db.candles().upsert_many(&batch).await.expect("seed");

    let all = db
        .candles()
        .range("BTC-PERP", CandleInterval::M1, None, None, 100)
        .await
        .expect("read");
    assert_eq!(
        all.iter().map(|c| c.open_time).collect::<Vec<_>>(),
        (0..6)
            .map(|m| t(&format!("2026-01-01T00:0{m}:00Z")))
            .collect::<Vec<_>>(),
        "oldest first"
    );

    let windowed = db
        .candles()
        .range(
            "BTC-PERP",
            CandleInterval::M1,
            Some(t("2026-01-01T00:02:00Z")),
            Some(t("2026-01-01T00:04:00Z")),
            100,
        )
        .await
        .expect("read");
    assert_eq!(
        windowed.iter().map(|c| c.close).collect::<Vec<_>>(),
        vec![d("102"), d("103")],
        "`from` is inclusive and `to` is exclusive"
    );

    let from_only = db
        .candles()
        .range(
            "BTC-PERP",
            CandleInterval::M1,
            Some(t("2026-01-01T00:04:00Z")),
            None,
            100,
        )
        .await
        .expect("read");
    assert_eq!(from_only.len(), 2, "an open-ended window runs to the end");

    let limited = db
        .candles()
        .range("BTC-PERP", CandleInterval::M1, None, None, 2)
        .await
        .expect("read");
    assert_eq!(
        limited.iter().map(|c| c.close).collect::<Vec<_>>(),
        vec![d("100"), d("101")],
        "limit keeps the oldest, because the order is oldest-first"
    );

    assert!(
        db.candles()
            .range("NOPE", CandleInterval::M1, None, None, 10)
            .await
            .expect("read")
            .is_empty(),
        "an unknown symbol is an empty range, not an error"
    );
}

#[tokio::test]
async fn latest_returns_the_newest_bar_or_none() {
    let db = db().await;
    assert_eq!(
        db.candles()
            .latest("BTC-PERP", CandleInterval::H1)
            .await
            .expect("read"),
        None
    );

    db.candles()
        .upsert_many(&[
            candle("BTC-PERP", "1h", "2026-01-01T00:00:00Z", "100"),
            candle("BTC-PERP", "1h", "2026-01-01T02:00:00Z", "102"),
            candle("BTC-PERP", "1h", "2026-01-01T01:00:00Z", "101"),
        ])
        .await
        .expect("seed");

    let latest = db
        .candles()
        .latest("BTC-PERP", CandleInterval::H1)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(latest.open_time, t("2026-01-01T02:00:00Z"));
}

#[tokio::test]
async fn prune_drops_old_bars_of_one_interval_only() {
    let db = db().await;
    db.candles()
        .upsert_many(&[
            candle("BTC-PERP", "1m", "2026-01-01T00:00:00Z", "1"),
            candle("BTC-PERP", "1m", "2026-01-01T00:01:00Z", "2"),
            candle("ETH-PERP", "1m", "2026-01-01T00:00:00Z", "3"),
            candle("BTC-PERP", "1h", "2026-01-01T00:00:00Z", "4"),
        ])
        .await
        .expect("seed");

    let removed = db
        .candles()
        .prune(CandleInterval::M1, t("2026-01-01T00:01:00Z"))
        .await
        .expect("prune");
    assert_eq!(removed, 2, "both symbols' old 1m bars go; the 1h bar stays");

    assert_eq!(
        db.candles()
            .range("BTC-PERP", CandleInterval::M1, None, None, 10)
            .await
            .expect("read")
            .len(),
        1,
        "the bar exactly at the cutoff survives"
    );
    assert!(
        db.candles()
            .latest("BTC-PERP", CandleInterval::H1)
            .await
            .expect("read")
            .is_some(),
        "pruning 1m must not touch 1h"
    );
}

// ----------------------------------------------------------------- ledger --

#[tokio::test]
async fn ledger_entries_round_trip_newest_first_and_filter_by_kind() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let faucet = entry("e1", "u1", "faucet", "USD", "1000", "2026-01-01T00:00:00Z");
    let fee = entry("e2", "u1", "fee", "USD", "-0.25", "2026-01-01T00:01:00Z");
    let funding = entry("e3", "u1", "funding", "USD", "-1.5", "2026-01-01T00:02:00Z");
    db.ledger().append(&faucet).await.expect("append");
    db.ledger()
        .append_many(&[fee.clone(), funding.clone()])
        .await
        .expect("append");

    let all = db.ledger().for_user("u1", None, 10).await.expect("read");
    assert_eq!(
        all.iter().map(|e| e.entry_id.as_str()).collect::<Vec<_>>(),
        vec!["e3", "e2", "e1"],
        "newest first"
    );
    assert_eq!(
        all[2], faucet,
        "every column survives, including the optional ones"
    );

    let kinds = vec!["fee".to_string(), "funding".to_string()];
    let filtered = db
        .ledger()
        .for_user("u1", Some(&kinds), 10)
        .await
        .expect("read");
    assert_eq!(filtered, vec![funding, fee]);

    assert_eq!(
        db.ledger()
            .for_user("u1", Some(&[]), 10)
            .await
            .expect("read"),
        vec![],
        "an empty kind filter selects nothing, which is what it says"
    );
    assert_eq!(
        db.ledger()
            .for_user("u1", None, 1)
            .await
            .expect("read")
            .len(),
        1,
        "limit is applied"
    );
}

#[tokio::test]
async fn optional_ledger_columns_survive_as_none() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let mut sparse = entry("e1", "u1", "faucet", "USD", "1", "2026-01-01T00:00:00Z");
    sparse.memo = None;
    sparse.tx_hash = None;
    sparse.height = None;
    db.ledger().append(&sparse).await.expect("append");

    let read = db.ledger().for_user("u1", None, 10).await.expect("read");
    assert_eq!(
        read,
        vec![sparse],
        "NULL columns come back as None, not defaults"
    );
}

/// The atomicity guarantee, tested by making the third entry unwritable.
///
/// A fill's fee entry and its realised-PnL entry only balance as a pair, so a
/// half-applied batch would leave the off-chain ledger asserting that value
/// appeared from nowhere — and nothing revisits these rows to notice.
#[tokio::test]
async fn append_many_lands_entirely_or_not_at_all() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;

    let good_a = entry("e1", "u1", "fee", "USD", "-1", "2026-01-01T00:00:00Z");
    let good_b = entry("e2", "u1", "fee", "USD", "-2", "2026-01-01T00:01:00Z");
    // References a user who does not exist: the foreign key rejects it.
    let doomed = entry("e3", "ghost", "fee", "USD", "-3", "2026-01-01T00:02:00Z");
    let never_reached = entry("e4", "u1", "fee", "USD", "-4", "2026-01-01T00:03:00Z");

    let err = db
        .ledger()
        .append_many(&[good_a, good_b, doomed, never_reached])
        .await
        .expect_err("a batch with an invalid entry must fail");
    assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");

    assert!(
        db.ledger()
            .for_user("u1", None, 100)
            .await
            .expect("read")
            .is_empty(),
        "the two valid entries written before the failure must have rolled back"
    );

    // The store is still usable afterwards — a leaked transaction would have
    // left the connection poisoned or the database locked.
    db.ledger()
        .append(&entry(
            "e5",
            "u1",
            "fee",
            "USD",
            "-5",
            "2026-01-01T00:04:00Z",
        ))
        .await
        .expect("the store recovers");
}

#[tokio::test]
async fn a_duplicate_entry_id_is_a_conflict() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    let e = entry("e1", "u1", "fee", "USD", "-1", "2026-01-01T00:00:00Z");
    db.ledger().append(&e).await.expect("append");

    let err = db.ledger().append(&e).await.expect_err("ids are unique");
    assert!(matches!(err, PortError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn net_by_asset_sums_a_half_open_window_exactly() {
    let db = db().await;
    seed_user(&db, &user("u1", "alice")).await;
    seed_user(&db, &user("u2", "bob")).await;

    db.ledger()
        .append_many(&[
            entry("e0", "u1", "faucet", "USD", "1000", "2025-12-31T23:59:59Z"),
            entry(
                "e1",
                "u1",
                "fee",
                "USD",
                "-0.00000001",
                "2026-01-01T00:00:00Z",
            ),
            entry(
                "e2",
                "u1",
                "funding",
                "USD",
                "-0.00000002",
                "2026-01-01T12:00:00Z",
            ),
            entry(
                "e3",
                "u1",
                "realized_pnl",
                "BTC",
                "0.25",
                "2026-01-01T12:00:00Z",
            ),
            entry("e4", "u1", "fee", "USD", "-500", "2026-01-02T00:00:00Z"),
            entry("e5", "u2", "fee", "USD", "-9", "2026-01-01T06:00:00Z"),
        ])
        .await
        .expect("seed");

    let net = db
        .ledger()
        .net_by_asset("u1", t("2026-01-01T00:00:00Z"), t("2026-01-02T00:00:00Z"))
        .await
        .expect("read");

    assert_eq!(
        net,
        vec![
            ("BTC".to_string(), d("0.25")),
            ("USD".to_string(), d("-0.00000003")),
        ],
        "the window includes its start, excludes its end, and sums exactly"
    );

    let err = db
        .ledger()
        .net_by_asset("u1", t("2026-01-02T00:00:00Z"), t("2026-01-01T00:00:00Z"))
        .await
        .expect_err("an inverted window is a caller error");
    assert!(matches!(err, PortError::InvalidArgument(_)), "got {err:?}");
}

// ------------------------------------------------------------ leaderboard --

#[tokio::test]
async fn a_leaderboard_window_is_replaced_wholesale() {
    let db = db().await;

    assert!(db
        .leaderboard()
        .top(LeaderboardWindow::Day, 10)
        .await
        .expect("read")
        .is_empty());
    assert_eq!(
        db.leaderboard()
            .computed_at(LeaderboardWindow::Day)
            .await
            .expect("read"),
        None
    );

    let first_pass = vec![
        rank(1, "alice", "5000"),
        rank(2, "bob", "4000"),
        rank(3, "carol", "3000"),
    ];
    let at = t("2026-01-01T00:00:00Z");
    db.leaderboard()
        .replace_window(LeaderboardWindow::Day, at, &first_pass)
        .await
        .expect("replace");

    assert_eq!(
        db.leaderboard()
            .top(LeaderboardWindow::Day, 10)
            .await
            .expect("read"),
        first_pass
    );
    assert_eq!(
        db.leaderboard()
            .computed_at(LeaderboardWindow::Day)
            .await
            .expect("read"),
        Some(at)
    );
    assert_eq!(
        db.leaderboard()
            .for_handle(LeaderboardWindow::Day, "bob")
            .await
            .expect("read"),
        Some(rank(2, "bob", "4000"))
    );
    assert_eq!(
        db.leaderboard()
            .for_handle(LeaderboardWindow::Day, "nobody")
            .await
            .expect("read"),
        None
    );

    // Carol drops out entirely; a merge would have left her at rank 3.
    let second_pass = vec![rank(1, "bob", "9000"), rank(2, "alice", "5000")];
    let later = t("2026-01-01T01:00:00Z");
    db.leaderboard()
        .replace_window(LeaderboardWindow::Day, later, &second_pass)
        .await
        .expect("replace");

    assert_eq!(
        db.leaderboard()
            .top(LeaderboardWindow::Day, 10)
            .await
            .expect("read"),
        second_pass,
        "a shrinking ranking must not leave stale rows behind"
    );
    assert_eq!(
        db.leaderboard()
            .computed_at(LeaderboardWindow::Day)
            .await
            .expect("read"),
        Some(later)
    );
    assert_eq!(
        db.leaderboard()
            .top(LeaderboardWindow::Day, 1)
            .await
            .expect("read")
            .len(),
        1,
        "limit is applied"
    );
}

#[tokio::test]
async fn leaderboard_windows_are_independent() {
    let db = db().await;
    db.leaderboard()
        .replace_window(
            LeaderboardWindow::Day,
            t("2026-01-01T00:00:00Z"),
            &[rank(1, "alice", "100")],
        )
        .await
        .expect("replace");
    db.leaderboard()
        .replace_window(
            LeaderboardWindow::AllTime,
            t("2026-01-01T00:00:00Z"),
            &[rank(1, "bob", "999")],
        )
        .await
        .expect("replace");

    assert_eq!(
        db.leaderboard()
            .top(LeaderboardWindow::Day, 10)
            .await
            .expect("read")[0]
            .handle,
        "alice"
    );
    assert_eq!(
        db.leaderboard()
            .top(LeaderboardWindow::AllTime, 10)
            .await
            .expect("read")[0]
            .handle,
        "bob"
    );
    assert!(
        db.leaderboard()
            .top(LeaderboardWindow::Week, 10)
            .await
            .expect("read")
            .is_empty(),
        "an untouched window stays empty"
    );
}

#[tokio::test]
async fn an_empty_ranking_still_records_when_it_was_computed() {
    let db = db().await;
    let at = t("2026-01-01T00:00:00Z");
    db.leaderboard()
        .replace_window(LeaderboardWindow::Week, at, &[])
        .await
        .expect("replace");

    assert!(db
        .leaderboard()
        .top(LeaderboardWindow::Week, 10)
        .await
        .expect("read")
        .is_empty());
    assert_eq!(
        db.leaderboard()
            .computed_at(LeaderboardWindow::Week)
            .await
            .expect("read"),
        Some(at),
        "\"nobody qualified\" is a result, and it must be distinguishable from \
         \"never computed\""
    );
}

// ------------------------------------------------------------- market meta --

#[tokio::test]
async fn markets_round_trip_including_every_nullable_column() {
    let db = db().await;
    let mut dated = perp("BTC-0327");
    dated.kind = "future".into();
    dated.expiry = Some(t("2026-03-27T08:00:00Z"));

    let markets = vec![perp("BTC-PERP"), spot("BTC-USD"), dated.clone()];
    db.markets().replace_all(&markets).await.expect("replace");

    let all = db.markets().all().await.expect("read");
    assert_eq!(
        all.iter().map(|m| m.symbol.as_str()).collect::<Vec<_>>(),
        vec!["BTC-0327", "BTC-PERP", "BTC-USD"],
        "ordered by symbol"
    );

    assert_eq!(
        db.markets().by_symbol("BTC-PERP").await.expect("read"),
        Some(perp("BTC-PERP"))
    );
    assert_eq!(
        db.markets().by_symbol("BTC-USD").await.expect("read"),
        Some(spot("BTC-USD"))
    );
    assert_eq!(
        db.markets().by_symbol("BTC-0327").await.expect("read"),
        Some(dated)
    );
    assert_eq!(db.markets().by_symbol("NOPE").await.expect("read"), None);
}

#[tokio::test]
async fn replace_all_is_a_replacement_not_a_merge() {
    let db = db().await;
    db.markets()
        .replace_all(&[perp("BTC-PERP"), perp("DOGE-PERP")])
        .await
        .expect("replace");
    db.markets()
        .replace_all(&[perp("BTC-PERP")])
        .await
        .expect("replace");

    assert_eq!(
        db.markets().all().await.expect("read").len(),
        1,
        "a delisted market must disappear from the cache"
    );
}

#[tokio::test]
async fn set_status_updates_one_market_and_reports_an_unknown_symbol() {
    let db = db().await;
    db.markets()
        .replace_all(&[perp("BTC-PERP"), perp("ETH-PERP")])
        .await
        .expect("replace");

    db.markets()
        .set_status("BTC-PERP", "halted")
        .await
        .expect("halt");
    assert_eq!(
        db.markets()
            .by_symbol("BTC-PERP")
            .await
            .expect("read")
            .expect("present")
            .status,
        "halted"
    );
    assert_eq!(
        db.markets()
            .by_symbol("ETH-PERP")
            .await
            .expect("read")
            .expect("present")
            .status,
        "trading",
        "halting one market must not halt another"
    );

    let err = db
        .markets()
        .set_status("NOPE", "halted")
        .await
        .expect_err("halting an unknown market is an error");
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

// ------------------------------------------------------------------ audit --

#[tokio::test]
async fn audit_records_round_trip_with_their_json_detail_intact() {
    let db = db().await;

    let a = audit(
        "r1",
        Some("u1"),
        "order.place",
        "BTC-PERP",
        "2026-01-01T00:00:00Z",
    );
    let b = audit(
        "r2",
        Some("u1"),
        "faucet.claim",
        "u1",
        "2026-01-01T00:01:00Z",
    );
    let system = audit(
        "r3",
        None,
        "market.halt",
        "BTC-PERP",
        "2026-01-01T00:02:00Z",
    );
    for r in [&a, &b, &system] {
        db.audit().append(r).await.expect("append");
    }

    let by_subject = db.audit().for_subject("BTC-PERP", 10).await.expect("read");
    assert_eq!(
        by_subject
            .iter()
            .map(|r| r.record_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r3", "r1"],
        "newest first"
    );
    assert_eq!(by_subject[1], a, "detail JSON survives the round trip");
    assert_eq!(by_subject[0].actor, None, "a system action has no actor");

    let by_actor = db.audit().for_actor("u1", 10).await.expect("read");
    assert_eq!(
        by_actor
            .iter()
            .map(|r| r.record_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r2", "r1"],
        "a NULL actor never matches a for_actor lookup"
    );

    assert_eq!(
        db.audit()
            .for_subject("BTC-PERP", 1)
            .await
            .expect("read")
            .len(),
        1
    );
    assert!(db
        .audit()
        .for_actor("nobody", 10)
        .await
        .expect("read")
        .is_empty());
}

#[tokio::test]
async fn the_audit_trail_has_no_foreign_key_so_it_outlives_its_actor() {
    // Deliberately no user row for `u-deleted`: the audit trail must be able
    // to record what an account did after the account is gone.
    let db = db().await;
    db.audit()
        .append(&audit(
            "r1",
            Some("u-deleted"),
            "order.cancel",
            "BTC-PERP",
            "2026-01-01T00:00:00Z",
        ))
        .await
        .expect("an audit record needs no live user");

    assert_eq!(
        db.audit()
            .for_actor("u-deleted", 10)
            .await
            .expect("read")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_duplicate_audit_record_id_is_a_conflict() {
    let db = db().await;
    let r = audit("r1", None, "x", "y", "2026-01-01T00:00:00Z");
    db.audit().append(&r).await.expect("append");
    let err = db.audit().append(&r).await.expect_err("ids are unique");
    assert!(matches!(err, PortError::Conflict(_)), "got {err:?}");
}
