//! The bounded peer table.
//!
//! The table remembers every node id this node has heard of, whether or not it
//! is currently connected: score and ban state have to outlive a connection,
//! otherwise a banned peer is rehabilitated by the act of reconnecting.
//!
//! It is bounded because an adversary can otherwise make us spend memory for
//! free by gossiping addresses. When the table is full, the entry evicted is
//! the least useful one we are *not* talking to: disconnected first, then
//! lowest score, then least recently seen. A connected peer is never evicted
//! to make room for an address someone merely told us about — a live,
//! block-serving peer outranks a rumour every time.
//!
//! Iteration is over a `BTreeMap` keyed by node id rather than a `HashMap`.
//! That costs a little lookup speed at peer-table scale (hundreds of entries)
//! and buys deterministic ordering, which is what makes the gossip tests
//! reproducible instead of flaky.

use crate::p2p::config::P2pConfig;
use crate::p2p::identity::NodeId;
use crate::p2p::message::PeerAddr;
use crate::p2p::score::{self, Behaviour};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;

/// Local handle for one connection. Allocated by the driver (the transport or
/// the in-process harness); the engine treats it as opaque.
///
/// It identifies a *connection*, not a node: a node that reconnects gets a new
/// `PeerId`, which is exactly what lets the engine notice a duplicate
/// connection from a node id it already holds one for.
pub type PeerId = u64;

/// What the table knows about one node id.
#[derive(Clone, Debug)]
pub struct PeerRecord {
    /// Stable identity.
    pub node_id: NodeId,
    /// Where to dial it, if it advertised a listen address.
    pub addr: Option<SocketAddr>,
    /// Current behaviour score.
    pub score: i32,
    /// Monotonic millisecond timestamp the ban expires at; 0 means not banned.
    pub banned_until_ms: u64,
    /// Monotonic millisecond timestamp before which we do not dial it,
    /// because it turned *us* away. Not a ban: see [`PeerTable::set_backoff`].
    pub dial_backoff_until_ms: u64,
    /// Monotonic millisecond timestamp we last heard from it.
    pub last_seen_ms: u64,
    /// Highest head height it has advertised.
    pub head_height: u64,
    /// The live connection to it, if any.
    pub connection: Option<PeerId>,
    /// Whether the live connection was dialed by us.
    pub outbound: bool,
    /// Monotonic millisecond timestamp before which this peer must not be
    /// chosen as a catch-up source. Set when it fails to serve a range it
    /// claimed it could.
    pub no_range_until_ms: u64,
}

impl PeerRecord {
    /// Whether this record currently has a live connection.
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }
}

/// A bounded, score-carrying directory of peers.
#[derive(Debug)]
pub struct PeerTable {
    records: BTreeMap<NodeId, PeerRecord>,
    by_connection: HashMap<PeerId, NodeId>,
    max_known: usize,
    initial_score: i32,
}

impl PeerTable {
    /// Build an empty table sized by `cfg`.
    pub fn new(cfg: &P2pConfig) -> Self {
        Self {
            records: BTreeMap::new(),
            by_connection: HashMap::new(),
            max_known: cfg.max_known_peers,
            initial_score: cfg.score.initial,
        }
    }

    /// Number of node ids remembered, connected or not.
    pub fn known_count(&self) -> usize {
        self.records.len()
    }

    /// Number of live connections.
    pub fn connected_count(&self) -> usize {
        self.by_connection.len()
    }

    /// The record for `node_id`, if the table has one.
    pub fn get(&self, node_id: &NodeId) -> Option<&PeerRecord> {
        self.records.get(node_id)
    }

    /// The node id behind a live connection.
    pub fn node_of(&self, conn: PeerId) -> Option<NodeId> {
        self.by_connection.get(&conn).copied()
    }

    /// Every live connection, in deterministic node-id order.
    pub fn connections(&self) -> Vec<(NodeId, PeerId)> {
        self.records
            .values()
            .filter_map(|r| r.connection.map(|c| (r.node_id, c)))
            .collect()
    }

