//! Node identity and chain identity.
//!
//! Two identities travel in the handshake and they answer different questions.
//!
//! * [`NodeId`] answers "who are you?". It is the Ethereum-style address of the
//!   node's secp256k1 *network* key, which is deliberately distinct from the
//!   validator key: a non-validating observer still needs a stable identity so
//!   the peer table can suppress duplicate connections and self-connections.
//!   Deriving it from a key (rather than handing out a random UUID) means a
//!   peer can prove possession, so a node id cannot be borrowed just by
//!   claiming it.
//!
//! * [`GenesisId`] answers "are we on the same chain?". It is a fingerprint of
//!   the chain's founding facts read straight out of the local database.
//!
//! ## Why the genesis fingerprint is derived, not stored
//!
//! The strongest possible answer would be the state root immediately after
//! genesis was applied, which covers every initial balance. That value is only
//! observable at height 0 and would need a new `Meta` key to survive; `cb-state`
//! is owned by another workstream this cycle, so persisting one is out of
//! bounds here. The derived fingerprint below uses facts that are written once
//! by genesis and never mutated afterwards — chain id, treasury address,
//! community wallet address, and the ordered validator set with weights — so
//! every honest node on a chain computes the same value at every height,
//! with no migration and no new storage.
//!
//! What it trades away: it does **not** cover the initial balance sheet. Two
//! chains that share a validator set and treasury but were funded differently
//! would hand each other the same `GenesisId` and only diverge later, at the
//! first state-root comparison. That is exactly the "discovered later" failure
//! mode the handshake exists to shrink, and it is shrunk, not eliminated. When
//! a genesis state root becomes available in `Meta`, fold it into
//! [`GenesisId::from_parts`] and the wire format does not change.

use cb_crypto::{address_from_pubkey, keccak256, PublicKey};
use cb_state::{Database, Meta, Validators};
use cb_types::{Address, H256};

/// Stable network identity of a node: the address of its secp256k1 network key.
///
/// Printing a `NodeId` prints the underlying address, so log lines correlate
/// with the addresses used everywhere else in the system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub Address);

impl NodeId {
    /// The all-zero node id. Never produced by a real key; used as the
    /// "unknown peer" placeholder before a handshake completes.
    pub const UNKNOWN: NodeId = NodeId(Address::ZERO);

    /// Derive the node id from the public half of the node's network key.
    pub fn from_pubkey(pk: &PublicKey) -> Self {
        NodeId(address_from_pubkey(pk))
    }

    /// The 20 identity bytes, for hashing and wire encoding.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0 .0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Fingerprint of the chain a node believes it is on.
///
/// Mismatched fingerprints are a connect-time rejection: see the module header
/// for what the fingerprint covers and what it deliberately does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenesisId(pub H256);

/// Domain separator for the genesis fingerprint preimage. Changing this string
/// partitions the network, so it is frozen.
const GENESIS_DOMAIN: &[u8] = b"CB-GENESIS\0";

impl GenesisId {
    /// Compute the fingerprint from explicit parts.
    ///
    /// `validators` must be supplied in the canonical genesis order (the order
    /// `Validators::list` returns, which is the order genesis wrote them);
    /// leader election already depends on that order, so reusing it keeps one
    /// notion of "the validator set" rather than inventing a second.
    pub fn from_parts(
        chain_id: u32,
        company: Address,
        community: Address,
        validators: &[(Address, u64)],
    ) -> Self {
        let mut buf = Vec::with_capacity(GENESIS_DOMAIN.len() + 4 + 40 + validators.len() * 28 + 8);
        buf.extend_from_slice(GENESIS_DOMAIN);
        buf.extend_from_slice(&chain_id.to_be_bytes());
        buf.extend_from_slice(&company.0);
        buf.extend_from_slice(&community.0);
        buf.extend_from_slice(&(validators.len() as u64).to_be_bytes());
        for (addr, weight) in validators {
            buf.extend_from_slice(&addr.0);
            buf.extend_from_slice(&weight.to_be_bytes());
        }
        GenesisId(keccak256(&buf))
    }

    /// Compute the fingerprint from an initialized database.
    pub fn from_db(db: &Database) -> Self {
        let meta = Meta::new(db);
        let validators: Vec<(Address, u64)> = Validators::new(db)
            .list()
            .into_iter()
            .map(|v| (v.addr, v.weight))
            .collect();
        Self::from_parts(
            meta.chain_id(),
            meta.company_addr(),
            meta.community_wallet_addr(),
            &validators,
        )
    }
}

impl std::fmt::Display for GenesisId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_id_is_order_and_content_sensitive() {
        let a = Address([1; 20]);
        let b = Address([2; 20]);
        let base = GenesisId::from_parts(1, a, b, &[(a, 1), (b, 1)]);
        assert_ne!(base, GenesisId::from_parts(2, a, b, &[(a, 1), (b, 1)]));
        assert_ne!(base, GenesisId::from_parts(1, b, a, &[(a, 1), (b, 1)]));
        assert_ne!(base, GenesisId::from_parts(1, a, b, &[(b, 1), (a, 1)]));
        assert_ne!(base, GenesisId::from_parts(1, a, b, &[(a, 2), (b, 1)]));
        assert_eq!(base, GenesisId::from_parts(1, a, b, &[(a, 1), (b, 1)]));
    }

    #[test]
    fn node_id_follows_the_key() {
        let (_, pk1) = cb_crypto::generate_keypair();
        let (_, pk2) = cb_crypto::generate_keypair();
        assert_eq!(NodeId::from_pubkey(&pk1), NodeId::from_pubkey(&pk1));
        assert_ne!(NodeId::from_pubkey(&pk1), NodeId::from_pubkey(&pk2));
    }
}
