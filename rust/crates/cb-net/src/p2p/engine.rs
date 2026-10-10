//! The gossip state machine.
//!
//! # Why this is a pure state machine
//!
//! [`P2pEngine`] owns no socket, no database and no clock. It takes an
//! [`Event`] and returns a list of [`Action`]s, and the driver — either the
//! real transport in [`crate::p2p::transport`] or the in-process harness in
//! [`crate::p2p::sim`] — performs them. Three things fall out of that:
//!
//! * The whole of discovery, gossip, catch-up and banning is testable without
//!   a single OS socket, at wall-clock speeds, deterministically. A networking
//!   layer that can only be tested by starting six processes is a networking
//!   layer that does not get tested.
//! * Block verification happens *outside* the engine. The engine emits
//!   [`Action::VerifyBlock`] and later receives [`Event::BlockAccepted`] or
//!   [`Event::BlockRejected`]. Re-executing a block is milliseconds of CPU; if
//!   it ran inside `handle` it would stall every other peer behind whichever
//!   peer happened to send a body.
//! * The engine physically cannot adopt a block. It has no executor, no state
//!   and no trie. The only way a block enters the chain is for the driver to
//!   run the existing `apply_remote_block`, which re-executes and compares the
//!   state root, and report the verdict back. There is no gossip shortcut
//!   because there is nowhere to put one.
//!
//! # Announce, fetch, verify, then forward
//!
//! A node forwards a block announcement only after *its own* verification has
//! accepted the block. Not on hearing the announce, not on receiving the body.
//! This is the load-bearing ordering decision in the file:
//!
//! * It makes "forward at most once" structural. Acceptance happens once per
//!   height, so the forward happens once.
//! * It stops a malicious peer from making the entire network fetch a body
//!   that will fail verification. The lie stops at the first honest hop.
//! * It costs latency — a block's propagation is now gated on per-hop
//!   re-execution rather than racing ahead of it. That is the trade this
//!   design makes on purpose: propagation speed is never allowed to become a
//!   reason to trust a peer.
//!
//! # Time
//!
//! All timestamps are monotonic milliseconds supplied by the driver through
//! [`Event::Tick`]. The engine never reads a clock, so a test can advance an
//! hour in a microsecond and a ban expiry can be asserted rather than waited
//! for.

use crate::p2p::config::{P2pConfig, PROTOCOL_VERSION};
use crate::p2p::dedup::SeenSet;
use crate::p2p::identity::{GenesisId, NodeId};
use crate::p2p::message::{DisconnectReason, Hello, Message, PeerAddr};
use crate::p2p::peer_table::{PeerId, PeerTable};
use crate::p2p::score::Behaviour;
use cb_crypto::SecretKey;
use cb_types::H256;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

/// This node's identity and the chain it believes it is on.
#[derive(Clone)]
pub struct LocalNode {
    /// Network key. Signs the handshake; never signs a block or a vote.
    pub key: SecretKey,
    /// Identity derived from `key`.
    pub node_id: NodeId,
    /// Chain id, taken from `Meta::chain_id()`.
    pub chain_id: u32,
    /// Chain fingerprint, taken from [`GenesisId::from_db`].
    pub genesis: GenesisId,
    /// Build string advertised in the handshake.
    pub software: String,
}

impl LocalNode {
    /// Build a local identity from a network key plus chain facts.
    pub fn new(key: SecretKey, chain_id: u32, genesis: GenesisId, software: String) -> Self {
        let pk = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key);
        Self {
            node_id: NodeId::from_pubkey(&pk),
            key,
            chain_id,
            genesis,
            software,
        }
    }
}

/// Why a verifier refused a body. Chosen so the engine can tell "this peer is
/// feeding me garbage" apart from "this arrived at an awkward moment".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Re-execution produced a different state root. The sender is serving a
    /// chain we are not on, or one that does not exist.
    StateRootMismatch,
    /// The proposer signature was malformed or recovered to the wrong address.
    BadProposerSignature,
    /// The bytes were not a decodable block.
    Undecodable,
    /// A transaction in the block failed to apply.
    ExecutionFailed,
    /// The block did not sit at `head + 1` by the time we got to it. Benign:
    /// a race with our own progress, not evidence about the sender.
    OutOfOrder,
    /// A transaction failed static validation or signature recovery.
    InvalidTransaction,
}

impl RejectReason {
    /// How this rejection should move the sender's score.
    pub fn behaviour(&self) -> Behaviour {
        match self {
            RejectReason::StateRootMismatch
            | RejectReason::Undecodable
            | RejectReason::ExecutionFailed => Behaviour::BadBlock,
            RejectReason::BadProposerSignature | RejectReason::InvalidTransaction => {
                Behaviour::BadSignature
            }
            RejectReason::OutOfOrder => Behaviour::Duplicate,
        }
    }
}

/// Something that happened, for the engine to react to.
#[derive(Clone, Debug)]
pub enum Event {
    /// A transport connection was established. The engine has not yet been
    /// told who is on the other end.
    Connected {
        /// Driver-allocated connection handle.
        peer: PeerId,
        /// Remote socket address, when the transport knows one.
        addr: Option<SocketAddr>,
        /// True when this node initiated the connection.
        outbound: bool,
    },
    /// A message arrived on a connection.
    Message {
        /// Connection it arrived on.
        peer: PeerId,
        /// The message.
        msg: Message,
    },
    /// A connection went away.
    Disconnected {
        /// Connection that closed.
        peer: PeerId,
    },
    /// A block was verified and adopted. `from` is `None` when this node
    /// produced it, which is how the sequencer injects its own blocks into
    /// gossip without a special case.
    BlockAccepted {
        /// Block hash.
        hash: H256,
        /// Block height.
        height: u64,
        /// Connection the body came from, if any.
        from: Option<PeerId>,
    },
    /// A block body failed verification.
    BlockRejected {
        /// Connection the body came from, if any.
        from: Option<PeerId>,
        /// Height the body claimed.
        height: u64,
        /// Why it was refused.
        reason: RejectReason,
    },
    /// A transaction passed validation and entered the mempool.
    TxAccepted {
        /// Transaction hash.
        hash: H256,
        /// Connection it came from, if any.
        from: Option<PeerId>,
    },
    /// A transaction failed validation.
    TxRejected {
        /// Connection it came from, if any.
        from: Option<PeerId>,
        /// Why it was refused.
        reason: RejectReason,
    },
    /// Time passed. Drives dialing, discovery, timeouts and pings.
    Tick {
        /// Monotonic milliseconds since the engine's epoch.
        now_ms: u64,
    },
}

