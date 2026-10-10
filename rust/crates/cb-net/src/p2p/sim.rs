//! An in-process network harness.
//!
//! This is the tool that makes the gossip layer testable. It runs N engines in
//! one thread with no sockets, no timers and no threads: dialing, message
//! delivery, verification and the clock are all explicit function calls, so a
//! test can partition the network, advance an hour, and assert on the exact
//! set of messages that were sent.
//!
//! The alternative — asserting on gossip by starting six OS processes under a
//! supervisor and grepping their logs — produces a test that is slow, flaky
//! and, in practice, not run. The real-socket transport gets its own small
//! set of tests for the things only a socket can break (framing, partial
//! reads, connection teardown); everything about the *protocol* is tested
//! here.
//!
//! What this trades away: the harness cannot catch a bug that lives in the
//! codec or in tokio. It is a protocol simulator, not a network emulator. By
//! default it delivers messages in FIFO order with no reordering and no loss
//! except the loss a test asks for. [`SimNetwork::set_faults`] turns on
//! seeded message loss, duplication and delay, which is as close to
//! adversarial scheduling as it gets: enough to show the protocol converges
//! when messages go missing, not a substitute for a real network.

use crate::p2p::chain::ChainAccess;
use crate::p2p::config::P2pConfig;
use crate::p2p::engine::{Action, Event, LocalNode, P2pEngine, RejectReason};
use crate::p2p::identity::{GenesisId, NodeId};
use crate::p2p::message::{DisconnectReason, Message};
use crate::p2p::peer_table::PeerId;
use cb_types::H256;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;

/// Index of a node inside a [`SimNetwork`].
pub type NodeIndex = usize;

/// One simulated node: an engine plus the chain it verifies against.
pub struct SimNode<C: ChainAccess> {
    /// The gossip state machine under test.
    pub engine: P2pEngine,
    /// The chain the driver reads from and applies to.
    pub chain: C,
    /// The address other simulated nodes dial to reach this one.
    pub listen: SocketAddr,
    /// Partition group; nodes in different groups cannot reach each other.
    pub group: usize,
}

/// Message faults a [`SimNetwork`] injects, each a probability in percent,
/// drawn from a seeded generator so a failing run replays exactly.
///
/// Faults apply to messages only. Connection setup and teardown are what a
/// TCP connection guarantees, and faking their loss would test a transport
/// this harness does not have.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Chance a message is lost.
    pub drop_pct: u8,
    /// Chance a message is delivered twice.
    pub dup_pct: u8,
    /// Chance a message is held back until the next tick, arriving after
    /// messages sent later — on the same connection or others.
    pub delay_pct: u8,
    /// Generator seed.
    pub seed: u64,
}

struct Link {
    a: NodeIndex,
    a_peer: PeerId,
    b: NodeIndex,
    b_peer: PeerId,
}