    /// Whether `node_id` is currently serving a ban at `now_ms`.
    pub fn is_banned(&self, node_id: &NodeId, now_ms: u64) -> bool {
        self.records
            .get(node_id)
            .is_some_and(|r| r.banned_until_ms > now_ms)
    }

    /// Whether the dialable `addr` belongs to a peer currently serving a ban.
    pub fn is_addr_banned(&self, addr: &SocketAddr, now_ms: u64) -> bool {
        self.records
            .values()
            .any(|r| r.addr.as_ref() == Some(addr) && r.banned_until_ms > now_ms)
    }

    /// Remember a dialable address for a node id, inserting a record if the
    /// node is new. Returns `false` when the table was full of connected peers
    /// and the address had to be dropped.
    pub fn note_address(&mut self, node_id: NodeId, addr: Option<SocketAddr>, now_ms: u64) -> bool {
        if let Some(rec) = self.records.get_mut(&node_id) {
            if addr.is_some() {
                rec.addr = addr;
            }
            rec.last_seen_ms = rec.last_seen_ms.max(now_ms);
            return true;
        }
        if self.records.len() >= self.max_known && !self.evict_one() {
            return false;
        }
        self.records.insert(
            node_id,
            PeerRecord {
                node_id,
                addr,
                score: self.initial_score,
                banned_until_ms: 0,
                dial_backoff_until_ms: 0,
                last_seen_ms: now_ms,
                head_height: 0,
                connection: None,
                outbound: false,
                no_range_until_ms: 0,
            },
        );
        true
    }

    /// Bind a live connection to a node id. Returns `false` if the table is
    /// full and could not make room, in which case the caller must disconnect.
    pub fn attach(
        &mut self,
        node_id: NodeId,
        conn: PeerId,
        addr: Option<SocketAddr>,
        outbound: bool,
        head_height: u64,
        now_ms: u64,
    ) -> bool {
        if !self.note_address(node_id, addr, now_ms) {
            return false;
        }
        let Some(rec) = self.records.get_mut(&node_id) else {
            return false;
        };
        rec.connection = Some(conn);
        rec.outbound = outbound;
        rec.head_height = rec.head_height.max(head_height);
        rec.last_seen_ms = now_ms;
        self.by_connection.insert(conn, node_id);
        true
    }

    /// Release a connection. The record, its score and its ban survive.
    pub fn detach(&mut self, conn: PeerId) -> Option<NodeId> {
        let node_id = self.by_connection.remove(&conn)?;
        if let Some(rec) = self.records.get_mut(&node_id) {
            if rec.connection == Some(conn) {
                rec.connection = None;
            }
        }
        Some(node_id)
    }

    /// Record that we heard from a connection at `now_ms`.
    pub fn touch(&mut self, conn: PeerId, now_ms: u64) {
        if let Some(node_id) = self.by_connection.get(&conn) {
            if let Some(rec) = self.records.get_mut(node_id) {
                rec.last_seen_ms = now_ms;
            }
        }
    }

    /// Note a peer's advertised head height. Heights only move forward here;
    /// a peer that reports a lower head than before is more likely to be
    /// confused than to have rewound, and catch-up targets should not chase it
    /// backwards.
    ///
    /// This number is a *claim*, not a fact — see [`Self::correct_head`].
    pub fn note_head(&mut self, node_id: &NodeId, height: u64) {
        if let Some(rec) = self.records.get_mut(node_id) {
            rec.head_height = rec.head_height.max(height);
        }
    }

