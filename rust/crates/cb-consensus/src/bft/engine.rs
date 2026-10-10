//! The consensus state machine: one validator's view of the protocol.
//!
//! Pure in the sense the peer-to-peer engine is pure: messages and clock
//! readings go in, messages to send and proposals to execute come out. It
//! opens no sockets and reads no clock, so the whole protocol runs in a
//! deterministic simulator (`sim.rs`) with Byzantine, crashed and partitioned
//! validators, and the node is a thin driver around it.
//!
//! # The protocol, in one page
//!
//! Chained HotStuff with a two-chain commit rule and a timeout certificate
//! pacemaker — the shape of DiemBFT v4 / Jolteon, which is also what
//! Hyperliquid describes HyperBFT as descending from.
//!
//! * **Rounds.** Time is a sequence of rounds. Round `r` has one leader,
//!   `validators[r % n]` unless leader reputation has that validator
//!   standing aside (see [`Engine::leader_for`]), who may make one
//!   proposal. A round ends with a
//!   *quorum certificate* (a quorum voted for the proposal) or a *timeout
//!   certificate* (a quorum gave up), and the next round begins.
//!
//! * **Proposals extend certificates.** A proposal carries the highest
//!   certificate its leader knows, and extends the proposal that certificate
//!   names. If rounds were skipped it also carries the timeout certificate
//!   that ended the previous round, whose embedded certificates prove the
//!   leader is extending something at least as high as any honest validator
//!   is locked on.
//!
//! * **Voting rule.** A validator votes for at most one proposal per round,
//!   never for a round it has already timed out, and only when the proposal
//!   is justified: `round == qc.round + 1`, or `round == tc.round + 1` with
//!   `qc.round >= tc.high_qc_round`.
//!
//! * **Commit rule.** A certified proposal whose parent was certified in the
//!   immediately preceding round commits the parent, and every uncommitted
//!   ancestor with it. Two consecutive certificates: the first says a quorum
//!   accepted the parent, the second says a quorum saw that acceptance, so
//!   any later timeout certificate carries it and every later proposal
//!   extends it.
//!
//! * **Votes are broadcast**, not sent to the next leader. Every validator
//!   forms every certificate itself, which costs `n` extra messages a round
//!   and saves one hop of latency on the commit path. At four validators the
//!   messages are nothing and the hop is everything.
//!
//! # What is decided here and what is not
//!
//! The engine decides ordering. What a payload *is* — transactions, their
//! signatures, whether the proposer's claimed executed root matches ours — is
//! the driver's business, and it tells the engine with a `veto` when it has
//! checked a proposal and will not vote for it. The engine still records a
//! vetoed proposal so the tree stays connected; it just never votes for it.
//!
//! Execution happens after commit, in the driver, on the committed sequence
//! only. Nothing here is ever rolled back.

use super::types::*;
use cb_crypto::SecretKey;
use cb_types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Rounds a validator stands aside from leading after a round it led ended
/// in a timeout certificate; doubled for each further one in a row, up to
/// sixty-four times this.
pub const BENCH_ROUNDS: Round = 128;
/// Doublings of [`BENCH_ROUNDS`] at most: sixty-four times.
const MAX_BENCH_SHIFT: u32 = 6;
/// Timeouts remembered per validator: enough to reach the longest bench.
const MAX_STRIKES: usize = MAX_BENCH_SHIFT as usize + 1;
/// Rounds past the chain's last link (see [`Reputation`]) after which the
/// leader schedule is the plain round robin. With two: after round `p - 1`
/// times out, the proposal for `p` (two past the link at `p - 2`) is still
/// scheduled by reputation; round `p + 1` is the plain round robin's,
/// whether it extends that proposal or `p` timed out as well. So validators
/// that disagree about the schedule lose at most two rounds before they
/// agree, and in normal operation one round after any timeout is led by
/// the plain round robin — a dead validator's turn there costs one more
/// timer, `1/n` of the time, and strikes it again.
pub const FALLBACK_AFTER: Round = 2;
/// How far back the leader schedule looks: a timeout more than this many
/// rounds before the round being scheduled no longer counts. As long as
/// the longest bench, so it changes nothing for a validator whose history
/// is right; for one whose starting snapshot is wrong or missing (a wiped
/// disk, a block-sync catch-up past rounds it never saw) it bounds the
/// disagreement: once this many rounds pass above its root, its schedule
/// is everyone's again.
pub const REPUTATION_WINDOW: Round = BENCH_ROUNDS << MAX_BENCH_SHIFT;

/// Who stands aside from leading, as of one proposal: a fold over the
/// chain of proposals that ends there. See [`Engine::leader_for`].
///
/// Kept per validator in set order: the highest round it led on the chain,
/// and the rounds it was due to lead by the plain round robin that ended in
/// a timeout certificate above that, the latest [`MAX_STRIKES`]; and the
/// highest round on the chain whose proposal extends the round right
/// before it (a *link*: two in a row is what commits). Every entry comes
/// from one header alone, and folding a header in is idempotent and
/// order-insensitive — a join. So a snapshot taken further up the same
/// chain may stand in for an earlier one, which is what lets a restarted
/// validator start from the snapshot it last wrote at its commit point,
/// whatever its executed head: the proposals in between come back from
/// peers and change nothing. And a validator that lacks some history (a
/// wiped disk, a catch-up past rounds it never saw) is exact again once
/// what it missed is older than [`REPUTATION_WINDOW`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reputation {
    led: Vec<Round>,
    failed: Vec<Vec<Round>>,
    last_link: Round,
}

impl Reputation {
    fn sized(mut self, n: usize) -> Self {
        self.led.resize(n, 0);
        self.failed.resize(n, Vec::new());
        self
    }

    /// This snapshot with `h` folded in, or `None` if `h` changes nothing.
    /// The plain round robin's leader of the round a carried timeout
    /// certificate ended takes a strike. A substitute that failed is
    /// blamed as the validator it stood in for: wrong, but a function of
    /// the header alone, so every validator blames the same one whatever
    /// it believes about who stands aside — and a substitute that keeps
    /// failing is caught by the fallback in [`Reputation::leader`]. `h`'s
    /// proposer, having led a round on the chain, loses its strikes.
    fn with(&self, set: &ValidatorSet, h: &ProposalHeader) -> Option<Self> {
        let vals = set.as_slice();
        let mut next: Option<Self> = None;
        if let Some(tc) = &h.tc {
            let i = (tc.round % vals.len() as u64) as usize;
            let f = tc.round;
            if f > self.led[i] && !self.failed[i].contains(&f) {
                let r = next.get_or_insert_with(|| self.clone());
                r.failed[i].push(f);
                r.failed[i].sort_unstable();
                if r.failed[i].len() > MAX_STRIKES {
                    r.failed[i].remove(0);
                }
            }
        }
        if let Some(i) = vals.iter().position(|v| *v == h.proposer) {
            if h.round > next.as_ref().unwrap_or(self).led[i] {
                let r = next.get_or_insert_with(|| self.clone());
                r.led[i] = h.round;
                r.failed[i].retain(|f| *f > h.round);
            }
        }
        if h.qc.round + 1 == h.round && h.round > self.last_link {
            next.get_or_insert_with(|| self.clone()).last_link = h.round;
        }
        next
    }