/// A simulated network of engines.
pub struct SimNetwork<C: ChainAccess> {
    nodes: Vec<SimNode<C>>,
    links: Vec<Link>,
    events: VecDeque<(NodeIndex, Event)>,
    next_peer_id: PeerId,
    now_ms: u64,
    /// Messages dropped because they crossed a partition boundary.
    pub dropped_across_partition: u64,
    /// Every `(node, message-kind)` pair the network carried, in order. Tests
    /// assert on this to count forwards.
    pub sent_log: Vec<(NodeIndex, &'static str)>,
    /// Every dial attempted, in order, whether or not it reached anything.
    /// Tests assert on this to catch reconnect storms.
    pub dials: Vec<(NodeIndex, SocketAddr)>,
    /// Every disconnect a node initiated, with its reason. Tests assert on
    /// this to check that a rejection carried the right explanation.
    pub disconnects: Vec<(NodeIndex, DisconnectReason)>,
    faults: Faults,
    rng: rand::rngs::StdRng,
    delayed: Vec<(NodeIndex, Event)>,
    /// Messages lost, duplicated and delayed by [`Faults`].
    pub fault_counts: (u64, u64, u64),
}

impl<C: ChainAccess> Default for SimNetwork<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: ChainAccess> SimNetwork<C> {
    /// An empty network at time zero.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            links: Vec::new(),
            events: VecDeque::new(),
            next_peer_id: 1,
            now_ms: 0,
            dropped_across_partition: 0,
            sent_log: Vec::new(),
            dials: Vec::new(),
            disconnects: Vec::new(),
            faults: Faults::default(),
            rng: rand::SeedableRng::seed_from_u64(0),
            delayed: Vec::new(),
            fault_counts: (0, 0, 0),
        }
    }

    /// Inject message faults from now on. `Faults::default()` turns them off.
    pub fn set_faults(&mut self, faults: Faults) {
        self.faults = faults;
        self.rng = rand::SeedableRng::seed_from_u64(faults.seed);
    }

    /// Add a node. Its listen address is derived from its index so a test can
    /// write `net.addr_of(0)` when configuring another node's seeds.
    pub fn add(&mut self, cfg: P2pConfig, local: LocalNode, chain: C) -> NodeIndex {
        let idx = self.nodes.len();
        let listen = cfg
            .listen
            .unwrap_or_else(|| sim_addr(u16::try_from(idx).unwrap_or(0)));
        let head = chain.head_height();
        let engine = P2pEngine::new(cfg, local, head, self.now_ms);
        self.nodes.push(SimNode {
            engine,
            chain,
            listen,
            group: 0,
        });
        idx
    }

    /// The address a node listens on.
    pub fn addr_of(&self, idx: NodeIndex) -> SocketAddr {
        self.nodes[idx].listen
    }

    /// Immutable access to a node.
    pub fn node(&self, idx: NodeIndex) -> &SimNode<C> {
        &self.nodes[idx]
    }

    /// Mutable access to a node.
    pub fn node_mut(&mut self, idx: NodeIndex) -> &mut SimNode<C> {
        &mut self.nodes[idx]
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the network has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The current simulated time.
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// Put every node in group 0: one connected network.
    pub fn heal(&mut self) {
        for n in self.nodes.iter_mut() {
            n.group = 0;
        }
    }

    /// Split the network. `groups[i]` is node `i`'s group; links that now
    /// cross a boundary are torn down, as a real partition would tear down
    /// the TCP connections beneath them.
    pub fn partition(&mut self, groups: &[usize]) {
        for (i, g) in groups.iter().enumerate() {
            if i < self.nodes.len() {
                self.nodes[i].group = *g;
            }
        }
        let doomed: Vec<usize> = self
            .links
            .iter()
            .enumerate()
            .filter(|(_, l)| self.nodes[l.a].group != self.nodes[l.b].group)
            .map(|(i, _)| i)
            .collect();
        for i in doomed.into_iter().rev() {
            let link = self.links.remove(i);
            self.events
                .push_back((link.a, Event::Disconnected { peer: link.a_peer }));
            self.events
                .push_back((link.b, Event::Disconnected { peer: link.b_peer }));
        }
    }

    /// Dial every node's configured seeds, then settle.
    pub fn bootstrap(&mut self) {
        for i in 0..self.nodes.len() {
            let actions = self.nodes[i].engine.bootstrap();
            self.exec(i, actions);
        }
        self.settle();
    }

    /// Advance the clock and tick every node, then settle.
    pub fn tick(&mut self, advance_ms: u64) {
        self.now_ms += advance_ms;
        // Delayed messages land before this tick's traffic, in a shuffled
        // order, and are not delayed again.
        let mut late = std::mem::take(&mut self.delayed);
        rand::seq::SliceRandom::shuffle(late.as_mut_slice(), &mut self.rng);
        self.events.extend(late);
        for i in 0..self.nodes.len() {
            let now = self.now_ms;
            let actions = self.nodes[i].engine.handle(Event::Tick { now_ms: now });
            self.exec(i, actions);
        }
        self.settle();
    }

    /// Tell a node it produced a block, then settle. This is the sequencer's
    /// entry point into gossip.
    pub fn announce_block(&mut self, idx: NodeIndex, hash: H256, height: u64) {
        let actions = self.nodes[idx].engine.announce_local_block(hash, height);
        self.exec(idx, actions);
        self.settle();
    }

    /// Tell a node it accepted a local transaction, then settle.
    pub fn announce_tx(&mut self, idx: NodeIndex, hash: H256) {
        let actions = self.nodes[idx].engine.announce_local_tx(hash);
        self.exec(idx, actions);
        self.settle();
    }

    /// Whether `node` ever refused a connection with `reason`.
    pub fn refused(&self, node: NodeIndex, reason: DisconnectReason) -> bool {
        self.disconnects
            .iter()
            .any(|(n, r)| *n == node && *r == reason)
    }

    /// How many dials node `idx` has attempted.
    pub fn dial_count(&self, idx: NodeIndex) -> usize {
        self.dials.iter().filter(|(n, _)| *n == idx).count()
    }

    /// How many messages of `kind` node `idx` sent.
    pub fn sent_count(&self, idx: NodeIndex, kind: &str) -> usize {
        self.sent_log
            .iter()
            .filter(|(n, k)| *n == idx && *k == kind)
            .count()
    }

    /// Run until no event is pending, or until a generous step budget is
    /// exhausted — a runaway loop should fail a test, not hang it.
    pub fn settle(&mut self) {
        let mut steps = 0u64;
        while let Some((idx, event)) = self.events.pop_front() {
            steps += 1;
            assert!(
                steps < 1_000_000,
                "simulated network did not settle; likely a gossip loop"
            );
            let actions = self.nodes[idx].engine.handle(event);
            self.exec(idx, actions);
        }
    }

    // ------------------------------------------------------------------ driver

    fn exec(&mut self, idx: NodeIndex, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Dial { addr } => self.dial(idx, addr),
                Action::Send { peer, msg } => {
                    self.sent_log.push((idx, msg.kind()));
                    self.route(idx, peer, msg);
                }
                Action::Broadcast { peers, msg } => {
                    for peer in peers {
                        self.sent_log.push((idx, msg.kind()));
                        self.route(idx, peer, msg.clone());
                    }
                }
                Action::Consensus { .. } => {}
                Action::Disconnect { peer, reason } => self.disconnect(idx, peer, reason),
                Action::VerifyBlock {
                    from,
                    height,
                    bytes,
                } => match self.nodes[idx].chain.verify_and_apply(&bytes) {
                    Ok((hash, applied_height)) => self.events.push_back((
                        idx,
                        Event::BlockAccepted {
                            hash,
                            height: applied_height,
                            from,
                        },
                    )),
                    Err(reason) => self.events.push_back((
                        idx,
                        Event::BlockRejected {
                            from,
                            height,
                            reason,
                        },
                    )),
                },
                Action::VerifyTx { from, bytes } => match self.nodes[idx].chain.accept_tx(&bytes) {
                    Ok(hash) => self.events.push_back((
                        idx,
                        Event::TxAccepted {
                            hash,
                            from: Some(from),
                        },
                    )),
                    Err(reason) => self.events.push_back((
                        idx,
                        Event::TxRejected {
                            from: Some(from),
                            reason,
                        },
                    )),
                },
                Action::ServeBlock { peer, hash } => {
                    let found = self.nodes[idx]
                        .chain
                        .height_of(&hash)
                        .and_then(|h| self.nodes[idx].chain.block_bytes(h).map(|b| (h, b)));
                    if let Some((height, bytes)) = found {
                        self.route(idx, peer, Message::BlockBody { height, bytes });
                    }
                }
                Action::ServeBlockRange { peer, from, to } => {
                    let mut blocks = Vec::new();
                    for h in from..=to {
                        match self.nodes[idx].chain.block_bytes(h) {
                            Some(b) => blocks.push((h, b)),
                            None => break,
                        }
                    }
                    self.route(idx, peer, Message::BlockRange { blocks });
                }
                Action::ServeTx { peer, hash } => {
                    if let Some(bytes) = self.nodes[idx].chain.tx_bytes(&hash) {
                        self.route(idx, peer, Message::TxBody { bytes });
                    }
                }
            }
        }
    }

    fn dial(&mut self, from: NodeIndex, addr: SocketAddr) {
        self.dials.push((from, addr));
        let Some(to) = self.nodes.iter().position(|n| n.listen == addr) else {
            return; // nothing is listening there
        };
        if self.nodes[from].group != self.nodes[to].group {
            self.dropped_across_partition += 1;
            return;
        }
        let a_peer = self.alloc_peer();
        let b_peer = self.alloc_peer();
        self.links.push(Link {
            a: from,
            a_peer,
            b: to,
            b_peer,
        });
        self.events.push_back((
            from,
            Event::Connected {
                peer: a_peer,
                addr: Some(addr),
                outbound: true,
            },
        ));
        let back = self.nodes[from].listen;
        self.events.push_back((
            to,
            Event::Connected {
                peer: b_peer,
                addr: Some(back),
                outbound: false,
            },
        ));
    }

    fn peer_endpoint(&self, from: NodeIndex, peer: PeerId) -> Option<(NodeIndex, PeerId)> {
        self.links.iter().find_map(|l| {
            if l.a == from && l.a_peer == peer {
                Some((l.b, l.b_peer))
            } else if l.b == from && l.b_peer == peer {
                Some((l.a, l.a_peer))
            } else {
                None
            }
        })
    }

    fn route(&mut self, from: NodeIndex, peer: PeerId, msg: Message) {
        let Some((to, to_peer)) = self.peer_endpoint(from, peer) else {
            return;
        };
        if self.nodes[from].group != self.nodes[to].group {
            self.dropped_across_partition += 1;
            return;
        }
        let event = Event::Message { peer: to_peer, msg };
        let f = self.faults;
        if f.drop_pct == 0 && f.dup_pct == 0 && f.delay_pct == 0 {
            self.events.push_back((to, event));
            return;
        }
        let roll = |rng: &mut rand::rngs::StdRng, pct: u8| {
            pct > 0 && rand::Rng::gen_range(rng, 0u8..100) < pct
        };
        if roll(&mut self.rng, f.drop_pct) {
            self.fault_counts.0 += 1;
            return;
        }
        if roll(&mut self.rng, f.dup_pct) {
            self.fault_counts.1 += 1;
            self.events.push_back((to, event.clone()));
        }
        if roll(&mut self.rng, f.delay_pct) {
            self.fault_counts.2 += 1;
            self.delayed.push((to, event));
        } else {
            self.events.push_back((to, event));
        }
    }

    fn disconnect(&mut self, from: NodeIndex, peer: PeerId, reason: DisconnectReason) {
        self.disconnects.push((from, reason));
        // Deliver the reason first, exactly as the real transport does, so the
        // other end learns why instead of seeing a reset connection.
        if let Some((to, to_peer)) = self.peer_endpoint(from, peer) {
            if self.nodes[from].group == self.nodes[to].group {
                self.events.push_back((
                    to,
                    Event::Message {
                        peer: to_peer,
                        msg: Message::Disconnect(reason),
                    },
                ));
            }
            self.events
                .push_back((to, Event::Disconnected { peer: to_peer }));
        }
        self.links
            .retain(|l| !((l.a == from && l.a_peer == peer) || (l.b == from && l.b_peer == peer)));
        self.events.push_back((from, Event::Disconnected { peer }));
    }

    fn alloc_peer(&mut self) -> PeerId {
        let id = self.next_peer_id;
        self.next_peer_id += 1;
        id
    }
}

