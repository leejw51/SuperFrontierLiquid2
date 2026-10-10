//! Ethereum-compatible identity: EIP-55 addresses, EIP-191 signatures, BIP-44
//! derivation.
//!
//! ## Why this is not this exchange's own scheme
//!
//! The API contract is explicit that login is **Ethereum-compatible**: the
//! same mnemonic produces the same address here, in MetaMask, in `cb-client`
//! and in the SuperFrontier Wallet. That is only true if every primitive matches
//! the ecosystem exactly, so each one here is the standard one and none of
//! them is a variation:
//!
//! * **Address** — the last 20 bytes of `keccak256` over the *uncompressed*
//!   public key with its `0x04` prefix stripped.
//! * **Checksum** — EIP-55: the hex digits are upper-cased where the
//!   corresponding nibble of `keccak256(lowercase_hex)` is 8 or more.
//! * **Message hash** — EIP-191 `personal_sign`:
//!   `keccak256("\x19Ethereum Signed Message:\n" + len(message) + message)`,
//!   with the length as ASCII decimal *bytes*, not characters.
//! * **Derivation** — BIP-39 seed, BIP-32 path `m/44'/60'/0'/0/{index}`.
//!
//! The contract names one canonical vector, and
//! [`tests::the_canonical_vector_from_the_contract`] pins it: the all-zero
//! entropy phrase at index 0 must derive
//! `0x9858EfFD232B4033E47d90003D41EC34EcaEda94`. A build that does not
//! reproduce that address is implementing a different derivation from the rest
//! of the ecosystem, and would silently give every user the wrong account.

use cb_crypto::{keccak256, Signature};
use cb_types::{Address, H256};

/// Why an address or signature could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EthError {
    /// The address is not 20 bytes of hex.
    #[error("`{0}` is not a 20-byte hex address")]
    MalformedAddress(String),
    /// A mixed-case address whose EIP-55 checksum does not validate.
    #[error(
        "`{0}` has a mixed-case form but its EIP-55 checksum does not match; \
         that means a typo or a corrupted copy, and accepting it would credit \
         a different account"
    )]
    BadChecksum(String),
    /// The signature is not 65 bytes of hex.
    #[error("signature must be 65 bytes of hex: {0}")]
    MalformedSignature(String),
    /// The recovery id is not one of the accepted values.
    #[error("signature recovery id {0} is not 0, 1, 27 or 28")]
    BadRecoveryId(u8),
    /// The signature does not recover to a public key.
    #[error("signature does not recover a signer: {0}")]
    Unrecoverable(String),
    /// The signature's `s` is in the upper half of the curve order.
    #[error(
        "signature is not canonical: `s` is in the upper half of the curve order. \
         secp256k1 signatures are malleable — (r, s, v) and (r, N-s, v^1) recover the same \
         signer — so only the low-s form is accepted"
    )]
    HighS,
    /// The mnemonic is not a valid BIP-39 phrase.
    #[error("invalid BIP-39 mnemonic: {0}")]
    BadMnemonic(String),
    /// BIP-32 derivation failed.
    #[error("BIP-32 derivation failed: {0}")]
    Derivation(String),
}

