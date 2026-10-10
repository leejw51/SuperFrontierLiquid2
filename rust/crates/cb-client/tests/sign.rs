//! Signing correctness for cb-client wallets.
//!
//! Pins the property that a Transfer tx signed with `WalletEntry::secret_key()`
//! recovers back to `WalletEntry::address()`. This is the contract `cmd_send`
//! relies on — if it ever drifts (e.g. mismatched derivation between sk/pk and
//! the stored hex address), submitted txs would be attributed to a different
//! sender than the one the user just logged in as.

use cb_client::wallet::WalletEntry;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const LETTER: &str =
    "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

fn sample_transfer(nonce: u64) -> Transaction {
    Transaction {
        nonce,
        chain_id: 1,
        body: TxBody::Transfer {
            to: Address([0xbb; 20]),
            asset: AssetId::Usd,
            amount: 1_000,
        },
    }
}

#[test]
fn signed_tx_recovers_to_wallet_address() {
    let wallet = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    let sk = wallet.secret_key().unwrap();
    let expected = wallet.address().unwrap();

    let signed = SignedTx::sign_with(&sk, sample_transfer(0)).unwrap();
    assert_eq!(signed.sender().unwrap(), expected);
}

#[test]
fn signed_tx_recovers_consistently_across_indexes() {
    for index in [0u32, 1, 7, 999] {
        let wallet = WalletEntry::derive("bob", LEGAL, index).unwrap();
        let sk = wallet.secret_key().unwrap();
        let expected = wallet.address().unwrap();
        let signed = SignedTx::sign_with(&sk, sample_transfer(index as u64)).unwrap();
        assert_eq!(
            signed.sender().unwrap(),
            expected,
            "index {index} signed sender must equal derived address",
        );
    }
}

#[test]
fn different_wallets_produce_different_signatures_and_senders() {
    let alice = WalletEntry::derive("alice", LEGAL, 0).unwrap();
    let bob = WalletEntry::derive("bob", LETTER, 0).unwrap();
    let tx_a = sample_transfer(0);
    let tx_b = sample_transfer(0);

    let sig_a = SignedTx::sign_with(&alice.secret_key().unwrap(), tx_a).unwrap();
    let sig_b = SignedTx::sign_with(&bob.secret_key().unwrap(), tx_b).unwrap();

    assert_ne!(sig_a.sender().unwrap(), sig_b.sender().unwrap());
    assert_eq!(sig_a.sender().unwrap(), alice.address().unwrap());
    assert_eq!(sig_b.sender().unwrap(), bob.address().unwrap());
    // Different signers ⇒ different signatures even on textually identical txs.
    assert!(
        sig_a.sig_r != sig_b.sig_r || sig_a.sig_s != sig_b.sig_s,
        "two distinct signers must yield distinct signatures",
    );
}

#[test]
fn signature_changes_when_tx_body_changes() {
    let w = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    let sk = w.secret_key().unwrap();
    let s1 = SignedTx::sign_with(&sk, sample_transfer(0)).unwrap();
    let mut tx2 = sample_transfer(0);
    if let TxBody::Transfer { amount, .. } = &mut tx2.body {
        *amount = 2_000;
    }
    let s2 = SignedTx::sign_with(&sk, tx2).unwrap();
    // Same signer, but different message → different sig + different hash.
    assert_eq!(s1.sender().unwrap(), s2.sender().unwrap());
    assert_ne!(s1.hash(), s2.hash());
}

#[test]
fn signature_is_tampering_evident() {
    let w = WalletEntry::derive("alice", LETTER, 0).unwrap();
    let sk = w.secret_key().unwrap();
    let mut signed = SignedTx::sign_with(&sk, sample_transfer(5)).unwrap();
    let original_sender = signed.sender().unwrap();
    // Flip a byte in the recipient — the signing hash changes, so recovered
    // sender either differs from `original_sender` or recovery fails outright.
    if let TxBody::Transfer { to, .. } = &mut signed.tx.body {
        to.0[0] ^= 0xff;
    }
    match signed.sender() {
        Ok(recovered) => assert_ne!(
            recovered, original_sender,
            "tampered tx must not recover to the original signer",
        ),
        Err(_) => { /* recovery failure also acceptable */ }
    }
}