/// The address scheme simulated nodes use. Never bound to; it only has to be
/// unique and parseable.
pub fn sim_addr(index: u16) -> SocketAddr {
    format!("127.0.0.1:{}", 40_000 + index)
        .parse()
        .expect("simulated address")
}

/// Build a [`LocalNode`] from a deterministic seed, so a failing test is
/// reproducible and node ids are stable across runs.
pub fn local_node(seed: &str, chain_id: u32, genesis: GenesisId) -> LocalNode {
    let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
    LocalNode::new(sk, chain_id, genesis, format!("cb-sim/{seed}"))
}

// ---------------------------------------------------------------- MemChain

/// A toy chain for the parts of the protocol that have nothing to do with
/// execution.
///
/// A "block" here is a fixed 41-byte record: an 8-byte height, a 32-byte
/// hash, and one byte saying whether it is the real thing or a forgery. The
/// forgery byte is how a test makes a peer serve a block that fails
/// verification without needing a whole executor: `verify_and_apply` refuses
/// it with [`RejectReason::StateRootMismatch`], which is precisely the verdict
/// the real verifier returns when re-execution disagrees with the header.
pub struct MemChain {
    blocks: BTreeMap<u64, Vec<u8>>,
    by_hash: BTreeMap<H256, u64>,
    head: u64,
    txs: HashMap<H256, Vec<u8>>,
    /// Bodies this chain hands out are forged: a test flips this on the node
    /// playing the malicious peer.
    pub serve_forgeries: bool,
}

