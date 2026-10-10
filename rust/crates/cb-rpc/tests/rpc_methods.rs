//! Every RPC method, with a valid request, the boundary values of its
//! parameters, and malformed ones.
//!
//! What this proves: no request a client can build makes the server panic
//! (a panic in a capnp method would take down the connection's task, and in
//! a `LocalSet` the whole worker), each method answers a valid request, and
//! each malformed one is refused with the error its contract names — a
//! `rejected` result with the right code where the schema has one, a failed
//! call with a message otherwise.

use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_rpc::conv::{self, MAX_EXTERNAL_REF_LEN, MAX_VOTE_SIG_LEN};
use cb_rpc::server::{SequencerServer, VoteAccepted, VoteCode, VoteHandler};
use cb_schema::rpc_capnp;
use cb_schema::rpc_capnp::SubmitErrorCode;
use cb_state::{AccountState, Accounts, Database, Mempool, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, DerivId, MarketId, H256};
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

fn cap_for(server: SequencerServer) -> Cap {
    capnp_rpc::new_client(server)
}

fn plain(db: &Database) -> Cap {
    cap_for(SequencerServer::without_voting(
        db.clone(),
        Arc::new(Notify::new()),
    ))
}

fn sign(sk: &SecretKey, nonce: u64, chain_id: u32, body: TxBody) -> SignedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id,
            body,
        },
    )
    .unwrap()
}

fn transfer(amount: u128) -> TxBody {
    TxBody::Transfer {
        to: Address([2; 20]),
        asset: AssetId::Usd,
        amount,
    }
}

/// What a `submitTx` came back with.
#[derive(Debug, PartialEq)]
enum Submit {
    Ok,
    Rejected(SubmitErrorCode, String),
    Failed(String),
}

async fn submit(cap: &Cap, s: &SignedTx) -> Submit {
    let mut req = cap.submit_tx_request();
    conv::signed_tx_to_capnp(req.get().init_tx(), s);
    read_submit(req.send().promise.await)
}

fn read_submit(
    r: capnp::Result<
        capnp::capability::Response<rpc_capnp::super_frontier_liquid::submit_tx_results::Owned>,
    >,
) -> Submit {
    let resp = match r {
        Ok(r) => r,
        Err(e) => return Submit::Failed(e.to_string()),
    };
    match resp.get().unwrap().get_result().unwrap().which().unwrap() {
        rpc_capnp::submit_result::Ok(()) => Submit::Ok,
        rpc_capnp::submit_result::Rejected(r) => {
            let r = r.unwrap();
            Submit::Rejected(
                r.get_code().unwrap(),
                r.get_message().unwrap().to_string().unwrap(),
            )
        }
    }
}

fn failed_with(r: &Submit, needle: &str) -> bool {
    matches!(r, Submit::Failed(m) if m.contains(needle))
}

// ---------------------------------------------------------------- submitTx

