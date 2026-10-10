//! secp256k1 sign / verify / recover, plus Ethereum-style address derivation.

use crate::keccak::keccak256;
use cb_types::{Address, H256};
use rand::rngs::OsRng;
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("secp256k1 error: {0}")]
    Secp(#[from] secp256k1::Error),
    #[error("invalid recovery id")]
    RecoveryId,
    #[error("signature must be exactly 65 bytes (got {0})")]
    SignatureLength(usize),
    /// The signature's `s` is in the upper half of the curve order.
    ///
    /// For any valid `(r, s)` the pair `(r, n - s)` with the recovery bit
    /// flipped verifies for the same key and message. Accepting both means one
    /// signed transaction has two encodings and two hashes, which a relayer
    /// can use to put the same transaction in the mempool twice. libsecp256k1
    /// only ever produces the low form, so refusing the high one costs no
    /// honest signer anything.
    #[error("signature s is not in the low half of the curve order")]
    HighS,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    pub recovery_id: u8,
}

impl Signature {
    /// Serialize as the canonical 65-byte form: `r || s || v` where `v` is the recovery id.
    pub fn to_bytes65(&self) -> [u8; 65] {
        let mut out = [0u8; 65];
        out[..32].copy_from_slice(&self.r);
        out[32..64].copy_from_slice(&self.s);
        out[64] = self.recovery_id;
        out
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.to_bytes65().to_vec()
    }

    /// Parse a 65-byte signature. Returns `SignatureLength` if the slice isn't 65 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != 65 {
            return Err(CryptoError::SignatureLength(bytes.len()));
        }
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&bytes[..32]);
        s.copy_from_slice(&bytes[32..64]);
        Ok(Signature {
            r,
            s,
            recovery_id: bytes[64],
        })
    }
}

pub fn generate_keypair() -> (SecretKey, PublicKey) {
    let secp = Secp256k1::new();
    secp.generate_keypair(&mut OsRng)
}

/// Derive a deterministic keypair from `seed`. Re-hashes until a valid scalar is reached
/// (probability of needing a retry is < 2^-127).
pub fn keypair_from_seed(seed: &[u8]) -> (SecretKey, PublicKey) {
    let secp = Secp256k1::new();
    let mut bytes = crate::keccak::keccak256(seed).0;
    loop {
        match SecretKey::from_slice(&bytes) {
            Ok(sk) => {
                let pk = PublicKey::from_secret_key(&secp, &sk);
                return (sk, pk);
            }
            Err(_) => {
                bytes = crate::keccak::keccak256(&bytes).0;
            }
        }
    }
}

pub fn address_from_pubkey(pk: &PublicKey) -> Address {
    let uncompressed = pk.serialize_uncompressed();
    // Skip the 0x04 prefix; hash the 64-byte X||Y.
    let hashed = keccak256(&uncompressed[1..]);
    let mut out = [0u8; 20];
    out.copy_from_slice(&hashed.0[12..]);
    Address(out)
}

pub fn sign(secret: &SecretKey, msg_hash: H256) -> Result<Signature, CryptoError> {
    let secp = Secp256k1::new();
    let msg = Message::from_digest(msg_hash.0);
    let sig: RecoverableSignature = secp.sign_ecdsa_recoverable(&msg, secret);
    let (rec_id, compact) = sig.serialize_compact();
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&compact[..32]);
    s.copy_from_slice(&compact[32..]);
    Ok(Signature {
        r,
        s,
        recovery_id: rec_id.to_i32() as u8,
    })
}

