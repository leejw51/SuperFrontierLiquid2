//! Admission through the verification pipeline: nonce ordering, gaps,
//! replays, and the mempool bound.
//!
//! What this proves: the coordinator admits each sender's pipelined
//! transactions in the order they were sent even when several senders
//! interleave; a gap or a replay is refused with `badNonce` naming the
//! nonce expected; a full mempool (overall or for one sender) is refused
//! with `mempoolFull` and a message saying which limit, *without* advancing
//! the nonce ledger, so the refused nonce is exactly what the client sends
//! again once a block has made room.

use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_rpc::{conv, server::SequencerServer, AdmissionCtx, PendingNonces, Verifier};
use cb_schema::rpc_capnp::{self, SubmitErrorCode};
use cb_state::mempool_cf::MempoolLimits;
use cb_state::{AccountState, Accounts, Database, Mempool, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, H256};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

type Cap = rpc_capnp::super_frontier_liquid::Client;

fn run_local<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    LocalSet::new().block_on(&rt, f())
}

fn chain() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    (dir, db)
}

fn gated(db: &Database) -> Cap {
    let notify = Arc::new(Notify::new());
    let pending = Arc::new(PendingNonces::new());
    let verifier = Verifier::spawn(
        AdmissionCtx {
            db: db.clone(),
            notify: notify.clone(),
            pending_nonces: Some(pending.clone()),
            tx_observer: None,
        },
        4,
    )
    .unwrap();
    capnp_rpc::new_client(SequencerServer {
        db: db.clone(),
        notify,
        vote_handler: None,
        tx_observer: None,
        pending_nonces: Some(pending),
        verifier: Some(verifier),
        rejected: None,
    })
}

fn tx(sk: &SecretKey, nonce: u64) -> SignedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([2; 20]),
                asset: AssetId::Usd,
                amount: 1 + nonce as u128,
            },
        },
    )
    .unwrap()
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Ok,
    Rejected(SubmitErrorCode, String),
}

fn send(cap: &Cap, s: &SignedTx) -> impl std::future::Future<Output = Outcome> {
    let mut req = cap.submit_tx_request();
    conv::signed_tx_to_capnp(req.get().init_tx(), s);
    let p = req.send().promise;
    async move {
        let resp = p.await.expect("call");
        match resp.get().unwrap().get_result().unwrap().which().unwrap() {
            rpc_capnp::submit_result::Ok(()) => Outcome::Ok,
            rpc_capnp::submit_result::Rejected(r) => {
                let r = r.unwrap();
                Outcome::Rejected(
                    r.get_code().unwrap(),
                    r.get_message().unwrap().to_string().unwrap(),
                )
            }
        }
    }
}

fn bad_nonce(o: &Outcome, expected: u64, got: u64) -> bool {
    matches!(o, Outcome::Rejected(SubmitErrorCode::BadNonce, m)
        if m.contains(&format!("expected {expected}, got {got}")))
}

#[test]
fn gaps_and_replays_are_refused_with_the_expected_nonce() {
    let (_d, db) = chain();
    let (sk, _) = generate_keypair();
    run_local(|| async move {
        let cap = gated(&db);
        assert_eq!(send(&cap, &tx(&sk, 0)).await, Outcome::Ok);
        assert_eq!(send(&cap, &tx(&sk, 1)).await, Outcome::Ok);
        // A gap.
        let o = send(&cap, &tx(&sk, 3)).await;
        assert!(bad_nonce(&o, 2, 3), "{o:?}");
        // Filling it makes the next one acceptable.
        assert_eq!(send(&cap, &tx(&sk, 2)).await, Outcome::Ok);
        assert_eq!(send(&cap, &tx(&sk, 3)).await, Outcome::Ok);
        // The very same bytes again: still held, so admitted again and not
        // queued twice (see `a_resubmitted_transaction_is_never_held_twice`).
        assert_eq!(send(&cap, &tx(&sk, 1)).await, Outcome::Ok);
        assert_eq!(send(&cap, &tx(&sk, 3)).await, Outcome::Ok);
        // A different transaction at an admitted nonce is a replay attempt,
        // and is refused with the nonce expected.
        let other = SignedTx::sign_with(
            &sk,
            Transaction {
                nonce: 1,
                chain_id: 1,
                body: TxBody::Transfer {
                    to: Address([9; 20]),
                    asset: AssetId::Usd,
                    amount: 5,
                },
            },
        )
        .unwrap();
        let o = send(&cap, &other).await;
        assert!(bad_nonce(&o, 4, 1), "{o:?}");
        // The far end of the range.
        let o = send(&cap, &tx(&sk, u64::MAX)).await;
        assert!(bad_nonce(&o, 4, u64::MAX), "{o:?}");
        assert_eq!(Mempool::new(&db).len(), 4);
    });
}

#[test]
fn the_chain_nonce_is_the_floor() {
    let (_d, db) = chain();
    let (sk, pk) = generate_keypair();
    let me = address_from_pubkey(&pk);
    Accounts::new(&db).put(
        &me,
        &AccountState {
            nonce: 5,
            ..Default::default()
        },
    );
    run_local(|| async move {
        let cap = gated(&db);
        let o = send(&cap, &tx(&sk, 0)).await;
        assert!(bad_nonce(&o, 5, 0), "{o:?}");
        assert_eq!(send(&cap, &tx(&sk, 5)).await, Outcome::Ok);
    });
}