    /// Overwrite a peer's recorded head with what it actually demonstrated,
    /// and stand it down as a catch-up source until `until_ms`.
    ///
    /// An advertised head is unverifiable and free to inflate. A peer that
    /// claims `u64::MAX` would otherwise be elected the best catch-up source
    /// forever, answer every range with nothing, take a one-point penalty that
    /// can never ban it, and halt sync — while every honest peer, being
    /// "behind" it, is never asked. Correcting the claim downward to what it
    /// served, and benching it for a timeout, is what makes the claim
    /// self-limiting.
    pub fn correct_head(&mut self, node_id: &NodeId, demonstrated: u64, until_ms: u64) {
        if let Some(rec) = self.records.get_mut(node_id) {
            rec.head_height = demonstrated;
            rec.no_range_until_ms = rec.no_range_until_ms.max(until_ms);
        }
    }

    /// Stand a peer down as a catch-up source until `until_ms`, leaving its
    /// advertised head alone.
    ///
    /// For a peer that did not answer at all, as opposed to one that answered
    /// short ([`Self::correct_head`]): silence says nothing about what it has,
    /// only that asking it again right away is a waste of a timeout.
    pub fn bench_as_source(&mut self, node_id: &NodeId, until_ms: u64) {
        if let Some(rec) = self.records.get_mut(node_id) {
            rec.no_range_until_ms = rec.no_range_until_ms.max(until_ms);
        }
    }

    /// Apply a behaviour to a peer's score.
    ///
    /// Returns the outcome so the caller can disconnect and log without
    /// re-deriving the decision.
    pub fn record_behaviour(
        &mut self,
        node_id: &NodeId,
        behaviour: Behaviour,
        cfg: &crate::p2p::config::ScoreConfig,
        now_ms: u64,
    ) -> ScoreOutcome {
        let Some(rec) = self.records.get_mut(node_id) else {
            return ScoreOutcome {
                score: 0,
                newly_banned: false,
            };
        };
        rec.score = score::apply(rec.score, behaviour, cfg);
        // Benign behaviour — a duplicate announce, a range the peer has
        // pruned — costs reputation but must never be the thing that bans a
        // peer. Gossip is redundant by design, so an honest peer generates
        // these constantly; letting them accumulate into a ban punishes
        // participation.
        if behaviour.is_benign() && score::should_ban(rec.score, cfg) {
            rec.score = cfg.ban_at_or_below.saturating_add(1);
        }
        let newly_banned = score::should_ban(rec.score, cfg) && rec.banned_until_ms <= now_ms;
        if newly_banned {
            rec.banned_until_ms = now_ms.saturating_add(cfg.ban.as_millis() as u64);
            // Reset the score to the initial value so that when the ban
            // expires the peer gets a real second chance rather than being
            // re-banned by the first duplicate announce it sends.
            rec.score = cfg.initial;
        }
        ScoreOutcome {
            score: rec.score,
            newly_banned,
        }
    }

    /// Refuse a peer until `until_ms` without going through the scorer.
    ///
    /// Used when a peer tells us *it* has closed the door: a node that has
    /// banned us, or that is full. Re-dialing it every few hundred
    /// milliseconds helps nobody, and the first live cluster run did exactly
    /// that until this existed.
    ///
    /// This is a dial backoff, not a ban, and is kept apart from
    /// `banned_until_ms` on purpose. It used to be written into the ban
    /// field, so a peer that had merely closed on us — a timeout, a duplicate
    /// connection — was then *refused as banned* when it dialed back, and
    /// told so; the refused side, hearing `Banned`, backed off for the full
    /// ban period in return. One ordinary disconnect escalated into two nodes
    /// exiling each other for five minutes. The lossy-network tests found it
    /// as a node that never rejoined. A peer we are backing off from may
    /// still dial us; we only refrain from dialing it.
    pub fn set_backoff(&mut self, node_id: &NodeId, until_ms: u64) {
        if let Some(rec) = self.records.get_mut(node_id) {
            rec.dial_backoff_until_ms = rec.dial_backoff_until_ms.max(until_ms);
        }
    }

