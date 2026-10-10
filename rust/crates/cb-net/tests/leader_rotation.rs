//! A replica must accept the block of whoever is entitled to lead that view,
//! and refuse everyone else.
//!
//! # Why this is the first thing failover needs
//!
//! `apply_remote_block` used to check the proposer signature against
//! `Meta::proposer_addr` — the one address genesis names. On a chain where
//! that node leads for ever, correct. On a chain where a validator takes over
//! because the leader crashed, every replica refuses the new leader's blocks:
//! the survivors keep their old head, the new leader builds a chain nobody
//! accepts, and what looked like failover is a split. The check now asks the
//! validator set who leads `header.view`, which is what `build_and_apply`
//! already documented and what the header has always carried.
//!
//! The rule, stated once: a block at view V is valid only if it is signed by
//! `validators[V % N]`. Round robin, so a crashed leader is skipped by
//! advancing the view.

use cb_block::{build_and_apply, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign, SecretKey};
use cb_net::{apply_remote_block, SyncError};
use cb_state::{AccountState, Accounts, Database, Meta, ValidatorInfo, Validators};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use secp256k1::PublicKey;
use tempfile::tempdir;

struct Node {
    _dir: tempfile::TempDir,
    db: Database,
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

/// Three validators, each a real key, in a fixed order: index 0 leads view 0,
/// index 1 leads view 1, and so on.
fn validator_set(keys: &[(SecretKey, PublicKey)]) -> Vec<ValidatorInfo> {
    keys.iter()
        .map(|(_, pk)| ValidatorInfo {
            addr: address_from_pubkey(pk),
            pubkey: pk.serialize().to_vec(),
            weight: 1,
        })
        .collect()
}

fn node(set: &[ValidatorInfo], alice: Address) -> Node {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    // The old rule's address, deliberately the first validator, so a test that
    // fails does so because the view was consulted and not because the
    // fallback happened to agree.
    Meta::new(&db).set_proposer_addr(set[0].addr);
    Validators::new(&db).put_set(set);
    let accts = Accounts::new(&db);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000_000).unwrap();
    accts.put(&alice, &s);
    Node { _dir: dir, db }
}

fn transfer(sk: &SecretKey, nonce: u64, to: Address) -> cb_tx::VerifiedTx {
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
    .verify()
    .unwrap()
}

/// Build one block on `leader`'s database, proposed in `view` and signed by
/// `sk`.
fn block_at(
    leader: &Node,
    sk: &SecretKey,
    view: u64,
    alice_sk: &SecretKey,
    nonce: u64,
    bob: Address,
) -> cb_block::Block {
    let signer = |preimage: &[u8]| sign(sk, keccak256(preimage)).unwrap().to_vec();
    build_and_apply(
        &leader.db,
        &ctx(),
        vec![transfer(alice_sk, nonce, bob)],
        1_000 + view,
        view,
        Vec::new(),
        signer,
    )
    .expect("leader builds")
    .block
}

#[test]
fn a_replica_follows_whoever_leads_the_view() {
    let keys: Vec<(SecretKey, PublicKey)> = (0..3).map(|_| generate_keypair()).collect();
    let set = validator_set(&keys);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);

    let v0 = node(&set, alice); // leads view 0
    let v1 = node(&set, alice); // leads view 1
    let replica = node(&set, alice);

    // View 0: validator 0 leads. The replica follows.
    let b1 = block_at(&v0, &keys[0].0, 0, &alice_sk, 0, bob);
    apply_remote_block(&replica.db, &ctx(), &b1).expect("view 0 block from validator 0");
    assert_eq!(Meta::new(&replica.db).head_height(), 1);

    // Validator 0 goes away. Validator 1 takes view 1 and proposes the next
    // block. It must be accepted — this is the handoff, and it is exactly
    // what the old check refused.
    apply_remote_block(&v1.db, &ctx(), &b1).expect("the new leader is caught up");
    let b2 = block_at(&v1, &keys[1].0, 1, &alice_sk, 1, bob);
    apply_remote_block(&replica.db, &ctx(), &b2).expect("view 1 block from validator 1");
    assert_eq!(Meta::new(&replica.db).head_height(), 2);
    assert_eq!(
        Meta::new(&replica.db).state_root(),
        Meta::new(&v1.db).state_root(),
        "the replica and the new leader agree on the state"
    );
}

#[test]
fn a_validator_that_does_not_lead_this_view_is_refused() {
    let keys: Vec<(SecretKey, PublicKey)> = (0..3).map(|_| generate_keypair()).collect();
    let set = validator_set(&keys);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);

    let builder = node(&set, alice);
    let replica = node(&set, alice);

    // Validator 2 signs a block that claims view 1, whose leader is
    // validator 1. Being in the validator set is not enough.
    let forged = block_at(&builder, &keys[2].0, 1, &alice_sk, 0, bob);
    let err = apply_remote_block(&replica.db, &ctx(), &forged).unwrap_err();
    match err {
        SyncError::WrongProposer {
            view,
            expected,
            got,
            ..
        } => {
            assert_eq!(view, 1);
            assert_eq!(expected, set[1].addr, "view 1 belongs to validator 1");
            assert_eq!(got, set[2].addr);
        }
        other => panic!("expected WrongProposer, got {other:?}"),
    }
    assert_eq!(
        Meta::new(&replica.db).head_height(),
        0,
        "nothing was applied"
    );
}

/// Rotation wraps: view 3 is validator 0 again with three validators.
#[test]
fn leadership_wraps_around_the_set() {
    let keys: Vec<(SecretKey, PublicKey)> = (0..3).map(|_| generate_keypair()).collect();
    let set = validator_set(&keys);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);

    let leader = node(&set, alice);
    let replica = node(&set, alice);
    let b = block_at(&leader, &keys[0].0, 3, &alice_sk, 0, bob);
    apply_remote_block(&replica.db, &ctx(), &b).expect("view 3 belongs to validator 0 again");
}

/// A chain that registered no validator set keeps the old rule, so every
/// single-node chain and every older fixture behaves exactly as before.
#[test]
fn with_no_validator_set_the_genesis_proposer_still_leads() {
    let (sk, pk) = generate_keypair();
    let proposer = address_from_pubkey(&pk);
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);

    let mk = || {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Meta::new(&db).set_chain_id(1);
        Meta::new(&db).set_proposer_addr(proposer);
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000_000).unwrap();
        accts.put(&alice, &s);
        Node { _dir: dir, db }
    };
    let leader = mk();
    let replica = mk();
    assert_eq!(Validators::new(&replica.db).count(), 0);

    // Any view at all: with no set there is nobody to rotate to.
    let b = block_at(&leader, &sk, 7, &alice_sk, 0, bob);
    apply_remote_block(&replica.db, &ctx(), &b).expect("the genesis proposer still leads");
}