/// Something the driver must do on the engine's behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open a connection to `addr`.
    Dial {
        /// Where to dial.
        addr: SocketAddr,
    },
    /// Send `msg` on a connection.
    Send {
        /// Connection to send on.
        peer: PeerId,
        /// Message to send.
        msg: Message,
    },
    /// Send one `msg` to each of `peers`.
    ///
    /// The same as a [`Action::Send`] per peer, except that the message is
    /// built once. It exists for consensus broadcasts, whose proposals carry
    /// megabytes: one `Send` per peer meant one copy of the payload per peer
    /// before the transport had even started encoding, and the transport then
    /// encoded each copy separately. A driver that sends one frame to every
    /// listed peer is exactly as correct as one that loops.
    Broadcast {
        /// Connections to send on.
        peers: Vec<PeerId>,
        /// Message to send to each.
        msg: Message,
    },
    /// Close a connection, after sending the reason.
    Disconnect {
        /// Connection to close.
        peer: PeerId,
        /// Why.
        reason: DisconnectReason,
    },
    /// Re-execute a block body and report back with [`Event::BlockAccepted`]
    /// or [`Event::BlockRejected`]. The engine will not emit another
    /// `VerifyBlock` until this one is answered.
    VerifyBlock {
        /// Connection the body came from, if any.
        from: Option<PeerId>,
        /// Height the body claimed.
        height: u64,
        /// Canonical `Block::encode` bytes.
        bytes: Vec<u8>,
    },
    /// Validate a transaction body and report back.
    VerifyTx {
        /// Connection it came from.
        from: PeerId,
        /// Canonical `SignedTx` bincode bytes.
        bytes: Vec<u8>,
    },
    /// Look up a block by hash and send it to `peer` as a `blockBody`.
    ServeBlock {
        /// Who asked.
        peer: PeerId,
        /// Which block.
        hash: H256,
    },
    /// Look up heights `from..=to` and send them to `peer` as a `blockRange`.
    ServeBlockRange {
        /// Who asked.
        peer: PeerId,
        /// First height, inclusive.
        from: u64,
        /// Last height, inclusive.
        to: u64,
    },
    /// Look up a transaction by hash and send it to `peer` as a `txBody`.
    ServeTx {
        /// Who asked.
        peer: PeerId,
        /// Which transaction.
        hash: H256,
    },
    /// A consensus message arrived from a handshaked peer. Hand it to the
    /// consensus driver; the engine has not looked inside.
    Consensus {
        /// Connection it arrived on, for a reply.
        from: PeerId,
        /// `cb_consensus::bft::Message` bincode bytes.
        bytes: Vec<u8>,
    },
}

/// Per-connection state the engine tracks before and after the handshake.
#[derive(Debug)]
struct ConnState {
    addr: Option<SocketAddr>,
    outbound: bool,
    node_id: Option<NodeId>,
    hello_received: bool,
    window_start_ms: u64,
    messages_in_window: u32,
    last_ping_ms: u64,
    /// When the connection was registered, for the handshake deadline.
    connected_ms: u64,
}

#[derive(Debug)]
struct Inflight {
    peer: PeerId,
    deadline_ms: u64,
}

#[derive(Debug)]
struct RangeRequest {
    peer: PeerId,
    from: u64,
    to: u64,
    deadline_ms: u64,
}

/// Counters an operator or a test can read to see what the engine did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct P2pStats {
    /// Block announcements this node originated or relayed.
    pub blocks_forwarded: u64,
    /// Block bodies requested.
    pub blocks_requested: u64,
    /// Transaction announcements relayed.
    pub txs_forwarded: u64,
    /// Peers banned since start.
    pub bans: u64,
    /// Catch-up ranges requested.
    pub ranges_requested: u64,
    /// Peers currently connected and handshaked. A live gauge, not a counter.
    pub connected_peers: usize,
    /// Peer-table size: everything known, connected or not. A live gauge.
    pub known_peers: usize,
}

/// The gossip state machine. See the module header.
pub struct P2pEngine {
    cfg: P2pConfig,
    /// Round robin over peers for `send_consensus_any`.
    consensus_rr: usize,
    local: LocalNode,
    table: PeerTable,
    conns: HashMap<PeerId, ConnState>,
    head_height: u64,
    /// Hash of the block at `head_height`, once this engine has seen one
    /// accepted. Re-announced on the ping cadence; see `on_tick`.
    head_hash: Option<H256>,
    now_ms: u64,

    seen_blocks: SeenSet<H256>,
    forwarded_blocks: SeenSet<H256>,
    inflight_blocks: HashMap<H256, Inflight>,
    pending_bodies: BTreeMap<u64, (Option<PeerId>, Vec<u8>)>,
    verifying_block: bool,

    seen_txs: SeenSet<H256>,
    forwarded_txs: SeenSet<H256>,
    inflight_txs: HashMap<H256, Inflight>,

    range: Option<RangeRequest>,
    /// Addresses with a dial in flight, mapped to the moment we give up on
    /// them. A dial that never completes — a partitioned peer, a dead host —
    /// must not pin its address out of the candidate list forever, which is
    /// exactly what a plain set would do.
    dialing: HashMap<SocketAddr, u64>,
    /// Addresses not to dial before this moment, because the peer behind them
    /// told us it does not want the connection. Keyed by address rather than
    /// node id so it also covers a rejection that arrived before we learned
    /// who we were talking to.
    dial_backoff: HashMap<SocketAddr, u64>,
    last_discovery_ms: u64,
    stats: P2pStats,
}

impl P2pEngine {
    /// Build an engine. `head_height` is the node's current chain head;
    /// `now_ms` seeds the monotonic clock.
    pub fn new(cfg: P2pConfig, local: LocalNode, head_height: u64, now_ms: u64) -> Self {
        let table = PeerTable::new(&cfg);
        let seen = cfg.seen_cache;
        Self {
            cfg,
            consensus_rr: 0,
            local,
            table,
            conns: HashMap::new(),
            head_height,
            head_hash: None,
            now_ms,
            seen_blocks: SeenSet::new(seen),
            forwarded_blocks: SeenSet::new(seen),
            inflight_blocks: HashMap::new(),
            pending_bodies: BTreeMap::new(),
            verifying_block: false,
            seen_txs: SeenSet::new(seen),
            forwarded_txs: SeenSet::new(seen),
            inflight_txs: HashMap::new(),
            range: None,
            dialing: HashMap::new(),
            dial_backoff: HashMap::new(),
            last_discovery_ms: now_ms,
            stats: P2pStats::default(),
        }
    }

