//! Reproductions for `docs/core-review.md` findings that do not have a natural
//! home in one of the feature test files: M4 (block size) and M5 (withdrawals).

use cb_block::{apply_tx, build_and_apply, verify_and_commit, BlockState, ExecCtx, ExecError};
use cb_block::{VerifyError, MAX_BLOCK_TXS};
use cb_crypto::{address_from_pubkey, keccak256, keypair_from_seed, sign as crypto_sign};
use cb_state::{Accounts, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, H256};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    (dir, db)
}

#[test]
fn withdrawals_are_refused_rather_than_stranding_funds() {
    // Core review M5. `Withdraw` locked the amount and `WithdrawSettled`
    // advanced the treasury's nonce and did nothing else, so every withdrawal
    // was a permanent reduction of a balance. Until there is a ledger for
    // them to settle against, both fail closed like `AdjustMargin`.
    let (_dir, db) = open();
    let (sk, pk) = keypair_from_seed(b"core-review-withdrawer");
    let alice = address_from_pubkey(&pk);
    {
        let mut s = cb_state::AccountState::default();
        s.add(AssetId::Usd, 1_000).unwrap();
        Accounts::new(&db).put(&alice, &s);
    }
    let (treasury_sk, treasury_pk) = keypair_from_seed(b"core-review-treasury");
    let treasury = address_from_pubkey(&treasury_pk);
    let ctx = ExecCtx::for_treasury(treasury);

    let withdraw = SignedTx::sign_with(
        &sk,
        Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Withdraw {
                asset: AssetId::Usd,
                amount: 400,
                external_address: "0xelsewhere".into(),
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap();
    let mut state = BlockState::new(&db);
    assert_eq!(
        apply_tx(&mut state, &ctx, &withdraw).unwrap_err(),
        ExecError::Unsupported
    );
    state.commit().unwrap();
    let s = Accounts::new(&db).get(&alice).unwrap();
    assert_eq!(s.balance(AssetId::Usd), 1_000);
    assert_eq!(s.locked_of(AssetId::Usd), 0, "nothing was locked");
    assert_eq!(s.nonce, 0, "and the transaction did not consume a nonce");

    let settled = SignedTx::sign_with(
        &treasury_sk,
        Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::WithdrawSettled {
                withdraw_id: H256([1; 32]),
                external_tx_ref: "tx".into(),
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap();
    let mut state = BlockState::new(&db);
    assert_eq!(
        apply_tx(&mut state, &ctx, &settled).unwrap_err(),
        ExecError::Unsupported
    );
}

#[test]
fn a_block_larger_than_pack_seq_can_number_is_refused_by_builder_and_verifier() {
    // Core review M4. `pack_seq` keeps sixteen bits of the transaction's index,
    // and the book index is keyed by `(price, seq)`, so transaction 0 and
    // transaction 65 536 of one block would share a key. Nothing capped the
    // block; `--max-block-txs` is operator-set.
    let (_dir, db) = open();
    let (sk, pk) = keypair_from_seed(b"core-review-sender");
    let alice = address_from_pubkey(&pk);
    let (proposer_sk, proposer_pk) = keypair_from_seed(b"core-review-proposer");
    Meta::new(&db).set_proposer_addr(address_from_pubkey(&proposer_pk));

    // One signed transaction, cloned: the size check runs before anything
    // is executed, so the copies never need to be valid against state.
    let one = SignedTx::sign_with(
        &sk,
        Transaction {
            nonce: 0,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([2; 20]),
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap();
    let txs: Vec<VerifiedTx> = (0..=MAX_BLOCK_TXS)
        .map(|_| VerifiedTx::from_parts_unchecked(one.clone(), alice))
        .collect();
    assert_eq!(txs.len(), MAX_BLOCK_TXS + 1);

    let signer = |preimage: &[u8]| {
        crypto_sign(&proposer_sk, keccak256(preimage))
            .unwrap()
            .to_vec()
    };
    let err = match build_and_apply(
        &db,
        &ExecCtx::for_treasury(Address::ZERO),
        txs.clone(),
        0,
        0,
        Vec::new(),
        signer,
    ) {
        Err(e) => e,
        Ok(_) => panic!("an oversized block was built"),
    };
    assert_eq!(
        err,
        ExecError::TooManyTxs {
            got: MAX_BLOCK_TXS + 1,
            max: MAX_BLOCK_TXS
        }
    );
    assert_eq!(Meta::new(&db).head_height(), 0);

    let err = verify_and_commit(
        &db,
        &ExecCtx::for_treasury(Address::ZERO),
        &txs,
        1,
        H256([7; 32]),
        b"irrelevant",
        H256::ZERO,
        0,
        &[],
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            VerifyError::Apply(ExecError::TooManyTxs { got, max }) if got == MAX_BLOCK_TXS + 1 && max == MAX_BLOCK_TXS
        ),
        "got {err:?}"
    );
    assert_eq!(Meta::new(&db).head_height(), 0);
}
