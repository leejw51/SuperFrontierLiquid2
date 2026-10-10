//! Wallet derivation invariants under BIP-39 + BIP-44.
//!
//! All fixtures here use real BIP-39 English-wordlist phrases — anything
//! else is rejected by `WalletEntry::derive` after the migration to strict
//! Ethereum-standard derivation. We use three canonical test vectors so
//! the assertions stay reproducible against external tooling (MetaMask,
//! ethers.js).

use cb_client::wallet::{derive_keypair, WalletEntry};
use cb_crypto::{address_from_pubkey, keccak256};

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const LETTER: &str =
    "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

#[test]
fn same_mnemonic_and_index_yields_same_address() {
    let a = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    let b = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    assert_eq!(a.address, b.address);
    assert_eq!(a.privkey, b.privkey);
    assert_eq!(a.pubkey, b.pubkey);
}

#[test]
fn different_index_yields_different_address() {
    let a = WalletEntry::derive("alice", LEGAL, 0).unwrap();
    let b = WalletEntry::derive("alice", LEGAL, 1).unwrap();
    assert_ne!(a.address, b.address);
}

#[test]
fn different_mnemonic_yields_different_address() {
    let a = WalletEntry::derive("a", LEGAL, 0).unwrap();
    let b = WalletEntry::derive("a", LETTER, 0).unwrap();
    assert_ne!(a.address, b.address);
}

/// Confirm `address_from_pubkey` matches the keccak256 lower-20-bytes
/// definition the rest of the system uses, so a wallet entry's address
/// will round-trip through the engine.
#[test]
fn address_matches_keccak_low20_of_pubkey_xy() {
    let (_sk, pk, _m) = derive_keypair(LETTER, 7).unwrap();
    let uncompressed = pk.serialize_uncompressed();
    let h = keccak256(&uncompressed[1..]);
    let mut expected = [0u8; 20];
    expected.copy_from_slice(&h.0[12..]);
    let derived = address_from_pubkey(&pk);
    assert_eq!(derived.0, expected);

    // And the WalletEntry's stored hex address agrees.
    let entry = WalletEntry::derive("x", LETTER, 7).unwrap();
    assert_eq!(entry.address, hex::encode(expected));
}

#[test]
fn entry_address_decodes_to_address_struct() {
    let entry = WalletEntry::derive("bob", LEGAL, 3).unwrap();
    let addr = entry.address().expect("decode");
    assert_eq!(hex::encode(addr.0), entry.address);
}

#[test]
fn entry_secret_key_matches_stored_hex() {
    let entry = WalletEntry::derive("c", LETTER, 9).unwrap();
    let sk = entry.secret_key().expect("decode privkey");
    assert_eq!(hex::encode(sk.secret_bytes()), entry.privkey);
}

#[test]
fn privkey_masked_does_not_leak_full_secret() {
    let entry = WalletEntry::derive("d", ABANDON, 0).unwrap();
    let masked = entry.privkey_masked();
    assert!(masked.ends_with("..."));
    assert!(!masked.contains(&entry.privkey[5..])); // tail isn't present
    assert!(masked.len() < entry.privkey.len());
}

/// Pin: `m/44'/60'/0'/0/0` over `abandon ×11 about` is the universally
/// documented BIP-39 + BIP-44 Ethereum vector. If this address ever
/// changes, MetaMask import compatibility silently broke.
#[test]
fn abandon_x11_about_index0_is_canonical_eth_address() {
    let entry = WalletEntry::derive("canon", ABANDON, 0).unwrap();
    assert_eq!(
        entry.address.to_lowercase(),
        "9858effd232b4033e47d90003d41ec34ecaeda94",
    );
}
