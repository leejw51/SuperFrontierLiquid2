//! Keystore: load a sequencer/validator secp256k1 secret key from disk.
//!
//! On-disk format is a hex-encoded 32-byte secret (with optional "0x" prefix and
//! trailing whitespace). If no key file is provided at startup, callers fall back to
//! a deterministic dev key derived from a seed via `cb_crypto::keypair_from_seed`.

use anyhow::{anyhow, Context};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_types::Address;
use std::path::Path;

/// Load a 32-byte secret from a hex-encoded file. Trailing whitespace and a leading
/// `0x` prefix are tolerated.
pub fn load_key_from_file(path: &Path) -> anyhow::Result<SecretKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read key file {}", path.display()))?;
    let trimmed = raw.trim();
    let stripped = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let bytes = hex::decode(stripped).context("decode hex from key file")?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "key file must contain exactly 32 bytes (got {})",
            bytes.len()
        ));
    }
    SecretKey::from_slice(&bytes).context("invalid secp256k1 secret in key file")
}

/// Deterministic dev key derived from a seed string. Used when `--key-file` is not
/// supplied so `make start` keeps working without an out-of-band keystore. Logs at
/// warn level — never use in production.
pub fn dev_key_from_seed(seed: &str) -> (SecretKey, Address) {
    let (sk, pk) = keypair_from_seed(seed.as_bytes());
    let addr = address_from_pubkey(&pk);
    tracing::warn!(seed, %addr, "no --key-file provided; using deterministic dev key");
    (sk, addr)
}

/// Either load from file or fall back to a dev key for `seed`. Returns `(secret, addr)`
/// so the caller can log / cross-check against the genesis validator set.
pub fn load_or_dev_key(
    key_file: Option<&Path>,
    fallback_seed: &str,
) -> anyhow::Result<(SecretKey, Address)> {
    if let Some(p) = key_file {
        let sk = load_key_from_file(p)?;
        let pk = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &sk);
        let addr = address_from_pubkey(&pk);
        tracing::info!(path = %p.display(), %addr, "loaded sequencer key");
        Ok((sk, addr))
    } else {
        Ok(dev_key_from_seed(fallback_seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn load_hex_with_0x_prefix() {
        let (expected_sk, _) = keypair_from_seed(b"test-seed");
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "0x{}", hex::encode(expected_sk.secret_bytes())).unwrap();
        let loaded = load_key_from_file(f.path()).unwrap();
        assert_eq!(loaded.secret_bytes(), expected_sk.secret_bytes());
    }

    #[test]
    fn load_plain_hex() {
        let (expected_sk, _) = keypair_from_seed(b"another-seed");
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", hex::encode(expected_sk.secret_bytes())).unwrap();
        let loaded = load_key_from_file(f.path()).unwrap();
        assert_eq!(loaded.secret_bytes(), expected_sk.secret_bytes());
    }

    #[test]
    fn rejects_wrong_length() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", hex::encode([0u8; 16])).unwrap();
        let err = load_key_from_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("32 bytes"));
    }

    #[test]
    fn dev_key_is_deterministic() {
        let (sk1, addr1) = dev_key_from_seed("cb-validator-0");
        let (sk2, addr2) = dev_key_from_seed("cb-validator-0");
        assert_eq!(sk1.secret_bytes(), sk2.secret_bytes());
        assert_eq!(addr1, addr2);
    }
}