    /// Whether validator `i` stands aside in `round`.
    fn benched(&self, i: usize, round: Round) -> bool {
        let Some(failed) = self.failed.get(i) else {
            return false;
        };
        let recent = failed
            .iter()
            .filter(|f| f.saturating_add(REPUTATION_WINDOW) > round);
        let strikes = recent.clone().count() as u32;
        let Some(last) = recent.max() else {
            return false;
        };
        let stretch = BENCH_ROUNDS << (strikes - 1).min(MAX_BENCH_SHIFT);
        round <= last.saturating_add(stretch)
    }

    /// The leader of `round` in a proposal extending the proposal this
    /// snapshot was taken at: the round robin over those not standing
    /// aside, or over everyone if nobody is left.
    ///
    /// Or over everyone if the round is more than [`FALLBACK_AFTER`] rounds
    /// past the chain's last link: then the chain is not committing,
    /// whatever the reason, and the plain round robin needs no history to
    /// agree on. A dead or misbehaving validator costs a round timer in its
    /// turn while it lasts, and takes the strike that benches it once the
    /// chain moves. Without this, two validators whose snapshots differ
    /// could disagree about every round for as long as a bench lasts, and a
    /// substitute failing every one of its rounds would leave the blame on
    /// the validators it replaced.
    fn leader(&self, set: &ValidatorSet, round: Round) -> Address {
        let vals = set.as_slice();
        if round > self.last_link.saturating_add(FALLBACK_AFTER)
            || self.failed.iter().all(|f| f.is_empty())
        {
            return set.leader(round);
        }
        let active: Vec<Address> = vals
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.benched(*i, round))
            .map(|(_, v)| *v)
            .collect();
        if active.is_empty() {
            return set.leader(round);
        }
        active[(round % active.len() as u64) as usize]
    }
}

/// Tunables. Milliseconds throughout.
#[derive(Clone, Debug)]
pub struct Config {
    /// Round timer after a round that ended with a certificate.
    pub base_timeout_ms: u64,
    /// Ceiling on the doubled timer after consecutive timeouts.
    pub max_timeout_ms: u64,
    /// How many rounds behind the commit point proposals stay available for
    /// a peer that asks. Beyond this a lagging validator needs block sync.
    pub retain_rounds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_timeout_ms: 1_000,
            max_timeout_ms: 16_000,
            retain_rounds: 4_096,
        }
    }
}

/// Where a validator's proposal tree starts: the chain's genesis on a fresh
/// node, or the last proposal it executed on a restarted one. Starting from
/// the executed head is what lets a validator rejoin from what it has on
/// disk: it needs only the proposals since, which peers keep for
/// [`Config::retain_rounds`] rounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: H256,
    pub round: Round,
    pub height: u64,
    /// The certificate for `id`. [`QuorumCert::genesis`] at the genesis.
    pub cert: QuorumCert,
    /// Who stands aside from leading as of `id` (see [`Reputation`]):
    /// what [`Engine::committed_reputation`] returned when the node last
    /// wrote it, or empty on a fresh chain.
    pub reputation: Reputation,
}

impl Root {
    pub fn genesis(genesis_id: H256) -> Self {
        Self {
            id: genesis_id,
            round: 0,
            height: 0,
            cert: QuorumCert::genesis(genesis_id),
            reputation: Reputation::default(),
        }
    }

    /// The same root, starting from a reputation snapshot the node wrote
    /// earlier on the same chain.
    pub fn with_reputation(mut self, reputation: Reputation) -> Self {
        self.reputation = reputation;
        self
    }

    /// The root an executed block's finality proof describes.
    pub fn from_finality(f: &Finality) -> Self {
        Self {
            id: f.header.id(),
            round: f.header.round,
            height: f.header.height,
            cert: f.cert.clone(),
            reputation: Reputation::default(),
        }
    }
}

/// What the driver must do.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Action {
    /// Send to every validator.
    Broadcast(Message),
    /// This node leads the current round and may propose. The driver calls
    /// [`Engine::propose`] when it has a payload, or after a short idle
    /// wait, and must do so well inside the round timer.
    ReadyToPropose {
        round: Round,
        parent_id: H256,
        /// Neither the parent nor the committed proposal still waiting for
        /// its certificate carried transactions. Otherwise follow at once:
        /// the next certificate commits the parent, or seals the waiting
        /// one's block.
        parent_empty: bool,
    },
    /// Proposals are final, in this order, each with the certificate to
    /// seal into its block. The first may be one already handed over by
    /// [`Action::Ordered`] and executed; the rest are new: execute the
    /// non-empty ones.
    Commit(Vec<Committed>),
    /// A proposal is final and follows everything committed so far, but its
    /// certificate is not known yet (see [`Committed`]). Execute it now if
    /// it is non-empty; seal its block when the next [`Action::Commit`]
    /// names it, with the certificate given there.
    Ordered(Ordered),
    /// This node is missing a proposal it needs. Ask the network.
    RequestProposal {
        id: H256,
        /// How many of its parents to ask for as well: none for a proposal
        /// whose certificate arrived first, a stretch when walking an
        /// orphan chain back towards the commit point.
        ancestors: u32,
    },
}

/// A committed proposal with its certificate: the one carried in the header
/// of its successor on the committed chain, so every validator records the
/// same bytes.
///
/// # Why the certificate comes one commit behind
///
/// A proposal can be certified twice, by different quorums, and a validator
/// commits it on whichever certified child it sees first — a child that may
/// itself be abandoned. The certificate a validator happens to hold is
/// therefore its own; the one carried by the proposal's committed child is
/// everyone's, because the committed chain is. The certificate goes into
/// the block header's hashed preimage, so honest validators' block hashes
/// diverge unless it is that one. So the newest committed proposal is handed
/// over at once as [`Action::Ordered`] — what execution needs is fixed at
/// commit — and again, with its certificate, in the [`Action::Commit`] that
/// the next commit produces. Only the sealing of its block waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    pub header: ProposalHeader,
    pub payload: Vec<u8>,
    pub cert: QuorumCert,
}

/// A committed proposal whose certificate is still to come: see
/// [`Action::Ordered`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ordered {
    pub header: ProposalHeader,
    pub payload: Vec<u8>,
}

impl Committed {
    pub fn finality(&self) -> Finality {
        Finality {
            header: self.header.clone(),
            cert: self.cert.clone(),
        }
    }
}

/// Why a message was dropped. Surfaced for tests and for peer scoring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dropped {
    Stale,
    Duplicate,
    BadSignature,
    NotTheLeader,
    BadCertificate(BftError),
    Unjustified,
    PayloadMismatch,
    BadHeight,
    TimestampRegressed,
    Equivocation,
    NotInSet,
    /// A vote or timeout for a round further ahead than proposals are
    /// retained: nothing this node could do with it, and buffering it
    /// would let one member fill memory with rounds that never come.
    FarFuture,
}

struct Node {
    header: Option<ProposalHeader>,
    payload: Vec<u8>,
    round: Round,
    height: u64,
    parent_id: H256,
    timestamp: u64,
    /// Who stands aside from leading as of this proposal, folded along the
    /// chain from the root.
    rep: Arc<Reputation>,
}

pub struct Engine {
    cfg: Config,
    set: ValidatorSet,
    me: Address,
    key: SecretKey,
    genesis_id: H256,
    hash_payload: fn(&[u8]) -> H256,

