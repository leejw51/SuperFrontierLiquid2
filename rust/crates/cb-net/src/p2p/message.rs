//! The peer-to-peer message set.
//!
//! This is the *logical* protocol: an enum the engine matches on. The wire
//! encoding lives in [`crate::p2p::codec`] and is a separate concern on
//! purpose, so the state machine can be tested exhaustively in-process without
//! a socket, a codec, or a byte anywhere near it.
//!
//! ## Announce, then fetch
//!
//! Blocks and transactions are never pushed as bodies. A node announces a hash
//! and a peer that does not already have it asks for the body. At the
//! throughput this chain targets, a block body is the dominant object on the
//! wire; pushing it to every peer multiplies the leader's egress by the peer
//! count for data most peers already have. A 32-byte announce costs a round
//! trip in exchange for not sending a megabyte twice.
//!
//! What it trades away: latency. The first hop of a block now costs
//! announce -> request -> body instead of a single push. For a chain whose
//! block interval is measured in tens of milliseconds and whose blocks are
//! measured in hundreds of kilobytes, bandwidth is the scarcer resource.
//!
//! ## Bodies are opaque bytes
//!
//! `blockBody` and `txBody` carry the canonical `bincode` encodings already
//! used by `Block::encode` and the mempool. The gossip layer never interprets
//! them; it hands them to the verifier and learns the verdict. That is what
//! keeps the "adopted only after re-execution reproduces the state root" rule
//! enforceable — the network layer has no code path that could adopt a block,
//! because it cannot even parse one.

use crate::p2p::identity::{GenesisId, NodeId};
use cb_types::H256;
use std::net::SocketAddr;

/// A peer's address as gossiped in a `peers` response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerAddr {
    /// The identity that address claimed at handshake, so a receiver can
    /// suppress a dial to a node it is already connected to.
    pub node_id: NodeId,
    /// Where to dial.
    pub addr: SocketAddr,
}

/// The handshake a node sends immediately on connect, in both directions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Protocol version; see [`crate::p2p::config::PROTOCOL_VERSION`].
    pub protocol: u32,
    /// Chain id, which must equal `Meta::chain_id()` — the same value the
    /// executor checks every transaction against, so a peer accepted here
    /// cannot be one whose transactions we would all reject.
    pub chain_id: u32,
    /// Chain fingerprint; see [`GenesisId`].
    pub genesis: GenesisId,
    /// Claimed identity, proven by `sig`.
    pub node_id: NodeId,
    /// Human-readable build string, logged but never used for a decision.
    pub software: String,
    /// The sender's head height, so the receiver knows immediately whether
    /// this peer can serve a catch-up range.
    pub head_height: u64,
    /// Address the sender accepts inbound connections on, for discovery.
    /// `None` when the sender is outbound-only.
    pub listen: Option<SocketAddr>,
    /// Random per-connection value. Included in the signed preimage so two
    /// connections from the same node do not present byte-identical proofs.
    pub nonce: u64,
    /// 65-byte secp256k1 signature over [`hello_signing_bytes`].
    pub sig: Vec<u8>,
}

/// Canonical preimage a node signs in its `hello`.
///
/// Binding the chain id and genesis fingerprint into the signature means a
/// `hello` captured on one chain cannot be replayed onto another.
///
/// Known limitation, stated plainly: the nonce is chosen by the *sender*, so
/// this proves key possession but not freshness — a captured `hello` can be
/// replayed to a third party within the same chain. Closing that needs a
/// receiver-chosen challenge, which is a second round trip the current
/// threat model does not pay for: the only thing an impersonator gains is a
/// peer-table slot under someone else's name, and the block and transaction
/// paths derive no trust from a node id whatsoever.
pub fn hello_signing_bytes(
    protocol: u32,
    chain_id: u32,
    genesis: GenesisId,
    head_height: u64,
    nonce: u64,
) -> H256 {
    let mut buf = Vec::with_capacity(11 + 4 + 4 + 32 + 8 + 8);
    buf.extend_from_slice(b"CB-P2P-HELLO\0");
    buf.extend_from_slice(&protocol.to_be_bytes());
    buf.extend_from_slice(&chain_id.to_be_bytes());
    buf.extend_from_slice(&genesis.0 .0);
    buf.extend_from_slice(&head_height.to_be_bytes());
    buf.extend_from_slice(&nonce.to_be_bytes());
    cb_crypto::keccak256(&buf)
}

