use cb_block::{apply_tx, build_and_apply, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair};
use cb_state::{AccountState, Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use tempfile::tempdir;

#[test]
fn transfer_moves_balance_and_increments_nonce() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);

    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);

    let accts = Accounts::new(&db);
    let mut a = AccountState::default();
    a.add(AssetId::Usd, 1_000).expect("seed balance");
    accts.put(&alice, &a);

    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to: bob,
            asset: AssetId::Usd,
            amount: 250,
        },
    };
    let verified = SignedTx::sign_with(&sk, tx).unwrap().verify().unwrap();

    let mut state = BlockState::new(&db);
    apply_tx(
        &mut state,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        &verified,
    )
    .unwrap();
    state.commit().unwrap();

    let alice_after = accts.get(&alice).unwrap();
    let bob_after = accts.get(&bob).unwrap();
    assert_eq!(alice_after.balance(AssetId::Usd), 750);
    assert_eq!(bob_after.balance(AssetId::Usd), 250);
    assert_eq!(alice_after.nonce, 1);
}

#[test]
fn build_and_apply_persists_block_and_advances_head() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (sk, pk) = generate_keypair();
    let alice = address_from_pubkey(&pk);
    let bob = Address([2; 20]);
    let accts = Accounts::new(&db);
    let mut a = AccountState::default();
    a.add(AssetId::Usd, 1_000).expect("seed balance");
    accts.put(&alice, &a);

    let tx = Transaction {
        nonce: 0,
        chain_id: 1,
        body: TxBody::Transfer {
            to: bob,
            asset: AssetId::Usd,
            amount: 100,
        },
    };
    let verified = SignedTx::sign_with(&sk, tx).unwrap().verify().unwrap();

    let res = build_and_apply(
        &db,
        &ExecCtx {
            treasury_signer: Address::ZERO,
            ..Default::default()
        },
        vec![verified],
        1_700_000_000,
        0,
        Vec::new(),
        |_preimage| vec![0u8; 65],
    )
    .unwrap();
    assert_eq!(res.block.header.height, 1);
    let meta = Meta::new(&db);
    assert_eq!(meta.head_height(), 1);
    assert_ne!(meta.state_root(), cb_types::H256::ZERO);
}