/// Render an address as EIP-55 checksummed hex.
///
/// This is the form the gateway always emits. Mixed case is not decoration: it
/// is a checksum over the address, and a wallet that pastes it somewhere else
/// will have it validated.
pub fn checksum_address(address: &Address) -> String {
    let lower = hex::encode(address.0);
    let hash = keccak256(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (index, ch) in lower.chars().enumerate() {
        // The nibble of the hash that corresponds to this hex digit: byte
        // `index / 2`, high nibble for even positions.
        let byte = hash.0[index / 2];
        let nibble = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        if ch.is_ascii_digit() || nibble < 8 {
            out.push(ch);
        } else {
            out.push(ch.to_ascii_uppercase());
        }
    }
    out
}

/// Parse an address, validating the EIP-55 checksum when one is present.
///
/// All-lowercase and all-uppercase forms carry no checksum and are accepted as
/// written. A mixed-case form *is* a checksum, and a wrong one is rejected:
/// the contract is explicit that accepting it would credit a different
/// account.
pub fn parse_checked_address(text: &str) -> Result<Address, EthError> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if body.len() != 40 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EthError::MalformedAddress(text.to_string()));
    }
    let bytes = hex::decode(body).map_err(|_| EthError::MalformedAddress(text.to_string()))?;
    let raw: [u8; 20] = bytes
        .try_into()
        .map_err(|_| EthError::MalformedAddress(text.to_string()))?;
    let address = Address(raw);

    let has_upper = body.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = body.bytes().any(|b| b.is_ascii_lowercase());
    if has_upper && has_lower {
        let expected = checksum_address(&address);
        if !expected[2..].eq(body) {
            return Err(EthError::BadChecksum(text.to_string()));
        }
    }
    Ok(address)
}

/// The EIP-191 `personal_sign` prefix.
const PERSONAL_SIGN_PREFIX: &[u8] = b"\x19Ethereum Signed Message:\n";

/// The digest a wallet actually signs for `personal_sign`.
///
/// The length is the message's byte length, rendered as ASCII decimal. Using
/// the character count instead would differ for any message with a non-ASCII
/// character, and the challenge text is under the server's control precisely
/// so that never becomes a compatibility surprise.
pub fn personal_sign_hash(message: &[u8]) -> H256 {
    let mut preimage = Vec::with_capacity(PERSONAL_SIGN_PREFIX.len() + 20 + message.len());
    preimage.extend_from_slice(PERSONAL_SIGN_PREFIX);
    preimage.extend_from_slice(message.len().to_string().as_bytes());
    preimage.extend_from_slice(message);
    keccak256(&preimage)
}

/// `floor(N/2)` for the secp256k1 group order `N`, big-endian.
///
/// A signature is canonical when `s <= SECP256K1_HALF_ORDER`. The constant is
/// checked against `N` in
/// [`tests::the_half_order_constant_is_actually_half_the_curve_order`] rather
/// than trusted, because a wrong value here would either reject valid
/// signatures or accept malleable ones and nothing else would notice.
const SECP256K1_HALF_ORDER: [u8; 32] = [
    0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B, 0x20, 0xA0,
];

/// Whether `s` is in the lower half of the curve order.
fn is_canonical_s(s: &[u8; 32]) -> bool {
    // Big-endian comparison; `<=` because `N` is odd, so `floor(N/2)` is
    // itself a legal low-s value.
    *s <= SECP256K1_HALF_ORDER
}

/// Parse a `0x`-prefixed 65-byte `r || s || v` signature.
///
/// `v` is normalised from the EIP-155-era 27/28 to the 0/1 the recovery
/// routine wants. Both are accepted because both are produced in the wild:
/// `eth_sign` and `personal_sign` return 27/28, while several libraries return
/// the raw recovery id.
///
/// **Only the canonical low-s form is accepted.** secp256k1 signatures are
/// malleable: `(r, s, v)` and `(r, N-s, v^1)` recover the same signer, so a
/// captured signature has a valid twin that is a different 65 bytes. Nothing
/// in this gateway currently treats a signature as an identifier — challenges
/// are single-use, which is what keeps the twin from buying a second login —
/// but a rule the signer already obeys and the verifier does not is a
/// difference waiting to be leaned on, and [`sign_personal_sign`] emits low-s.
pub fn parse_signature(text: &str) -> Result<Signature, EthError> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(body).map_err(|e| EthError::MalformedSignature(e.to_string()))?;
    if bytes.len() != 65 {
        return Err(EthError::MalformedSignature(format!(
            "got {} bytes",
            bytes.len()
        )));
    }
    let recovery_id = match bytes[64] {
        v @ (0 | 1) => v,
        v @ (27 | 28) => v - 27,
        other => return Err(EthError::BadRecoveryId(other)),
    };
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&bytes[..32]);
    s.copy_from_slice(&bytes[32..64]);
    if !is_canonical_s(&s) {
        return Err(EthError::HighS);
    }
    Ok(Signature { r, s, recovery_id })
}