/// Four senders pipelining 150 transactions each, all requests in flight at
/// once and interleaved: every one is admitted, and each sender's run sits
/// in the mempool in nonce order.
#[test]
fn interleaved_pipelined_senders_are_each_admitted_in_order() {
    let (_d, db) = chain();
    let keys: Vec<(SecretKey, Address)> = (0..4)
        .map(|_| {
            let (sk, pk) = generate_keypair();
            (sk, address_from_pubkey(&pk))
        })
        .collect();
    const PER: u64 = 150;
    let signed: Vec<Vec<SignedTx>> = keys
        .iter()
        .map(|(sk, _)| (0..PER).map(|n| tx(sk, n)).collect())
        .collect();
    let check = db.clone();
    run_local(|| async move {
        let cap = gated(&db);
        let mut futs = Vec::new();
        for n in 0..PER as usize {
            for s in &signed {
                futs.push(send(&cap, &s[n]));
            }
        }
        let outcomes = futures::future::join_all(futs).await;
        assert!(
            outcomes.iter().all(|o| *o == Outcome::Ok),
            "{:?}",
            outcomes.iter().find(|o| **o != Outcome::Ok)
        );
    });
    let drained = Mempool::new(&check).drain(10_000);
    assert_eq!(drained.len(), 4 * PER as usize);
    let mut per: std::collections::HashMap<Address, Vec<u64>> = Default::default();
    for (_, bytes) in drained {
        let rec: cb_tx::MempoolRecord = bincode::deserialize(&bytes).unwrap();
        per.entry(rec.sender).or_default().push(rec.signed.tx.nonce);
    }
    for (_, a) in &keys {
        assert_eq!(per[a], (0..PER).collect::<Vec<_>>());
    }
}

#[test]
fn a_full_mempool_refuses_without_advancing_the_nonce() {
    let (_d, db) = chain();
    let (sk, _) = generate_keypair();
    let (sk2, _) = generate_keypair();
    let (sk3, _) = generate_keypair();
    Mempool::new(&db).set_limits(MempoolLimits {
        max_total: 3,
        max_per_sender: 2,
    });
    run_local(|| async move {
        let cap = gated(&db);
        assert_eq!(send(&cap, &tx(&sk, 0)).await, Outcome::Ok);
        assert_eq!(send(&cap, &tx(&sk, 1)).await, Outcome::Ok);
        // This sender's share is full.
        let o = send(&cap, &tx(&sk, 2)).await;
        assert!(
            matches!(&o, Outcome::Rejected(SubmitErrorCode::MempoolFull, m) if m.contains("this sender has 2")),
            "{o:?}"
        );
        // The ledger did not move: 2 is still the nonce expected, not 3.
        let o = send(&cap, &tx(&sk, 3)).await;
        assert!(bad_nonce(&o, 2, 3), "{o:?}");
        // The pool's total.
        assert_eq!(send(&cap, &tx(&sk2, 0)).await, Outcome::Ok);
        let o = send(&cap, &tx(&sk3, 0)).await;
        assert!(
            matches!(&o, Outcome::Rejected(SubmitErrorCode::MempoolFull, m) if m.contains("mempool is full")),
            "{o:?}"
        );
        // A block takes the first sender's first transaction: its refused
        // nonce now goes in, and so does the third sender's.
        Mempool::new(&db).delete(&tx(&sk, 0).hash());
        assert_eq!(send(&cap, &tx(&sk, 2)).await, Outcome::Ok);
        Mempool::new(&db).delete(&tx(&sk, 1).hash());
        assert_eq!(send(&cap, &tx(&sk3, 0)).await, Outcome::Ok);
        assert_eq!(Mempool::new(&db).len(), 3);
    });
}

/// A resubmission of a transaction already in the mempool is a nonce
/// replay to the ledger, and never a second mempool entry.
/// A copy of a transaction this node already holds, or committed moments
/// ago, is admitted without a look at its signature and never queued
/// again. A client that resubmits after a timeout hears "held", not a nonce
/// error that would send it re-reading its nonce; a forwarded copy that
/// lost the race against the leader's proposal costs one hash instead of a
/// signature recovery and a dead mempool entry.
#[test]
fn a_resubmitted_transaction_is_never_held_twice() {
    let (_d, db) = chain();
    let (sk, _) = generate_keypair();
    run_local(|| async move {
        let cap = gated(&db);
        let t = tx(&sk, 0);
        assert_eq!(send(&cap, &t).await, Outcome::Ok);
        assert_eq!(
            send(&cap, &t).await,
            Outcome::Ok,
            "still held: admitted again"
        );
        assert_eq!(Mempool::new(&db).len(), 1);
        assert!(Mempool::new(&db).get(&t.hash()).is_some());
        assert!(Mempool::new(&db).get(&H256([0; 32])).is_none());

        // Committed: gone from the queue, remembered, and a late copy is
        // answered without being queued.
        let h = t.hash();
        Mempool::new(&db).delete_many(&[h]);
        Mempool::new(&db).mark_committed(&[h]);
        assert!(Mempool::new(&db).is_known(&h));
        assert_eq!(send(&cap, &t).await, Outcome::Ok);
        assert_eq!(Mempool::new(&db).len(), 0, "a committed copy is not queued");
    });
}
