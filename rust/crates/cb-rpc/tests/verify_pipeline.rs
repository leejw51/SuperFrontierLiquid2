//! The parallel verifier must admit a sender's pipelined transactions in
//! the order they were sent, and must still refuse a bad signature.
//!
//! Recovery runs on a pool; admission runs on one coordinator in arrival
//! order. If that order were ever lost, a client pipelining nonces 7, 8, 9
//! would see 8 refused as `badNonce` because 7 had not been admitted yet —
//! the failure is architectural, not a matter of speed, and it would show
//! up here as anything other than 2,000 acceptances.

use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_rpc::{conv, server::SequencerServer, AdmissionCtx, PendingNonces, Verifier};
use cb_schema::rpc_capnp;
use cb_state::{AccountState, Accounts, Database, Mempool, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Notify;
use tokio::task::LocalSet;

fn run_local<F, Fut, R>(f: F) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f()))
}

fn server_with_verifier(db: Database) -> rpc_capnp::super_frontier_liquid::Client {
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
    .expect("verifier");
    let server = SequencerServer {
        db,
        notify,
        vote_handler: None,
        tx_observer: None,
        pending_nonces: Some(pending),
        verifier: Some(verifier),
        rejected: None,
    };
    capnp_rpc::new_client(server)
}

fn transfer(sk: &SecretKey, nonce: u64, to: Address) -> SignedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to,
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap()
}

#[test]
fn two_thousand_pipelined_nonces_are_admitted_in_order() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000_000).unwrap();
        accts.put(&alice, &s);
    }
    let bob = Address([0xbb; 20]);
    let db_for_check = db.clone();
    run_local(|| async move {
        let cap = server_with_verifier(db);
        // Fire every request before awaiting any: that is what a pipelining
        // client does, and it is what puts 2,000 of one sender's nonces in the
        // queue at once.
        let mut promises = Vec::with_capacity(2_000);
        for nonce in 0..2_000u64 {
            let mut req = cap.submit_tx_request();
            conv::signed_tx_to_capnp(req.get().init_tx(), &transfer(&sk, nonce, bob));
            promises.push(req.send().promise);
        }
        let mut accepted = 0usize;
        let mut refusals = Vec::new();
        for (i, p) in promises.into_iter().enumerate() {
            let resp = p.await.expect("submit_tx send");
            let result = resp.get().unwrap().get_result().unwrap();
            match result.which().unwrap() {
                rpc_capnp::submit_result::Ok(()) => accepted += 1,
                rpc_capnp::submit_result::Rejected(r) => {
                    let r = r.unwrap();
                    refusals.push((
                        i,
                        format!("{:?}", r.get_code()),
                        r.get_message().unwrap().to_string().unwrap(),
                    ));
                }
            }
        }
        assert!(
            refusals.is_empty(),
            "{} of 2,000 pipelined submissions refused; first: {:?}",
            refusals.len(),
            refusals.first()
        );
        assert_eq!(accepted, 2_000);
        assert_eq!(Mempool::new(&db_for_check).len(), 2_000);
    });
}

#[test]
fn a_tampered_signature_is_refused_through_the_pool() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, _pk) = generate_keypair();
    // Recovery-based authentication has no "wrong signature", only "whose":
    // a flipped bit in `r` recovers to some other address, which then simply
    // is the sender. What cannot recover at all is an impossible recovery id.
    let mut signed = transfer(&sk, 0, Address([0xbb; 20]));
    signed.sig_recovery = 9;
    run_local(|| async move {
        let cap = server_with_verifier(db);
        let mut req = cap.submit_tx_request();
        conv::signed_tx_to_capnp(req.get().init_tx(), &signed);
        let resp = req.send().promise.await.expect("send");
        let result = resp.get().unwrap().get_result().unwrap();
        match result.which().unwrap() {
            rpc_capnp::submit_result::Ok(()) => panic!("a tampered signature was admitted"),
            rpc_capnp::submit_result::Rejected(r) => {
                let r = r.unwrap();
                let code = r.get_code().unwrap();
                assert!(
                    matches!(code, rpc_capnp::SubmitErrorCode::BadSignature),
                    "unexpected code {code:?}"
                );
            }
        }
    });
}