/// Recover the address that signed `message` with `personal_sign`.
pub fn recover_personal_sign(message: &str, signature: &str) -> Result<Address, EthError> {
    let signature = parse_signature(signature)?;
    let digest = personal_sign_hash(message.as_bytes());
    cb_crypto::recover(&signature, digest).map_err(|e| EthError::Unrecoverable(e.to_string()))
}

/// Sign `message` with `personal_sign`, producing the `0x`-prefixed 65-byte
/// form a wallet would.
///
/// Used by the development login, which derives a key from a supplied mnemonic
/// and signs the gateway's own challenge with it.
pub fn sign_personal_sign(key: &cb_crypto::SecretKey, message: &str) -> Result<String, EthError> {
    let digest = personal_sign_hash(message.as_bytes());
    let signature =
        cb_crypto::sign(key, digest).map_err(|e| EthError::Unrecoverable(e.to_string()))?;
    let mut bytes = signature.to_bytes65();
    // Emit the 27/28 form, which is what every EVM wallet produces.
    bytes[64] += 27;
    Ok(format!("0x{}", hex::encode(bytes)))
}

/// Derive a key pair from a BIP-39 mnemonic at `m/44'/60'/0'/0/{index}`.
///
/// Empty BIP-39 passphrase, matching `cb-client` and MetaMask's defaults.
pub fn derive_from_mnemonic(
    mnemonic: &str,
    index: u32,
) -> Result<(cb_crypto::SecretKey, Address), EthError> {
    use bip32::{DerivationPath, XPrv};
    use bip39::{Language, Mnemonic};

    let trimmed = mnemonic.trim();
    if trimmed.is_empty() {
        return Err(EthError::BadMnemonic("empty".into()));
    }
    let parsed = Mnemonic::parse_in_normalized(Language::English, trimmed)
        .map_err(|e| EthError::BadMnemonic(e.to_string()))?;
    let seed = parsed.to_seed_normalized("");
    let path: DerivationPath = format!("m/44'/60'/0'/0/{index}")
        .parse()
        .map_err(|e| EthError::Derivation(format!("{e}")))?;
    let xprv =
        XPrv::derive_from_path(seed, &path).map_err(|e| EthError::Derivation(e.to_string()))?;
    let secret = cb_crypto::SecretKey::from_slice(&xprv.private_key().to_bytes())
        .map_err(|e| EthError::Derivation(e.to_string()))?;
    let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &secret);
    Ok((secret, cb_crypto::address_from_pubkey(&public)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The all-zero-entropy phrase every BIP-39 implementation tests with.
    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                           abandon abandon abandon about";

    #[test]
    fn the_canonical_vector_from_the_contract() {
        // `docs/api-contract.md` §2: "A test that does not reproduce that
        // address is testing a different derivation than the rest of the
        // ecosystem."
        let (_secret, address) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        assert_eq!(
            checksum_address(&address),
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
        );
    }

    #[test]
    fn a_different_index_is_a_different_account() {
        let (_s0, a0) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        let (_s1, a1) = derive_from_mnemonic(ABANDON, 1).expect("derive");
        assert_ne!(a0, a1);
    }

    #[test]
    fn an_invalid_mnemonic_is_rejected() {
        for bad in [
            "",
            "not a mnemonic at all",
            // Right word count, wrong checksum.
            "abandon abandon abandon abandon abandon abandon abandon abandon \
             abandon abandon abandon abandon",
        ] {
            assert!(
                derive_from_mnemonic(bad, 0).is_err(),
                "`{bad}` must not derive a key"
            );
        }
    }

    #[test]
    fn eip55_checksums_match_the_published_examples() {
        // From EIP-55 itself. These are the values every EVM tool produces.
        for expected in [
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
            "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
            "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
        ] {
            let address = parse_checked_address(&expected.to_ascii_lowercase()).expect("parse");
            assert_eq!(checksum_address(&address), expected);
        }
    }

    #[test]
    fn a_lower_or_upper_case_address_carries_no_checksum_and_is_accepted() {
        let canonical = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
        let address = parse_checked_address(canonical).expect("canonical");
        assert_eq!(
            parse_checked_address(&canonical.to_ascii_lowercase()).expect("lower"),
            address
        );
        let upper = format!("0x{}", canonical[2..].to_ascii_uppercase());
        assert_eq!(parse_checked_address(&upper).expect("upper"), address);
    }

    #[test]
    fn a_mixed_case_address_with_a_wrong_checksum_is_refused() {
        // One digit's case flipped from the canonical form.
        let wrong = "0x5AAeb6053F3E94C9b9A09f33669435E7Ef1BeAed";
        assert!(matches!(
            parse_checked_address(wrong),
            Err(EthError::BadChecksum(_))
        ));
    }

    #[test]
    fn malformed_addresses_are_refused() {
        for bad in [
            "",
            "0x",
            "0x1234",
            "not-hex-at-all-not-hex-at-all-not-hex-1234",
            "0xzz",
        ] {
            assert!(
                matches!(
                    parse_checked_address(bad),
                    Err(EthError::MalformedAddress(_))
                ),
                "`{bad}` must not parse"
            );
        }
    }

    #[test]
    fn the_personal_sign_prefix_uses_the_byte_length() {
        // "hello" is five bytes; the digest must be over the prefixed form.
        let digest = personal_sign_hash(b"hello");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x19Ethereum Signed Message:\n5hello");
        assert_eq!(digest, keccak256(&expected));

        // A multi-byte character counts as its bytes, not as one character.
        let message = "é";
        assert_eq!(message.len(), 2);
        let digest = personal_sign_hash(message.as_bytes());
        assert_eq!(
            digest,
            keccak256(b"\x19Ethereum Signed Message:\n2\xc3\xa9")
        );
    }

    #[test]
    fn a_signature_round_trips_through_sign_and_recover() {
        let (secret, address) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        let message = "SuperFrontierLiquid wants you to sign in.";
        let signature = sign_personal_sign(&secret, message).expect("sign");
        assert!(signature.starts_with("0x"));
        assert_eq!(signature.len(), 2 + 130);
        assert_eq!(
            recover_personal_sign(message, &signature).expect("recover"),
            address
        );
    }

    #[test]
    fn a_signature_over_a_different_message_recovers_a_different_signer() {
        let (secret, address) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        let signature = sign_personal_sign(&secret, "message one").expect("sign");
        let recovered = recover_personal_sign("message two", &signature);
        // Recovery still succeeds — it always does — but it yields somebody
        // else. That is exactly why the caller must compare the result to the
        // claimed address rather than merely checking that recovery worked.
        assert_ne!(recovered.expect("recovers something"), address);
    }

    #[test]
    fn both_recovery_id_conventions_are_accepted() {
        let (secret, address) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        let message = "either convention";
        let canonical = sign_personal_sign(&secret, message).expect("sign");

        // The same signature with v as 0/1 instead of 27/28.
        let mut bytes = hex::decode(&canonical[2..]).expect("hex");
        bytes[64] -= 27;
        let raw = format!("0x{}", hex::encode(&bytes));
        assert_eq!(
            recover_personal_sign(message, &raw).expect("recover"),
            address
        );
    }

    /// The secp256k1 group order, big-endian.
    const SECP256K1_ORDER: [u8; 32] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36,
        0x41, 0x41,
    ];

    /// `N - s`, big-endian. `s` is always below `N` for a real signature.
    fn negate_scalar(s: &[u8; 32]) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let diff = SECP256K1_ORDER[i] as i16 - s[i] as i16 - borrow;
            if diff < 0 {
                out[i] = (diff + 256) as u8;
                borrow = 1;
            } else {
                out[i] = diff as u8;
                borrow = 0;
            }
        }
        out
    }

    #[test]
    fn the_half_order_constant_is_actually_half_the_curve_order() {
        // 2 * floor(N/2) + 1 == N, because N is odd. A wrong constant here
        // would silently either reject valid signatures or admit malleable
        // ones, and no other test would see it.
        let mut doubled = [0u8; 32];
        let mut carry = 0u16;
        for i in (0..32).rev() {
            let sum = (SECP256K1_HALF_ORDER[i] as u16) * 2 + carry;
            doubled[i] = (sum & 0xff) as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0);
        // Add one.
        for i in (0..32).rev() {
            let (value, overflow) = doubled[i].overflowing_add(if i == 31 { 1 } else { 0 });
            doubled[i] = value;
            assert!(!overflow, "the low byte cannot carry here");
        }
        assert_eq!(doubled, SECP256K1_ORDER);
    }

    #[test]
    fn the_high_s_twin_of_a_valid_signature_is_refused() {
        // Finding 6. Both twins recover the same signer, so a verifier that
        // accepts both treats one authorisation as two distinct 65-byte
        // values. The signer emits low-s; so now does the verifier.
        let (secret, address) = derive_from_mnemonic(ABANDON, 0).expect("derive");
        let message = "SuperFrontierLiquid wants you to sign in.";
        let canonical = sign_personal_sign(&secret, message).expect("sign");

        // Twin one: the shipped signature. Accepted, and it recovers the
        // signer.
        let parsed = parse_signature(&canonical).expect("the signer emits low-s");
        assert!(is_canonical_s(&parsed.s), "sign_personal_sign emits low-s");
        assert_eq!(
            recover_personal_sign(message, &canonical).expect("recover"),
            address
        );

        // Twin two: s -> N-s, v flipped. Same signer, different bytes.
        let mut bytes = hex::decode(&canonical[2..]).expect("hex");
        let mut s = [0u8; 32];
        s.copy_from_slice(&bytes[32..64]);
        let high = negate_scalar(&s);
        assert!(!is_canonical_s(&high), "N-s is in the upper half");
        bytes[32..64].copy_from_slice(&high);
        bytes[64] = if bytes[64] == 27 { 28 } else { 27 };
        let twin = format!("0x{}", hex::encode(&bytes));

        assert_ne!(twin, canonical, "the twin really is different bytes");
        assert!(matches!(parse_signature(&twin), Err(EthError::HighS)));
        assert!(
            matches!(recover_personal_sign(message, &twin), Err(EthError::HighS)),
            "the malleable twin must not authenticate"
        );
    }

    #[test]
    fn the_boundary_value_of_s_is_still_canonical() {
        // N is odd, so floor(N/2) is itself a legal low-s value; rejecting it
        // would refuse one signature in 2^255 for no reason.
        assert!(is_canonical_s(&SECP256K1_HALF_ORDER));
        let mut over = SECP256K1_HALF_ORDER;
        over[31] += 1;
        assert!(!is_canonical_s(&over));
    }

    #[test]
    fn malformed_signatures_are_refused() {
        assert!(matches!(
            parse_signature("0x1234"),
            Err(EthError::MalformedSignature(_))
        ));
        assert!(matches!(
            parse_signature("not hex"),
            Err(EthError::MalformedSignature(_))
        ));
        let bad_v = format!("0x{}{}", "00".repeat(64), "07");
        assert!(matches!(
            parse_signature(&bad_v),
            Err(EthError::BadRecoveryId(7))
        ));
    }
}

