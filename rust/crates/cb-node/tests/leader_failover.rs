//! Kill the leader; another validator continues the same chain.
//!
//! Three validators hold the same chain. Validator 1 leads view 0 and stops.
//! Validator 2 leads view 1, and the blocks it signs are accepted by the node
//! that never led — the survivors end on one head with one state root. That
//! is the difference between a handoff and a split, and it is the whole point
//! of the exercise.
//!
//! The pieces this exercises, each of which is useless alone:
//!
//! * `apply_remote_block` verifies the proposer against
//!   `validators[header.view % N]`, so the new leader's blocks are valid to
//!   everyone rather than to nobody.
//! * `failover::decide` picks that validator, and only when its head is
//!   exactly the highest block it holds a quorum certificate for.
//! * The sequencer stamps `Meta::view()` into every header it signs, so
//!   taking the view is what makes the blocks check out.
//!
//! Driven in process against three databases. The transport is not what is
//! being tested; the rule is.

use cb_block::{build_and_apply, ExecCtx};
use cb_consensus::{vote_signing_bytes, QuorumCert, Vote};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign, SecretKey};
use cb_net::apply_remote_block;
use cb_node::failover::{decide, take_view, Decision, NotLeading};
use cb_state::{AccountState, Accounts, Database, Meta, Qcs, ValidatorInfo, Validators};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, H256};
use secp256k1::PublicKey;
use tempfile::tempdir;

struct Validator {
    sk: SecretKey,
    addr: Address,
    _dir: tempfile::TempDir,
    db: Database,
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
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

/// Three validators on the same genesis, each with its own database.
fn cluster(alice: Address) -> Vec<Validator> {
    let keys: Vec<(SecretKey, PublicKey)> = (0..3).map(|_| generate_keypair()).collect();
    let set: Vec<ValidatorInfo> = keys
        .iter()
        .map(|(_, pk)| ValidatorInfo {
            addr: address_from_pubkey(pk),
            pubkey: pk.serialize().to_vec(),
            weight: 1,
        })
        .collect();
    keys.into_iter()
        .map(|(sk, pk)| {
            let dir = tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            Meta::new(&db).set_chain_id(1);
            Meta::new(&db).set_proposer_addr(set[0].addr);
            Validators::new(&db).put_set(&set);
            let accts = Accounts::new(&db);
            let mut s = AccountState::default();
            s.add(AssetId::Usd, 1_000_000).unwrap();
            accts.put(&alice, &s);
            Validator {
                sk,
                addr: address_from_pubkey(&pk),
                _dir: dir,
                db,
            }
        })
        .collect()
}

/// A quorum certificate for `(view, height, hash)` signed by two of the three,
/// which is what `ceil(2n/3)` asks for at n = 3.
fn qc_for(vs: &[Validator], view: u64, height: u64, hash: H256) -> QuorumCert {
    let votes: Vec<Vote> = vs
        .iter()
        .take(2)
        .map(|v| {
            let preimage = H256(vote_signing_bytes(view, height, hash));
            Vote {
                view,
                height,
                block_hash: hash,
                validator: v.addr,
                sig: sign(&v.sk, preimage).unwrap().to_vec(),
            }
        })
        .collect();
    QuorumCert {
        view,
        height,
        block_hash: hash,
        votes,
    }
}

/// Record on `db` that `height` is certified, the way a header's `parent_qc`
/// does when the next block arrives.
fn certify(db: &Database, qc: &QuorumCert) {
    Qcs::new(db).put(qc.height, &qc.encode());
    if qc.height > Meta::new(db).last_qc_height() {
        Meta::new(db).set_last_qc_height(qc.height);
    }
}

#[test]
fn the_chain_continues_under_a_new_leader_when_the_old_one_stops() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let vs = cluster(alice);
    let mut nonce = 0u64;

    // ---- view 0: validator 1 leads three blocks, each certified ----
    for _ in 0..3 {
        let leader = &vs[0];
        let signer = |p: &[u8]| sign(&leader.sk, keccak256(p)).unwrap().to_vec();
        let res = build_and_apply(
            &leader.db,
            &ctx(),
            vec![transfer(&alice_sk, nonce, bob)],
            1_000 + nonce,
            0,
            Vec::new(),
            signer,
        )
        .expect("leader builds");
        nonce += 1;
        let h = res.block.header.height;
        let hash = res.block.header.hash();
        let qc = qc_for(&vs, 0, h, hash);
        certify(&leader.db, &qc);
        for follower in &vs[1..] {
            apply_remote_block(&follower.db, &ctx(), &res.block).expect("follower applies");
            certify(&follower.db, &qc);
        }
    }
    for v in &vs {
        assert_eq!(Meta::new(&v.db).head_height(), 3);
    }