pub fn recover(sig: &Signature, msg_hash: H256) -> Result<Address, CryptoError> {
    let secp = Secp256k1::new();
    let mut compact = [0u8; 64];
    compact[..32].copy_from_slice(&sig.r);
    compact[32..].copy_from_slice(&sig.s);
    let rec_id =
        RecoveryId::from_i32(sig.recovery_id as i32).map_err(|_| CryptoError::RecoveryId)?;
    let rsig = RecoverableSignature::from_compact(&compact, rec_id)?;
    // Low-S rule. `normalize_s` rewrites a high `s` into its low twin and is a
    // no-op otherwise, so "did it change" is exactly "was it high".
    let mut normalized = rsig.to_standard();
    normalized.normalize_s();
    if normalized != rsig.to_standard() {
        return Err(CryptoError::HighS);
    }
    let msg = Message::from_digest(msg_hash.0);
    let pk = secp.recover_ecdsa(&msg, &rsig)?;
    Ok(address_from_pubkey(&pk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_recover_yields_signer_address() {
        let (sk, pk) = generate_keypair();
        let expected = address_from_pubkey(&pk);
        let msg = keccak256(b"hello SuperFrontierLiquid");
        let sig = sign(&sk, msg).unwrap();
        let recovered = recover(&sig, msg).unwrap();
        assert_eq!(recovered, expected);
    }

    #[test]
    fn recover_with_wrong_message_yields_different_address() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let msg_a = keccak256(b"a");
        let msg_b = keccak256(b"b");
        let sig = sign(&sk, msg_a).unwrap();
        let recovered = recover(&sig, msg_b).unwrap();
        assert_ne!(recovered, signer);
    }

    #[test]
    fn address_is_low_20_bytes_of_keccak_pubkey() {
        let (_, pk) = generate_keypair();
        let uncompressed = pk.serialize_uncompressed();
        let h = keccak256(&uncompressed[1..]);
        let mut expected = [0u8; 20];
        expected.copy_from_slice(&h.0[12..]);
        assert_eq!(address_from_pubkey(&pk).0, expected);
    }

    #[test]
    fn signature_bytes_roundtrip() {
        let (sk, _) = generate_keypair();
        let msg = keccak256(b"roundtrip");
        let sig = sign(&sk, msg).unwrap();
        let bytes = sig.to_bytes65();
        let parsed = Signature::from_bytes(&bytes).unwrap();
        assert_eq!(sig, parsed);
    }

    #[test]
    fn a_high_s_twin_of_a_valid_signature_is_rejected() {
        // Build the malleated twin by hand: s' = n - s, recovery bit flipped.
        // It is a valid ECDSA signature for the same key and message, which is
        // exactly why it must not be accepted as a distinct one.
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ];
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let msg = keccak256(b"malleable");
        let sig = sign(&sk, msg).unwrap();
        assert_eq!(recover(&sig, msg).unwrap(), signer, "the low form recovers");

        // n - s, big-endian, with borrow.
        let mut twin_s = [0u8; 32];
        let mut borrow = 0u16;
        for i in (0..32).rev() {
            let n = N[i] as i32;
            let s = sig.s[i] as i32 + borrow as i32;
            let (d, b) = if n >= s { (n - s, 0) } else { (n + 256 - s, 1) };
            twin_s[i] = d as u8;
            borrow = b;
        }
        let twin = Signature {
            r: sig.r,
            s: twin_s,
            recovery_id: sig.recovery_id ^ 1,
        };
        assert!(
            matches!(recover(&twin, msg), Err(CryptoError::HighS)),
            "the high-S twin must be refused, not recovered"
        );
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        assert!(matches!(
            Signature::from_bytes(&[0u8; 64]),
            Err(CryptoError::SignatureLength(64))
        ));
        assert!(matches!(
            Signature::from_bytes(&[0u8; 66]),
            Err(CryptoError::SignatureLength(66))
        ));
    }

    #[test]
    fn address_derivation_matches_the_ethereum_vector_for_private_key_one() {
        // The generator point, hashed the Ethereum way. Pins keccak, the
        // uncompressed-point serialisation and the twelve-byte skip to the
        // scheme every wallet implements, rather than to this crate's own
        // output — the other address tests would pass with SHA-3 swapped in.
        let mut sk_bytes = [0u8; 32];
        sk_bytes[31] = 1;
        let sk = SecretKey::from_slice(&sk_bytes).unwrap();
        let pk = PublicKey::from_secret_key(&Secp256k1::new(), &sk);
        assert_eq!(
            hex::encode(address_from_pubkey(&pk).0),
            "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    #[test]
    fn ethereums_v_of_27_or_28_is_not_a_recovery_id_here() {
        // A signature copied out of an Ethereum tool carries `v` as 27 or 28.
        // This chain's `v` is the raw 0..=3 recovery id; the Ethereum form
        // must be refused, not read modulo something into a valid id.
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let msg = keccak256(b"v");
        let sig = sign(&sk, msg).unwrap();
        for v in [4u8, 27, 28, 255] {
            let bad = Signature {
                recovery_id: v,
                ..sig
            };
            assert!(
                matches!(recover(&bad, msg), Err(CryptoError::RecoveryId)),
                "v={v} must be refused as a recovery id"
            );
        }
        // 2 and 3 are syntactically valid ids that only ever apply when
        // `r` exceeds the curve order, which a fresh signature's never does:
        // the outcome is an error or a stranger, never the signer.
        for v in [2u8, 3] {
            let odd = Signature {
                recovery_id: v,
                ..sig
            };
            if let Ok(addr) = recover(&odd, msg) {
                assert_ne!(addr, signer, "v={v} recovered the real signer");
            }
        }
    }

    #[test]
    fn an_all_zero_signature_is_an_error_not_a_panic() {
        let msg = keccak256(b"zero");
        let zero = Signature {
            r: [0; 32],
            s: [0; 32],
            recovery_id: 0,
        };
        assert!(matches!(recover(&zero, msg), Err(CryptoError::Secp(_))));
    }

    #[test]
    fn a_tampered_r_never_recovers_to_the_signer() {
        let (sk, pk) = generate_keypair();
        let signer = address_from_pubkey(&pk);
        let msg = keccak256(b"tamper");
        let sig = sign(&sk, msg).unwrap();
        for i in [0usize, 15, 31] {
            let mut t = sig;
            t.r[i] ^= 0x01;
            if let Ok(addr) = recover(&t, msg) {
                assert_ne!(
                    addr, signer,
                    "flipping r byte {i} still recovered the signer"
                );
            }
        }
    }

    #[test]
    fn keypair_from_seed_is_deterministic_and_seeds_do_not_collide() {
        // Every dev validator's identity is `keypair_from_seed("cb-validator-N")`.
        // Two nodes booting from the same seed must agree on the key, and two
        // different seeds must not share an address.
        let (a1, pa1) = keypair_from_seed(b"cb-validator-0");
        let (a2, pa2) = keypair_from_seed(b"cb-validator-0");
        assert_eq!(a1, a2);
        assert_eq!(pa1, pa2);
        let (_, pb) = keypair_from_seed(b"cb-validator-1");
        assert_ne!(address_from_pubkey(&pa1), address_from_pubkey(&pb));
        let (_, pc) = keypair_from_seed(b"cb-validator-0 ");
        assert_ne!(
            address_from_pubkey(&pa1),
            address_from_pubkey(&pc),
            "a trailing space is a different validator"
        );
        // And the derived key signs: a seeded validator can vote.
        let msg = keccak256(b"vote");
        assert_eq!(
            recover(&sign(&a1, msg).unwrap(), msg).unwrap(),
            address_from_pubkey(&pa1)
        );
    }
}
