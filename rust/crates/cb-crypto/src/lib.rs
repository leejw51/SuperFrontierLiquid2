//! Cryptographic primitives: keccak256 and secp256k1 (Ethereum-style).

pub mod keccak;
pub mod secp;

pub use keccak::keccak256;
pub use secp::{
    address_from_pubkey, generate_keypair, keypair_from_seed, recover, sign, CryptoError, Signature,
};

// Re-exports for callers that need to handle keypair material directly.
pub use secp256k1::{PublicKey, SecretKey};