    // ---- validator 1 stops. Who takes over? ----
    // Nobody, while it is still producing.
    assert_eq!(
        decide(&vs[1].db, vs[1].addr, false),
        Decision::Stay(NotLeading::LeaderIsAlive)
    );
    // Once it is silent, exactly one node may: the leader of view 1.
    let decisions: Vec<Decision> = vs.iter().map(|v| decide(&v.db, v.addr, true)).collect();
    assert_eq!(
        decisions
            .iter()
            .filter(|d| matches!(d, Decision::Promote { .. }))
            .count(),
        1,
        "exactly one validator promotes, or the chain forks: {decisions:?}"
    );
    assert_eq!(decisions[1], Decision::Promote { view: 1, head: 3 });

    // ---- view 1: validator 2 leads, and everyone else follows it ----
    take_view(&vs[1].db, 1);
    let new_leader = &vs[1];
    let signer = |p: &[u8]| sign(&new_leader.sk, keccak256(p)).unwrap().to_vec();
    let view = Meta::new(&new_leader.db).view();
    assert_eq!(view, 1, "the new leader stamps the view it took");
    let res = build_and_apply(
        &new_leader.db,
        &ctx(),
        vec![transfer(&alice_sk, nonce, bob)],
        2_000,
        view,
        Vec::new(),
        signer,
    )
    .expect("the new leader builds");

    // The survivor that never led accepts it. Validator 1 is dead and is not
    // asked; that is the fault being tolerated.
    apply_remote_block(&vs[2].db, &ctx(), &res.block)
        .expect("the third node follows the new leader");

    assert_eq!(Meta::new(&vs[1].db).head_height(), 4);
    assert_eq!(Meta::new(&vs[2].db).head_height(), 4);
    assert_eq!(
        Meta::new(&vs[1].db).state_root(),
        Meta::new(&vs[2].db).state_root(),
        "the survivors hold one chain, not two"
    );
    assert_eq!(
        Meta::new(&vs[1].db).head_hash(),
        Meta::new(&vs[2].db).head_hash()
    );
}

/// The safety rule, end to end: one uncertified block at the tip is the
/// ordinary state of a follower and may be led from; two is more than the rest
/// of the network can be assumed to hold, and the candidate waits.
#[test]
fn a_candidate_too_far_ahead_of_the_last_certificate_waits() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let vs = cluster(alice);

    // Two blocks, neither certified: the leader died before either
    // certificate could travel in a later header.
    let leader = &vs[0];
    let mut last = None;
    for nonce in 0..2u64 {
        let signer = |p: &[u8]| sign(&leader.sk, keccak256(p)).unwrap().to_vec();
        let res = build_and_apply(
            &leader.db,
            &ctx(),
            vec![transfer(&alice_sk, nonce, bob)],
            1_000 + nonce,
            0,
            Vec::new(),
            signer,
        )
        .unwrap();
        apply_remote_block(&vs[1].db, &ctx(), &res.block).unwrap();
        last = Some(res.block.header.hash());
    }

    assert_eq!(
        decide(&vs[1].db, vs[1].addr, true),
        Decision::Stay(NotLeading::TooFarAboveLastQuorum {
            head: 2,
            last_qc: 0
        }),
        "two blocks nobody certified is more than the network can be assumed to hold"
    );

    // The certificate for the first arrives; one uncertified block is left at
    // the tip, which is the ordinary case, and it may lead.
    certify(&vs[1].db, &qc_for(&vs, 0, 1, hash_at(&vs[1].db, 1)));
    let _ = last;
    assert_eq!(
        decide(&vs[1].db, vs[1].addr, true),
        Decision::Promote { view: 1, head: 2 }
    );
}

/// The hash of the block this node holds at `height`.
fn hash_at(db: &Database, height: u64) -> H256 {
    let bytes = cb_state::Blocks::new(db).get(height).expect("block stored");
    cb_block::Block::decode(&bytes)
        .expect("decodes")
        .header
        .hash()
}
