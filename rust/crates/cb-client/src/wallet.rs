//! Wallet derivation + on-disk record.
//!
//! Derivation is **strict BIP-39 + BIP-44** (path `m/44'/60'/0'/0/N`):
//! identical to MetaMask, ethers.js, and any other Ethereum-standard
//! wallet. Importing the same mnemonic into MetaMask and selecting account
//! index `N` will produce the same address this client derives at
//! `(mnemonic, N)`.
//!
//! - Mnemonic: must be 12 / 15 / 18 / 21 / 24 words from the BIP-39 English
//!   wordlist. Anything else is rejected up front.
//! - Seed: PBKDF2-HMAC-SHA512 over the normalized mnemonic with empty
//!   passphrase (no BIP-39 passphrase support — keep the surface small).
//! - Path: `m/44'/60'/0'/0/N` — `N` is the *address index* (MetaMask-style
//!   account discovery).
//!
//! On-disk records are stored in `wallets.jsonl` (see `store.rs`). They
//! include the **private key in plain hex**: this client is for local dev
//! and operator workflows, not custody — `mode 0600` on the file is the
//! only protection.
//!
//! Mnemonic and private-key bytes must NEVER be logged at any tracing level.
//! The store layer enforces 0600 on Unix; this module does not log.

use anyhow::{anyhow, Context, Result};
use cb_crypto::address_from_pubkey;
use cb_types::Address;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};

/// Persisted wallet record. Hex strings are lowercase, **without** the `0x`
/// prefix on disk to keep the JSONL line compact; the display layer adds
/// `0x` back in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletEntry {
    pub label: String,
    pub mnemonic: String,
    pub index: u32,
    /// Lowercase 40-char hex. (Address.0 -> hex)
    pub address: String,
    /// Lowercase 130-char hex of the uncompressed pubkey.
    pub pubkey: String,
    /// Lowercase 64-char hex of the secret scalar. Treat as secret.
    pub privkey: String,
}

impl WalletEntry {
    /// Derive a wallet from a BIP-39 mnemonic and a BIP-44 address index.
    ///
    /// Returns `Err` if the mnemonic is not a valid BIP-39 phrase (wrong
    /// word count, unknown wordlist token, bad checksum) or if the
    /// resulting BIP-32 child key happens to fall outside the secp256k1
    /// scalar field (statistically negligible but typed for safety).
    pub fn derive(label: impl Into<String>, mnemonic: impl AsRef<str>, index: u32) -> Result<Self> {
        let label = label.into();
        let mnemonic_raw = mnemonic.as_ref();
        let (sk, pk, mnemonic_canon) = derive_keypair(mnemonic_raw, index)?;
        let addr = address_from_pubkey(&pk);
        Ok(WalletEntry {
            label,
            mnemonic: mnemonic_canon,
            index,
            address: hex::encode(addr.0),
            pubkey: hex::encode(pk.serialize_uncompressed()),
            privkey: hex::encode(sk.secret_bytes()),
        })
    }

    pub fn address(&self) -> Result<Address> {
        let bytes = hex::decode(&self.address).context("decode wallet address hex")?;
        if bytes.len() != 20 {
            return Err(anyhow!("address hex must decode to 20 bytes"));
        }
        let mut a = [0u8; 20];
        a.copy_from_slice(&bytes);
        Ok(Address(a))
    }

    pub fn secret_key(&self) -> Result<SecretKey> {
        let bytes = hex::decode(&self.privkey).context("decode wallet privkey hex")?;
        SecretKey::from_slice(&bytes).context("invalid secp256k1 secret")
    }

    /// Mask the privkey for display — show the first 4 hex chars then ellipsis.
    pub fn privkey_masked(&self) -> String {
        if self.privkey.len() < 4 {
            return "....".into();
        }
        format!("{}...", &self.privkey[..4])
    }

    /// Short pubkey prefix for table display.
    pub fn pubkey_prefix(&self) -> String {
        if self.pubkey.len() < 8 {
            self.pubkey.clone()
        } else {
            format!("0x{}...", &self.pubkey[..8])
        }
    }
}

/// Derive `(SecretKey, PublicKey, canonical-mnemonic)` from a BIP-39 phrase
/// and a BIP-44 address index using `m/44'/60'/0'/0/{index}`. Returns the
/// trimmed mnemonic alongside so callers can persist a normalized form.
///
/// Public so tests can pin determinism without going through the JSONL
/// serialization.
///
/// Implementation note: we use the `bip39` crate for the wordlist + seed
/// derivation (PBKDF2-HMAC-SHA512), then `bip32` for the BIP-32 path
/// walk. `bip32::Mnemonic` in 0.5 only accepts 24-word phrases, which
/// would silently break MetaMask import for 12/15/18/21-word mnemonics —
/// hence the split.
pub fn derive_keypair(mnemonic: &str, index: u32) -> Result<(SecretKey, PublicKey, String)> {
    use bip32::{DerivationPath, XPrv};
    use bip39::{Language, Mnemonic};
    let trimmed = mnemonic.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "invalid BIP-39 mnemonic: empty \
             (expected 12, 15, 18, 21, or 24 words from the BIP-39 English wordlist)"
        ));
    }
    let m = Mnemonic::parse_in_normalized(Language::English, trimmed).map_err(|e| {
        anyhow!(
            "invalid BIP-39 mnemonic: {e} \
             (expected 12, 15, 18, 21, or 24 words from the BIP-39 English wordlist)"
        )
    })?;
    // Empty BIP-39 passphrase — keep the surface small. Users who want a
    // passphrase can derive an extra word out-of-band.
    let seed = m.to_seed_normalized("");
    let path: DerivationPath = format!("m/44'/60'/0'/0/{index}")
        .parse()
        .expect("static derivation path is well-formed");
    let xprv = XPrv::derive_from_path(seed, &path)
        .map_err(|e| anyhow!("BIP-32 derivation failed: {e}"))?;
    let sk = SecretKey::from_slice(&xprv.private_key().to_bytes())
        .map_err(|e| anyhow!("derived bytes are not a valid secp256k1 scalar: {e}"))?;
    let secp = Secp256k1::new();
    let pk = PublicKey::from_secret_key(&secp, &sk);
    // Re-emit the canonical, single-spaced normalized form so the persisted
    // mnemonic round-trips byte-for-byte through the same parser.
    let canonical = m.words().collect::<Vec<_>>().join(" ");
    Ok((sk, pk, canonical))
}
