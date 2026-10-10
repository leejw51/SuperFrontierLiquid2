//! Peer-to-peer networking: handshake, discovery, gossip, catch-up, scoring.
//!
//! # What changed, and why
//!
//! The node used to run a star. A replica dialed exactly one configured
//! sequencer address, streamed blocks from it, and voted back. That topology
//! has three problems that no amount of tuning fixes: the leader's egress
//! scales with the replica count, the leader is a single point of failure for
//! *sync* as well as for production, and a replica that falls behind has
//! exactly one place to catch up from.
//!
//! This module replaces it with a mesh. Nodes find each other from seeds plus
//! peer exchange, blocks and transactions propagate peer to peer by
//! announcement, and a node that fell behind asks whichever peer is furthest
//! ahead for the heights it is missing.
//!
//! # The invariant none of this is allowed to weaken
//!
//! A block is adopted only after re-execution reproduces the state root in its
//! header. Gossip changes *where* bytes come from; it changes nothing about
//! what makes them a block. The architecture enforces this rather than
//! documenting it: [`engine::P2pEngine`] has no database, no executor and no
//! decoder, so there is no code path in it that could adopt a block even if
//! someone wanted one. The only way a block lands is for the driver to call
//! [`chain::ChainAccess::verify_and_apply`], which on a real node is
//! [`crate::apply_remote_block`].
//!
//! That is also why catching up from an untrusted peer is safe, and why it is
//! the right design rather than a compromise: a peer that serves a bad block
//! is caught by the verification that already existed, and is then scored
//! down and eventually banned for wasting our CPU.
//!
//! # Layering
//!
//! * [`config`] — every threshold, in one place, loadable from TOML.
//! * [`identity`] — who a node is, and which chain it is on.
//! * [`message`] — the logical protocol.
//! * [`codec`] — the Cap'n Proto wire encoding of that protocol.
//! * [`peer_table`] — the bounded, score-carrying peer directory.
//! * [`score`] — behaviour classification.
//! * [`dedup`] — bounded "have I seen this?" sets.
//! * [`engine`] — the state machine. No I/O.
//! * [`chain`] — the seam to the executor and the mempool.
//! * [`transport`] — the state machine over real TCP sockets.
//! * [`sim`] — the state machine over an in-process network, for tests.

pub mod chain;
pub mod codec;
pub mod config;
pub mod dedup;
pub mod engine;
pub mod identity;
pub mod message;
pub mod peer_table;
pub mod score;
pub mod sim;
pub mod transport;

pub use chain::{ChainAccess, DbChain};
pub use config::{ConfigError, P2pConfig, ScoreConfig, PROTOCOL_VERSION};
pub use engine::{Action, Event, LocalNode, P2pEngine, P2pStats, RejectReason};
pub use identity::{GenesisId, NodeId};
pub use message::{DisconnectReason, Hello, Message, PeerAddr};
pub use peer_table::{PeerId, PeerRecord, PeerTable};
pub use score::Behaviour;
pub use transport::{ConsensusInbound, P2pHandle};
