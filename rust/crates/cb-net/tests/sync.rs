use cb_block::{build_and_apply, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign};
use cb_net::{apply_remote_block, SyncError};
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use tempfile::tempdir;

fn fund_alice(db: &Database, alice: Address) {
    let accts = Accounts::new(db);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000).expect("seed balance");
    accts.put(&alice, &s);
}

fn make_transfer(sk: &cb_crypto::SecretKey, to: Address, amount: u128) -> cb_tx::VerifiedTx {
    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to,
            asset: AssetId::Usd,
            amount,
        },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

#[test]
fn replica_converges_with_sequencer() {
    let seq_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_db = Database::open(seq_dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&seq_db).set_chain_id(1);
    let rep_db = Database::open(rep_dir.path()).unwrap();
    Meta::new(&rep_db).set_chain_id(1);

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([2; 20]);
    fund_alice(&seq_db, alice);
    fund_alice(&rep_db, alice);

    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(&seq_db).set_proposer_addr(proposer_addr);
    Meta::new(&rep_db).set_proposer_addr(proposer_addr);

    let verified = make_transfer(&alice_sk, bob, 100);
    let signer = |preimage: &[u8]| sign(&proposer_sk, keccak256(preimage)).unwrap().to_vec();
    let res = build_and_apply(
        &seq_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        vec![verified],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &res.block,
    )
    .unwrap();
    assert_eq!(
        Meta::new(&rep_db).state_root(),
        Meta::new(&seq_db).state_root()
    );
    assert_eq!(Meta::new(&rep_db).head_height(), 1);
}

#[test]
fn replica_rejects_block_signed_by_wrong_proposer() {
    let seq_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_db = Database::open(seq_dir.path()).unwrap();
    Meta::new(&seq_db).set_chain_id(1);
    let rep_db = Database::open(rep_dir.path()).unwrap();
    Meta::new(&rep_db).set_chain_id(1);

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([2; 20]);
    fund_alice(&seq_db, alice);
    fund_alice(&rep_db, alice);

    // Sequencer signs with key A; replica is configured to expect key B.
    let (key_a, pk_a) = generate_keypair();
    let (_key_b, pk_b) = generate_keypair();
    let addr_a = address_from_pubkey(&pk_a);
    let addr_b = address_from_pubkey(&pk_b);
    assert_ne!(addr_a, addr_b);
    Meta::new(&seq_db).set_proposer_addr(addr_a);
    Meta::new(&rep_db).set_proposer_addr(addr_b);

    let verified = make_transfer(&alice_sk, bob, 100);
    let signer = |preimage: &[u8]| sign(&key_a, keccak256(preimage)).unwrap().to_vec();
    let res = build_and_apply(
        &seq_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        vec![verified],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();

    let err = apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &res.block,
    )
    .unwrap_err();
    match err {
        SyncError::WrongProposer { expected, got, .. } => {
            assert_eq!(expected, addr_b);
            assert_eq!(got, addr_a);
        }
        other => panic!("expected WrongProposer, got {other:?}"),
    }
    // Replica's head must NOT advance on rejection.
    assert_eq!(Meta::new(&rep_db).head_height(), 0);
}

#[test]
fn replica_rejects_malformed_proposer_sig() {
    let seq_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_db = Database::open(seq_dir.path()).unwrap();
    Meta::new(&seq_db).set_chain_id(1);
    let rep_db = Database::open(rep_dir.path()).unwrap();
    Meta::new(&rep_db).set_chain_id(1);

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([2; 20]);
    fund_alice(&seq_db, alice);
    fund_alice(&rep_db, alice);

    let (_, pk) = generate_keypair();
    let addr = address_from_pubkey(&pk);
    Meta::new(&seq_db).set_proposer_addr(addr);
    Meta::new(&rep_db).set_proposer_addr(addr);

    let verified = make_transfer(&alice_sk, bob, 100);
    // Sign with a 64-byte garbage payload (wrong length → parse failure).
    let signer = |_preimage: &[u8]| vec![0u8; 64];
    let res = build_and_apply(
        &seq_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        vec![verified],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    let err = apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &res.block,
    )
    .unwrap_err();
    matches!(err, SyncError::MalformedProposerSig { .. });
}

