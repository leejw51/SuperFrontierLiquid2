//! Under BFT a refused transaction consumes its nonce, and the block says
//! which transactions applied.
//!
//! A committed payload is final and the block keeps every transaction in
//! it. A refusal used to leave the sender's nonce where it was, which
//! punched a hole in that sender's sequence: every later transaction the
//! sender had pipelined was "ahead" of the chain for good, stuck in every
//! validator's mempool until the client re-signed from the hole. Under load
//! a fifth of submissions were refused at admission for exactly that. Now a
//! transaction that was the sender's next one consumes the nonce whether or
//! not its body succeeded; one refused *before* the nonce check (a gap, a
//! wrong chain id) still changes nothing.
//!
//! Because the nonce no longer tells a refused transaction from an applied
//! one, the block carries an `applied` mask, every validator computes it,
//! and a replica refuses a block whose mask differs from its own
//! re-execution.

use cb_block::{
    execute_block_with, failure_consumes_nonce, synthetic_system_txs, verify_and_commit_with,
    BlockOrigin, ExecCtx, ExecError, Executed, VerifyError,
};
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, H256};
use tempfile::tempdir;

fn db_with(alice: Address) -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Meta::new(&db).set_consensus_scheme(cb_state::CONSENSUS_BFT);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000).unwrap();
    Accounts::new(&db).put(&alice, &s);
    (dir, db)
}

fn transfer(sk: &secp256k1::SecretKey, nonce: u64, to: Address, amount: u128) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to,
                asset: AssetId::Usd,
                amount,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn commit(db: &Database, ex: Executed) {
    let mut batch = db.batch();
    Database::maps_into_batch(&mut batch, &ex.rows).unwrap();
    db.write_batch(batch).unwrap();
    Meta::new(db).set_head(ex.height, H256([ex.height as u8; 32]), H256::ZERO);
}

#[test]
fn a_transaction_refused_by_its_body_consumes_its_nonce() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let origin = BlockOrigin {
        seed: H256([3; 32]),
        proposer,
    };
    // Nonce 1 fails in its body — more than alice holds — and nonce 2 was
    // signed behind it, as a pipelining client does.
    let payload = vec![
        transfer(&alice_sk, 0, proposer, 1),
        transfer(&alice_sk, 1, proposer, 1_000_000),
        transfer(&alice_sk, 2, proposer, 1),
    ];
    let ordered = cb_block::order_for_block(payload, origin.seed);
    let (_d, db) = db_with(alice);
    let ex = execute_block_with(&db, &ctx(), ordered, 10, Some(origin), |_, _, _| {}).unwrap();
    assert_eq!(ex.applied, vec![true, false, true]);
    assert_eq!(ex.rejected.len(), 1);
    assert!(matches!(ex.rejected[0].error, ExecError::Insufficient));
    commit(&db, ex);
    let a = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(a.nonce, 3, "the refused transaction consumed its nonce");
    assert_eq!(a.balance(AssetId::Usd), 998, "and moved nothing else");
}

#[test]
fn a_transaction_refused_before_the_nonce_check_consumes_nothing() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let origin = BlockOrigin {
        seed: H256([4; 32]),
        proposer,
    };
    let payload = vec![
        transfer(&alice_sk, 0, proposer, 1),
        transfer(&alice_sk, 5, proposer, 1), // a gap: not alice's next
    ];
    let ordered = cb_block::order_for_block(payload, origin.seed);
    let (_d, db) = db_with(alice);
    let ex = execute_block_with(&db, &ctx(), ordered, 10, Some(origin), |_, _, _| {}).unwrap();
    assert_eq!(ex.applied, vec![true, false]);
    assert!(matches!(ex.rejected[0].error, ExecError::BadNonce { .. }));
    commit(&db, ex);
    assert_eq!(Accounts::new(&db).get(&alice).unwrap().nonce, 1);

    // The rule itself, stated once.
    let v = transfer(&alice_sk, 1, proposer, 1);
    assert!(failure_consumes_nonce(&v, &ExecError::Insufficient));
    assert!(failure_consumes_nonce(&v, &ExecError::FokUnfilled));
    assert!(!failure_consumes_nonce(
        &v,
        &ExecError::BadNonce {
            expected: 1,
            got: 5
        }
    ));
    assert!(!failure_consumes_nonce(
        &v,
        &ExecError::WrongChainId {
            expected: 1,
            got: 2
        }
    ));
}

#[test]
fn a_replica_reproduces_the_mask_and_refuses_a_block_whose_mask_lies() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (_psk, ppk) = generate_keypair();
    let proposer = address_from_pubkey(&ppk);
    let origin = BlockOrigin {
        seed: H256([5; 32]),
        proposer,
    };
    let payload = vec![
        transfer(&alice_sk, 0, proposer, 1),
        transfer(&alice_sk, 1, proposer, 1_000_000),
        transfer(&alice_sk, 2, proposer, 1),
    ];
    let ordered = cb_block::order_for_block(payload, origin.seed);

    let (_d1, v) = db_with(alice);
    let ex =
        execute_block_with(&v, &ctx(), ordered.clone(), 10, Some(origin), |_, _, _| {}).unwrap();
    let mask = ex.applied.clone();
    let (diff, _) = cb_state::diff_hash(&ex.rows);
    let root_v = cb_state::chain_state_hash(Meta::new(&v).state_root(), diff);
    commit(&v, ex);

    let block_ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: 1,
        proposer_addr: proposer,
    };
    let replay = |expected: Option<&[bool]>| -> Result<H256, VerifyError> {
        let (_d, o) = db_with(alice);
        let mut txs = synthetic_system_txs(&o, 1, proposer, 1);
        txs.extend(ordered.iter().cloned());
        let r = verify_and_commit_with(
            &o,
            &block_ctx,
            &txs,
            1,
            H256([9; 32]),
            b"body",
            root_v,
            10,
            &[],
            true,
            expected,
        );
        if r.is_ok() {
            assert_eq!(Accounts::new(&o).get(&alice).unwrap().nonce, 3);
        }
        r
    };
    assert_eq!(replay(Some(&mask)).unwrap(), root_v);
    assert_eq!(
        replay(None).unwrap(),
        root_v,
        "a block with no mask is checked by root alone"
    );
    let lie = vec![true, true, true];
    assert!(matches!(
        replay(Some(&lie)).unwrap_err(),
        VerifyError::AppliedMaskMismatch { h: 1 }
    ));
}