#[test]
fn submit_tx_valid_boundary_and_malformed() {
    let (_d, db) = chain();
    let (sk, pk) = generate_keypair();
    let me = address_from_pubkey(&pk);
    let check = db.clone();
    run_local(|| async move {
        let cap = plain(&db);
        // Valid.
        assert_eq!(
            submit(&cap, &sign(&sk, 0, 1, transfer(1))).await,
            Submit::Ok
        );
        // Boundary: the largest amount, and a text field exactly at its cap.
        assert_eq!(
            submit(&cap, &sign(&sk, 1, 1, transfer(u128::MAX))).await,
            Submit::Ok
        );
        let at_cap = TxBody::Deposit {
            user: me,
            asset: AssetId::Usd,
            amount: 1,
            external_tx_ref: "r".repeat(MAX_EXTERNAL_REF_LEN),
        };
        assert_eq!(submit(&cap, &sign(&sk, 2, 1, at_cap)).await, Submit::Ok);
        // One byte past the cap, in each of the three free-text fields.
        let over = "x".repeat(MAX_EXTERNAL_REF_LEN + 1);
        for body in [
            TxBody::Deposit {
                user: me,
                asset: AssetId::Usd,
                amount: 1,
                external_tx_ref: over.clone(),
            },
            TxBody::Withdraw {
                asset: AssetId::Usd,
                amount: 1,
                external_address: over.clone(),
            },
            TxBody::WithdrawSettled {
                withdraw_id: H256([1; 32]),
                external_tx_ref: over.clone(),
            },
        ] {
            let r = submit(&cap, &sign(&sk, 3, 1, body)).await;
            assert!(failed_with(&r, "limit"), "{r:?}");
        }
        // A megabyte of text is refused the same way, not copied.
        let huge = TxBody::Withdraw {
            asset: AssetId::Usd,
            amount: 1,
            external_address: "y".repeat(1 << 20),
        };
        assert!(failed_with(
            &submit(&cap, &sign(&sk, 3, 1, huge)).await,
            "limit"
        ));
        // Static rules.
        assert!(matches!(
            submit(&cap, &sign(&sk, 3, 1, transfer(0))).await,
            Submit::Rejected(SubmitErrorCode::InvalidAmount, _)
        ));
        // Another chain's transaction.
        assert!(matches!(
            submit(&cap, &sign(&sk, 3, 2, transfer(1))).await,
            Submit::Rejected(SubmitErrorCode::NotAuthorized, _)
        ));
        // A signature that does not recover.
        let mut bad = sign(&sk, 3, 1, transfer(1));
        bad.sig_r = [0xff; 32];
        assert!(matches!(
            submit(&cap, &bad).await,
            Submit::Rejected(SubmitErrorCode::BadSignature, _)
        ));
        // Wrong-length signature halves and a missing transaction.
        for (r_len, s_len) in [(31usize, 32usize), (32, 33), (0, 0)] {
            let mut req = cap.submit_tx_request();
            let mut tx = req.get().init_tx();
            conv::transaction_to_capnp(tx.reborrow().init_tx(), &sign(&sk, 3, 1, transfer(1)).tx);
            let mut sig = tx.init_sig();
            sig.set_r(&vec![1u8; r_len]);
            sig.set_s(&vec![1u8; s_len]);
            let r = read_submit(req.send().promise.await);
            assert!(failed_with(&r, "32 bytes"), "{r:?}");
        }
        let req = cap.submit_tx_request();
        assert!(matches!(
            read_submit(req.send().promise.await),
            Submit::Failed(_)
        ));
        // A 19-byte recipient.
        let mut req = cap.submit_tx_request();
        {
            let mut tx = req.get().init_tx();
            let mut t = tx.reborrow().init_tx();
            t.set_chain_id(1);
            let mut tr = t.init_body().init_transfer();
            tr.reborrow().init_to().set_bytes(&[0u8; 19]);
            conv::u128_to_capnp(tr.init_amount(), 1);
            let mut sig = tx.init_sig();
            sig.set_r(&[1u8; 32]);
            sig.set_s(&[1u8; 32]);
        }
        assert!(failed_with(
            &read_submit(req.send().promise.await),
            "20 bytes"
        ));
    });
    // Only the three accepted transactions reached the mempool.
    assert_eq!(Mempool::new(&check).len(), 3);
}

// ------------------------------------------------------------- account reads

#[test]
fn get_account_positions_and_margin_answer_or_refuse_bad_addresses() {
    let (_d, db) = chain();
    let alice = Address([0xaa; 20]);
    let mut st = AccountState::default();
    st.add(AssetId::Usd, 42).unwrap();
    Accounts::new(&db).put(&alice, &st);
    run_local(|| async move {
        let cap = plain(&db);
        // Funded and unknown accounts.
        for (addr, usd) in [
            (alice, 42u128),
            (Address([0; 20]), 0),
            (Address([0xff; 20]), 0),
        ] {
            let mut req = cap.get_account_request();
            conv::address_to_capnp(req.get().init_addr(), &addr);
            let resp = req.send().promise.await.unwrap();
            let bals = resp
                .get()
                .unwrap()
                .get_state()
                .unwrap()
                .get_balances()
                .unwrap();
            let mut got = 0u128;
            for i in 0..bals.len() {
                let e = bals.get(i);
                if e.get_asset().unwrap() as u8 == AssetId::Usd.as_u8() {
                    got = conv::u128_from_capnp(e.get_amount().unwrap());
                }
            }
            assert_eq!(got, usd);
        }
        for len in [0usize, 19, 21, 64] {
            let mut req = cap.get_account_request();
            req.get().init_addr().set_bytes(&vec![1u8; len]);
            let e = req.send().promise.await.err().expect("bad address refused");
            assert!(e.to_string().contains("20 bytes"), "{e}");

            let mut req = cap.get_positions_request();
            req.get().init_addr().set_bytes(&vec![1u8; len]);
            assert!(req.send().promise.await.is_err());

            let mut req = cap.get_margin_account_request();
            req.get().init_addr().set_bytes(&vec![1u8; len]);
            assert!(req.send().promise.await.is_err());
        }
        // A request with no address at all.
        assert!(cap.get_account_request().send().promise.await.is_err());
        // Valid positions (none) and a default margin account.
        let mut req = cap.get_positions_request();
        conv::address_to_capnp(req.get().init_addr(), &alice);
        let resp = req.send().promise.await.unwrap();
        assert_eq!(resp.get().unwrap().get_positions().unwrap().len(), 0);
        let mut req = cap.get_margin_account_request();
        conv::address_to_capnp(req.get().init_addr(), &alice);
        assert!(req.send().promise.await.is_ok());
    });
}

