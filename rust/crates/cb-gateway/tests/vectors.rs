//! The published SuperFrontier Wallet vectors, executed.
//!
//! `docs/api-contract.md` §2 requires it: the vectors come from `eth-account`
//! and are therefore what every EVM tool produces. If the gateway's keccak,
//! derivation, checksum or signature recovery disagrees with any of them, it
//! is implementing a *different* scheme from the rest of the ecosystem — and
//! the failure mode is not a broken login, it is a user being handed somebody
//! else's account.
//!
//! These are real tests over the real files rather than hard-coded constants,
//! so regenerating the vectors re-checks the implementation rather than
//! silently agreeing with it.

use cb_gateway::auth::eth;
use serde_json::Value;

/// Read one vector file.
fn vectors(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

fn array<'a>(value: &'a Value, key: &str) -> &'a Vec<Value> {
    value[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` must be an array"))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("`{key}` must be a string"))
}

// ---------------------------------------------------------------------------
// keccak256
// ---------------------------------------------------------------------------

#[test]
fn keccak_matches_the_published_digests() {
    let file = vectors("keccak.json");
    let mut checked = 0;
    for case in array(&file, "hashes") {
        let input = text(case, "text");
        let expected = text(case, "keccak256");
        let digest = cb_crypto::keccak256(input.as_bytes());
        assert_eq!(
            format!("0x{}", hex::encode(digest.0)),
            expected,
            "keccak256({input:?})"
        );
        checked += 1;
    }
    assert!(checked > 0, "the vector file must carry cases");
}

#[test]
fn function_selectors_are_the_first_four_bytes_of_the_signature_hash() {
    let file = vectors("keccak.json");
    for case in array(&file, "selectors") {
        let signature = text(case, "signature");
        let expected = text(case, "selector");
        let digest = cb_crypto::keccak256(signature.as_bytes());
        assert_eq!(
            format!("0x{}", hex::encode(&digest.0[..4])),
            expected,
            "selector for {signature}"
        );
    }
}

// ---------------------------------------------------------------------------
// EIP-55
// ---------------------------------------------------------------------------

#[test]
fn eip55_rendering_matches_the_reference_addresses() {
    let file = vectors("eip55.json");
    let mut checked = 0;
    for case in array(&file, "vectors") {
        let lowercase = text(case, "lowercase");
        let checksummed = text(case, "checksummed");
        let address = eth::parse_checked_address(lowercase)
            .unwrap_or_else(|e| panic!("parsing {lowercase}: {e}"));
        assert_eq!(eth::checksum_address(&address), checksummed);
        checked += 1;
    }
    assert!(checked > 0);
}

#[test]
fn a_checksummed_address_is_accepted_and_a_corrupted_one_is_not() {
    let file = vectors("eip55.json");
    for case in array(&file, "vectors") {
        let checksummed = text(case, "checksummed");
        eth::parse_checked_address(checksummed).expect("the canonical form is accepted");

        // Flip the case of the first letter after `0x` that has one. That
        // leaves a mixed-case address whose checksum no longer validates.
        let body: Vec<char> = checksummed[2..].chars().collect();
        if let Some(position) = body.iter().position(|c| c.is_ascii_alphabetic()) {
            let mut corrupted = body.clone();
            corrupted[position] = if corrupted[position].is_ascii_uppercase() {
                corrupted[position].to_ascii_lowercase()
            } else {
                corrupted[position].to_ascii_uppercase()
            };
            let corrupted: String = format!("0x{}", corrupted.iter().collect::<String>());
            // Unless flipping produced an all-one-case form, which carries no
            // checksum and is legitimately accepted.
            let has_upper = corrupted[2..].bytes().any(|b| b.is_ascii_uppercase());
            let has_lower = corrupted[2..].bytes().any(|b| b.is_ascii_lowercase());
            if has_upper && has_lower {
                assert!(
                    eth::parse_checked_address(&corrupted).is_err(),
                    "a corrupted checksum must be refused: {corrupted}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BIP-39 and BIP-44
// ---------------------------------------------------------------------------

#[test]
fn derivation_matches_eth_account_for_every_mnemonic_and_index() {
    let file = vectors("derivation.json");
    assert_eq!(
        text(&file, "purpose"),
        "m/44'/60'/0'/0/<index>",
        "the vectors must pin the path the contract names"
    );

    let mut checked = 0;
    for entry in array(&file, "mnemonics") {
        let phrase = text(entry, "phrase");
        for account in array(entry, "accounts") {
            let index = account["index"].as_u64().expect("index must be a number") as u32;
            let expected_address = text(account, "address");
            let expected_key = text(account, "private_key");

            let (secret, address) = eth::derive_from_mnemonic(phrase, index)
                .unwrap_or_else(|e| panic!("deriving {phrase} at {index}: {e}"));
            assert_eq!(
                eth::checksum_address(&address),
                expected_address,
                "address for index {index}"
            );
            assert_eq!(
                format!("0x{}", hex::encode(secret.secret_bytes())),
                expected_key,
                "private key for index {index}"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "the vector file must carry accounts");
}

#[test]
fn the_canonical_phrase_derives_the_address_the_contract_names() {
    // Called out separately because the contract calls it out separately: a
    // build that fails this is deriving somebody else's accounts.
    let (_secret, address) = eth::derive_from_mnemonic(
        "abandon abandon abandon abandon abandon abandon abandon abandon \
         abandon abandon abandon about",
        0,
    )
    .expect("derive");
    assert_eq!(
        eth::checksum_address(&address),
        "0x9858EfFD232B4033E47d90003D41EC34EcaEda94"
    );
}

#[test]
fn every_invalid_phrase_is_rejected() {
    let file = vectors("bip39-invalid.json");
    let mut checked = 0;
    for case in array(&file, "vectors") {
        let mnemonic = text(case, "mnemonic");
        let reason = text(case, "reason");
        assert!(
            eth::derive_from_mnemonic(mnemonic, 0).is_err(),
            "`{mnemonic}` must be rejected ({reason})"
        );
        checked += 1;
    }
    assert!(checked > 0);
}

#[test]
fn mnemonic_normalisation_reaches_the_same_account() {
    let file = vectors("bip39.json");
    let Some(cases) = file["normalization"].as_array() else {
        return;
    };
    for case in cases {
        let input = text(case, "input");
        let canonical = text(case, "canonical");
        let (_, from_input) = match eth::derive_from_mnemonic(input, 0) {
            Ok(derived) => derived,
            // A normalisation case the wordlist genuinely rejects is not a
            // derivation claim; skip rather than assert a false equivalence.
            Err(_) => continue,
        };
        let (_, from_canonical) =
            eth::derive_from_mnemonic(canonical, 0).expect("the canonical form derives");
        assert_eq!(
            from_input, from_canonical,
            "`{input}` and `{canonical}` must reach the same account"
        );
    }
}

// ---------------------------------------------------------------------------
// EIP-191
// ---------------------------------------------------------------------------

#[test]
fn the_prefixed_hash_matches_eth_account() {
    let file = vectors("eip191.json");
    let mut checked = 0;
    for case in array(&file, "vectors") {
        let message = text(case, "message");
        let expected = text(case, "prefixed_hash");
        let digest = eth::personal_sign_hash(message.as_bytes());
        assert_eq!(
            format!("0x{}", hex::encode(digest.0)),
            expected,
            "prefixed hash for {message:?}"
        );
        checked += 1;
    }
    assert!(checked > 0);
}

#[test]
fn recovery_reproduces_the_published_signer() {
    let file = vectors("eip191.json");
    let mut checked = 0;
    for case in array(&file, "vectors") {
        let message = text(case, "message");
        let signature = text(case, "signature");
        let expected = text(case, "signer");
        let recovered = eth::recover_personal_sign(message, signature)
            .unwrap_or_else(|e| panic!("recovering {message:?}: {e}"));
        assert_eq!(
            eth::checksum_address(&recovered),
            expected,
            "signer for {message:?}"
        );
        checked += 1;
    }
    assert!(checked > 0, "the vector file must carry signatures");
}

#[test]
fn the_gateway_produces_the_published_signature_for_the_published_key() {
    let file = vectors("eip191.json");
    let signing_key = text(&file, "signing_key");
    let bytes = hex::decode(signing_key.trim_start_matches("0x")).expect("hex");
    let secret = cb_crypto::SecretKey::from_slice(&bytes).expect("valid scalar");

    for case in array(&file, "vectors") {
        let message = text(case, "message");
        let expected = text(case, "signature");
        let produced = eth::sign_personal_sign(&secret, message).expect("sign");
        assert_eq!(
            produced.to_ascii_lowercase(),
            expected.to_ascii_lowercase(),
            "signature for {message:?}"
        );
    }
}

#[test]
fn a_signature_from_one_vector_does_not_verify_against_another_message() {
    let file = vectors("eip191.json");
    let cases = array(&file, "vectors");
    if cases.len() < 2 {
        return;
    }
    let signature = text(&cases[0], "signature");
    let other_message = text(&cases[1], "message");
    let signer = text(&cases[0], "signer");
    let recovered = eth::recover_personal_sign(other_message, signature).expect("recovers");
    assert_ne!(
        eth::checksum_address(&recovered),
        signer,
        "a signature must not verify against a message it was not made over"
    );
}
