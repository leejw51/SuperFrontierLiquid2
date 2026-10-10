//! Edge cases on the basic Transfer execution path that aren't covered in
//! `transfer.rs`. These pin invariants that consensus-critical code depends on:
//!
//! - Self-transfer: alice → alice. Same-account debit + credit must net to
//!   zero balance change. Nonce still advances.
//! - Two consecutive transfers from the same sender (nonce 0 then 1) inside
//!   a single block must both apply and produce a final state where alice's
//!   nonce == 2 and the sum of both amounts has moved to bob.
//! - Transfer to a previously-unknown address creates the recipient with the
//!   credited amount (the executor uses `state.load(to)` which falls back to
//!   `AccountState::default()` — pin that).
//! - Insufficient balance rejection.
//! - Off-by-one nonce: nonce N+1 when the account is at N must be rejected.
//! - Repeating the same nonce in two consecutive txs must reject the second.
//!
//! Properties we deliberately do NOT test here (out of scope per audit):
//! - chain_id mismatch enforcement (executor doesn't check it; gap noted in
//!   the audit report — backend agent's call).

use cb_block::{apply_tx, BlockState, ExecCtx, ExecError};
use cb_crypto::{address_from_pubkey, generate_keypair, SecretKey};
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use tempfile::tempdir;

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn signed_transfer(
    sk: &SecretKey,
    nonce: u64,
    to: Address,
    asset: AssetId,
    amount: u128,
) -> cb_tx::VerifiedTx {
    let tx = Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::Transfer { to, asset, amount },
    };
    SignedTx::sign_with(sk, tx).unwrap().verify().unwrap()
}

fn fund(db: &Database, addr: &Address, asset: AssetId, amount: u128) {
    let accts = Accounts::new(db);
    let mut s = accts.get(addr).unwrap_or_default();
    s.add(asset, amount).expect("seed balance");
    accts.put(addr, &s);
}

#[test]
fn self_transfer_nets_to_zero_balance_change_and_advances_nonce() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, &alice, AssetId::Usd, 1_000);

    let v = signed_transfer(&sk, 0, alice, AssetId::Usd, 250);
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx(), &v).unwrap();
    state.commit().unwrap();

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(
        after.balance(AssetId::Usd),
        1_000,
        "self-transfer: balance must net to zero (sub then add to the same account)",
    );
    assert_eq!(after.nonce, 1, "self-transfer must still advance the nonce");
}

#[test]
fn self_transfer_to_full_balance_does_not_underflow() {
    // Edge: amount == full balance → sub then add cycles through 0 and back.
    // If the executor sub-then-loaded `to_state` from the *post-sub* in-memory
    // state, this works; if it accidentally read the pre-sub snapshot from
    // RocksDB, the credit would replay the old balance (huge bug). Verify.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    fund(&db, &alice, AssetId::Usd, 500);

    let v = signed_transfer(&sk, 0, alice, AssetId::Usd, 500);
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx(), &v).unwrap();
    state.commit().unwrap();

    let after = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(after.balance(AssetId::Usd), 500);
    assert_eq!(after.nonce, 1);
}

#[test]
fn two_consecutive_transfers_in_one_block_both_apply() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    fund(&db, &alice, AssetId::Usd, 1_000);

    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, 0, bob, AssetId::Usd, 100),
    )
    .unwrap();
    apply_tx(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, 1, bob, AssetId::Usd, 250),
    )
    .unwrap();
    state.commit().unwrap();

    let accts = Accounts::new(&db);
    let alice_after = accts.get(&alice).unwrap();
    let bob_after = accts.get(&bob).unwrap();
    assert_eq!(alice_after.balance(AssetId::Usd), 650);
    assert_eq!(bob_after.balance(AssetId::Usd), 350);
    assert_eq!(
        alice_after.nonce, 2,
        "two consecutive applied txs in one block must advance nonce twice",
    );
}

#[test]
fn transfer_to_unknown_address_creates_recipient() {
    // Bob has never been seen by the DB. After alice→bob 1000, bob must
    // exist with balance 1000 and nonce 0.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([0xbb; 20]);
    fund(&db, &alice, AssetId::Usd, 5_000);

    // Sanity: bob doesn't exist yet.
    assert!(Accounts::new(&db).get(&bob).is_none());

    let v = signed_transfer(&sk, 0, bob, AssetId::Usd, 1_000);
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx(), &v).unwrap();
    state.commit().unwrap();

    let bob_after = Accounts::new(&db)
        .get(&bob)
        .expect("recipient must exist after credit");
    assert_eq!(bob_after.balance(AssetId::Usd), 1_000);
    assert_eq!(
        bob_after.nonce, 0,
        "recipient nonce must NOT be touched by an inbound transfer",
    );
}