    /// Addresses worth dialing right now: known, dialable, not connected, not
    /// banned, and not already being dialed.
    pub fn dial_candidates(
        &self,
        now_ms: u64,
        in_progress: &std::collections::HashSet<SocketAddr>,
    ) -> Vec<SocketAddr> {
        let mut out: Vec<(i32, SocketAddr)> = self
            .records
            .values()
            .filter(|r| {
                !r.is_connected()
                    && r.banned_until_ms <= now_ms
                    && r.dial_backoff_until_ms <= now_ms
            })
            .filter_map(|r| r.addr.map(|a| (r.score, a)))
            .filter(|(_, a)| !in_progress.contains(a))
            .collect();
        // Best-scoring first: prefer re-dialing peers that have been useful.
        out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        out.into_iter().map(|(_, a)| a).collect()
    }

    /// Up to `limit` dialable peers to share in a `peers` response, excluding
    /// `exclude` (never tell a peer about itself).
    pub fn share(&self, exclude: Option<NodeId>, limit: usize, now_ms: u64) -> Vec<PeerAddr> {
        self.records
            .values()
            .filter(|r| Some(r.node_id) != exclude)
            .filter(|r| r.banned_until_ms <= now_ms)
            .filter_map(|r| {
                r.addr.map(|addr| PeerAddr {
                    node_id: r.node_id,
                    addr,
                })
            })
            .take(limit)
            .collect()
    }

    /// Connections whose peer has been silent since before `deadline_ms`.
    pub fn stale_connections(&self, deadline_ms: u64) -> Vec<PeerId> {
        self.records
            .values()
            .filter(|r| r.last_seen_ms < deadline_ms)
            .filter_map(|r| r.connection)
            .collect()
    }

    /// The connected peer with the highest advertised head above `above`,
    /// preferring the highest head and breaking ties by score then node id so
    /// the choice is deterministic.
    pub fn best_source_above(&self, above: u64, now_ms: u64) -> Option<(NodeId, PeerId, u64)> {
        self.records
            .values()
            .filter(|r| r.head_height > above && r.no_range_until_ms <= now_ms)
            .filter_map(|r| r.connection.map(|c| (r.node_id, c, r.head_height, r.score)))
            .max_by(|a, b| a.2.cmp(&b.2).then(a.3.cmp(&b.3)).then(b.0.cmp(&a.0)))
            .map(|(n, c, h, _)| (n, c, h))
    }

    /// Evict the least useful disconnected record. Returns `false` when every
    /// record is connected and nothing may be evicted.
    fn evict_one(&mut self) -> bool {
        let victim = self
            .records
            .values()
            .filter(|r| !r.is_connected())
            .min_by(|a, b| {
                a.score
                    .cmp(&b.score)
                    .then(a.last_seen_ms.cmp(&b.last_seen_ms))
                    .then(a.node_id.cmp(&b.node_id))
            })
            .map(|r| r.node_id);
        match victim {
            Some(node_id) => {
                self.records.remove(&node_id);
                true
            }
            None => false,
        }
    }
}