    round: Round,
    high_qc: QuorumCert,
    last_tc: Option<TimeoutCert>,
    last_voted_round: Round,
    deadline_ms: u64,
    consecutive_timeouts: u32,
    timed_out_round: Option<Round>,
    proposed_round: Option<Round>,
    /// This node leads `round` but the proposal it must extend has not
    /// arrived yet; prompt again when it does.
    awaiting_parent: Option<Round>,

    tree: HashMap<H256, Node>,
    children: HashMap<H256, Vec<H256>>,
    /// The first proposal seen from each proposer in each round: a second,
    /// different one is equivocation.
    by_round: HashMap<(Round, Address), H256>,
    /// Proposals whose parent has not arrived, by the parent they wait for,
    /// each with the driver's veto: the verdict was given on arrival and
    /// must still stand when the parent turns up.
    orphans: HashMap<H256, Vec<(Proposal, Option<String>)>>,
    /// Every orphan's id and its parent, so a proposal already held is
    /// recognised without hashing the orphan lists, and a missing ancestor
    /// is asked for once, at the bottom of the chain of orphans, rather
    /// than once per orphan above it.
    orphan_ids: HashMap<H256, H256>,
    /// The orphan each proposer has waiting in each round. One at most,
    /// as in the tree: without it a member could park any number of
    /// distinct justified proposals for one round with a validator that
    /// lacks their parent — one rejoining after a restart, say.
    orphan_slots: HashMap<(Round, Address), H256>,
    /// Proposals asked for, when, and with how many ancestors, so an
    /// unanswered request is repeated.
    requested: HashMap<H256, (u64, u32)>,
    /// Certificates seen, by the proposal they certify. The first one seen
    /// is kept; any quorum is as good as any other.
    certified: HashMap<H256, QuorumCert>,
    votes: BTreeMap<Round, HashMap<H256, Vec<Vote>>>,
    /// What each validator voted for in each round: a second vote for the
    /// same proposal is a repeat, for another one it is equivocation.
    voters: BTreeMap<Round, HashMap<Address, H256>>,
    timeouts: BTreeMap<Round, Vec<Timeout>>,
    timeout_senders: BTreeMap<Round, HashSet<Address>>,
    tcs: HashMap<Round, TimeoutCert>,

    committed_id: H256,
    committed_round: Round,
    committed_height: u64,
    /// The highest committed proposal, handed over as [`Action::Ordered`]
    /// but not yet as [`Committed`]: its certificate is the one its
    /// committed successor carries, which the next commit names.
    withheld: Option<(ProposalHeader, Vec<u8>)>,
    /// Rounds this node was told to drop a message for, for diagnostics.
    pub dropped: Vec<(Round, Dropped)>,
    /// Actions produced by construction, held until the driver takes them.
    pending: Vec<Action>,
    /// The latest clock reading any input carried.
    now_ms: u64,
}