// --------------------------------------------------------- blocks and chain

#[test]
fn chain_info_blocks_and_streams() {
    let (_d, db) = chain();
    run_local(|| async move {
        let cap = plain(&db);
        let resp = cap.chain_info_request().send().promise.await.unwrap();
        assert_eq!(resp.get().unwrap().get_info().unwrap().get_chain_id(), 1);
        // No block at genesis, none at the top of the range.
        for h in [0u64, 1, u64::MAX] {
            let mut req = cap.get_block_request();
            req.get().set_height(h);
            let e = req.send().promise.await.err().expect("no block");
            assert!(e.to_string().contains("no block"), "{e}");
        }
        // Subscriptions are handed out for any start height.
        for h in [0u64, u64::MAX] {
            let mut req = cap.stream_blocks_request();
            req.get().set_from_height(h);
            assert!(req.send().promise.await.is_ok());
            let mut req = cap.stream_blocks_raw_request();
            req.get().set_from_height(h);
            assert!(req.send().promise.await.is_ok());
        }
        // streamMarket is declared but not served.
        let req = cap.stream_market_request();
        let e = req.send().promise.await.err().expect("not implemented");
        assert!(e.to_string().contains("not implemented"), "{e}");
    });
}

#[test]
fn order_books_take_every_depth() {
    let (_d, db) = chain();
    run_local(|| async move {
        let cap = plain(&db);
        for depth in [0u32, 1, 1_000, 1_001, u32::MAX] {
            for m in MarketId::ALL {
                let mut req = cap.get_order_book_request();
                req.get().set_market(conv::market_to_capnp(m));
                req.get().set_depth(depth);
                assert!(req.send().promise.await.is_ok());
            }
            let mut req = cap.get_perp_order_book_request();
            conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::perp(0));
            req.get().set_depth(depth);
            assert!(req.send().promise.await.is_ok());
        }
    });
}

// ------------------------------------------------------- derivatives reads

/// Every read keyed by a `DerivId`, with a valid id, a well-formed id for a
/// market that does not exist, and three malformed ones.
#[test]
fn derivative_reads_answer_or_refuse_bad_ids() {
    let (_d, db) = chain();
    run_local(|| async move {
        let cap = plain(&db);
        let bad: [&[u8]; 4] = [&[1, 0, 0], &[9, 0, 0, 0], &[1, 0, 0, 7], &[]];
        macro_rules! by_id {
            ($req:ident, $missing_is_err:expr) => {{
                // Valid id, nothing stored.
                let mut req = cap.$req();
                conv::deriv_id_to_capnp(req.get().init_market(), &DerivId::perp(3));
                let r = req.send().promise.await;
                assert_eq!(r.is_err(), $missing_is_err, stringify!($req));
                for b in bad {
                    let mut req = cap.$req();
                    req.get().init_market().set_bytes(b);
                    let e = req.send().promise.await.err().expect(stringify!($req));
                    assert!(
                        e.to_string().contains("DerivId"),
                        "{}: {e}",
                        stringify!($req)
                    );
                }
            }};
        }
        by_id!(get_funding_index_request, false);
        by_id!(get_oracle_price_request, true);
        by_id!(get_mark_price_request, true);
        by_id!(get_perp_market_request, true);
        by_id!(get_future_market_request, true);
        by_id!(get_liquidation_queue_request, false);
        let resp = cap
            .list_perp_markets_request()
            .send()
            .promise
            .await
            .unwrap();
        assert_eq!(resp.get().unwrap().get_markets().unwrap().len(), 0);
        let resp = cap
            .list_future_markets_request()
            .send()
            .promise
            .await
            .unwrap();
        assert_eq!(resp.get().unwrap().get_markets().unwrap().len(), 0);
    });
}

// --------------------------------------------------------------- submitVote

struct Fixed(Result<VoteAccepted, (VoteCode, String)>);
impl VoteHandler for Fixed {
    fn handle(&self, _vote: cb_consensus::Vote) -> Result<VoteAccepted, (VoteCode, String)> {
        self.0.clone()
    }
}