    /// This node's identity.
    pub fn node_id(&self) -> NodeId {
        self.local.node_id
    }

    /// The configuration in force.
    pub fn config(&self) -> &P2pConfig {
        &self.cfg
    }

    /// Current chain head as the engine understands it.
    pub fn head_height(&self) -> u64 {
        self.head_height
    }

    /// Read-only view of the peer table, for the dashboard and for tests.
    pub fn peers(&self) -> &PeerTable {
        &self.table
    }

    /// Counters and gauges, for the dashboard and for tests.
    pub fn stats(&self) -> P2pStats {
        P2pStats {
            connected_peers: self.table.connected_count(),
            known_peers: self.table.known_count(),
            ..self.stats
        }
    }

    /// Node ids this engine has completed a handshake with.
    pub fn handshaked_peers(&self) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = self.conns.values().filter_map(|c| c.node_id).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Dial the configured seeds. Called once at startup; separate from
    /// `Tick` so a driver can bootstrap before the first tick lands.
    pub fn bootstrap(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        let seeds = self.cfg.seeds.clone();
        for addr in seeds {
            if self.is_own_listen_addr(&addr) {
                // Refusing our own listen address here is the cheap half of
                // self-connection defence; the node-id check in the handshake
                // is the half that also catches a second address for the same
                // node.
                tracing::debug!(%addr, "skipping seed: it is our own listen address");
                continue;
            }
            if self.begin_dial(addr) {
                actions.push(Action::Dial { addr });
            }
        }
        actions
    }