/// What a scoring update decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoreOutcome {
    /// The peer's score after the update.
    pub score: i32,
    /// True when this update pushed the peer into a fresh ban.
    pub newly_banned: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::Address;

    fn node(n: u8) -> NodeId {
        NodeId(Address([n; 20]))
    }
    fn addr(n: u16) -> SocketAddr {
        format!("127.0.0.1:{n}").parse().expect("addr")
    }
    fn cfg(max_known: usize) -> P2pConfig {
        P2pConfig {
            max_known_peers: max_known,
            max_peers: max_known,
            target_outbound: 1,
            ..P2pConfig::default()
        }
    }

    #[test]
    fn table_is_bounded_and_evicts_the_worst_disconnected_peer() {
        let c = cfg(3);
        let mut t = PeerTable::new(&c);
        for i in 1..=3u8 {
            assert!(t.note_address(node(i), Some(addr(9000 + i as u16)), 0));
        }
        // Make node 2 the worst.
        t.record_behaviour(&node(2), Behaviour::BadBlock, &c.score, 0);
        assert!(t.note_address(node(4), Some(addr(9004)), 1));
        assert_eq!(t.known_count(), 3);
        assert!(t.get(&node(2)).is_none(), "worst peer should be evicted");
        assert!(t.get(&node(4)).is_some());
    }

    #[test]
    fn a_connected_peer_is_never_evicted_for_a_rumour() {
        let c = cfg(1);
        let mut t = PeerTable::new(&c);
        assert!(t.attach(node(1), 10, Some(addr(9001)), true, 5, 0));
        assert!(
            !t.note_address(node(2), Some(addr(9002)), 1),
            "should refuse rather than drop a live peer"
        );
        assert!(t.get(&node(1)).is_some());
    }

    #[test]
    fn score_and_ban_survive_a_reconnect() {
        let c = cfg(8);
        let mut t = PeerTable::new(&c);
        t.attach(node(1), 10, Some(addr(9001)), true, 0, 0);
        for _ in 0..3 {
            t.record_behaviour(&node(1), Behaviour::BadBlock, &c.score, 0);
        }
        assert!(t.is_banned(&node(1), 0));
        t.detach(10);
        assert!(t.is_banned(&node(1), 0), "ban must outlive the connection");
        let ban_ms = c.score.ban.as_millis() as u64;
        assert!(!t.is_banned(&node(1), ban_ms + 1), "bans expire");
    }

    #[test]
    fn duplicate_announces_alone_never_ban_a_peer() {
        let c = cfg(8);
        let mut t = PeerTable::new(&c);
        t.attach(node(1), 10, Some(addr(9001)), true, 0, 0);
        for _ in 0..10_000 {
            let outcome = t.record_behaviour(&node(1), Behaviour::Duplicate, &c.score, 0);
            assert!(!outcome.newly_banned, "a duplicate must never ban");
        }
        assert!(!t.is_banned(&node(1), 0));
        // But real misbehaviour on top of them still bans.
        for _ in 0..10 {
            t.record_behaviour(&node(1), Behaviour::BadBlock, &c.score, 0);
        }
        assert!(t.is_banned(&node(1), 0));
    }

    #[test]
    fn share_never_returns_the_requester_to_itself() {
        let c = cfg(8);
        let mut t = PeerTable::new(&c);
        t.note_address(node(1), Some(addr(9001)), 0);
        t.note_address(node(2), Some(addr(9002)), 0);
        let shared = t.share(Some(node(1)), 8, 0);
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].node_id, node(2));
    }

    #[test]
    fn best_source_prefers_the_highest_head() {
        let c = cfg(8);
        let mut t = PeerTable::new(&c);
        t.attach(node(1), 11, Some(addr(9001)), true, 5, 0);
        t.attach(node(2), 12, Some(addr(9002)), true, 9, 0);
        let (id, conn, head) = t.best_source_above(4, 0).expect("a source");
        assert_eq!((id, conn, head), (node(2), 12, 9));
        assert!(t.best_source_above(9, 0).is_none());
    }

    #[test]
    fn a_peer_that_cannot_serve_what_it_claimed_stops_being_the_source() {
        let c = cfg(8);
        let mut t = PeerTable::new(&c);
        t.attach(node(1), 11, Some(addr(9001)), true, 5, 0);
        // A liar claiming an impossible head outranks the honest peer.
        t.attach(node(2), 12, Some(addr(9002)), true, u64::MAX, 0);
        assert_eq!(t.best_source_above(0, 0).map(|s| s.0), Some(node(2)));
        // It serves nothing; we correct the claim and bench it.
        t.correct_head(&node(2), 0, 10_000);
        assert_eq!(
            t.best_source_above(0, 0).map(|s| s.0),
            Some(node(1)),
            "the honest peer must become the source"
        );
    }
}