impl Default for MemChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MemChain {
    /// An empty chain at height 0.
    pub fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            by_hash: BTreeMap::new(),
            head: 0,
            txs: HashMap::new(),
            serve_forgeries: false,
        }
    }

    /// Encode a toy block body.
    pub fn encode_block(height: u64, hash: H256, valid: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(41);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&hash.0);
        out.push(u8::from(valid));
        out
    }

    /// Decode a toy block body.
    pub fn decode_block(bytes: &[u8]) -> Option<(u64, H256, bool)> {
        if bytes.len() != 41 {
            return None;
        }
        let mut h = [0u8; 8];
        h.copy_from_slice(&bytes[0..8]);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[8..40]);
        Some((u64::from_be_bytes(h), H256(hash), bytes[40] == 1))
    }

    /// The canonical hash of the toy block at `height`, so every honest node
    /// agrees on what block 7 is without running a consensus protocol.
    pub fn hash_at(height: u64) -> H256 {
        cb_crypto::keccak256(&height.to_be_bytes())
    }

    /// Append the canonical block at `head + 1` and return its hash. Used by
    /// the simulated sequencer.
    pub fn produce(&mut self) -> (H256, u64) {
        let height = self.head + 1;
        let hash = Self::hash_at(height);
        self.blocks
            .insert(height, Self::encode_block(height, hash, true));
        self.by_hash.insert(hash, height);
        self.head = height;
        (hash, height)
    }

    /// Remember a transaction body so this node can serve it.
    pub fn insert_tx(&mut self, bytes: Vec<u8>) -> H256 {
        let hash = cb_crypto::keccak256(&bytes);
        self.txs.insert(hash, bytes);
        hash
    }
}