/// Helper: build a block on a sequencer DB, returning the produced block.
/// Replica DB is a fresh, separately-funded copy.
fn make_block_for_replica(txs_count: usize) -> (tempfile::TempDir, Database, cb_block::Block) {
    let seq_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_db = Database::open(seq_dir.path()).unwrap();
    Meta::new(&seq_db).set_chain_id(1);
    let rep_db = Database::open(rep_dir.path()).unwrap();
    Meta::new(&rep_db).set_chain_id(1);

    // Many funded senders so multi-tx blocks stay valid.
    let mut verifieds = Vec::new();
    for i in 0..txs_count {
        let (sk, pk) = generate_keypair();
        let addr = address_from_pubkey(&pk);
        fund_alice(&seq_db, addr);
        fund_alice(&rep_db, addr);
        let to = Address([(i as u8).wrapping_add(50); 20]);
        verifieds.push(make_transfer(&sk, to, 1));
    }

    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(&seq_db).set_proposer_addr(proposer_addr);
    Meta::new(&rep_db).set_proposer_addr(proposer_addr);

    let signer = |preimage: &[u8]| sign(&proposer_sk, keccak256(preimage)).unwrap().to_vec();
    let res = build_and_apply(
        &seq_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        verifieds,
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    // We keep rep_dir alive via the returned Database by leaking the tempdir.
    // Instead, return rep_dir alongside. Adjust the signature.
    (rep_dir, rep_db, res.block)
}

#[test]
fn replica_applies_block_with_pre_recovered_senders() {
    let (_rep_dir, rep_db, block) = make_block_for_replica(4);
    // Leader-built block carries populated senders.
    assert_eq!(block.senders.len(), block.txs.len());
    assert!(!block.senders.is_empty());
    apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &block,
    )
    .unwrap();
    assert_eq!(Meta::new(&rep_db).head_height(), 1);
}

#[test]
fn replica_applies_legacy_block_with_empty_senders() {
    let (_rep_dir, rep_db, mut block) = make_block_for_replica(4);
    // Strip senders to simulate a block produced before the field existed.
    // Replica must fall back to parallel ECDSA recovery and still converge.
    block.senders.clear();
    apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &block,
    )
    .unwrap();
    assert_eq!(Meta::new(&rep_db).head_height(), 1);
}

#[test]
fn replica_rejects_block_with_mismatched_senders_length() {
    let (_rep_dir, rep_db, mut block) = make_block_for_replica(4);
    // Drop one sender → length mismatch but non-empty: malformed leader output.
    block.senders.pop();
    let err = apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &block,
    )
    .unwrap_err();
    match err {
        SyncError::SendersMismatch { expected, got, .. } => {
            assert_eq!(expected, 4);
            assert_eq!(got, 3);
        }
        other => panic!("expected SendersMismatch, got {other:?}"),
    }
    assert_eq!(Meta::new(&rep_db).head_height(), 0);
}

// ---------------------------------------------------------------------------
// Core review C2 and H1: what a verifying replica actually verifies.
// ---------------------------------------------------------------------------

#[test]
fn replica_rejects_a_block_whose_senders_are_forged() {
    // C2. The leader ships pre-recovered `senders` and the replica used them
    // verbatim, so a leader could sign a transaction with its own key, list a
    // victim as the sender, debit the victim, and every replica would
    // re-execute the same debit, reproduce the root, and vote. The header is
    // untouched here — `senders` is outside `tx_root` — which is exactly the
    // shape of that attack.
    let (_rep_dir, rep_db, mut block) = make_block_for_replica(2);
    let honest = block.senders[1];
    let victim = Address([0xd0; 20]);
    fund_alice(&rep_db, victim);
    block.senders[1] = victim;

    let err = apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &block,
    )
    .unwrap_err();
    match err {
        SyncError::SenderMismatch {
            index,
            claimed,
            recovered,
            ..
        } => {
            assert_eq!(index, 1);
            assert_eq!(claimed, victim);
            assert_eq!(recovered, honest);
        }
        other => panic!("expected SenderMismatch, got {other:?}"),
    }
    assert_eq!(Meta::new(&rep_db).head_height(), 0);
    assert_eq!(
        Accounts::new(&rep_db)
            .get(&victim)
            .unwrap()
            .balance(AssetId::Usd),
        1_000,
        "the victim was not debited"
    );
}

#[test]
fn replica_rejects_a_body_that_does_not_hash_to_its_header() {
    // H1, tx_root half. Swapping two transactions keeps every signature valid
    // and leaves the header — signed by the proposer — untouched. Only the
    // root check notices.
    let (_rep_dir, rep_db, mut block) = make_block_for_replica(2);
    block.txs.swap(0, 1);
    block.senders.swap(0, 1);

    let err = apply_remote_block(
        &rep_db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &block,
    )
    .unwrap_err();
    assert!(
        matches!(err, SyncError::TxRootMismatch { h: 1, .. }),
        "expected TxRootMismatch, got {err:?}"
    );
    assert_eq!(Meta::new(&rep_db).head_height(), 0);
}

