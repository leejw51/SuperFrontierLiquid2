//! Property tests for signing, recovery and the signature encoding.
//!
//! The unit tests in `secp.rs` pin known vectors and one example of each
//! refusal. These ask the same questions over random keys and random messages,
//! because the failure modes of ECDSA code are input-dependent: a recovery id
//! mishandled only when `r` has a particular parity, a low-S check that is right
//! for most `s` and wrong near the half-order, a length check that only bites
//! on one size.
//!
//! What the group proves:
//! - sign → recover returns the signer, for any key and any message;
//! - the signature is bound to its message: recovering against any other
//!   message never yields the signer;
//! - every signature `sign` emits is low-S, and its high-S twin is always
//!   refused, so one signed transaction has exactly one valid encoding;
//! - `Signature::from_bytes` / `to_bytes65` round-trip, and arbitrary bytes
//!   fed to `from_bytes` and `recover` never panic.

use cb_crypto::{
    address_from_pubkey, keccak256, keypair_from_seed, recover, sign, CryptoError, PublicKey,
    SecretKey, Signature,
};
use cb_types::H256;
use proptest::prelude::*;

/// The secp256k1 group order, big-endian.
const N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

/// Half the group order, rounded down: the largest low-S value.
const HALF_N: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

/// `N - s`, big-endian, with borrow.
fn negate_s(s: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = 0i32;
    for i in (0..32).rev() {
        let d = N[i] as i32 - s[i] as i32 - borrow;
        (out[i], borrow) = if d < 0 {
            ((d + 256) as u8, 1)
        } else {
            (d as u8, 0)
        };
    }
    out
}

/// A valid secret key from 32 random bytes. Out-of-range scalars (zero, or at
/// or above the order) are rare enough that rejecting them costs nothing.
fn secret_key() -> impl Strategy<Value = SecretKey> {
    any::<[u8; 32]>().prop_filter_map("not a valid scalar", |b| SecretKey::from_slice(&b).ok())
}

fn message() -> impl Strategy<Value = H256> {
    any::<[u8; 32]>().prop_map(H256)
}

fn signer_of(sk: &SecretKey) -> cb_types::Address {
    address_from_pubkey(&PublicKey::from_secret_key(
        &secp256k1::Secp256k1::new(),
        sk,
    ))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn sign_then_recover_returns_the_signer(sk in secret_key(), msg in message()) {
        let sig = sign(&sk, msg).unwrap();
        prop_assert_eq!(recover(&sig, msg).unwrap(), signer_of(&sk));
    }

    /// Recovery against a different message is either an error or some other
    /// address, never the signer: a signature authorises one message.
    #[test]
    fn a_signature_is_bound_to_its_message(
        sk in secret_key(), msg in message(), other in message(),
    ) {
        prop_assume!(msg != other);
        let sig = sign(&sk, msg).unwrap();
        if let Ok(addr) = recover(&sig, other) {
            prop_assert_ne!(addr, signer_of(&sk));
        }
    }

    /// `sign` only ever emits the low form, and the high twin of what it
    /// emits is always refused rather than recovered as a second encoding.
    #[test]
    fn every_signature_is_low_s_and_its_twin_is_refused(sk in secret_key(), msg in message()) {
        let sig = sign(&sk, msg).unwrap();
        prop_assert!(sig.s <= HALF_N, "sign emitted a high s");
        prop_assert!(sig.recovery_id <= 1, "a fresh signature's id is 0 or 1");
        let twin = Signature { r: sig.r, s: negate_s(&sig.s), recovery_id: sig.recovery_id ^ 1 };
        prop_assert!(matches!(recover(&twin, msg), Err(CryptoError::HighS)));
    }

    /// Any single-bit change to `r`, `s` or the recovery id stops the
    /// signature recovering to the signer.
    #[test]
    fn any_bit_flip_breaks_the_signature(
        sk in secret_key(), msg in message(), bit in 0usize..(65 * 8),
    ) {
        let sig = sign(&sk, msg).unwrap();
        let mut bytes = sig.to_bytes65();
        bytes[bit / 8] ^= 1 << (bit % 8);
        let flipped = Signature::from_bytes(&bytes).unwrap();
        if let Ok(addr) = recover(&flipped, msg) {
            prop_assert_ne!(addr, signer_of(&sk), "flipping bit {} still verified", bit);
        }
    }

    #[test]
    fn the_65_byte_encoding_round_trips(sk in secret_key(), msg in message()) {
        let sig = sign(&sk, msg).unwrap();
        prop_assert_eq!(Signature::from_bytes(&sig.to_bytes65()).unwrap(), sig);
        prop_assert_eq!(sig.to_vec(), sig.to_bytes65().to_vec());
    }

    /// Arbitrary bytes, any length: `from_bytes` refuses every length but 65,
    /// and `recover` on any 65 bytes returns rather than panicking.
    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..80), msg in message(),
    ) {
        match Signature::from_bytes(&bytes) {
            Ok(sig) => {
                prop_assert_eq!(bytes.len(), 65);
                prop_assert_eq!(sig.to_bytes65().to_vec(), bytes);
                let _ = recover(&sig, msg);
            }
            Err(e) => {
                prop_assert!(matches!(e, CryptoError::SignatureLength(n) if n == bytes.len()));
                prop_assert_ne!(bytes.len(), 65);
            }
        }
    }

    /// Seeded keys are a pure function of the seed and sign like any other.
    #[test]
    fn seeded_keys_are_deterministic_and_usable(
        seed in proptest::collection::vec(any::<u8>(), 0..64), msg in message(),
    ) {
        let (a, pa) = keypair_from_seed(&seed);
        let (b, _) = keypair_from_seed(&seed);
        prop_assert_eq!(a, b);
        let sig = sign(&a, msg).unwrap();
        prop_assert_eq!(recover(&sig, msg).unwrap(), address_from_pubkey(&pa));
    }

    /// keccak256 is a function: equal input, equal output, and (over this
    /// many samples) distinct inputs never collide.
    #[test]
    fn keccak_is_deterministic_and_input_sensitive(
        a in proptest::collection::vec(any::<u8>(), 0..200),
        b in proptest::collection::vec(any::<u8>(), 0..200),
    ) {
        prop_assert_eq!(keccak256(&a), keccak256(&a));
        prop_assert_eq!(keccak256(&a) == keccak256(&b), a == b);
    }
}

#[test]
fn the_half_order_constant_is_the_boundary_the_library_uses() {
    // `negate_s` of the largest low-S value is the smallest high-S value, and
    // the two sit either side of the order's midpoint. Pinned so the constants
    // above cannot drift from the curve.
    let low = HALF_N;
    let high = negate_s(&low);
    assert!(high > low);
    let mut plus_one = low;
    for i in (0..32).rev() {
        let (v, carry) = plus_one[i].overflowing_add(1);
        plus_one[i] = v;
        if !carry {
            break;
        }
    }
    assert_eq!(high, plus_one, "N - floor(N/2) = floor(N/2) + 1");
}
