//! Networking for SuperFrontierLiquid nodes.
//!
//! Two layers live here:
//!
//! * [`syncer`] — block acceptance. Verify the proposer signature, re-execute
//!   every transaction, recompute the state root, and adopt the block only if
//!   it matches. This is the chain's trust boundary and it predates the
//!   peer-to-peer work; nothing below is allowed to bypass it.
//! * [`p2p`] — how blocks and transactions find their way between nodes:
//!   handshake, peer discovery, gossip, catch-up from any peer, and peer
//!   scoring. It moves bytes; it never decides whether bytes are a block.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod p2p;
pub mod syncer;

pub use syncer::{apply_remote_block, SyncError};