// ---------------------------------------------------------------------------
// The trading key
// ---------------------------------------------------------------------------

/// HKDF info string, so a secret reused for another purpose derives different
/// material.
const TRADING_KEY_INFO: &[u8] = b"superfrontier-liquid/trading-key/v1";

/// Derive an account's **trading** key from its **login** address.
///
/// ## Why an account has two addresses
///
/// Signing in proves control of an EVM address; it does not hand the gateway a
/// key, so the gateway cannot sign chain transactions as that address. The
/// contract therefore gives every account two: the `login_address` the user
/// holds the key for, and a `trading_address` holding their test money, whose
/// key the gateway derives here.
///
/// `HKDF-SHA256(server_secret, login_address)` has the two properties that
/// matter: it is **deterministic**, so one login address always maps to the
/// same trading account and nothing has to be stored; and it is **one-way**,
/// so a leaked trading address does not reveal the secret or any other
/// account's key.
///
/// Rotating the server secret orphans every trading account. That is why the
/// secret is configuration with no default and the runbook says not to change
/// it.
///
/// The derived scalar is checked against the curve order; the negligible
/// chance of an invalid scalar is handled by advancing a counter rather than
/// by an `unwrap` that would one day fire.
pub fn derive_trading_key(
    server_secret: &[u8],
    login_address: &Address,
) -> Result<(cb_crypto::SecretKey, Address), EthError> {
    for counter in 0u8..=255 {
        let mut info = TRADING_KEY_INFO.to_vec();
        if counter > 0 {
            info.push(counter);
        }
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&login_address.0), server_secret);
        let mut material = [0u8; 32];
        hk.expand(&info, &mut material)
            .map_err(|e| EthError::Derivation(e.to_string()))?;
        if let Ok(secret) = cb_crypto::SecretKey::from_slice(&material) {
            let public =
                cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &secret);
            return Ok((secret, cb_crypto::address_from_pubkey(&public)));
        }
    }
    Err(EthError::Derivation(
        "no valid secp256k1 scalar after 256 attempts".into(),
    ))
}