impl Hello {
    /// Sign a `hello` with the node's network key.
    #[allow(clippy::too_many_arguments)]
    pub fn signed(
        key: &cb_crypto::SecretKey,
        protocol: u32,
        chain_id: u32,
        genesis: GenesisId,
        software: String,
        head_height: u64,
        listen: Option<SocketAddr>,
        nonce: u64,
    ) -> Result<Self, cb_crypto::CryptoError> {
        let digest = hello_signing_bytes(protocol, chain_id, genesis, head_height, nonce);
        let sig = cb_crypto::sign(key, digest)?.to_vec();
        let pk = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), key);
        Ok(Hello {
            protocol,
            chain_id,
            genesis,
            node_id: NodeId::from_pubkey(&pk),
            software,
            head_height,
            listen,
            nonce,
            sig,
        })
    }

    /// Check that `sig` recovers to `node_id`. A `hello` that fails this is
    /// claiming an identity it cannot prove.
    pub fn verify_identity(&self) -> bool {
        let digest = hello_signing_bytes(
            self.protocol,
            self.chain_id,
            self.genesis,
            self.head_height,
            self.nonce,
        );
        match cb_crypto::Signature::from_bytes(&self.sig) {
            Ok(sig) => match cb_crypto::recover(&sig, digest) {
                Ok(addr) => NodeId(addr) == self.node_id,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }
}

/// Why a connection is being closed. Sent to the peer before the socket drops
/// so the operator on the other end reads a reason instead of guessing at a
/// reset connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The peer's `chain_id` is not ours.
    ChainIdMismatch,
    /// The peer's genesis fingerprint is not ours.
    GenesisMismatch,
    /// The peer speaks a different protocol version.
    VersionMismatch,
    /// The peer's node id is our own — we dialed ourselves.
    SelfConnection,
    /// We already hold a connection to this node id.
    DuplicateConnection,
    /// The peer's `hello` signature did not recover to its claimed node id.
    BadIdentity,
    /// The peer is serving a ban.
    Banned,
    /// The peer's score fell to the ban threshold during this connection.
    Misbehaving,
    /// No room in the peer table.
    TooManyPeers,
    /// The peer broke the message state machine.
    ProtocolViolation,
    /// The peer stopped responding.
    Timeout,
    /// Local shutdown.
    Shutdown,
}

