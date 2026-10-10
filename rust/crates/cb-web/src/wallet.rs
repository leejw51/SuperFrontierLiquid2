//! Keys, addresses and signatures, entirely in the browser.
//!
//! `docs/api-contract.md` §2 makes identity an EVM key pair: an account is a
//! secp256k1 key derived from a BIP-39 mnemonic at `m/44'/60'/0'/0/{index}`, an
//! address is the EIP-55 rendering of the last twenty bytes of the keccak-256 of
//! the uncompressed public key, and a login is an EIP-191 `personal_sign`
//! signature over a message the gateway issues.
//!
//! The clause that decides this module's shape is the one in bold: **the
//! mnemonic is never sent to the server.** So the derivation, the hashing and
//! the signing all happen here, and the only thing that crosses the wire is a
//! 65-byte signature. There is no code path in this crate that puts a mnemonic
//! or a private key in a request body, and the `/auth/dev-login` endpoint —
//! which does take a mnemonic — is deliberately not implemented in the client.
//!
//! Correctness is pinned to the ecosystem's own published vector rather than to
//! anything invented here: the all-zero-entropy phrase must derive
//! `0x9858EfFD232B4033E47d90003D41EC34EcaEda94` at index 0. A derivation that
//! does not reproduce that address is a different derivation from the wallet,
//! the CLI and MetaMask, and would silently strand a user's funds under an
//! address they cannot reach from anywhere else.

use bip32::{DerivationPath, XPrv};
use bip39::{Language, Mnemonic};
use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use sha3::{Digest, Keccak256};

/// Anything that can go wrong turning a phrase into a signature.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WalletError {
    /// The phrase is not a valid BIP-39 mnemonic.
    ///
    /// The message deliberately does not say which word is wrong: a recovery
    /// phrase typed into a shared screen should not have its errors narrated.
    #[error("that is not a valid recovery phrase")]
    BadPhrase,
    /// The derivation path or the seed could not produce a key.
    #[error("that phrase could not produce a key: {0}")]
    Derivation(String),
    /// The browser has no source of entropy, so a new wallet cannot be minted.
    #[error("this browser would not provide randomness, so no wallet was created")]
    NoEntropy,
    /// Signing failed.
    #[error("the message could not be signed: {0}")]
    Signing(String),
}

/// The BIP-44 account path this exchange derives at, as the contract specifies.
#[must_use]
pub fn derivation_path(index: u32) -> String {
    format!("m/44'/60'/0'/0/{index}")
}

/// keccak-256, the hash the whole EVM address and signing scheme is built on.
///
/// Note that this is Keccak, not the later NIST SHA-3 padding. They differ, and
/// a SHA-3 implementation here would produce addresses that are valid-looking
/// and wrong.
#[must_use]
pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Renders twenty address bytes as EIP-55 checksummed hex.
///
/// The checksum lives in the letter casing: a nibble of the keccak hash of the
/// lowercase hex decides whether each hex letter is capitalised. It is how a
/// mistyped address is caught before it is credited to somebody else.
#[must_use]
pub fn to_eip55(address: &[u8; 20]) -> String {
    let lower = hex::encode(address);
    let hash = keccak256(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (index, ch) in lower.chars().enumerate() {
        let nibble = if index % 2 == 0 {
            hash[index / 2] >> 4
        } else {
            hash[index / 2] & 0x0f
        };
        if ch.is_ascii_digit() || nibble < 8 {
            out.push(ch);
        } else {
            out.push(ch.to_ascii_uppercase());
        }
    }
    out
}

/// True when an address string is well formed and, if mixed case, checksummed.
///
/// An all-lower or all-upper address carries no checksum and is accepted as the
/// contract says. A mixed-case address whose checksum does not validate is
/// rejected, because a wrong checksum means a typo and crediting it would pay a
/// different account.
#[must_use]
pub fn is_valid_address(value: &str) -> bool {
    let Some(body) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    else {
        return false;
    };
    if body.len() != 40 || !body.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return false;
    }
    let has_upper = body
        .chars()
        .any(|ch| ch.is_ascii_uppercase() && ch.is_ascii_alphabetic());
    let has_lower = body
        .chars()
        .any(|ch| ch.is_ascii_lowercase() && ch.is_ascii_alphabetic());
    if !(has_upper && has_lower) {
        return true;
    }
    let Ok(bytes) = hex::decode(body.to_ascii_lowercase()) else {
        return false;
    };
    let mut address = [0_u8; 20];
    address.copy_from_slice(&bytes);
    to_eip55(&address).eq_ignore_ascii_case(value) && to_eip55(&address) == format!("0x{body}")
}