impl ChainAccess for MemChain {
    fn head_height(&self) -> u64 {
        self.head
    }

    fn block_bytes(&self, height: u64) -> Option<Vec<u8>> {
        let stored = self.blocks.get(&height)?;
        if self.serve_forgeries {
            let (h, hash, _) = Self::decode_block(stored)?;
            return Some(Self::encode_block(h, hash, false));
        }
        Some(stored.clone())
    }

    fn height_of(&self, hash: &H256) -> Option<u64> {
        self.by_hash.get(hash).copied()
    }

    fn verify_and_apply(&mut self, bytes: &[u8]) -> Result<(H256, u64), RejectReason> {
        let (height, hash, valid) = Self::decode_block(bytes).ok_or(RejectReason::Undecodable)?;
        if !valid {
            return Err(RejectReason::StateRootMismatch);
        }
        if hash != Self::hash_at(height) {
            return Err(RejectReason::StateRootMismatch);
        }
        if height != self.head + 1 {
            return Err(RejectReason::OutOfOrder);
        }
        self.blocks
            .insert(height, Self::encode_block(height, hash, true));
        self.by_hash.insert(hash, height);
        self.head = height;
        Ok((hash, height))
    }

    fn tx_bytes(&self, hash: &H256) -> Option<Vec<u8>> {
        self.txs.get(hash).cloned()
    }

    fn accept_tx(&mut self, bytes: &[u8]) -> Result<H256, RejectReason> {
        if bytes.is_empty() {
            return Err(RejectReason::InvalidTransaction);
        }
        Ok(self.insert_tx(bytes.to_vec()))
    }

    fn remember_tx(&mut self, hash: H256, bytes: Vec<u8>) {
        self.txs.insert(hash, bytes);
    }
}

/// Convenience: a network of `n` `MemChain` nodes where node 0 is the seed and
/// every other node is configured with node 0's address as its only seed.
///
/// This is the star *bootstrap* that peer exchange is supposed to dissolve
/// into a mesh, so tests can assert exactly that.
pub fn mem_network(n: usize, cfg: impl Fn(usize) -> P2pConfig) -> SimNetwork<MemChain> {
    let genesis = GenesisId(H256([7; 32]));
    let mut net = SimNetwork::new();
    for i in 0..n {
        let mut c = cfg(i);
        c.listen = Some(sim_addr(u16::try_from(i).unwrap_or(0)));
        if i > 0 && c.seeds.is_empty() {
            c.seeds = vec![sim_addr(0)];
        }
        let local = local_node(&format!("cb-sim-node-{i}"), 1, genesis);
        net.add(c, local, MemChain::new());
    }
    net
}

/// The node id a [`mem_network`] node will present.
pub fn mem_node_id(i: usize) -> NodeId {
    let (sk, _) = cb_crypto::keypair_from_seed(format!("cb-sim-node-{i}").as_bytes());
    let pk = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &sk);
    NodeId::from_pubkey(&pk)
}