impl DisconnectReason {
    /// A short, stable, human-readable reason, suitable for logs and for the
    /// wire. Stable because operators grep it.
    pub fn as_str(&self) -> &'static str {
        match self {
            DisconnectReason::ChainIdMismatch => "chain id mismatch",
            DisconnectReason::GenesisMismatch => "genesis mismatch",
            DisconnectReason::VersionMismatch => "protocol version mismatch",
            DisconnectReason::SelfConnection => "self connection",
            DisconnectReason::DuplicateConnection => "duplicate connection",
            DisconnectReason::BadIdentity => "bad identity proof",
            DisconnectReason::Banned => "banned",
            DisconnectReason::Misbehaving => "misbehaving",
            DisconnectReason::TooManyPeers => "too many peers",
            DisconnectReason::ProtocolViolation => "protocol violation",
            DisconnectReason::Timeout => "timeout",
            DisconnectReason::Shutdown => "shutdown",
        }
    }
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One protocol message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Handshake. Legal only as the first message in each direction.
    Hello(Hello),
    /// Closing, with a reason.
    Disconnect(DisconnectReason),
    /// "Who else do you know?"
    GetPeers,
    /// Answer to `GetPeers`.
    Peers(Vec<PeerAddr>),
    /// "I have this block." The height lets a receiver that is already past
    /// it discard the announce without a round trip.
    BlockAnnounce {
        /// Hash of the block.
        hash: H256,
        /// Its height.
        height: u64,
    },
    /// "Send me that block."
    GetBlock {
        /// Hash of the block wanted.
        hash: H256,
    },
    /// A block body: the canonical `Block::encode` bytes.
    BlockBody {
        /// Height the body claims.
        height: u64,
        /// Canonical `Block::encode` bytes.
        bytes: Vec<u8>,
    },
    /// "I have this transaction."
    TxAnnounce {
        /// Hash of the transaction.
        hash: H256,
    },
    /// "Send me that transaction."
    GetTx {
        /// Hash of the transaction wanted.
        hash: H256,
    },
    /// A transaction body: the canonical `SignedTx` bincode bytes.
    TxBody {
        /// Canonical `SignedTx` bincode bytes.
        bytes: Vec<u8>,
    },
    /// "Send me heights `from..=to`." Catch-up, addressed to any peer that
    /// claims a head at least `to`.
    GetBlockRange {
        /// First height wanted, inclusive.
        from: u64,
        /// Last height wanted, inclusive.
        to: u64,
    },
    /// Answer to `GetBlockRange`, ascending by height, possibly short of what
    /// was asked for if the peer has pruned.
    BlockRange {
        /// `(height, canonical bytes)` pairs, ascending.
        blocks: Vec<(u64, Vec<u8>)>,
    },
    /// Liveness probe.
    Ping {
        /// Value the peer must echo.
        nonce: u64,
    },
    /// Liveness reply, echoing the probe's nonce.
    Pong {
        /// The echoed value.
        nonce: u64,
    },
    /// A consensus-protocol message between validators. Opaque here: the
    /// p2p layer delivers it to the node's consensus driver and does not
    /// verify, store or forward it.
    Consensus {
        /// `cb_consensus::bft::Message` bincode bytes.
        bytes: Vec<u8>,
    },
}

impl Message {
    /// A short label for logs and metrics that does not print payload bytes.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::Hello(_) => "hello",
            Message::Disconnect(_) => "disconnect",
            Message::GetPeers => "getPeers",
            Message::Peers(_) => "peers",
            Message::BlockAnnounce { .. } => "blockAnnounce",
            Message::GetBlock { .. } => "getBlock",
            Message::BlockBody { .. } => "blockBody",
            Message::TxAnnounce { .. } => "txAnnounce",
            Message::GetTx { .. } => "getTx",
            Message::TxBody { .. } => "txBody",
            Message::GetBlockRange { .. } => "getBlockRange",
            Message::BlockRange { .. } => "blockRange",
            Message::Ping { .. } => "ping",
            Message::Pong { .. } => "pong",
            Message::Consensus { .. } => "consensus",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_signature_recovers_to_node_id() {
        let (sk, pk) = cb_crypto::generate_keypair();
        let genesis = GenesisId(H256([9; 32]));
        let hello = Hello::signed(&sk, 1, 7, genesis, "test".into(), 42, None, 1234).expect("sign");
        assert_eq!(hello.node_id, NodeId::from_pubkey(&pk));
        assert!(hello.verify_identity());
    }

    #[test]
    fn hello_with_a_borrowed_node_id_fails_verification() {
        let (sk, _) = cb_crypto::generate_keypair();
        let (_, other_pk) = cb_crypto::generate_keypair();
        let mut hello = Hello::signed(
            &sk,
            1,
            7,
            GenesisId(H256([9; 32])),
            "test".into(),
            42,
            None,
            1234,
        )
        .expect("sign");
        hello.node_id = NodeId::from_pubkey(&other_pk);
        assert!(!hello.verify_identity());
    }

    #[test]
    fn hello_signed_for_one_chain_does_not_verify_on_another() {
        let (sk, _) = cb_crypto::generate_keypair();
        let mut hello = Hello::signed(
            &sk,
            1,
            7,
            GenesisId(H256([9; 32])),
            "test".into(),
            42,
            None,
            1234,
        )
        .expect("sign");
        hello.chain_id = 8;
        assert!(!hello.verify_identity());
    }
}