/// The bytes EIP-191 `personal_sign` actually hashes.
///
/// `keccak256("\x19Ethereum Signed Message:\n" + len(message) + message)`, where
/// the length is the **byte** length written in decimal ASCII. Using the
/// character count instead would produce a valid signature over the wrong digest
/// for any message containing a non-ASCII character.
#[must_use]
pub fn eip191_hash(message: &str) -> [u8; 32] {
    let body = message.as_bytes();
    let mut prefixed = Vec::with_capacity(body.len() + 32);
    prefixed.extend_from_slice(b"\x19Ethereum Signed Message:\n");
    prefixed.extend_from_slice(body.len().to_string().as_bytes());
    prefixed.extend_from_slice(body);
    keccak256(&prefixed)
}

/// A derived account: the phrase, the key, and the address they produce.
///
/// [`Debug`] is implemented by hand so that a stray log line can never print a
/// mnemonic or a private key.
pub struct Wallet {
    phrase: String,
    index: u32,
    key: SigningKey,
    address: String,
}

impl core::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Wallet")
            .field("address", &self.address)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Wallet {
    /// Mints a fresh twelve-word wallet from the browser's entropy.
    ///
    /// This is the join flow: the product hands the user a wallet rather than
    /// asking them to bring one, and the phrase it hands over is the only way
    /// back into the account.
    pub fn generate() -> Result<Self, WalletError> {
        let mut entropy = [0_u8; 16];
        getrandom::getrandom(&mut entropy).map_err(|_| WalletError::NoEntropy)?;
        let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
            .map_err(|_| WalletError::NoEntropy)?;
        Self::from_phrase(&mnemonic.to_string(), 0)
    }

    /// Derives the account at `index` from an existing recovery phrase.
    pub fn from_phrase(phrase: &str, index: u32) -> Result<Self, WalletError> {
        let normalised = normalise_phrase(phrase);
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, &normalised)
            .map_err(|_| WalletError::BadPhrase)?;
        let seed = mnemonic.to_seed_normalized("");
        let path: DerivationPath = derivation_path(index)
            .parse()
            .map_err(|error: bip32::Error| WalletError::Derivation(error.to_string()))?;
        let extended = XPrv::derive_from_path(seed, &path)
            .map_err(|error| WalletError::Derivation(error.to_string()))?;
        let key = extended.private_key().clone();
        let address = address_of(&key);
        Ok(Self {
            phrase: normalised,
            index,
            key,
            address,
        })
    }

    /// The EIP-55 address this account signs as.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Which index of the account path this wallet was derived at.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The recovery phrase.
    ///
    /// Called in exactly two places: the hand-off screen that shows it once, and
    /// the download it offers. It is never put in a request.
    #[must_use]
    pub fn phrase(&self) -> &str {
        &self.phrase
    }

    /// Signs a message the way `personal_sign` does, returning `0x`-prefixed
    /// `r || s || v` with `v` of 27 or 28.
    pub fn sign_personal(&self, message: &str) -> Result<String, WalletError> {
        let digest = eip191_hash(message);
        let (signature, recovery): (Signature, RecoveryId) = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|error| WalletError::Signing(error.to_string()))?;
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&signature.to_bytes());
        out.push(recovery.to_byte() + 27);
        Ok(format!("0x{}", hex::encode(out)))
    }
}

/// Recovers the EIP-55 address that signed a message under EIP-191.
///
/// This is the gateway's half of the handshake. It lives here so the mock
/// backend can perform the real check rather than waving the signature through,
/// which makes signing in under the mock a genuine end-to-end exercise of the
/// derivation, the hash and the recovery id.
#[must_use]
pub fn recover_address(message: &str, signature: &str) -> Option<String> {
    use k256::ecdsa::VerifyingKey;
    let body = signature.strip_prefix("0x").unwrap_or(signature);
    let bytes = hex::decode(body).ok()?;
    if bytes.len() != 65 {
        return None;
    }
    let signature = Signature::from_slice(bytes.get(..64)?).ok()?;
    // A `v` of 0 or 1 is accepted alongside 27 or 28, because some signers
    // produce it and the contract says to take both.
    let v = *bytes.get(64)?;
    let recovery = RecoveryId::from_byte(if v >= 27 { v - 27 } else { v })?;
    let key =
        VerifyingKey::recover_from_prehash(&eip191_hash(message), &signature, recovery).ok()?;
    let point = key.to_encoded_point(false);
    let hash = keccak256(point.as_bytes().get(1..)?);
    let mut address = [0_u8; 20];
    address.copy_from_slice(hash.get(12..)?);
    Some(to_eip55(&address))
}

/// The EIP-55 address a signing key corresponds to.
fn address_of(key: &SigningKey) -> String {
    let point = key.verifying_key().to_encoded_point(false);
    // The leading 0x04 tag is not part of the hashed material.
    let hash = keccak256(&point.as_bytes()[1..]);
    let mut address = [0_u8; 20];
    address.copy_from_slice(&hash[12..]);
    to_eip55(&address)
}

