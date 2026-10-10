//! BIP-39 + BIP-44 compatibility pin.
//!
//! Anchors `WalletEntry::derive` to Ethereum-standard derivation
//! (`m/44'/60'/0'/0/N` over BIP-39 PBKDF2 seed) so that importing the same
//! mnemonic into MetaMask / ethers.js produces the same address. The
//! "abandon ×11 about" vector is the canonical BIP-39 test phrase whose
//! address at `m/44'/60'/0'/0/0` is universally documented; if this test
//! ever drifts, downstream tooling integration breaks silently.

use cb_client::wallet::WalletEntry;

const ABANDON_X11_ABOUT: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

#[test]
fn abandon_x11_about_index0_matches_metamask() {
    let w = WalletEntry::derive("test", ABANDON_X11_ABOUT, 0).expect("derive");
    // Reference: ethers.js / MetaMask / Trezor for `m/44'/60'/0'/0/0`.
    assert_eq!(
        w.address.to_lowercase(),
        "9858effd232b4033e47d90003d41ec34ecaeda94",
        "BIP-39/BIP-44 derivation must match the canonical Ethereum vector",
    );
}

#[test]
fn different_indexes_yield_different_addresses() {
    let a = WalletEntry::derive("t", ABANDON_X11_ABOUT, 0).expect("derive 0");
    let b = WalletEntry::derive("t", ABANDON_X11_ABOUT, 1).expect("derive 1");
    assert_ne!(a.address, b.address, "index 0 and index 1 must differ");
}

#[test]
fn invalid_single_word_is_rejected() {
    let err = WalletEntry::derive("t", "hello", 0).unwrap_err();
    assert!(
        err.to_string().contains("BIP-39"),
        "error must mention BIP-39: got {err}",
    );
}

#[test]
fn invalid_two_words_is_rejected() {
    let err = WalletEntry::derive("t", "abandon abandon", 0).unwrap_err();
    assert!(
        err.to_string().contains("BIP-39"),
        "error must mention BIP-39: got {err}",
    );
}

#[test]
fn invalid_unknown_wordlist_token_is_rejected() {
    // 12 tokens, but the last one isn't on the BIP-39 English wordlist.
    let bad =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon notaword";
    let err = WalletEntry::derive("t", bad, 0).unwrap_err();
    assert!(
        err.to_string().contains("BIP-39"),
        "error must mention BIP-39: got {err}",
    );
}

#[test]
fn whitespace_around_mnemonic_is_tolerated() {
    let with_ws = format!("  {ABANDON_X11_ABOUT}  \n");
    let w = WalletEntry::derive("t", &with_ws, 0).expect("derive trims whitespace");
    assert_eq!(
        w.address.to_lowercase(),
        "9858effd232b4033e47d90003d41ec34ecaeda94"
    );
    // The persisted form is normalized.
    assert_eq!(w.mnemonic, ABANDON_X11_ABOUT);
}