#[cfg(test)]
mod trading_key_tests {
    use super::*;

    const SECRET: &[u8] = b"a deployment's server secret";

    #[test]
    fn one_login_address_always_maps_to_the_same_trading_account() {
        let login = Address([0x98; 20]);
        let (first_key, first) = derive_trading_key(SECRET, &login).expect("derive");
        let (second_key, second) = derive_trading_key(SECRET, &login).expect("derive");
        assert_eq!(first, second);
        assert_eq!(first_key.secret_bytes(), second_key.secret_bytes());
    }

    #[test]
    fn different_login_addresses_get_different_trading_accounts() {
        let (_k1, a) = derive_trading_key(SECRET, &Address([1; 20])).expect("derive");
        let (_k2, b) = derive_trading_key(SECRET, &Address([2; 20])).expect("derive");
        assert_ne!(a, b);
    }

    #[test]
    fn the_trading_address_is_not_the_login_address() {
        let login = Address([0x98; 20]);
        let (_key, trading) = derive_trading_key(SECRET, &login).expect("derive");
        assert_ne!(trading, login, "the gateway signs for a different account");
    }

    #[test]
    fn rotating_the_server_secret_orphans_every_trading_account() {
        // Stated in the contract, and asserted here so nobody discovers it in
        // production: the same login address under a different secret is a
        // different trading account with different money.
        let login = Address([0x98; 20]);
        let (_k1, before) = derive_trading_key(SECRET, &login).expect("derive");
        let (_k2, after) = derive_trading_key(b"a different secret", &login).expect("derive");
        assert_ne!(before, after);
    }

    #[test]
    fn the_derived_key_actually_controls_the_derived_address() {
        let login = Address([0x77; 20]);
        let (key, trading) = derive_trading_key(SECRET, &login).expect("derive");
        let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key);
        assert_eq!(cb_crypto::address_from_pubkey(&public), trading);
    }
}