impl Engine {
    /// `genesis_id` is the chain's genesis identity, which every round-0
    /// certificate names; `root` is where this node's tree starts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        set: ValidatorSet,
        me: Address,
        key: SecretKey,
        genesis_id: H256,
        root: Root,
        hash_payload: fn(&[u8]) -> H256,
        now_ms: u64,
    ) -> Self {
        let mut tree = HashMap::new();
        tree.insert(
            root.id,
            Node {
                header: None,
                payload: Vec::new(),
                round: root.round,
                height: root.height,
                parent_id: H256::ZERO,
                timestamp: 0,
                rep: Arc::new(root.reputation.clone().sized(set.len())),
            },
        );
        let mut e = Self {
            cfg,
            set,
            me,
            key,
            genesis_id,
            hash_payload,
            round: root.round,
            high_qc: root.cert.clone(),
            last_tc: None,
            last_voted_round: root.round,
            deadline_ms: 0,
            consecutive_timeouts: 0,
            timed_out_round: None,
            proposed_round: None,
            awaiting_parent: None,
            tree,
            children: HashMap::new(),
            by_round: HashMap::new(),
            orphans: HashMap::new(),
            orphan_ids: HashMap::new(),
            orphan_slots: HashMap::new(),
            requested: HashMap::new(),
            certified: HashMap::new(),
            votes: BTreeMap::new(),
            voters: BTreeMap::new(),
            timeouts: BTreeMap::new(),
            timeout_senders: BTreeMap::new(),
            tcs: HashMap::new(),
            committed_id: root.id,
            committed_round: root.round,
            committed_height: root.height,
            withheld: None,
            dropped: Vec::new(),
            pending: Vec::new(),
            now_ms,
        };
        e.certified.insert(root.id, root.cert.clone());
        let mut out = Vec::new();
        e.enter_round(root.round + 1, now_ms, false, &mut out);
        e.pending = out;
        e
    }

    // ----- observers -----------------------------------------------------

    pub fn round(&self) -> Round {
        self.round
    }
    pub fn high_qc(&self) -> &QuorumCert {
        &self.high_qc
    }
    /// The highest round this node has voted or timed out in. With
    /// [`Engine::high_qc`] it is what a restart must be told (see
    /// [`Engine::restore_safety`]).
    pub fn last_voted_round(&self) -> Round {
        self.last_voted_round
    }
    pub fn committed_round(&self) -> Round {
        self.committed_round
    }
    pub fn committed_height(&self) -> u64 {
        self.committed_height
    }
    pub fn committed_id(&self) -> H256 {
        self.committed_id
    }
    pub fn me(&self) -> Address {
        self.me
    }
    pub fn validators(&self) -> &ValidatorSet {
        &self.set
    }
    pub fn is_leader(&self) -> bool {
        self.leader_for(self.round) == self.me
    }

    /// Who leads `round` if it extends this node's highest certificate:
    /// the validator set's round robin, skipping any validator standing
    /// aside.
    ///
    /// # Leader reputation
    ///
    /// A dead validator's turn would otherwise cost a full round timer every
    /// `n` rounds — with four validators and a one-second timer, a quarter
    /// of all rounds — for as long as it stays dead. So a validator whose
    /// round ended in a timeout certificate stands aside from leading for a
    /// stretch of rounds that doubles each time it happens again in a row,
    /// and is reset once it leads a round on the chain.
    ///
    /// # Whose schedule
    ///
    /// The schedule for round `r` is a function of the proposal a round-`r`
    /// proposal extends and that proposal's ancestors ([`Reputation`] is a
    /// fold along the chain), not of anything a validator learns on its
    /// own. Every validator that could vote for a proposal holds its
    /// parent's chain — an orphan gets no vote — so every one of them
    /// reaches the same verdict on whether its proposer leads; and a leader
    /// judges itself on its highest certificate, which is exactly the
    /// parent its proposal will carry. Validators whose highest
    /// certificates differ can disagree about who should propose, never
    /// about whether a given proposal is the leader's: at worst two
    /// validators both propose, the votes split, the round times out, and
    /// the timeout certificate settles which chain the next round extends.
    ///
    /// A restarted validator starts its fold from the snapshot it wrote to
    /// disk ([`Engine::committed_reputation`], [`Root::reputation`]). One
    /// with no snapshot or a stale one (a wiped disk, a catch-up by block
    /// sync) may disagree, but only about timeouts it never saw, and those
    /// stop counting [`REPUTATION_WINDOW`] rounds later. If every validator
    /// were standing aside the plain round robin applies.
    pub fn leader_for(&self, round: Round) -> Address {
        self.leader_on(round, &self.high_qc.id)
    }

    /// The validators standing aside from leading `round` if it extends
    /// this node's highest certificate, fallback aside (see
    /// [`Engine::leader_for`]).
    pub fn standing_aside(&self, round: Round) -> Vec<Address> {
        let Some(n) = self
            .tree
            .get(&self.high_qc.id)
            .or_else(|| self.tree.get(&self.committed_id))
        else {
            return Vec::new();
        };
        self.set
            .as_slice()
            .iter()
            .enumerate()
            .filter(|(i, _)| n.rep.benched(*i, round))
            .map(|(_, v)| *v)
            .collect()
    }

    /// Who leads `round` in a proposal extending `parent`. A parent this
    /// node does not hold is judged by its commit point.
    fn leader_on(&self, round: Round, parent: &H256) -> Address {
        match self
            .tree
            .get(parent)
            .or_else(|| self.tree.get(&self.committed_id))
        {
            Some(n) => n.rep.leader(&self.set, round),
            None => self.set.leader(round),
        }
    }

    /// Who stands aside as of this node's commit point. The driver writes
    /// it to disk whenever it changes and hands it back through
    /// [`Root::with_reputation`] on a restart.
    pub fn committed_reputation(&self) -> Reputation {
        self.tree
            .get(&self.committed_id)
            .map(|n| (*n.rep).clone())
            .unwrap_or_default()
    }

    /// Whether a leader extending `parent` can take its time: `parent`
    /// carried no transactions, and neither did the committed proposal
    /// still waiting for its certificate. Otherwise the next certificate
    /// commits the parent, or brings the certificate that seals the waiting
    /// one's block (its state is already executed; its block body, head
    /// hash and announcement wait for this).
    fn parent_empty(&self, parent: &H256) -> bool {
        let parent_empty = self
            .tree
            .get(parent)
            .and_then(|n| n.header.as_ref())
            .map(|h| h.is_empty())
            .unwrap_or(true);
        parent_empty && self.withheld.as_ref().is_none_or(|(h, _)| h.is_empty())
    }

    /// Diagnostics: proposals held, orphans waiting, requests outstanding.
    pub fn tree_len(&self) -> usize {
        self.tree.len()
    }
    pub fn orphan_count(&self) -> usize {
        self.orphans.values().map(|v| v.len()).sum()
    }
    /// Diagnostics: the lowest and highest round among orphans, and the
    /// lowest round among outstanding requests' known children.
    pub fn orphan_rounds(&self) -> (Round, Round) {
        let rounds = self.orphans.values().flatten().map(|(p, _)| p.header.round);
        (rounds.clone().min().unwrap_or(0), rounds.max().unwrap_or(0))
    }
    pub fn committed_id_known(&self) -> bool {
        self.tree.contains_key(&self.committed_id)
    }
    pub fn outstanding_requests(&self) -> usize {
        self.requested.len()
    }

    /// When the round timer fires next. The driver ticks at or before this.
    pub fn next_deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
    /// Actions produced by construction (the first leader's prompt). Take
    /// them once.
    pub fn take_initial_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pending)
    }

    /// What this node remembered from before a restart: the highest round
    /// it voted or timed out in, and the highest certificate it held when
    /// it last did. Call once, after construction, before taking the
    /// initial actions.
    ///
    /// # Why a restart must remember these
    ///
    /// The tree restarts from the executed head, but this node may have
    /// voted in rounds above it before it went down. Without the floor it
    /// would vote again in those rounds — for a different proposal, if the
    /// leader equivocated or a timeout certificate let another leader in —
    /// and two votes from one validator in one round is exactly what the
    /// quorum intersection argument assumes never happens. Without the
    /// certificate, its next timeout would carry a stale one, and a timeout
    /// certificate built on it could justify a proposal that abandons a
    /// parent this node had already voted to commit. The driver writes both
    /// to disk before any vote or timeout leaves the process.
    pub fn restore_safety(&mut self, voted_round: Round, high_qc: QuorumCert) {
        self.last_voted_round = self.last_voted_round.max(voted_round);
        if high_qc.round > self.high_qc.round {
            // Adopted like any other certificate: it raises the round, and
            // the proposal it names is asked for if the tree lacks it.
            let mut out = Vec::new();
            let now = self.now_ms;
            self.process_qc(high_qc, now, &mut out);
            self.pending.extend(out);
        }
    }

    /// Whether this node holds the proposal, in its tree or waiting for a
    /// parent. The driver skips validating one it already holds.
    pub fn holds(&self, id: &H256) -> bool {
        self.tree.contains_key(id) || self.orphan_ids.contains_key(id)
    }
    /// A proposal this node holds and up to `ancestors` of its parents,
    /// oldest first, so a peer can add them to its tree without orphaning
    /// any. Stops at the first one this node no longer holds.
    pub fn proposal_chain(&self, id: &H256, ancestors: u32) -> Vec<Proposal> {
        let mut out = Vec::new();
        let mut cur = *id;
        for _ in 0..=ancestors {
            let Some(p) = self.proposal(&cur) else { break };
            cur = p.header.parent_id;
            out.push(p);
        }
        out.reverse();
        out
    }

    /// A proposal this node holds, for a peer that asked.
    pub fn proposal(&self, id: &H256) -> Option<Proposal> {
        let n = self.tree.get(id)?;
        let header = n.header.clone()?;
        Some(Proposal {
            header,
            payload: n.payload.clone(),
        })
    }
    /// The height the next non-empty proposal on the certified chain would
    /// execute at.
    pub fn next_height(&self) -> u64 {
        self.tree
            .get(&self.high_qc.id)
            .map(|n| n.height + 1)
            .unwrap_or(self.committed_height + 1)
    }

    // ----- inputs --------------------------------------------------------

    /// A message from another validator. `veto` is the driver's verdict on a
    /// proposal's payload and executed root: `Some(reason)` records the
    /// proposal but never votes for it.
    pub fn on_message(&mut self, msg: Message, veto: Option<&str>, now_ms: u64) -> Vec<Action> {
        self.now_ms = self.now_ms.max(now_ms);
        match msg {
            Message::Proposal(p) => self.on_proposal(p, veto, now_ms),
            Message::Vote(v) => self.on_vote(v, now_ms),
            Message::Timeout(t) => self.on_timeout_msg(t, now_ms),
            Message::ProposalRequest { .. } | Message::TxBatch { .. } => Vec::new(),
        }
    }

    /// The clock moved.
    pub fn on_tick(&mut self, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.now_ms = self.now_ms.max(now_ms);
        self.retry_requests(now_ms, &mut out);
        if now_ms < self.deadline_ms {
            return out;
        }
        // The timer fired. Time the round out, or if that is already done,
        // say so again in case the first message was lost, and push the
        // timer out for the next repeat.
        let round = self.round;
        if self.timed_out_round != Some(round) {
            self.local_timeout(round, now_ms, &mut out);
        } else if let Some(t) = self
            .timeouts
            .get(&round)
            .and_then(|ts| ts.iter().find(|t| t.voter == self.me).cloned())
        {
            out.push(Action::Broadcast(Message::Timeout(t)));
        }
        // And pass on the timeouts that ended the previous round, if that is
        // how this node got here. A peer still in that round — the
        // certificate's messages lost on the way to it — cannot otherwise
        // leave it: this node's own timeout names a round the peer has not
        // reached, and alone it is short of the `f + 1` that would make the
        // peer join. Each timeout is signed by its sender, so any validator
        // may forward it. Without this, validators split across two or three
        // rounds after a lossy spell stayed there for good.
        if let Some(tc) = self.last_tc.as_ref().filter(|tc| tc.round + 1 == round) {
            for t in &tc.timeouts {
                out.push(Action::Broadcast(Message::Timeout(t.clone())));
            }
        }
        self.deadline_ms = now_ms + self.timeout_for(self.consecutive_timeouts + 1);
        out
    }

    /// Make this round's proposal. Only the leader of the current round
    /// may, once. `payload_len` is the number of transactions in `payload`;
    /// zero is an empty proposal that produces no executed block.
    pub fn propose(
        &mut self,
        payload: Vec<u8>,
        payload_len: u32,
        timestamp: u64,
        exec_height: u64,
        exec_root: H256,
        now_ms: u64,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        self.now_ms = self.now_ms.max(now_ms);
        let round = self.round;
        if self.leader_for(round) != self.me || self.proposed_round == Some(round) {
            return out;
        }
        if self.timed_out_round == Some(round) {
            return out;
        }
        // A leader votes for its own proposal, so a round it has voted in
        // is one it may already have proposed in — before a restart, say.
        // A second proposal would be equivocation by an honest node.
        if round <= self.last_voted_round {
            return out;
        }
        let parent = match self.tree.get(&self.high_qc.id) {
            Some(p) => p,
            None => {
                // Votes can outrun the proposal they are for, so this node
                // may hold the certificate before the proposal. Ask for it
                // — or for whatever below it is missing, if it is already
                // waiting as an orphan — and propose when it lands.
                self.awaiting_parent = Some(round);
                let missing = self.lowest_missing(self.high_qc.id);
                out.extend(if missing == self.high_qc.id {
                    self.request(missing)
                } else {
                    self.request_chain(missing)
                });
                return out;
            }
        };
        self.awaiting_parent = None;
        let tc = if round == self.high_qc.round + 1 {
            None
        } else {
            match &self.last_tc {
                Some(tc) if tc.round.checked_add(1) == Some(round) => Some(tc.clone()),
                // Nothing justifies proposing in this round.
                _ => return out,
            }
        };
        let mut header = ProposalHeader {
            round,
            height: parent.height + u64::from(payload_len > 0),
            parent_id: self.high_qc.id,
            payload_hash: if payload_len == 0 {
                H256::ZERO
            } else {
                (self.hash_payload)(&payload)
            },
            payload_len,
            timestamp: timestamp.max(parent.timestamp),
            exec_height,
            exec_root,
            qc: self.high_qc.clone(),
            tc,
            proposer: self.me,
            sig: Vec::new(),
        };
        header.sign_with(&self.key);
        self.proposed_round = Some(round);
        let proposal = Proposal { header, payload };
        out.push(Action::Broadcast(Message::Proposal(proposal.clone())));
        out.extend(self.on_proposal(proposal, None, now_ms));
        out
    }

    // ----- proposals -----------------------------------------------------

    fn on_proposal(&mut self, p: Proposal, veto: Option<&str>, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        // Orphans adopted by this proposal are processed from a worklist,
        // not by recursion: a validator that was away for a while adopts a
        // chain of them at once, and a chain that deep does not fit a
        // stack.
        let mut work = vec![(p, veto.map(|s| s.to_string()))];
        while let Some((p, veto)) = work.pop() {
            if let Some(id) = self.on_one_proposal(p, veto.as_deref(), now_ms, &mut out) {
                if let Some(waiting) = self.orphans.remove(&id) {
                    for (child, child_veto) in waiting {
                        // No longer an orphan: it is about to be looked at
                        // as a fresh arrival, and must not be taken for a
                        // repeat of itself.
                        let child_id = child.header.id();
                        self.orphan_ids.remove(&child_id);
                        let slot = (child.header.round, child.header.proposer);
                        if self.orphan_slots.get(&slot) == Some(&child_id) {
                            self.orphan_slots.remove(&slot);
                        }
                        work.push((child, child_veto));
                    }
                }
            }
        }
        out
    }

    /// Returns the proposal's id when it was added to the tree.
    fn on_one_proposal(
        &mut self,
        p: Proposal,
        veto: Option<&str>,
        now_ms: u64,
        out: &mut Vec<Action>,
    ) -> Option<H256> {
        let h = &p.header;
        let round = h.round;
        if round <= self.committed_round {
            self.drop(round, Dropped::Stale);
            return None;
        }
        let id = h.id();
        if self.tree.contains_key(&id) || self.orphan_ids.contains_key(&id) {
            self.drop(round, Dropped::Duplicate);
            return None;
        }
        // Signed by the validator it names, who is in the set.
        if h.signer().ok() != Some(h.proposer) || !self.set.contains(&h.proposer) {
            self.drop(round, Dropped::BadSignature);
            return None;
        }
        if h.parent_id != h.qc.id || h.qc.round >= round {
            self.drop(round, Dropped::Unjustified);
            return None;
        }
        if let Err(e) = self.check_qc(&h.qc) {
            self.drop(round, Dropped::BadCertificate(e));
            return None;
        }
        // A timeout certificate it carries must end the previous round and
        // must verify, whether or not the proposal needs it: it is processed
        // below either way, moves this node's round, is trusted byte for
        // byte from then on, and counts against a leader's reputation. One
        // taken unchecked from a proposal its QC already justified let any
        // member jump a validator to a round of its choosing and then
        // justify a proposal below that validator's lock with it.
        if let Some(tc) = &h.tc {
            if tc.round.checked_add(1) != Some(round) {
                self.drop(round, Dropped::Unjustified);
                return None;
            }
            if let Err(e) = self.check_tc(tc) {
                self.drop(round, Dropped::BadCertificate(e));
                return None;
            }
        }
        // Justification: the certificate is for the previous round, or a
        // timeout certificate ends the previous round and the certificate is
        // at least as high as any it carries.
        let justified = round == h.qc.round + 1
            || h.tc
                .as_ref()
                .is_some_and(|tc| h.qc.round >= tc.high_qc_round());
        if !justified {
            self.drop(round, Dropped::Unjustified);
            return None;
        }
        let payload_ok = if h.payload_len == 0 {
            p.payload.is_empty() && h.payload_hash == H256::ZERO
        } else {
            !p.payload.is_empty() && (self.hash_payload)(&p.payload) == h.payload_hash
        };
        if !payload_ok {
            self.drop(round, Dropped::PayloadMismatch);
            return None;
        }

        // The certificates it carries move this node forward whether or not
        // the proposal itself is accepted.
        let qc = h.qc.clone();
        self.process_qc(qc, now_ms, out);
        if let Some(tc) = h.tc.clone() {
            self.process_tc(tc, now_ms, out);
        }

        let parent = match self.tree.get(&h.parent_id) {
            Some(parent) => parent,
            None => {
                // Held until the parent arrives. What is asked for is the
                // lowest ancestor not held: the parent may itself be an
                // orphan waiting on its own parent, and asking for it again
                // would be answered with a stretch of proposals this node
                // already has, each to be validated a second time. A
                // validator rebuilding thousands of rounds under load was
                // doing exactly that, sixty-four times over per round.
                let parent_id = h.parent_id;
                // Equivocation, as in the tree below, unless certified.
                let slot = (round, h.proposer);
                if let Some(first) = self.orphan_slots.get(&slot) {
                    if *first != id && !self.certified.contains_key(&id) {
                        self.drop(round, Dropped::Equivocation);
                        return None;
                    }
                }
                self.orphan_slots.insert(slot, id);
                self.orphan_ids.insert(id, parent_id);
                self.orphans
                    .entry(parent_id)
                    .or_default()
                    .push((p, veto.map(str::to_string)));
                //
                // And only one stretch at a time: the proposals of one
                // answer can arrive out of order, and each one whose
                // predecessor is still on its way would otherwise ask for
                // another sixty-four ancestors it is about to receive. While
                // a stretch is in flight a missing parent is asked for on
                // its own (the certificate above already did that), and the
                // retry picks up whatever the stretch did not bring.
                let missing = self.lowest_missing(parent_id);
                if !self.chain_in_flight() {
                    let req = self.request_chain(missing);
                    out.extend(req);
                }
                return None;
            }
        };
        // Whether this node would vote for it: only the round's leader, by
        // the schedule the parent's chain gives (see `leader_for`), gets a
        // vote. A justified proposal from any other member of the set is
        // still stored: only the leader's can be certified, and a stored
        // proposal is what keeps the tree connected if this node is wrong
        // (its reputation snapshot stale). Storing costs at most one
        // proposal per member per round, so a member spamming proposals
        // wastes its own effort.
        let from_leader = h.proposer == parent.rep.leader(&self.set, round);
        let rep = match parent.rep.with(&self.set, h) {
            Some(r) => Arc::new(r),
            None => parent.rep.clone(),
        };
        if h.height != parent.height + u64::from(h.payload_len > 0) {
            self.drop(round, Dropped::BadHeight);
            return None;
        }
        if h.timestamp < parent.timestamp {
            self.drop(round, Dropped::TimestampRegressed);
            return None;
        }
        if let Some(first) = self.by_round.get(&(round, h.proposer)) {
            // Two proposals for one round from one proposer. The first one
            // seen stands; a quorum cannot form for both, and this is the
            // evidence a slashing rule would want. Unless this one is
            // certified: then it is the one a quorum accepted, whatever
            // this node saw first, and refusing it would leave the tree
            // disconnected for good. (A leader that restarted mid-round and
            // proposed again did exactly that to itself.)
            if *first != id && !self.certified.contains_key(&id) {
                self.drop(round, Dropped::Equivocation);
                return None;
            }
        }

        self.by_round.insert((round, h.proposer), id);
        self.children.entry(h.parent_id).or_default().push(id);
        self.tree.insert(
            id,
            Node {
                header: Some(p.header.clone()),
                payload: p.payload,
                round,
                height: h.height,
                parent_id: h.parent_id,
                timestamp: h.timestamp,
                rep,
            },
        );

        // Vote, if this node still may.
        let may_vote = veto.is_none()
            && from_leader
            && round == self.round
            && round > self.last_voted_round
            && self.timed_out_round != Some(round);
        if let Some(reason) = veto {
            tracing::warn!(round, reason, "not voting for this proposal");
        }
        if may_vote {
            self.last_voted_round = round;
            let vote = Vote::sign(&self.key, self.me, round, id);
            out.push(Action::Broadcast(Message::Vote(vote.clone())));
            let more = self.on_vote(vote, now_ms);
            out.extend(more);
        }

        // A certificate may already exist for it, or for a child that was
        // waiting on it.
        self.try_commit_at(id, out);

        // The leader of the current round may have been waiting for exactly
        // this proposal — or was waiting for it to learn whether it leads.
        if self.awaiting_parent == Some(self.round) && id == self.high_qc.id {
            self.awaiting_parent = None;
            if self.leader_for(self.round) == self.me {
                out.push(Action::ReadyToPropose {
                    round: self.round,
                    parent_id: id,
                    parent_empty: self.parent_empty(&id),
                });
            }
        }
        Some(id)
    }

    // ----- votes and certificates ----------------------------------------

    fn on_vote(&mut self, v: Vote, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        if v.round <= self.committed_round {
            self.drop(v.round, Dropped::Stale);
            return out;
        }
        if !self.set.contains(&v.voter) {
            self.drop(v.round, Dropped::NotInSet);
            return out;
        }
        if v.signer().ok() != Some(v.voter) {
            self.drop(v.round, Dropped::BadSignature);
            return out;
        }
        if v.round > self.round.saturating_add(self.cfg.retain_rounds) {
            self.drop(v.round, Dropped::FarFuture);
            return out;
        }
        if self.certified.contains_key(&v.id) {
            return out;
        }
        // One vote per validator per round. A second vote for another
        // proposal is equivocation; for the same one, a repeat.
        let earlier = self
            .voters
            .get(&v.round)
            .and_then(|m| m.get(&v.voter))
            .copied();
        match earlier {
            Some(first) if first == v.id => {
                self.drop(v.round, Dropped::Duplicate);
                return out;
            }
            Some(first) => {
                // The first vote stands; the second is the evidence a
                // slashing rule would want.
                tracing::warn!(round = v.round, voter = %v.voter, first = %first, second = %v.id, "a validator voted twice in one round");
                self.drop(v.round, Dropped::Equivocation);
                return out;
            }
            None => {
                self.voters
                    .entry(v.round)
                    .or_default()
                    .insert(v.voter, v.id);
            }
        }
        let bucket = self
            .votes
            .entry(v.round)
            .or_default()
            .entry(v.id)
            .or_default();
        bucket.push(v.clone());
        if bucket.len() >= self.set.quorum() {
            let qc = QuorumCert {
                round: v.round,
                id: v.id,
                votes: bucket.clone(),
            };
            self.process_qc(qc, now_ms, &mut out);
        }
        out
    }

    /// Adopt a certificate: record it, raise the high certificate, advance
    /// the round, and commit whatever it commits. The certificate has been
    /// verified or was formed here.
    fn process_qc(&mut self, qc: QuorumCert, now_ms: u64, out: &mut Vec<Action>) {
        let id = qc.id;
        self.certified.entry(id).or_insert_with(|| qc.clone());
        if qc.round > self.high_qc.round {
            self.high_qc = qc.clone();
        }
        if qc.round >= self.round {
            self.enter_round(qc.round + 1, now_ms, false, out);
        }
        self.try_commit_at(id, out);
        if !self.holds(&id) && qc.round > self.committed_round {
            out.extend(self.request(id));
        }
    }

    /// Apply the commit rule at `id`: if `id` is certified and its parent
    /// was proposed in the round before, the parent and its ancestors are
    /// final. Also tried for each child of `id`, since a child may have been
    /// certified while `id` was still missing.
    fn try_commit_at(&mut self, id: H256, out: &mut Vec<Action>) {
        let mut candidates = vec![id];
        if let Some(kids) = self.children.get(&id) {
            candidates.extend(kids.iter().copied());
        }
        for c in candidates {
            if !self.certified.contains_key(&c) {
                continue;
            }
            let (round, parent_id) = match self.tree.get(&c) {
                Some(n) => (n.round, n.parent_id),
                None => continue,
            };
            let parent = match self.tree.get(&parent_id) {
                Some(p) => p,
                None => continue,
            };
            if parent.round + 1 != round || parent.round <= self.committed_round {
                continue;
            }
            self.commit_through(parent_id, out);
        }
    }

    /// Commit `tip` and every uncommitted ancestor, oldest first. Each is
    /// handed to the driver as [`Committed`] once its successor on the
    /// committed chain is known, with the certificate that successor
    /// carries; `tip` goes over at once as [`Action::Ordered`], for
    /// execution, and as [`Committed`] with the next commit.
    fn commit_through(&mut self, tip: H256, out: &mut Vec<Action>) {
        let mut chain: Vec<H256> = Vec::new();
        let mut cur = tip;
        loop {
            if cur == self.committed_id {
                break;
            }
            let node = match self.tree.get(&cur) {
                Some(n) => n,
                None => {
                    // An ancestor is missing: the chain cannot be executed
                    // in order yet. Ask for it and wait.
                    out.extend(self.request_chain(cur));
                    return;
                }
            };
            if node.round <= self.committed_round {
                // Reached a fork below the commit point. That cannot happen
                // under the safety argument; refuse rather than diverge.
                tracing::error!(
                    round = node.round,
                    committed = self.committed_round,
                    "commit rule reached a proposal below the commit point"
                );
                return;
            }
            chain.push(cur);
            cur = node.parent_id;
        }
        chain.reverse();
        let mut committed = Vec::with_capacity(chain.len());
        for id in &chain {
            let node = self.tree.get(id).expect("walked");
            let header = node.header.clone().expect("non-genesis");
            let (round, height, payload) = (node.round, node.height, node.payload.clone());
            if let Some((prev, prev_payload)) = self.withheld.take() {
                // `header` extends `prev` (the walk stopped at the commit
                // point, which `prev` is), and a proposal's certificate
                // names its parent (anything else is dropped as
                // unjustified), so its certificate is `prev`'s.
                committed.push(Committed {
                    header: prev,
                    payload: prev_payload,
                    cert: header.qc.clone(),
                });
            }
            self.committed_id = *id;
            self.committed_round = round;
            self.committed_height = height;
            self.withheld = Some((header, payload));
        }
        if !committed.is_empty() {
            out.push(Action::Commit(committed));
        }
        if let Some((header, payload)) = &self.withheld {
            if chain.last() == Some(&header.id()) {
                out.push(Action::Ordered(Ordered {
                    header: header.clone(),
                    payload: payload.clone(),
                }));
            }
        }
        if !chain.is_empty() {
            self.prune();
        }
    }

    // ----- timeouts ------------------------------------------------------

    fn on_timeout_msg(&mut self, t: Timeout, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        if t.round < self.round.saturating_sub(1) || t.round <= self.committed_round {
            self.drop(t.round, Dropped::Stale);
            return out;
        }
        if t.round > self.round.saturating_add(self.cfg.retain_rounds) {
            self.drop(t.round, Dropped::FarFuture);
            return out;
        }
        if !self.set.contains(&t.voter) {
            self.drop(t.round, Dropped::NotInSet);
            return out;
        }
        if t.signer().ok() != Some(t.voter) {
            self.drop(t.round, Dropped::BadSignature);
            return out;
        }
        if let Err(e) = self.check_qc(&t.high_qc) {
            self.drop(t.round, Dropped::BadCertificate(e));
            return out;
        }
        if !self
            .timeout_senders
            .entry(t.round)
            .or_default()
            .insert(t.voter)
        {
            self.drop(t.round, Dropped::Duplicate);
            return out;
        }
        let round = t.round;
        let high = t.high_qc.clone();
        let bucket = self.timeouts.entry(round).or_default();
        bucket.push(t);
        let count = bucket.len();
        let quorum = self.set.quorum();
        let honest = self.set.honest_threshold();

        // Its certificate may be news.
        self.process_qc(high, now_ms, &mut out);

        if count >= quorum && !self.tcs.contains_key(&round) {
            let tc = TimeoutCert {
                round,
                timeouts: self.timeouts[&round].clone(),
            };
            self.process_tc(tc, now_ms, &mut out);
        } else if count >= honest && round >= self.round && self.timed_out_round != Some(round) {
            // At least one honest validator gave up on this round. Join it,
            // so a quorum of timeouts forms without waiting for our own
            // timer.
            if round > self.round {
                self.enter_round(round, now_ms, true, &mut out);
            }
            self.local_timeout(round, now_ms, &mut out);
        }
        out
    }

    fn process_tc(&mut self, tc: TimeoutCert, now_ms: u64, out: &mut Vec<Action>) {
        self.tcs.entry(tc.round).or_insert_with(|| tc.clone());
        if tc.round >= self.round {
            let Some(next) = tc.round.checked_add(1) else {
                return;
            };
            self.last_tc = Some(tc.clone());
            self.enter_round(next, now_ms, true, out);
        }
    }

    fn local_timeout(&mut self, round: Round, now_ms: u64, out: &mut Vec<Action>) {
        self.timed_out_round = Some(round);
        // Having given up on the round, never vote in it.
        self.last_voted_round = self.last_voted_round.max(round);
        let t = Timeout::sign(&self.key, self.me, round, self.high_qc.clone());
        out.push(Action::Broadcast(Message::Timeout(t.clone())));
        out.extend(self.on_timeout_msg(t, now_ms));
    }

    // ----- rounds --------------------------------------------------------

    fn enter_round(
        &mut self,
        round: Round,
        now_ms: u64,
        after_timeout: bool,
        out: &mut Vec<Action>,
    ) {
        if round <= self.round && self.deadline_ms != 0 {
            return;
        }
        self.round = round;
        self.consecutive_timeouts = if after_timeout {
            self.consecutive_timeouts + 1
        } else {
            0
        };
        self.deadline_ms = now_ms + self.timeout_for(self.consecutive_timeouts);
        if after_timeout {
            // Timeouts for the round we are leaving cannot help any more.
        } else {
            self.last_tc = None;
        }
        if !self.tree.contains_key(&self.high_qc.id) {
            // Whether this node leads depends on the chain it would extend,
            // which has not arrived: decide when it does.
            self.awaiting_parent = Some(round);
        } else if self.leader_for(round) == self.me {
            out.push(Action::ReadyToPropose {
                round,
                parent_id: self.high_qc.id,
                parent_empty: self.parent_empty(&self.high_qc.id),
            });
        }
    }

    fn timeout_for(&self, consecutive: u32) -> u64 {
        let shift = consecutive.min(16);
        self.cfg
            .base_timeout_ms
            .saturating_mul(1u64 << shift)
            .min(self.cfg.max_timeout_ms)
    }

    // ----- helpers -------------------------------------------------------

    /// A certificate is taken on trust only when it is, byte for byte, one
    /// this node already verified. Merely naming a certified proposal is
    /// not enough: the round is what a certificate moves this node to, and
    /// a member could wrap a certified id in a certificate with any round
    /// at all — to send everyone to a round that never ends, or to extend
    /// a certified proposal on a dead fork from a round of its choosing.
    fn check_qc(&self, qc: &QuorumCert) -> Result<(), BftError> {
        if self.certified.get(&qc.id).is_some_and(|known| known == qc) {
            return Ok(());
        }
        qc.verify(&self.set, self.genesis_id)
    }

    /// Likewise: a timeout certificate is trusted only when it is the one
    /// already held for its round. The embedded certificates are what a
    /// proposal after a timeout must reach, and a fabricated set of low
    /// ones would let it extend below what honest validators are locked on.
    fn check_tc(&self, tc: &TimeoutCert) -> Result<(), BftError> {
        if self.tcs.get(&tc.round).is_some_and(|known| known == tc) {
            return Ok(());
        }
        tc.verify(&self.set, self.genesis_id)
    }

    /// Whether a request for a stretch of ancestors went out recently
    /// enough that its answer may still be arriving.
    fn chain_in_flight(&self) -> bool {
        let retry_after = (self.cfg.base_timeout_ms / 4).max(50);
        self.requested
            .values()
            .any(|(at, anc)| *anc > 0 && *at != 0 && self.now_ms < at + retry_after)
    }

    /// The first proposal at or below `id` that this node does not hold:
    /// `id` itself, or the parent that the bottom of its chain of orphans
    /// is waiting for.
    fn lowest_missing(&self, id: H256) -> H256 {
        let mut cur = id;
        while let Some(parent) = self.orphan_ids.get(&cur) {
            cur = *parent;
        }
        cur
    }

    /// Proposals asked for at once. Each request for a missing ancestor is
    /// answered with a stretch of its parents (`REQUEST_ANCESTORS`), so a
    /// validator rebuilding thousands of rounds needs only a few in flight;
    /// more than this turns a peer's answers into a flood that stalls it.
    const MAX_OUTSTANDING_REQUESTS: usize = 64;

    /// Parents asked for along with a missing ancestor.
    pub const REQUEST_ANCESTORS: u32 = 64;

    fn request(&mut self, id: H256) -> Vec<Action> {
        self.request_with(id, 0)
    }

    /// Ask for a missing ancestor and a stretch of its parents.
    fn request_chain(&mut self, id: H256) -> Vec<Action> {
        self.request_with(id, Self::REQUEST_ANCESTORS)
    }

    fn request_with(&mut self, id: H256, ancestors: u32) -> Vec<Action> {
        let outstanding = self.requested.len();
        match self.requested.entry(id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                if outstanding >= Self::MAX_OUTSTANDING_REQUESTS {
                    // Remembered as missing; asked for by `retry_requests`
                    // when there is room.
                    e.insert((0, ancestors));
                    return Vec::new();
                }
                e.insert((self.now_ms, ancestors));
                vec![Action::RequestProposal { id, ancestors }]
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                // Already asked for on its own (its certificate arrived
                // first); now it is a missing ancestor and the walk wants
                // its parents too. Ask again, with them, at once.
                if ancestors > e.get().1 {
                    *e.get_mut() = (self.now_ms, ancestors);
                    vec![Action::RequestProposal { id, ancestors }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// How far the highest certificate this node holds is above what it has
    /// committed: the rounds it is missing.
    pub fn behind_by(&self) -> Round {
        self.high_qc.round.saturating_sub(self.committed_round)
    }

    /// Ask again for anything still missing. A request can be lost or reach
    /// a peer that is down; the tree must not stay disconnected for it.
    fn retry_requests(&mut self, now_ms: u64, out: &mut Vec<Action>) {
        let retry_after = (self.cfg.base_timeout_ms / 4).max(50);
        let (tree, orphan_ids) = (&self.tree, &self.orphan_ids);
        self.requested
            .retain(|id, _| !tree.contains_key(id) && !orphan_ids.contains_key(id));
        let mut missing: Vec<(u64, H256, u32)> = self
            .requested
            .iter()
            .filter(|(_, (at, _))| now_ms >= *at + retry_after)
            .map(|(id, (at, anc))| (*at, *id, *anc))
            .collect();
        // Oldest first, and no more than the cap in flight per retry.
        missing.sort_unstable();
        for (_, id, ancestors) in missing.into_iter().take(Self::MAX_OUTSTANDING_REQUESTS) {
            self.requested.insert(id, (now_ms, ancestors));
            out.push(Action::RequestProposal { id, ancestors });
        }
    }

    /// Sizes of every buffer the engine keeps, for tests that bound them.
    #[cfg(test)]
    pub(crate) fn buffer_sizes(&self) -> BufferSizes {
        BufferSizes {
            tree: self.tree.len(),
            children: self.children.values().map(|v| v.len()).sum(),
            by_round: self.by_round.len(),
            orphans: self.orphan_count(),
            orphan_ids: self.orphan_ids.len(),
            orphan_slots: self.orphan_slots.len(),
            requested: self.requested.len(),
            certified: self.certified.len(),
            vote_rounds: self.votes.len(),
            votes: self
                .votes
                .values()
                .flat_map(|m| m.values())
                .map(|v| v.len())
                .sum(),
            voters: self.voters.values().map(|m| m.len()).sum(),
            timeout_rounds: self.timeouts.len(),
            timeouts: self.timeouts.values().map(|v| v.len()).sum(),
            timeout_senders: self.timeout_senders.values().map(|s| s.len()).sum(),
            tcs: self.tcs.len(),
            dropped: self.dropped.len(),
            pending: self.pending.len(),
        }
    }

    fn drop(&mut self, round: Round, why: Dropped) {
        tracing::debug!(
            round,
            ?why,
            committed = self.committed_round,
            current = self.round,
            "dropped"
        );
        if self.dropped.len() < 1_024 {
            self.dropped.push((round, why));
        }
    }

    /// Forget what nothing can need: votes and timeouts below the commit
    /// point, and proposals older than the retention window.
    fn prune(&mut self) {
        let floor = self.committed_round;
        self.votes = self.votes.split_off(&floor);
        self.voters = self.voters.split_off(&floor);
        self.timeouts = self.timeouts.split_off(&floor);
        self.timeout_senders = self.timeout_senders.split_off(&floor);
        self.tcs.retain(|r, _| *r >= floor);
        let keep_from = self.committed_round.saturating_sub(self.cfg.retain_rounds);
        let genesis = self.genesis_id;
        let committed = self.committed_id;
        let stale: Vec<H256> = self
            .tree
            .iter()
            .filter(|(id, n)| n.round < keep_from && **id != genesis && **id != committed)
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            if let Some(n) = self.tree.remove(id) {
                self.children.remove(id);
                if let Some(h) = &n.header {
                    self.by_round.remove(&(n.round, h.proposer));
                }
                self.certified.remove(id);
                self.requested.remove(id);
            }
        }
        let orphan_ids = &mut self.orphan_ids;
        self.orphans.retain(|_, v| {
            v.retain(|(p, _)| {
                let keep = p.header.round > floor;
                if !keep {
                    orphan_ids.remove(&p.header.id());
                }
                keep
            });
            !v.is_empty()
        });
        self.orphan_slots.retain(|(r, _), _| *r > floor);
    }
}

/// See [`Engine::buffer_sizes`].
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct BufferSizes {
    pub tree: usize,
    pub children: usize,
    pub by_round: usize,
    pub orphans: usize,
    pub orphan_ids: usize,
    pub orphan_slots: usize,
    pub requested: usize,
    pub certified: usize,
    pub vote_rounds: usize,
    pub votes: usize,
    pub voters: usize,
    pub timeout_rounds: usize,
    pub timeouts: usize,
    pub timeout_senders: usize,
    pub tcs: usize,
    pub dropped: usize,
    pub pending: usize,
}