    /// Mark `addr` as being dialed. Returns `false` when a dial to it is
    /// already outstanding.
    fn begin_dial(&mut self, addr: SocketAddr) -> bool {
        let deadline = self
            .now_ms
            .saturating_add(self.cfg.request_timeout.as_millis() as u64);
        match self.dialing.entry(addr) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(deadline);
                true
            }
        }
    }

    /// Feed an event in, get the work to do out.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::Connected {
                peer,
                addr,
                outbound,
            } => self.on_connected(peer, addr, outbound, &mut out),
            Event::Message { peer, msg } => self.on_message(peer, msg, &mut out),
            Event::Disconnected { peer } => self.on_disconnected(peer),
            Event::BlockAccepted { hash, height, from } => {
                self.on_block_accepted(hash, height, from, &mut out)
            }
            Event::BlockRejected {
                from,
                height,
                reason,
            } => self.on_block_rejected(from, height, reason, &mut out),
            Event::TxAccepted { hash, from } => self.on_tx_accepted(hash, from, &mut out),
            Event::TxRejected { from, reason } => {
                if let Some(peer) = from {
                    self.penalise(peer, reason.behaviour(), &mut out);
                }
            }
            Event::Tick { now_ms } => self.on_tick(now_ms, &mut out),
        }
        out
    }

    // ---------------------------------------------------------------- connect

    fn on_connected(
        &mut self,
        peer: PeerId,
        addr: Option<SocketAddr>,
        outbound: bool,
        out: &mut Vec<Action>,
    ) {
        if let Some(a) = addr {
            self.dialing.remove(&a);
        }
        if self.table.connected_count() >= self.cfg.max_peers {
            out.push(Action::Disconnect {
                peer,
                reason: DisconnectReason::TooManyPeers,
            });
            return;
        }
        if let Some(a) = addr {
            if self.table.is_addr_banned(&a, self.now_ms) {
                out.push(Action::Disconnect {
                    peer,
                    reason: DisconnectReason::Banned,
                });
                return;
            }
        }
        self.conns.insert(
            peer,
            ConnState {
                addr,
                outbound,
                node_id: None,
                hello_received: false,
                window_start_ms: self.now_ms,
                messages_in_window: 0,
                last_ping_ms: self.now_ms,
                connected_ms: self.now_ms,
            },
        );
        match self.build_hello() {
            Some(hello) => out.push(Action::Send {
                peer,
                msg: Message::Hello(hello),
            }),
            None => out.push(Action::Disconnect {
                peer,
                reason: DisconnectReason::Shutdown,
            }),
        }
    }

    fn build_hello(&self) -> Option<Hello> {
        let nonce = rand::random::<u64>();
        match Hello::signed(
            &self.local.key,
            PROTOCOL_VERSION,
            self.local.chain_id,
            self.local.genesis,
            self.local.software.clone(),
            self.head_height,
            self.cfg.listen,
            nonce,
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::error!(?e, "could not sign p2p hello; refusing the connection");
                None
            }
        }
    }

    fn on_disconnected(&mut self, peer: PeerId) {
        if let Some(conn) = self.conns.remove(&peer) {
            if let Some(a) = conn.addr {
                self.dialing.remove(&a);
            }
        }
        self.table.detach(peer);
        self.inflight_blocks.retain(|_, r| r.peer != peer);
        self.inflight_txs.retain(|_, r| r.peer != peer);
        if self.range.as_ref().is_some_and(|r| r.peer == peer) {
            self.range = None;
        }
    }

    // ---------------------------------------------------------------- messages

    fn on_message(&mut self, peer: PeerId, msg: Message, out: &mut Vec<Action>) {
        if !self.conns.contains_key(&peer) {
            return;
        }
        if self.rate_limited(peer, out) {
            return;
        }
        self.table.touch(peer, self.now_ms);

        let handshaked = self
            .conns
            .get(&peer)
            .is_some_and(|c| c.hello_received && c.node_id.is_some());
        match (&msg, handshaked) {
            (Message::Hello(_), _) | (Message::Disconnect(_), _) => {}
            (_, false) => {
                // Anything other than a handshake before the handshake is a
                // protocol violation. There is no peer-table record to score
                // yet, so the only sanction available is the disconnect.
                out.push(Action::Disconnect {
                    peer,
                    reason: DisconnectReason::ProtocolViolation,
                });
                return;
            }
            (_, true) => {}
        }

        match msg {
            Message::Hello(hello) => self.on_hello(peer, hello, out),
            Message::Disconnect(reason) => {
                tracing::info!(peer, %reason, "peer closed the connection");
                self.note_rejection(peer, reason);
                self.on_disconnected(peer);
            }
            Message::GetPeers => {
                let exclude = self.node_of(peer);
                let shared = self
                    .table
                    .share(exclude, self.cfg.peers_per_response, self.now_ms);
                out.push(Action::Send {
                    peer,
                    msg: Message::Peers(shared),
                });
            }
            Message::Peers(list) => self.on_peers(peer, list, out),
            Message::BlockAnnounce { hash, height } => {
                self.on_block_announce(peer, hash, height, out)
            }
            Message::GetBlock { hash } => out.push(Action::ServeBlock { peer, hash }),
            Message::BlockBody { height, bytes } => self.on_block_body(peer, height, bytes, out),
            Message::TxAnnounce { hash } => self.on_tx_announce(peer, hash, out),
            Message::GetTx { hash } => out.push(Action::ServeTx { peer, hash }),
            Message::TxBody { bytes } => self.on_tx_body(peer, bytes, out),
            Message::GetBlockRange { from, to } => {
                let capped = to.min(from.saturating_add(self.cfg.max_range_blocks - 1));
                if from > to {
                    self.penalise(peer, Behaviour::ProtocolViolation, out);
                    return;
                }
                out.push(Action::ServeBlockRange {
                    peer,
                    from,
                    to: capped,
                });
            }
            Message::BlockRange { blocks } => self.on_block_range(peer, blocks, out),
            Message::Ping { nonce } => out.push(Action::Send {
                peer,
                msg: Message::Pong { nonce },
            }),
            Message::Pong { .. } => {}
            Message::Consensus { bytes } => out.push(Action::Consensus { from: peer, bytes }),
        }
    }

    /// Send a consensus message to every handshaked peer. Validators find
    /// each other through ordinary peer discovery; a non-validator that
    /// receives one hands it to a driver that ignores it.
    ///
    /// One [`Action::Broadcast`] rather than a `Send` per peer: the payload is
    /// moved in once and never cloned here.
    pub fn broadcast_consensus(&self, bytes: Vec<u8>) -> Vec<Action> {
        let mut peers: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.hello_received && c.node_id.is_some())
            .map(|(peer, _)| *peer)
            .collect();
        if peers.is_empty() {
            return Vec::new();
        }
        peers.sort_unstable();
        vec![Action::Broadcast {
            peers,
            msg: Message::Consensus { bytes },
        }]
    }

    /// Whether the engine still has state for this connection.
    ///
    /// The transport uses it to keep one invariant: a socket is open only for
    /// a connection the engine knows about. A peer that sends `Disconnect` and
    /// then keeps its socket open is forgotten by the engine at once, and
    /// before this check the transport went on reading from it — holding an
    /// inbound slot — for as long as the peer liked.
    pub fn is_tracking(&self, peer: PeerId) -> bool {
        self.conns.contains_key(&peer)
    }

    /// Switch fetching of announced blocks and range sync on or off. A
    /// validator turns it on to catch up by executed blocks when it is
    /// further behind than its peers keep proposals for, then off again.
    pub fn set_fetch_blocks(&mut self, on: bool) {
        self.cfg.fetch_blocks = on;
    }

    /// Send a consensus message to one handshaked peer, a different one
    /// each time, for a request any of them can answer.
    pub fn send_consensus_any(&mut self, bytes: Vec<u8>) -> Vec<Action> {
        let mut peers: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.hello_received && c.node_id.is_some())
            .map(|(p, _)| *p)
            .collect();
        if peers.is_empty() {
            return Vec::new();
        }
        peers.sort_unstable();
        self.consensus_rr = self.consensus_rr.wrapping_add(1);
        let peer = peers[self.consensus_rr % peers.len()];
        vec![Action::Send {
            peer,
            msg: Message::Consensus { bytes },
        }]
    }

    /// Send a consensus message to one peer, if it is still connected.
    pub fn send_consensus(&self, peer: PeerId, bytes: Vec<u8>) -> Vec<Action> {
        if self.conns.get(&peer).is_some_and(|c| c.hello_received) {
            vec![Action::Send {
                peer,
                msg: Message::Consensus { bytes },
            }]
        } else {
            Vec::new()
        }
    }

    fn on_hello(&mut self, peer: PeerId, hello: Hello, out: &mut Vec<Action>) {
        if self.conns.get(&peer).is_some_and(|c| c.hello_received) {
            out.push(Action::Disconnect {
                peer,
                reason: DisconnectReason::ProtocolViolation,
            });
            return;
        }

        // Order matters for the operator reading the log: report the most
        // specific misconfiguration first.
        if hello.protocol != PROTOCOL_VERSION {
            return self.reject_handshake(peer, DisconnectReason::VersionMismatch, out);
        }
        if hello.chain_id != self.local.chain_id {
            tracing::warn!(
                peer,
                theirs = hello.chain_id,
                ours = self.local.chain_id,
                "rejecting peer: chain id mismatch"
            );
            return self.reject_handshake(peer, DisconnectReason::ChainIdMismatch, out);
        }
        if hello.genesis != self.local.genesis {
            tracing::warn!(
                peer,
                theirs = %hello.genesis,
                ours = %self.local.genesis,
                "rejecting peer: genesis mismatch"
            );
            return self.reject_handshake(peer, DisconnectReason::GenesisMismatch, out);
        }
        if !hello.verify_identity() {
            return self.reject_handshake(peer, DisconnectReason::BadIdentity, out);
        }
        if hello.node_id == self.local.node_id {
            tracing::debug!(peer, "rejecting peer: it is us");
            return self.reject_handshake(peer, DisconnectReason::SelfConnection, out);
        }
        if self.table.is_banned(&hello.node_id, self.now_ms) {
            return self.reject_handshake(peer, DisconnectReason::Banned, out);
        }
        // The slot check belongs here, not at `Connected`: a connection only
        // occupies a peer slot once we know whose it is, and several
        // handshakes can be in flight at the same time.
        let already_connected = self
            .table
            .get(&hello.node_id)
            .and_then(|r| r.connection)
            .is_some();
        if !already_connected && self.table.connected_count() >= self.cfg.max_peers {
            return self.reject_handshake(peer, DisconnectReason::TooManyPeers, out);
        }

        // Duplicate connection to the same node id. Both sides may have
        // dialed each other at the same instant, so the tie-break has to be a
        // function of the two node ids alone or the two sides will each drop a
        // different connection and end up with none.
        if let Some(existing) = self
            .table
            .get(&hello.node_id)
            .and_then(|r| r.connection)
            .filter(|c| *c != peer)
        {
            let keep_outbound = self.local.node_id < hello.node_id;
            let new_is_outbound = self.conns.get(&peer).is_some_and(|c| c.outbound);
            let drop_new = new_is_outbound != keep_outbound;
            let victim = if drop_new { peer } else { existing };
            tracing::debug!(
                node_id = %hello.node_id,
                victim,
                "dropping a duplicate connection"
            );
            out.push(Action::Disconnect {
                peer: victim,
                reason: DisconnectReason::DuplicateConnection,
            });
            if drop_new {
                self.conns.remove(&peer);
                return;
            }
            self.on_disconnected(existing);
        }

        let outbound = self.conns.get(&peer).is_some_and(|c| c.outbound);
        // Only an address we dialed, or one the peer explicitly advertised, is
        // dialable. The remote address of an *inbound* connection is an
        // ephemeral source port; recording it would put an unreachable
        // endpoint into the peer table and gossip it onward to everybody else.
        let addr = hello.listen.or_else(|| {
            if outbound {
                self.conns.get(&peer).and_then(|c| c.addr)
            } else {
                None
            }
        });
        if !self.table.attach(
            hello.node_id,
            peer,
            addr,
            outbound,
            hello.head_height,
            self.now_ms,
        ) {
            return self.reject_handshake(peer, DisconnectReason::TooManyPeers, out);
        }
        if let Some(conn) = self.conns.get_mut(&peer) {
            conn.node_id = Some(hello.node_id);
            conn.hello_received = true;
            conn.addr = addr.or(conn.addr);
        }
        tracing::info!(
            peer,
            node_id = %hello.node_id,
            software = %hello.software,
            head = hello.head_height,
            "peer handshake complete"
        );

        out.push(Action::Send {
            peer,
            msg: Message::GetPeers,
        });
        self.maybe_request_range(out);
    }

    fn reject_handshake(&mut self, peer: PeerId, reason: DisconnectReason, out: &mut Vec<Action>) {
        self.conns.remove(&peer);
        out.push(Action::Disconnect { peer, reason });
    }

    fn on_peers(&mut self, peer: PeerId, list: Vec<PeerAddr>, out: &mut Vec<Action>) {
        if list.len() > self.cfg.peers_per_response.saturating_mul(2) {
            self.penalise(peer, Behaviour::Flood, out);
            return;
        }
        let mut useful = 0usize;
        for entry in list {
            if entry.node_id == self.local.node_id || self.is_own_listen_addr(&entry.addr) {
                continue;
            }
            if self.table.get(&entry.node_id).is_some() {
                continue;
            }
            if self
                .table
                .note_address(entry.node_id, Some(entry.addr), self.now_ms)
            {
                useful += 1;
            }
        }
        if useful > 0 {
            if let Some(node_id) = self.node_of(peer) {
                self.table.record_behaviour(
                    &node_id,
                    Behaviour::GoodPeers,
                    &self.cfg.score,
                    self.now_ms,
                );
            }
        }
        self.dial_to_target(out);
    }

    // ---------------------------------------------------------------- blocks

    fn on_block_announce(&mut self, peer: PeerId, hash: H256, height: u64, out: &mut Vec<Action>) {
        if let Some(node_id) = self.node_of(peer) {
            self.table.note_head(&node_id, height);
        }
        if height <= self.head_height {
            // The peer is announcing a head we are past: it is behind, or
            // catching up and announcing as it lands blocks. Nothing to
            // fetch and nothing to hold against it.
            return;
        }
        if self.seen_blocks.contains(&hash) {
            self.penalise(peer, Behaviour::Duplicate, out);
            return;
        }
        if !self.cfg.fetch_blocks {
            // Noted the peer's head; that is all a validator wants from an
            // announcement.
            return;
        }
        if self.inflight_blocks.contains_key(&hash) {
            return;
        }
        if height > self.head_height.saturating_add(1) {
            // We are behind by more than one block. Fetching this single body
            // would leave it stranded in the pending buffer until the gap
            // below it is filled, so ask for the gap instead — one code path
            // serves both "I missed a block" and "I was offline for an hour".
            self.maybe_request_range(out);
            return;
        }
        self.inflight_blocks.insert(
            hash,
            Inflight {
                peer,
                deadline_ms: self
                    .now_ms
                    .saturating_add(self.cfg.request_timeout.as_millis() as u64),
            },
        );
        self.stats.blocks_requested += 1;
        out.push(Action::Send {
            peer,
            msg: Message::GetBlock { hash },
        });
    }

    fn on_block_body(&mut self, peer: PeerId, height: u64, bytes: Vec<u8>, out: &mut Vec<Action>) {
        // A body we never asked for is either a bug or an attempt to make us
        // spend a re-execution. Either way we do not run it.
        let solicited = self.inflight_blocks.iter().any(|(_, req)| req.peer == peer);
        if !solicited {
            self.penalise(peer, Behaviour::Unsolicited, out);
            return;
        }
        // `GetBlock` is only ever sent for `head + 1` — anything further
        // ahead is fetched as a range — so a single body claiming a greater
        // height is not an answer to anything we asked. It used to be buffered
        // anyway, unverified: one open request let a peer fill the pending
        // buffer with far-future bodies that are never verified and so never
        // cost it anything, after which the honest `head + 1` body was
        // refused as a flood, its honest sender was penalised, and sync
        // stopped for good. A body at or below the head is merely late, and
        // `buffer_body` drops it quietly.
        if height > self.head_height.saturating_add(1) {
            self.penalise(peer, Behaviour::ProtocolViolation, out);
            return;
        }
        self.inflight_blocks
            .retain(|_, req| !(req.peer == peer && req.deadline_ms >= self.now_ms));
        self.buffer_body(Some(peer), height, bytes, out);
        self.pump_verify(out);
    }

    fn on_block_range(&mut self, peer: PeerId, blocks: Vec<(u64, Vec<u8>)>, out: &mut Vec<Action>) {
        let Some(req) = self.range.as_ref().filter(|r| r.peer == peer) else {
            self.penalise(peer, Behaviour::Unsolicited, out);
            return;
        };
        let (from, to) = (req.from, req.to);
        self.range = None;
        let bench_until = self
            .now_ms
            .saturating_add(self.cfg.request_timeout.as_millis() as u64);
        let demonstrated = blocks
            .iter()
            .map(|(h, _)| *h)
            .max()
            .unwrap_or(from.saturating_sub(1));
        if let Some(node_id) = self.node_of(peer) {
            // Replace the peer's advertised head with what it actually
            // produced, and stand it down as a catch-up source for a timeout.
            // Otherwise a peer that claims an impossible head is elected the
            // best source forever and sync stops.
            if demonstrated < to {
                self.table.correct_head(&node_id, demonstrated, bench_until);
            }
        }
        if blocks.is_empty() {
            // The peer has nothing to give us for that window — pruned, or it
            // was not as far ahead as it claimed. Barely a penalty, and we
            // will pick a different source on the next tick.
            self.penalise(peer, Behaviour::UnservedRange, out);
            return;
        }
        if blocks.len() as u64 > self.cfg.max_range_blocks {
            self.penalise(peer, Behaviour::Flood, out);
            return;
        }
        for (height, bytes) in blocks {
            if height < from || height > to {
                self.penalise(peer, Behaviour::ProtocolViolation, out);
                return;
            }
            self.buffer_body(Some(peer), height, bytes, out);
        }
        self.pump_verify(out);
    }

    fn buffer_body(
        &mut self,
        from: Option<PeerId>,
        height: u64,
        bytes: Vec<u8>,
        out: &mut Vec<Action>,
    ) {
        if height <= self.head_height {
            return;
        }
        if self.pending_bodies.len() >= self.cfg.max_pending_blocks {
            if let Some(peer) = from {
                self.penalise(peer, Behaviour::Flood, out);
            }
            return;
        }
        self.pending_bodies.insert(height, (from, bytes));
    }

    /// Hand the next contiguous body to the verifier, if one is waiting and
    /// the verifier is idle. One block is verified at a time so that the
    /// chain's own ordering rule — a block applies at `head + 1` — is honoured
    /// without the engine having to reason about concurrent applies.
    fn pump_verify(&mut self, out: &mut Vec<Action>) {
        // Drop anything the chain has already moved past.
        let head = self.head_height;
        self.pending_bodies.retain(|h, _| *h > head);
        if self.verifying_block {
            return;
        }
        let want = self.head_height.saturating_add(1);
        if let Some((from, bytes)) = self.pending_bodies.remove(&want) {
            self.verifying_block = true;
            out.push(Action::VerifyBlock {
                from,
                height: want,
                bytes,
            });
        }
    }

    fn on_block_accepted(
        &mut self,
        hash: H256,
        height: u64,
        from: Option<PeerId>,
        out: &mut Vec<Action>,
    ) {
        // Only a verdict on a body we handed out frees the verifier. A block
        // this node produced itself never occupied it, and clearing it here
        // would let a second `VerifyBlock` out while one is still running.
        if from.is_some() {
            self.verifying_block = false;
        }
        if height >= self.head_height {
            self.head_hash = Some(hash);
        }
        self.head_height = self.head_height.max(height);
        self.seen_blocks.insert(hash);
        self.inflight_blocks.remove(&hash);

        if let Some(peer) = from {
            if let Some(node_id) = self.node_of(peer) {
                self.table.record_behaviour(
                    &node_id,
                    Behaviour::GoodBlock,
                    &self.cfg.score,
                    self.now_ms,
                );
            }
        }

        // The one place a block enters gossip: after our own verifier accepted
        // it. Announce, never push the body.
        //
        // But not while we are still replaying a backlog. A node catching up
        // through a thousand historical blocks would announce every one of
        // them to peers that are already past them — a thousand messages that
        // can only ever be answered "I have that", and which cost the
        // announcing node reputation at every peer it sends them to. A block is
        // worth gossiping when it is news; during catch-up it is not.
        //
        // The signal is our own pending buffer, deliberately, and not what
        // peers claim their heads are. A peer's advertised head is an
        // unverifiable number it chooses; basing this on it would let one peer
        // announcing a nonsense height switch off relaying across the network.
        let replaying_backlog = !self.pending_bodies.is_empty();
        if !replaying_backlog && self.forwarded_blocks.insert(hash) {
            self.stats.blocks_forwarded += 1;
            for (_, conn) in self.table.connections() {
                if Some(conn) == from {
                    continue;
                }
                out.push(Action::Send {
                    peer: conn,
                    msg: Message::BlockAnnounce { hash, height },
                });
            }
        }
        self.pump_verify(out);
        self.maybe_request_range(out);
    }

    fn on_block_rejected(
        &mut self,
        from: Option<PeerId>,
        height: u64,
        reason: RejectReason,
        out: &mut Vec<Action>,
    ) {
        self.verifying_block = false;
        if let Some(peer) = from {
            tracing::warn!(peer, height, ?reason, "peer served a block we refused");
            self.penalise(peer, reason.behaviour(), out);
        }
        self.pump_verify(out);
    }

    // ------------------------------------------------------------ transactions

    fn on_tx_announce(&mut self, peer: PeerId, hash: H256, out: &mut Vec<Action>) {
        if self.seen_txs.contains(&hash) {
            self.penalise(peer, Behaviour::Duplicate, out);
            return;
        }
        if self.inflight_txs.contains_key(&hash) {
            return;
        }
        self.inflight_txs.insert(
            hash,
            Inflight {
                peer,
                deadline_ms: self
                    .now_ms
                    .saturating_add(self.cfg.request_timeout.as_millis() as u64),
            },
        );
        out.push(Action::Send {
            peer,
            msg: Message::GetTx { hash },
        });
    }

    fn on_tx_body(&mut self, peer: PeerId, bytes: Vec<u8>, out: &mut Vec<Action>) {
        let solicited = self.inflight_txs.values().any(|r| r.peer == peer);
        if !solicited {
            self.penalise(peer, Behaviour::Unsolicited, out);
            return;
        }
        out.push(Action::VerifyTx { from: peer, bytes });
    }

    fn on_tx_accepted(&mut self, hash: H256, from: Option<PeerId>, out: &mut Vec<Action>) {
        self.inflight_txs.remove(&hash);
        self.seen_txs.insert(hash);
        if let Some(peer) = from {
            if let Some(node_id) = self.node_of(peer) {
                self.table.record_behaviour(
                    &node_id,
                    Behaviour::GoodTx,
                    &self.cfg.score,
                    self.now_ms,
                );
            }
        }
        if self.forwarded_txs.insert(hash) {
            self.stats.txs_forwarded += 1;
            for (_, conn) in self.table.connections() {
                if Some(conn) == from {
                    continue;
                }
                out.push(Action::Send {
                    peer: conn,
                    msg: Message::TxAnnounce { hash },
                });
            }
        }
    }

    // ------------------------------------------------------------------- tick

    fn on_tick(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        self.now_ms = now_ms;

        // Give up on dials that never completed, so a peer that was
        // unreachable during a partition becomes a candidate again once the
        // partition heals.
        self.dialing.retain(|_, deadline| *deadline > now_ms);
        self.dial_backoff.retain(|_, until| *until > now_ms);

        // Requests that went unanswered.
        //
        // A block or range request that times out also stands the silent
        // peer down as a catch-up source for a request timeout. Scoring alone
        // did not re-route anything: the next tick asked the peer with the
        // highest advertised head again, and a peer that claims a height and
        // never answers *is* the peer with the highest advertised head, so
        // sync waited out one timeout after another on it — twenty of them
        // before the score finally banned it. Benched, it is skipped, and the
        // `maybe_request_range` at the end of this tick sends the same window
        // to the next-best peer straight away.
        let bench_until = now_ms.saturating_add(self.cfg.request_timeout.as_millis() as u64);
        let timed_out_blocks: Vec<(H256, PeerId)> = self
            .inflight_blocks
            .iter()
            .filter(|(_, r)| r.deadline_ms <= now_ms)
            .map(|(h, r)| (*h, r.peer))
            .collect();
        for (hash, peer) in timed_out_blocks {
            self.inflight_blocks.remove(&hash);
            self.bench_source(peer, bench_until);
            self.penalise(peer, Behaviour::Timeout, out);
        }
        let timed_out_txs: Vec<(H256, PeerId)> = self
            .inflight_txs
            .iter()
            .filter(|(_, r)| r.deadline_ms <= now_ms)
            .map(|(h, r)| (*h, r.peer))
            .collect();
        for (hash, peer) in timed_out_txs {
            self.inflight_txs.remove(&hash);
            self.penalise(peer, Behaviour::Timeout, out);
        }
        if self.range.as_ref().is_some_and(|r| r.deadline_ms <= now_ms) {
            if let Some(r) = self.range.take() {
                tracing::debug!(
                    peer = r.peer,
                    from = r.from,
                    to = r.to,
                    "catch-up range timed out; re-routing"
                );
                self.bench_source(r.peer, bench_until);
                self.penalise(r.peer, Behaviour::Timeout, out);
            }
        }

        // Peers that have gone quiet.
        let deadline = now_ms.saturating_sub(self.cfg.peer_timeout.as_millis() as u64);
        for conn in self.table.stale_connections(deadline) {
            out.push(Action::Disconnect {
                peer: conn,
                reason: DisconnectReason::Timeout,
            });
            self.on_disconnected(conn);
        }

        // Connections that never completed a handshake. The TCP transport
        // has its own clock for this, but the engine is the one that holds
        // the state: a connection whose `Hello` was lost would otherwise sit
        // in `conns` for ever, and its peer — which may well have received
        // *our* `Hello` and consider the link up — would be sending into a
        // connection that refuses everything but a handshake.
        let handshake_deadline = now_ms.saturating_sub(self.cfg.request_timeout.as_millis() as u64);
        let unshaken: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|(_, c)| !c.hello_received && c.connected_ms < handshake_deadline)
            .map(|(p, _)| *p)
            .collect();
        for peer in unshaken {
            out.push(Action::Disconnect {
                peer,
                reason: DisconnectReason::Timeout,
            });
            self.on_disconnected(peer);
        }

        // Keep idle connections warm so `stale_connections` measures silence,
        // not idleness.
        //
        // The ping carries our head along with it, as an ordinary announce of
        // the head block. Heads were otherwise learned only from the `Hello`
        // and from announcements, each sent once; a node that lost the
        // announcement of the latest block never heard of it again, and sat
        // one block behind until the next block was produced — for ever, on a
        // chain that had stopped. The lossy-network tests found this. An
        // announce at or below a receiver's head costs it nothing and is not
        // scored, so this is safe to repeat.
        let ping_after = self.cfg.ping_interval.as_millis() as u64;
        let mut to_ping = Vec::new();
        for (peer, conn) in self.conns.iter_mut() {
            if conn.hello_received && now_ms.saturating_sub(conn.last_ping_ms) >= ping_after {
                conn.last_ping_ms = now_ms;
                to_ping.push(*peer);
            }
        }
        let head = self
            .head_hash
            .filter(|_| self.pending_bodies.is_empty())
            .map(|hash| (hash, self.head_height));
        for peer in to_ping {
            out.push(Action::Send {
                peer,
                msg: Message::Ping { nonce: now_ms },
            });
            if let Some((hash, height)) = head {
                out.push(Action::Send {
                    peer,
                    msg: Message::BlockAnnounce { hash, height },
                });
            }
        }

        if now_ms.saturating_sub(self.last_discovery_ms)
            >= self.cfg.discovery_interval.as_millis() as u64
        {
            self.last_discovery_ms = now_ms;
            for (_, conn) in self.table.connections() {
                out.push(Action::Send {
                    peer: conn,
                    msg: Message::GetPeers,
                });
            }
        }

        self.dial_to_target(out);
        self.maybe_request_range(out);
        self.pump_verify(out);
    }

    // -------------------------------------------------------------- local hooks

    /// Tell the engine this node produced or locally applied a block. The
    /// sequencer calls this; it is the same path a gossiped block takes after
    /// verification, which is why a leader needs no special case.
    pub fn announce_local_block(&mut self, hash: H256, height: u64) -> Vec<Action> {
        self.handle(Event::BlockAccepted {
            hash,
            height,
            from: None,
        })
    }

    /// Tell the engine this node accepted a transaction from a local client.
    pub fn announce_local_tx(&mut self, hash: H256) -> Vec<Action> {
        self.handle(Event::TxAccepted { hash, from: None })
    }

    // ----------------------------------------------------------------- helpers

    /// Remember that a peer refused us, and for how long not to bother it
    /// again.
    ///
    /// The durations are policy, not physics: a chain or genesis mismatch will
    /// still be a mismatch in five minutes, while a peer that was merely full
    /// may have room after a discovery interval.
    fn note_rejection(&mut self, peer: PeerId, reason: DisconnectReason) {
        let backoff_ms = match reason {
            DisconnectReason::Banned
            | DisconnectReason::Misbehaving
            | DisconnectReason::ChainIdMismatch
            | DisconnectReason::GenesisMismatch
            | DisconnectReason::VersionMismatch
            | DisconnectReason::BadIdentity => self.cfg.score.ban.as_millis() as u64,
            DisconnectReason::TooManyPeers
            | DisconnectReason::DuplicateConnection
            | DisconnectReason::ProtocolViolation
            | DisconnectReason::Timeout
            | DisconnectReason::SelfConnection
            | DisconnectReason::Shutdown => self.cfg.discovery_interval.as_millis() as u64,
        };
        if backoff_ms == 0 {
            return;
        }
        let until = self.now_ms.saturating_add(backoff_ms);
        if let Some(node_id) = self.node_of(peer) {
            self.table.set_backoff(&node_id, until);
        }
        if let Some(addr) = self.conns.get(&peer).and_then(|c| c.addr) {
            // Bounded: an adversary must not be able to grow this by
            // disconnecting us from a thousand addresses.
            if self.dial_backoff.len() >= self.cfg.max_known_peers {
                self.dial_backoff.retain(|_, until| *until > self.now_ms);
            }
            if self.dial_backoff.len() < self.cfg.max_known_peers {
                self.dial_backoff.insert(addr, until);
            }
        }
    }

    /// Skip `peer` as a catch-up source until `until_ms`, without touching
    /// its advertised head: it may yet be honest and merely slow.
    fn bench_source(&mut self, peer: PeerId, until_ms: u64) {
        if let Some(node_id) = self.node_of(peer) {
            self.table.bench_as_source(&node_id, until_ms);
        }
    }

    fn node_of(&self, peer: PeerId) -> Option<NodeId> {
        self.conns.get(&peer).and_then(|c| c.node_id)
    }

    fn is_own_listen_addr(&self, addr: &SocketAddr) -> bool {
        self.cfg.listen.as_ref() == Some(addr)
    }

    /// Score a peer down and, if that crosses the ban threshold, disconnect it.
    fn penalise(&mut self, peer: PeerId, behaviour: Behaviour, out: &mut Vec<Action>) {
        let Some(node_id) = self.node_of(peer) else {
            return;
        };
        let outcome =
            self.table
                .record_behaviour(&node_id, behaviour, &self.cfg.score, self.now_ms);
        if behaviour.is_notable() {
            tracing::warn!(
                peer,
                %node_id,
                ?behaviour,
                score = outcome.score,
                "peer misbehaved"
            );
        }
        if outcome.newly_banned {
            self.stats.bans += 1;
            tracing::warn!(peer, %node_id, "banning peer");
            out.push(Action::Disconnect {
                peer,
                reason: DisconnectReason::Misbehaving,
            });
            self.on_disconnected(peer);
        }
    }

    fn dial_to_target(&mut self, out: &mut Vec<Action>) {
        let connected = self.table.connected_count();
        if connected >= self.cfg.max_peers {
            return;
        }
        let want = self
            .cfg
            .target_outbound
            .saturating_sub(connected)
            .min(self.cfg.max_peers - connected);
        if want == 0 {
            return;
        }
        let in_progress: std::collections::HashSet<SocketAddr> =
            self.dialing.keys().copied().collect();
        let candidates = self.table.dial_candidates(self.now_ms, &in_progress);
        for addr in candidates.into_iter().take(want) {
            if self.is_own_listen_addr(&addr) {
                continue;
            }
            if self
                .dial_backoff
                .get(&addr)
                .is_some_and(|until| *until > self.now_ms)
            {
                continue;
            }
            if self.begin_dial(addr) {
                out.push(Action::Dial { addr });
            }
        }

        // With no peer at all, go back to the seeds. They are dialed once at
        // bootstrap and never enter the peer table until a handshake with
        // them completes, so a node whose first attempt failed — the seed was
        // restarting, the `Hello` was lost, the handshake timed out — had no
        // address left to try and stayed alone for good. The lossy-network
        // tests found this. `begin_dial` and the rejection backoff pace it.
        if connected == 0 {
            let seeds = self.cfg.seeds.clone();
            for addr in seeds {
                if self.is_own_listen_addr(&addr)
                    || self
                        .dial_backoff
                        .get(&addr)
                        .is_some_and(|until| *until > self.now_ms)
                    || self.conns.values().any(|c| c.addr == Some(addr))
                {
                    continue;
                }
                if self.begin_dial(addr) {
                    out.push(Action::Dial { addr });
                }
            }
        }
    }

    /// Ask the best-positioned connected peer for the heights we are missing.
    ///
    /// "Best-positioned" is the highest advertised head, not the leader. That
    /// is the point of the exercise: a node that fell behind catches up from
    /// whoever is ahead, and it is safe to do so because the block still has
    /// to re-execute to the state root it claims before it is adopted.
    fn maybe_request_range(&mut self, out: &mut Vec<Action>) {
        if !self.cfg.fetch_blocks || self.range.is_some() || self.verifying_block {
            return;
        }
        // Do not start a range while the pending buffer already holds the
        // next block; finish what we have first.
        if self
            .pending_bodies
            .contains_key(&self.head_height.saturating_add(1))
        {
            return;
        }
        let Some((node_id, conn, their_head)) =
            self.table.best_source_above(self.head_height, self.now_ms)
        else {
            return;
        };
        let from = self.head_height.saturating_add(1);
        // Saturating throughout: a peer's advertised head is a number it chose,
        // and `u64::MAX` must produce a bounded request rather than a panic.
        let to = their_head.min(from.saturating_add(self.cfg.max_range_blocks.saturating_sub(1)));
        if to < from {
            return;
        }
        tracing::debug!(%node_id, from, to, "requesting a catch-up range");
        self.stats.ranges_requested += 1;
        self.range = Some(RangeRequest {
            peer: conn,
            from,
            to,
            deadline_ms: self
                .now_ms
                .saturating_add(self.cfg.request_timeout.as_millis() as u64),
        });
        out.push(Action::Send {
            peer: conn,
            msg: Message::GetBlockRange { from, to },
        });
    }

    fn rate_limited(&mut self, peer: PeerId, out: &mut Vec<Action>) -> bool {
        let window = self.cfg.rate_window.as_millis() as u64;
        let limit = self.cfg.max_messages_per_window;
        let now = self.now_ms;
        let over = {
            let Some(conn) = self.conns.get_mut(&peer) else {
                return false;
            };
            if now.saturating_sub(conn.window_start_ms) >= window {
                conn.window_start_ms = now;
                conn.messages_in_window = 0;
            }
            conn.messages_in_window = conn.messages_in_window.saturating_add(1);
            conn.messages_in_window > limit
        };
        if over {
            self.penalise(peer, Behaviour::Flood, out);
        }
        over
    }
}