/// Collapses whitespace and lowercases a phrase before parsing it.
///
/// People paste recovery phrases out of text files and password managers, which
/// is to say with line breaks, double spaces and stray capitals. Rejecting those
/// would be the interface blaming the user for its own strictness.
#[must_use]
pub fn normalise_phrase(phrase: &str) -> String {
    phrase
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Checks a phrase without deriving anything, for live form validation.
#[must_use]
pub fn is_valid_phrase(phrase: &str) -> bool {
    Mnemonic::parse_in_normalized(Language::English, &normalise_phrase(phrase)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract's own vector: the all-zero-entropy phrase.
    const ABANDON: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn the_canonical_phrase_derives_the_canonical_address() {
        let wallet = Wallet::from_phrase(ABANDON, 0).expect("the canonical phrase is valid");
        assert_eq!(
            wallet.address(),
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        );
    }

    #[test]
    fn a_messy_paste_is_accepted() {
        let messy = format!("  {}  ", ABANDON.replace(' ', "\n  ").to_uppercase());
        let wallet = Wallet::from_phrase(&messy, 0).expect("whitespace and case are normalised");
        assert_eq!(
            wallet.address(),
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        );
    }

    #[test]
    fn a_wrong_phrase_is_refused_without_saying_which_word() {
        let error = Wallet::from_phrase("not a real recovery phrase at all thanks", 0)
            .expect_err("nonsense must not derive a key");
        assert!(matches!(error, WalletError::BadPhrase));
        assert_eq!(error.to_string(), "that is not a valid recovery phrase");
    }

    #[test]
    fn indexes_derive_different_accounts() {
        let zero = Wallet::from_phrase(ABANDON, 0).expect("valid");
        let one = Wallet::from_phrase(ABANDON, 1).expect("valid");
        assert_ne!(zero.address(), one.address());
        assert_eq!(derivation_path(1), "m/44'/60'/0'/0/1");
    }

    #[test]
    fn eip55_matches_the_published_checksum_vectors() {
        let cases = [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ];
        for expected in cases {
            let bytes = hex::decode(&expected[2..]).expect("hex");
            let mut address = [0_u8; 20];
            address.copy_from_slice(&bytes);
            assert_eq!(to_eip55(&address), expected);
        }
    }

    #[test]
    fn address_validation_rejects_a_broken_checksum_and_accepts_a_flat_one() {
        assert!(is_valid_address(
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        ));
        assert!(is_valid_address(
            "0x9858effd232b4033e47d90003d41ec34ecaeda94"
        ));
        assert!(!is_valid_address(
            "0x9858EFFD232B4033E47d90003D41EC34EcaEda94"
        ));
        assert!(!is_valid_address("0x9858"));
        assert!(!is_valid_address(
            "9858effd232b4033e47d90003d41ec34ecaeda94"
        ));
    }

    #[test]
    fn the_personal_sign_prefix_counts_bytes_not_characters() {
        // "é" is one character and two bytes; the prefix must say 2.
        let hash = eip191_hash("é");
        let expected = keccak256(b"\x19Ethereum Signed Message:\n2\xc3\xa9");
        assert_eq!(hash, expected);
    }

    #[test]
    fn a_signature_is_sixty_five_bytes_with_a_v_of_27_or_28() {
        let wallet = Wallet::from_phrase(ABANDON, 0).expect("valid");
        let signature = wallet
            .sign_personal("SuperFrontierLiquid wants you to sign in.")
            .expect("signing works");
        assert!(signature.starts_with("0x"));
        let bytes = hex::decode(&signature[2..]).expect("hex");
        assert_eq!(bytes.len(), 65);
        assert!(matches!(bytes[64], 27 | 28), "v was {}", bytes[64]);
    }

    #[test]
    fn a_signature_recovers_to_the_signing_address() {
        let wallet = Wallet::from_phrase(ABANDON, 0).expect("valid");
        let message = "SuperFrontierLiquid wants you to sign in.\n\nNonce: 7f2a91c4e8";
        let signature = wallet.sign_personal(message).expect("signing works");
        assert_eq!(
            recover_address(message, &signature).as_deref(),
            Some(wallet.address())
        );
    }

    #[test]
    fn a_signature_over_a_different_message_recovers_elsewhere() {
        let wallet = Wallet::from_phrase(ABANDON, 0).expect("valid");
        let signature = wallet.sign_personal("one message").expect("signing works");
        assert_ne!(
            recover_address("another message", &signature).as_deref(),
            Some(wallet.address()),
            "a signature must not validate against a message it did not sign"
        );
        assert_eq!(recover_address("one message", "0xdeadbeef"), None);
    }

    #[test]
    fn a_minted_wallet_is_twelve_words_and_round_trips() {
        let wallet = Wallet::generate().expect("the host has entropy");
        assert_eq!(wallet.phrase().split_whitespace().count(), 12);
        assert!(is_valid_phrase(wallet.phrase()));
        let again = Wallet::from_phrase(wallet.phrase(), 0).expect("valid");
        assert_eq!(again.address(), wallet.address());
    }
}
