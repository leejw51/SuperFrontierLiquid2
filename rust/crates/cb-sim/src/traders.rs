//! Deterministic synthetic traders. Keys derived from `keccak256("{seed_prefix}{i}")` so the
//! addresses match the ones funded at genesis time (see cb-genesis::SimSpec).

use cb_crypto::{address_from_pubkey, keypair_from_seed};
use cb_types::Address;
use secp256k1::{PublicKey, SecretKey};

pub struct Trader {
    pub index: u32,
    pub addr: Address,
    pub sk: SecretKey,
    #[allow(dead_code)]
    pub pk: PublicKey,
    /// Locally tracked nonce. Bumped after each submit; the sequencer rejects any tx whose
    /// nonce doesn't match its own view, so we can't share traders across simulator instances.
    pub nonce: u64,
}

pub struct TraderSet {
    pub traders: Vec<Trader>,
}

impl TraderSet {
    pub fn derive(seed_prefix: &str, n: u32) -> Self {
        Self::derive_range(seed_prefix, 0, n)
    }

    /// Traders `offset .. offset + n`. Parallel simulator processes each take
    /// a disjoint range, since a trader's nonce cannot be shared between them.
    pub fn derive_range(seed_prefix: &str, offset: u32, n: u32) -> Self {
        let traders = (offset..offset + n)
            .map(|i| {
                let seed = format!("{seed_prefix}{i}");
                let (sk, pk) = keypair_from_seed(seed.as_bytes());
                Trader {
                    index: i,
                    addr: address_from_pubkey(&pk),
                    sk,
                    pk,
                    nonce: 0,
                }
            })
            .collect();
        Self { traders }
    }

    pub fn len(&self) -> usize {
        self.traders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.traders.is_empty()
    }
}