#[test]
fn replica_rejects_a_block_that_does_not_extend_its_head() {
    // H1, prev_hash half. Two sequencers share a proposer key and both
    // produce a valid block 1, differing in content. The replica follows the
    // first; the second sequencer's block 2 is correctly signed, sits at the
    // right height, and links to a parent the replica never had.
    let seq_a_dir = tempdir().unwrap();
    let seq_b_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_a = Database::open(seq_a_dir.path()).unwrap();
    let seq_b = Database::open(seq_b_dir.path()).unwrap();
    let rep = Database::open(rep_dir.path()).unwrap();
    for db in [&seq_a, &seq_b, &rep] {
        Meta::new(db).set_chain_id(1);
    }

    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([2; 20]);
    for db in [&seq_a, &seq_b, &rep] {
        fund_alice(db, alice);
    }
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer_addr = address_from_pubkey(&proposer_pk);
    for db in [&seq_a, &seq_b, &rep] {
        Meta::new(db).set_proposer_addr(proposer_addr);
    }
    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };
    let signer = |preimage: &[u8]| sign(&proposer_sk, keccak256(preimage)).unwrap().to_vec();

    // Block 1 on both sequencers, different transfers, different hashes.
    let a1 = build_and_apply(
        &seq_a,
        &ctx,
        vec![make_transfer(&alice_sk, bob, 100)],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    let b1 = build_and_apply(
        &seq_b,
        &ctx,
        vec![make_transfer(&alice_sk, bob, 50)],
        0,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    assert_ne!(a1.block.header.hash(), b1.block.header.hash());
    apply_remote_block(&rep, &ctx, &a1.block).unwrap();
    assert_eq!(Meta::new(&rep).head_height(), 1);

    // Block 2 on sequencer B extends B's block 1, not A's.
    let alice_nonce_1 = {
        let tx = Transaction {
            nonce: 1,
            chain_id: 1,
            body: TxBody::Transfer {
                to: bob,
                asset: AssetId::Usd,
                amount: 1,
            },
        };
        SignedTx::sign_with(&alice_sk, tx)
            .unwrap()
            .verify()
            .unwrap()
    };
    let b2 = build_and_apply(&seq_b, &ctx, vec![alice_nonce_1], 0, 0, Vec::new(), signer).unwrap();
    assert_eq!(b2.block.header.height, 2);

    let err = apply_remote_block(&rep, &ctx, &b2.block).unwrap_err();
    match err {
        SyncError::WrongParent { h, claimed, head } => {
            assert_eq!(h, 2);
            assert_eq!(claimed, b1.block.header.hash());
            assert_eq!(head, a1.block.header.hash());
        }
        other => panic!("expected WrongParent, got {other:?}"),
    }
    assert_eq!(Meta::new(&rep).head_height(), 1, "the replica did not move");
}

#[test]
fn replica_rejects_a_block_whose_clock_runs_backwards() {
    // Core review L5. Timestamps are never bounded by a wall clock — replay
    // must stay deterministic — but they do not run backwards: a header
    // earlier than the head's is refused.
    let seq_dir = tempdir().unwrap();
    let rep_dir = tempdir().unwrap();
    let seq_db = Database::open(seq_dir.path()).unwrap();
    let rep_db = Database::open(rep_dir.path()).unwrap();
    for db in [&seq_db, &rep_db] {
        Meta::new(db).set_chain_id(1);
    }
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([2; 20]);
    fund_alice(&seq_db, alice);
    fund_alice(&rep_db, alice);
    let (proposer_sk, proposer_pk) = generate_keypair();
    let proposer_addr = address_from_pubkey(&proposer_pk);
    Meta::new(&seq_db).set_proposer_addr(proposer_addr);
    Meta::new(&rep_db).set_proposer_addr(proposer_addr);
    let ctx = ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    };
    let signer = |preimage: &[u8]| sign(&proposer_sk, keccak256(preimage)).unwrap().to_vec();

    // Block 1 at t=1000 on both.
    let b1 = build_and_apply(
        &seq_db,
        &ctx,
        vec![make_transfer(&alice_sk, bob, 1)],
        1_000,
        0,
        Vec::new(),
        signer,
    )
    .unwrap();
    apply_remote_block(&rep_db, &ctx, &b1.block).unwrap();
    assert_eq!(Meta::new(&rep_db).head_timestamp(), 1_000);

    // The leader's own clock stepped back; the builder clamps, so the block it
    // produces is at t=1000, not 500, and the replica accepts it.
    let tx2 = SignedTx::sign_with(
        &alice_sk,
        Transaction {
            nonce: 1,
            chain_id: 1,
            body: TxBody::Transfer {
                to: bob,
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap();
    let b2 = build_and_apply(&seq_db, &ctx, vec![tx2], 500, 0, Vec::new(), signer).unwrap();
    assert_eq!(
        b2.block.header.timestamp, 1_000,
        "the builder never regresses"
    );
    apply_remote_block(&rep_db, &ctx, &b2.block).unwrap();

    // A hand-built header that does regress is refused, before execution.
    let mut forged = b2.block.clone();
    forged.header.height = 3;
    forged.header.prev_hash = b2.block.header.hash();
    forged.header.timestamp = 999;
    forged.header.sequencer_sig = signer(&forged.header.signing_bytes());
    let err = apply_remote_block(&rep_db, &ctx, &forged).unwrap_err();
    assert!(
        matches!(
            err,
            SyncError::TimestampRegressed {
                h: 3,
                claimed: 999,
                head: 1_000
            }
        ),
        "got {err:?}"
    );
    assert_eq!(Meta::new(&rep_db).head_height(), 2);
}