#[test]
fn insufficient_balance_is_rejected_with_specific_error() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    fund(&db, &alice, AssetId::Usd, 100);

    let v = signed_transfer(&sk, 0, bob, AssetId::Usd, 1_000);
    let mut state = BlockState::new(&db);
    let err = apply_tx(&mut state, &ctx(), &v).unwrap_err();
    assert_eq!(err, ExecError::Insufficient);
}

#[test]
fn off_by_one_nonce_is_rejected() {
    // alice has nonce=0; submitting a tx with nonce=1 must fail with an
    // expected/got pair pointing at the off-by-one.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    fund(&db, &alice, AssetId::Usd, 1_000);

    let v = signed_transfer(&sk, 1, bob, AssetId::Usd, 100);
    let mut state = BlockState::new(&db);
    let err = apply_tx(&mut state, &ctx(), &v).unwrap_err();
    match err {
        ExecError::BadNonce { expected, got } => {
            assert_eq!(expected, 0);
            assert_eq!(got, 1);
        }
        other => panic!("expected BadNonce, got {other:?}"),
    }
}

#[test]
fn replayed_nonce_is_rejected_within_same_block() {
    // After applying a tx at nonce 0, the second apply at the same nonce in
    // the same block must reject — this is the consensus-critical "no double
    // spend" property at the executor level.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    fund(&db, &alice, AssetId::Usd, 1_000);

    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, 0, bob, AssetId::Usd, 100),
    )
    .unwrap();
    let err = apply_tx(
        &mut state,
        &ctx(),
        &signed_transfer(&sk, 0, bob, AssetId::Usd, 100),
    )
    .unwrap_err();
    match err {
        ExecError::BadNonce { expected, got } => {
            assert_eq!(
                expected, 1,
                "expected nonce should reflect the in-block update"
            );
            assert_eq!(got, 0);
        }
        other => panic!("expected BadNonce, got {other:?}"),
    }
}

#[test]
fn signing_then_recovering_yields_same_address_property() {
    // Property test (small fixed iterations to keep the test fast): for a
    // bunch of randomly generated keypairs, signing a transfer and then
    // calling `signed.sender()` must return the same address as
    // `address_from_pubkey(&pk)`. This already exists for one keypair in
    // cb-tx; this iteration pins it as a property across N pairs.
    for i in 0..16u64 {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let tx = Transaction {
            nonce: i,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([1; 20]),
                asset: AssetId::Usd,
                amount: 100 + i as u128,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).unwrap();
        assert_eq!(signed.sender().unwrap(), signer);
    }
}

#[test]
fn balance_update_is_atomic_debit_credit_nonce_advances() {
    // Pin the three-step atomic update:
    //   1. sender debited by `amount`
    //   2. recipient credited by `amount`
    //   3. sender's nonce advanced by 1
    //
    // Verifies that no partial state leaks if any step is buggy: total supply
    // must remain unchanged across the transfer.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    fund(&db, &alice, AssetId::Usd, 1_000);
    fund(&db, &bob, AssetId::Usd, 250);

    let alice_before = Accounts::new(&db).get(&alice).unwrap();
    let bob_before = Accounts::new(&db).get(&bob).unwrap();
    let sum_before = alice_before.balance(AssetId::Usd) + bob_before.balance(AssetId::Usd);

    let v = signed_transfer(&sk, 0, bob, AssetId::Usd, 333);
    let mut state = BlockState::new(&db);
    apply_tx(&mut state, &ctx(), &v).unwrap();
    state.commit().unwrap();

    let alice_after = Accounts::new(&db).get(&alice).unwrap();
    let bob_after = Accounts::new(&db).get(&bob).unwrap();
    let sum_after = alice_after.balance(AssetId::Usd) + bob_after.balance(AssetId::Usd);

    assert_eq!(sum_before, sum_after, "supply must be conserved");
    assert_eq!(alice_after.balance(AssetId::Usd), 1_000 - 333);
    assert_eq!(bob_after.balance(AssetId::Usd), 250 + 333);
    assert_eq!(alice_after.nonce, alice_before.nonce + 1);
    assert_eq!(
        bob_after.nonce, bob_before.nonce,
        "recipient nonce must NOT advance",
    );
}

// Silence the unused-import warning when AccountState ends up not directly
// referenced (it's reached transitively via Accounts; keep the import for
// documentation of the data model under test).
#[allow(dead_code)]
fn _unused() -> AccountState {
    AccountState::default()
}