fn vote() -> cb_consensus::Vote {
    cb_consensus::Vote {
        view: 0,
        height: 1,
        block_hash: H256([7; 32]),
        validator: Address([1; 20]),
        sig: vec![1; MAX_VOTE_SIG_LEN],
    }
}

#[derive(Debug, PartialEq)]
enum VoteReply {
    Accepted,
    Quorum,
    Rejected(rpc_capnp::VoteErrorCode),
    Failed(String),
}

async fn send_vote(cap: &Cap, v: &cb_consensus::Vote) -> VoteReply {
    let mut req = cap.submit_vote_request();
    conv::vote_to_capnp(req.get().init_vote(), v);
    match req.send().promise.await {
        Err(e) => VoteReply::Failed(e.to_string()),
        Ok(resp) => match resp.get().unwrap().get_result().unwrap().which().unwrap() {
            rpc_capnp::vote_result::Accepted(()) => VoteReply::Accepted,
            rpc_capnp::vote_result::Quorum(()) => VoteReply::Quorum,
            rpc_capnp::vote_result::Rejected(r) => {
                VoteReply::Rejected(r.unwrap().get_code().unwrap())
            }
        },
    }
}

#[test]
fn submit_vote_maps_every_outcome_and_caps_the_signature() {
    let (_d, db) = chain();
    run_local(|| async move {
        // A node that takes no votes says so.
        assert_eq!(
            send_vote(&plain(&db), &vote()).await,
            VoteReply::Rejected(rpc_capnp::VoteErrorCode::Internal)
        );
        let with = |r: Result<VoteAccepted, (VoteCode, String)>| {
            let mut s = SequencerServer::without_voting(db.clone(), Arc::new(Notify::new()));
            s.vote_handler = Some(Arc::new(Fixed(r)));
            cap_for(s)
        };
        assert_eq!(
            send_vote(&with(Ok(VoteAccepted::Stored)), &vote()).await,
            VoteReply::Accepted
        );
        assert_eq!(
            send_vote(&with(Ok(VoteAccepted::QuorumFormed)), &vote()).await,
            VoteReply::Quorum
        );
        for (code, wire) in [
            (
                VoteCode::UnknownValidator,
                rpc_capnp::VoteErrorCode::UnknownValidator,
            ),
            (
                VoteCode::MalformedSig,
                rpc_capnp::VoteErrorCode::MalformedSig,
            ),
            (
                VoteCode::SignerMismatch,
                rpc_capnp::VoteErrorCode::SignerMismatch,
            ),
            (VoteCode::StaleHeight, rpc_capnp::VoteErrorCode::StaleHeight),
            (
                VoteCode::UnknownBlock,
                rpc_capnp::VoteErrorCode::UnknownBlock,
            ),
            (VoteCode::Duplicate, rpc_capnp::VoteErrorCode::Duplicate),
            (VoteCode::Internal, rpc_capnp::VoteErrorCode::Internal),
        ] {
            assert_eq!(
                send_vote(&with(Err((code, "why".into()))), &vote()).await,
                VoteReply::Rejected(wire)
            );
        }
        let cap = with(Ok(VoteAccepted::Stored));
        // Boundary: an empty signature reaches the handler (which refuses it
        // as malformed); one byte past a signature is refused at decode.
        let mut v = vote();
        v.sig.clear();
        assert_eq!(send_vote(&cap, &v).await, VoteReply::Accepted);
        v.sig = vec![1; MAX_VOTE_SIG_LEN + 1];
        let r = send_vote(&cap, &v).await;
        assert!(
            matches!(&r, VoteReply::Failed(m) if m.contains("limit")),
            "{r:?}"
        );
        v.sig = vec![1; 1 << 20];
        assert!(matches!(send_vote(&cap, &v).await, VoteReply::Failed(_)));
        // A 31-byte block hash and a 21-byte validator.
        let mut req = cap.submit_vote_request();
        {
            let mut b = req.get().init_vote();
            b.reborrow().init_block_hash().set_bytes(&[0; 31]);
            b.reborrow().init_validator().set_bytes(&[0; 20]);
        }
        assert!(req.send().promise.await.is_err());
        let mut req = cap.submit_vote_request();
        {
            let mut b = req.get().init_vote();
            b.reborrow().init_block_hash().set_bytes(&[0; 32]);
            b.reborrow().init_validator().set_bytes(&[0; 21]);
        }
        assert!(req.send().promise.await.is_err());
    });
}
