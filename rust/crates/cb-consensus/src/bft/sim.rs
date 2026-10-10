//! A deterministic network of engines, for tests.
//!
//! Every message is delayed by a seeded random amount, may be dropped, may
//! be held back by a partition, and a validator may be crashed, restarted or
//! made to equivocate. Time is a counter the simulator advances to the next
//! event, so a run is reproducible from its seed and takes as long as the
//! CPU needs rather than as long as the timeouts say.
//!
//! What it checks after any scenario: every validator's committed sequence
//! is a prefix of every other's (agreement), the committed bytes for one
//! proposal are identical everywhere (so executed blocks will be too), and
//! heights step by exactly one per non-empty proposal.

use super::engine::{Action, Committed, Config, Engine, Reputation, Root};
use super::types::*;
use cb_crypto::{address_from_pubkey, keccak256, keypair_from_seed, SecretKey};
use cb_types::{Address, H256};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub validators: usize,
    pub base_timeout_ms: u64,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Per-message drop probability in parts per thousand.
    pub drop_permille: u32,
    pub seed: u64,
    /// Per-message probability, in parts per thousand, that the network
    /// delivers a second copy with its own delay.
    pub dup_permille: u32,
    /// Per-message probability, in parts per thousand, that the network
    /// also delivers some earlier message again (a replay).
    pub replay_permille: u32,
    /// Per-message probability, in parts per thousand, that the message is
    /// held up for up to twenty times `max_delay_ms`, so it overtakes
    /// nothing and is overtaken by everything sent after it.
    pub slow_permille: u32,
    /// Each engine's `Config::retain_rounds`.
    pub retain_rounds: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            validators: 4,
            base_timeout_ms: 200,
            min_delay_ms: 1,
            max_delay_ms: 5,
            drop_permille: 0,
            seed: 1,
            dup_permille: 0,
            replay_permille: 0,
            slow_permille: 0,
            retain_rounds: 256,
        }
    }
}

/// What a validator's payload looks like for a round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payloads {
    /// Every proposal carries transactions.
    Busy,
    /// No proposal carries transactions.
    Idle,
    /// Odd rounds carry transactions, even rounds do not.
    Alternating,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum Item {
    Msg(Message),
    /// The validator's driver decided to propose.
    Propose {
        round: Round,
    },
    /// `from` asks `to` for a proposal and `ancestors` of its parents. On
    /// the wire the transport knows who asked; here it is carried
    /// explicitly.
    Request {
        id: H256,
        ancestors: u32,
        from: usize,
    },
}

#[derive(Clone, Debug)]
struct InFlight {
    at: u64,
    seq: u64,
    to: usize,
    msg: Item,
}

impl PartialEq for InFlight {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for InFlight {}
impl PartialOrd for InFlight {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for InFlight {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(o.at, o.seq))
    }
}

pub struct Sim {
    pub cfg: SimConfig,
    pub keys: Vec<SecretKey>,
    pub set: ValidatorSet,
    engines: Vec<Option<Engine>>,
    pub now_ms: u64,
    queue: BinaryHeap<Reverse<InFlight>>,
    seq: u64,
    rng: u64,
    /// `(from, to)` pairs that cannot talk.
    cut: HashSet<(usize, usize)>,
    /// Validators whose proposals are split into two versions.
    equivocators: HashSet<usize>,
    /// Validators whose driver vetoes every peer's proposal from the given
    /// round on, as it would one whose executed root differed from its own.
    vetoed: HashMap<usize, Round>,
    pub payloads: Payloads,
    pub committed: Vec<Vec<Committed>>,
    /// What each validator's driver executed, in order: the proposals of
    /// every `Action::Ordered` and those an `Action::Commit` hands over
    /// for the first time. Executed-but-unsealed state is lost in a crash,
    /// so a restart truncates this to what was sealed.
    pub executed: Vec<Vec<H256>>,
    /// What each validator's driver has on disk for a restart: the highest
    /// round it voted or timed out in and its highest certificate at that
    /// moment, written before the vote or timeout went out.
    pub safety: Vec<Option<(Round, QuorumCert)>>,
    /// And the leader-reputation snapshot at its commit point, with the
    /// round it was taken at, rewritten whenever the commit point moves.
    pub reputation: Vec<Option<(Round, Reputation)>>,
    pub genesis_id: H256,
    pub messages_sent: u64,
    /// Votes each validator broadcast, and how many of them were for
    /// proposals other than its own.
    pub votes_sent: Vec<u64>,
    pub foreign_votes: Vec<u64>,
    /// The last vote each validator broadcast.
    pub last_vote: Vec<Option<Vote>>,
    own_proposals: Vec<HashSet<H256>>,
    /// Proposals delivered to each validator in answer to its requests.
    pub served: Vec<u64>,
    /// Print every delivery and action whose round is in this range.
    pub trace_rounds: Option<(Round, Round)>,
    request_rr: usize,
    /// Validators that vote for every proposal they see near their round,
    /// whatever they voted before (see [`Sim::double_vote`]).
    double_voters: HashSet<usize>,
    /// Ids a double voter already signed, so it signs each once.
    double_signed: HashSet<(usize, Round, H256)>,
    /// The most recent messages sent, for replays.
    replay_ring: Vec<Message>,
    /// Every vote each validator's engine cast, by round, across restarts.
    vote_log: HashMap<(usize, Round), H256>,
    /// Every proposal each validator's engine made, by round.
    proposal_log: HashMap<(usize, Round), H256>,
    /// The id each round's certificates named, from every certificate a
    /// proposal carried.
    cert_log: HashMap<Round, H256>,
    /// Per live engine: round, high certificate round, committed round and
    /// last voted round at the last observation. Reset on restart.
    watermarks: Vec<Option<[u64; 4]>>,
    /// Everything the invariant checks in `apply` found, as
    /// `(validator, description)`. A test decides which validators it
    /// holds to them: a double voter's own votes land here by design.
    pub violations: Vec<(usize, String)>,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Self {
        let mut keys = Vec::new();
        let mut addrs = Vec::new();
        for i in 0..cfg.validators {
            let (sk, pk) =
                keypair_from_seed(format!("sim-validator-{}-{}", cfg.seed, i).as_bytes());
            keys.push(sk);
            addrs.push(address_from_pubkey(&pk));
        }
        let set = ValidatorSet::new(addrs).unwrap();
        let genesis_id = keccak256(b"sim-genesis");
        let mut sim = Self {
            engines: Vec::new(),
            committed: vec![Vec::new(); cfg.validators],
            executed: vec![Vec::new(); cfg.validators],
            keys,
            set,
            now_ms: 0,
            queue: BinaryHeap::new(),
            seq: 0,
            rng: cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            cut: HashSet::new(),
            equivocators: HashSet::new(),
            vetoed: HashMap::new(),
            payloads: Payloads::Busy,
            safety: vec![None; cfg.validators],
            reputation: vec![None; cfg.validators],
            genesis_id,
            messages_sent: 0,
            votes_sent: vec![0; cfg.validators],
            foreign_votes: vec![0; cfg.validators],
            last_vote: vec![None; cfg.validators],
            own_proposals: vec![HashSet::new(); cfg.validators],
            served: vec![0; cfg.validators],
            trace_rounds: None,
            request_rr: 0,
            double_voters: HashSet::new(),
            double_signed: HashSet::new(),
            replay_ring: Vec::new(),
            vote_log: HashMap::new(),
            proposal_log: HashMap::new(),
            cert_log: HashMap::new(),
            watermarks: vec![None; cfg.validators],
            violations: Vec::new(),
            cfg,
        };
        for i in 0..sim.cfg.validators {
            let e = sim.fresh_engine(i, Root::genesis(genesis_id));
            sim.engines.push(Some(e));
        }
        for i in 0..sim.cfg.validators {
            let acts = sim.engines[i].as_mut().unwrap().take_initial_actions();
            sim.apply(i, acts);
        }
        sim
    }

    fn fresh_engine(&self, i: usize, root: Root) -> Engine {
        Engine::new(
            Config {
                base_timeout_ms: self.cfg.base_timeout_ms,
                max_timeout_ms: self.cfg.base_timeout_ms * 8,
                retain_rounds: self.cfg.retain_rounds,
            },
            self.set.clone(),
            self.set.as_slice()[i],
            self.keys[i],
            self.genesis_id,
            root,
            keccak256,
            self.now_ms,
        )
    }

    pub fn engine(&self, i: usize) -> Option<&Engine> {
        self.engines[i].as_ref()
    }

    /// Direct access, for a test that feeds one engine by hand. Actions the
    /// call returns are the caller's; hand them to [`Sim::apply_actions`] to
    /// let the network carry them.
    pub fn engine_mut(&mut self, i: usize) -> Option<&mut Engine> {
        self.engines[i].as_mut()
    }

    /// Carry out actions a test obtained from [`Sim::engine_mut`].
    pub fn apply_actions(&mut self, i: usize, actions: Vec<Action>) {
        self.apply(i, actions);
    }

    /// Deliver `msg` to `to` after `delay_ms`, from outside the validator
    /// set's links: no partition or loss applies. What an adversary with a
    /// direct connection sends.
    pub fn inject(&mut self, to: usize, msg: Message, delay_ms: u64) {
        self.seq += 1;
        self.queue.push(Reverse(InFlight {
            at: self.now_ms + delay_ms,
            seq: self.seq,
            to,
            msg: Item::Msg(msg),
        }));
    }

    /// Validator `i` becomes a double voter: it signs a vote for every
    /// proposal it receives for a round near its own, besides whatever its
    /// engine votes, and it is shown both halves of an equivocating
    /// leader's proposals. Two votes in one round from one validator is the
    /// fault the quorum intersection argument counts.
    pub fn double_vote(&mut self, i: usize) {
        self.double_voters.insert(i);
    }

    /// A crashed validator comes back with its disk wiped of the safety
    /// record: a Byzantine fault, since it may vote again in rounds it
    /// already voted in.
    pub fn restart_amnesiac(&mut self, i: usize) {
        self.safety[i] = None;
        self.reputation[i] = None;
        self.restart(i);
    }

    /// A deterministic random number, for tests that draw a scenario from
    /// the simulation's seed.
    pub fn rand(&mut self) -> u64 {
        self.next_rand()
    }

    pub fn address(&self, i: usize) -> Address {
        self.set.as_slice()[i]
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    // ----- faults --------------------------------------------------------

    pub fn crash(&mut self, i: usize) {
        self.engines[i] = None;
    }

    /// A crashed validator comes back with what its driver keeps on disk
    /// and nothing else: its tree starts at the last proposal it committed,
    /// it is told the highest round it voted in and the certificate it held
    /// then (see `Engine::restore_safety`), and it starts from the
    /// leader-reputation snapshot it last wrote (see `Engine::leader_for`).
    pub fn restart(&mut self, i: usize) {
        let root = self.committed[i]
            .last()
            .map(|c| Root::from_finality(&c.finality()))
            .unwrap_or_else(|| Root::genesis(self.genesis_id));
        let root = match &self.reputation[i] {
            Some((_, rep)) => root.with_reputation(rep.clone()),
            None => root,
        };
        let mut e = self.fresh_engine(i, root);
        if let Some((voted, high_qc)) = self.safety[i].clone() {
            e.restore_safety(voted, high_qc.clone());
            if e.last_voted_round() < voted || e.high_qc().round < high_qc.round {
                self.violations.push((
                    i,
                    format!("restart forgot its safety record (voted {voted})"),
                ));
            }
        }
        self.watermarks[i] = None;
        self.executed[i] = self.committed[i].iter().map(|c| c.header.id()).collect();
        self.engines[i] = Some(e);
        let acts = self.engines[i].as_mut().unwrap().take_initial_actions();
        self.apply(i, acts);
    }

    /// From `round` on, validator `i`'s driver vetoes every proposal from a
    /// peer: the engine records them but never votes for them.
    pub fn veto(&mut self, i: usize, round: Round) {
        self.vetoed.insert(i, round);
    }

    pub fn is_up(&self, i: usize) -> bool {
        self.engines[i].is_some()
    }

    /// Cut every link between the two groups, both directions.
    pub fn partition(&mut self, a: &[usize], b: &[usize]) {
        for x in a {
            for y in b {
                self.cut.insert((*x, *y));
                self.cut.insert((*y, *x));
            }
        }
    }

    pub fn heal(&mut self) {
        self.cut.clear();
    }

    pub fn equivocate(&mut self, i: usize) {
        self.equivocators.insert(i);
    }

    // ----- the clock -----------------------------------------------------

    /// Run until `until_ms`, delivering messages and firing timers.
    pub fn run_until(&mut self, until_ms: u64) {
        loop {
            let next_msg = self.queue.peek().map(|Reverse(m)| m.at);
            let next_deadline = self
                .engines
                .iter()
                .flatten()
                .map(|e| e.next_deadline_ms())
                .min();
            let next = match (next_msg, next_deadline) {
                (Some(m), Some(d)) => m.min(d),
                (Some(m), None) => m,
                (None, Some(d)) => d,
                (None, None) => return,
            };
            if next > until_ms {
                self.now_ms = until_ms;
                return;
            }
            self.now_ms = self.now_ms.max(next);
            // Deliver everything due now.
            while let Some(Reverse(m)) = self.queue.peek() {
                if m.at > self.now_ms {
                    break;
                }
                let Reverse(m) = self.queue.pop().unwrap();
                self.deliver(m);
            }
            // Fire every timer due now.
            for i in 0..self.engines.len() {
                let due = self.engines[i]
                    .as_ref()
                    .map(|e| e.next_deadline_ms() <= self.now_ms)
                    .unwrap_or(false);
                if due {
                    let acts = self.engines[i].as_mut().unwrap().on_tick(self.now_ms);
                    self.apply(i, acts);
                }
            }
        }
    }

    pub fn run_for(&mut self, ms: u64) {
        let until = self.now_ms + ms;
        self.run_until(until);
    }

    fn deliver(&mut self, m: InFlight) {
        let to = m.to;
        if self.engines[to].is_none() {
            return;
        }
        let acts = match m.msg {
            Item::Propose { round } => {
                let (payload, len) = match self.payloads {
                    Payloads::Busy => (format!("r{round}:v{to}").into_bytes(), 1u32),
                    Payloads::Idle => (Vec::new(), 0),
                    Payloads::Alternating if round % 2 == 1 => {
                        (format!("r{round}:v{to}").into_bytes(), 1)
                    }
                    Payloads::Alternating => (Vec::new(), 0),
                };
                let exec_height = self.committed[to]
                    .last()
                    .map(|c| c.header.height)
                    .unwrap_or(0);
                let now = self.now_ms;
                self.engines[to].as_mut().unwrap().propose(
                    payload,
                    len,
                    now,
                    exec_height,
                    H256::ZERO,
                    now,
                )
            }
            Item::Request {
                id,
                ancestors,
                from,
            } => {
                // The peer that was asked answers with what it has of the
                // proposal and its ancestors, oldest first.
                let chain = self.engines[to]
                    .as_ref()
                    .map(|e| e.proposal_chain(&id, ancestors))
                    .unwrap_or_default();
                for p in chain {
                    self.served[from] += 1;
                    self.send(to, from, Message::Proposal(p));
                }
                return;
            }
            Item::Msg(Message::ProposalRequest { .. }) | Item::Msg(Message::TxBatch { .. }) => {
                return
            }
            Item::Msg(msg) => {
                self.trace(to, &format!("recv {}", describe(&msg)), msg_round(&msg));
                let veto = match &msg {
                    Message::Proposal(p)
                        if self.vetoed.get(&to).is_some_and(|r| p.header.round >= *r) =>
                    {
                        Some("vetoed by the test")
                    }
                    _ => None,
                };
                let forged = match &msg {
                    Message::Proposal(p) if self.double_voters.contains(&to) => {
                        Some((p.header.round, p.header.id()))
                    }
                    _ => None,
                };
                let acts = self.engines[to]
                    .as_mut()
                    .unwrap()
                    .on_message(msg, veto, self.now_ms);
                if let Some((round, id)) = forged {
                    let near = self.engines[to]
                        .as_ref()
                        .is_some_and(|e| round + 2 >= e.round());
                    if near && self.double_signed.insert((to, round, id)) {
                        let v = Vote::sign(&self.keys[to], self.address(to), round, id);
                        self.broadcast(to, Message::Vote(v));
                    }
                }
                acts
            }
        };
        self.apply(to, acts);
    }

    fn send(&mut self, from: usize, to: usize, msg: Message) {
        if from != usize::MAX && self.cut.contains(&(from, to)) {
            return;
        }
        if self.cfg.drop_permille > 0 && (self.next_rand() % 1000) < self.cfg.drop_permille as u64 {
            return;
        }
        let copies = if self.cfg.dup_permille > 0
            && (self.next_rand() % 1000) < self.cfg.dup_permille as u64
        {
            2
        } else {
            1
        };
        if self.cfg.replay_permille > 0 {
            if (self.next_rand() % 1000) < self.cfg.replay_permille as u64
                && !self.replay_ring.is_empty()
            {
                let k = (self.next_rand() % self.replay_ring.len() as u64) as usize;
                let old = self.replay_ring[k].clone();
                let delay = self.delay();
                self.enqueue(to, old, delay);
            }
            const RING: usize = 512;
            if self.replay_ring.len() < RING {
                self.replay_ring.push(msg.clone());
            } else {
                let k = (self.next_rand() % RING as u64) as usize;
                self.replay_ring[k] = msg.clone();
            }
        }
        self.messages_sent += 1;
        for _ in 0..copies {
            let delay = self.delay();
            self.enqueue(to, msg.clone(), delay);
        }
    }

    fn delay(&mut self) -> u64 {
        let span = self.cfg.max_delay_ms.saturating_sub(self.cfg.min_delay_ms) + 1;
        let mut delay = self.cfg.min_delay_ms + self.next_rand() % span;
        if self.cfg.slow_permille > 0 && (self.next_rand() % 1000) < self.cfg.slow_permille as u64 {
            delay += self.next_rand() % (20 * self.cfg.max_delay_ms.max(1));
        }
        delay
    }

    fn enqueue(&mut self, to: usize, msg: Message, delay: u64) {
        self.seq += 1;
        self.queue.push(Reverse(InFlight {
            at: self.now_ms + delay,
            seq: self.seq,
            to,
            msg: Item::Msg(msg),
        }));
    }

    fn broadcast(&mut self, from: usize, msg: Message) {
        // Equivocation: half the validators get the proposal, the other half
        // get a different one for the same round, signed by the same key.
        if let (true, Message::Proposal(p)) = (self.equivocators.contains(&from), &msg) {
            let mut other = p.clone();
            other.payload = [b"equivocated:".as_slice(), &p.payload].concat();
            other.header.payload_hash = keccak256(&other.payload);
            other.header.payload_len = other.header.payload_len.max(1);
            other.header.sign_with(&self.keys[from]);
            let n = self.engines.len();
            for to in 0..n {
                if to == from {
                    continue;
                }
                if self.double_voters.contains(&to) {
                    // Colluding: a Byzantine voter is shown both halves.
                    self.send(from, to, msg.clone());
                    self.send(from, to, Message::Proposal(other.clone()));
                    continue;
                }
                let m = if to % 2 == 0 {
                    msg.clone()
                } else {
                    Message::Proposal(other.clone())
                };
                self.send(from, to, m);
            }
            return;
        }
        for to in 0..self.engines.len() {
            if to != from {
                self.send(from, to, msg.clone());
            }
        }
    }

    fn trace(&self, node: usize, what: &str, round: Round) {
        if let Some((lo, hi)) = self.trace_rounds {
            if round >= lo && round <= hi {
                let e = self.engines[node].as_ref();
                eprintln!(
                    "t={:>6} n{} r{} hq{} {}",
                    self.now_ms,
                    node,
                    e.map(|e| e.round()).unwrap_or(0),
                    e.map(|e| e.high_qc().round).unwrap_or(0),
                    what
                );
            }
        }
    }

    /// What every validator's engine must never do, checked after each input
    /// it handles: its round, high certificate, commit point and voting
    /// floor never move backwards within one run of the engine.
    fn observe(&mut self, i: usize) {
        let Some(e) = self.engines[i].as_ref() else {
            return;
        };
        let now = [
            e.round(),
            e.high_qc().round,
            e.committed_round(),
            e.last_voted_round(),
        ];
        if let Some(before) = self.watermarks[i] {
            const NAMES: [&str; 4] = [
                "round",
                "high_qc round",
                "committed round",
                "last voted round",
            ];
            for k in 0..4 {
                if now[k] < before[k] {
                    self.violations.push((
                        i,
                        format!("{} went back from {} to {}", NAMES[k], before[k], now[k]),
                    ));
                }
            }
        }
        self.watermarks[i] = Some(now);
    }

    fn apply(&mut self, i: usize, actions: Vec<Action>) {
        self.observe(i);
        if let Some(e) = self.engines[i].as_ref() {
            let at = e.committed_round();
            if self.reputation[i].as_ref().map(|(r, _)| *r) != Some(at) {
                self.reputation[i] = Some((at, e.committed_reputation()));
            }
        }
        for a in actions {
            match &a {
                Action::Broadcast(m) => {
                    self.trace(i, &format!("send {}", describe(m)), msg_round(m))
                }
                Action::ReadyToPropose { round, .. } => self.trace(i, "ready", *round),
                Action::Commit(list) => self.trace(
                    i,
                    &format!(
                        "commit {} up to r{}",
                        list.len(),
                        list.last().map(|c| c.header.round).unwrap_or(0)
                    ),
                    list.last().map(|c| c.header.round).unwrap_or(0),
                ),
                Action::RequestProposal { .. } => self.trace(
                    i,
                    "request",
                    self.engines[i].as_ref().map(|e| e.round()).unwrap_or(0),
                ),
                Action::Ordered(o) => self.trace(i, "ordered", o.header.round),
            }
            match a {
                Action::Broadcast(msg) => {
                    match &msg {
                        Message::Vote(v) => {
                            self.votes_sent[i] += 1;
                            if !self.own_proposals[i].contains(&v.id) {
                                self.foreign_votes[i] += 1;
                            }
                            self.last_vote[i] = Some(v.clone());
                            if let Some(first) = self.vote_log.insert((i, v.round), v.id) {
                                if first != v.id {
                                    self.violations
                                        .push((i, format!("voted twice in round {}", v.round)));
                                }
                            }
                        }
                        Message::Proposal(p) => {
                            let id = p.header.id();
                            self.own_proposals[i].insert(id);
                            if let Some(first) = self.proposal_log.insert((i, p.header.round), id) {
                                if first != id {
                                    self.violations.push((
                                        i,
                                        format!("proposed twice in round {}", p.header.round),
                                    ));
                                }
                            }
                            let qcs = std::iter::once(&p.header.qc).chain(
                                p.header
                                    .tc
                                    .iter()
                                    .flat_map(|tc| tc.timeouts.iter().map(|t| &t.high_qc)),
                            );
                            for qc in qcs {
                                if let Some(first) = self.cert_log.insert(qc.round, qc.id) {
                                    if first != qc.id {
                                        self.violations.push((
                                            i,
                                            format!("two certificates for round {}", qc.round),
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    if matches!(msg, Message::Vote(_) | Message::Timeout(_)) {
                        // The driver writes its safety record before the
                        // message leaves the process.
                        let e = self.engines[i].as_ref().unwrap();
                        self.safety[i] = Some((e.last_voted_round(), e.high_qc().clone()));
                    }
                    self.broadcast(i, msg)
                }
                Action::ReadyToPropose { round, .. } => {
                    // A driver takes a moment to assemble a payload. Without
                    // it a lone validator would commit for ever at t=0.
                    self.seq += 1;
                    self.queue.push(Reverse(InFlight {
                        at: self.now_ms + 1,
                        seq: self.seq,
                        to: i,
                        msg: Item::Propose { round },
                    }));
                }
                Action::Commit(list) => {
                    for c in &list {
                        let id = c.header.id();
                        // Only the newest can have been handed over already.
                        if self.executed[i].last() != Some(&id) {
                            self.executed[i].push(id);
                        }
                    }
                    self.committed[i].extend(list);
                }
                Action::Ordered(o) => {
                    self.executed[i].push(o.header.id());
                }
                Action::RequestProposal { id, ancestors } => {
                    // One peer is asked, the next one each time.
                    let n = self.engines.len();
                    self.request_rr = (self.request_rr + 1) % n.max(1);
                    let mut to = self.request_rr;
                    if to == i {
                        to = (to + 1) % n;
                    }
                    if to != i && !self.cut.contains(&(i, to)) {
                        self.seq += 1;
                        let delay = self.cfg.min_delay_ms;
                        self.queue.push(Reverse(InFlight {
                            at: self.now_ms + delay,
                            seq: self.seq,
                            to,
                            msg: Item::Request {
                                id,
                                ancestors,
                                from: i,
                            },
                        }));
                    }
                }
            }
        }
    }

    // ----- the properties ------------------------------------------------
}

fn msg_round(m: &Message) -> Round {
    match m {
        Message::Proposal(p) => p.header.round,
        Message::Vote(v) => v.round,
        Message::Timeout(t) => t.round,
        Message::ProposalRequest { .. } | Message::TxBatch { .. } => 0,
    }
}

fn describe(m: &Message) -> String {
    match m {
        Message::Proposal(p) => format!(
            "proposal r{} qc{} tc{:?} len{}",
            p.header.round,
            p.header.qc.round,
            p.header.tc.as_ref().map(|t| t.round),
            p.header.payload_len
        ),
        Message::Vote(v) => format!("vote r{} {}", v.round, &hex::encode(v.id.0)[..6]),
        Message::Timeout(t) => format!("timeout r{} hq{}", t.round, t.high_qc.round),
        Message::ProposalRequest { id, .. } => format!("request {}", &hex::encode(id.0)[..6]),
        Message::TxBatch { txs } => format!("txbatch {}", txs.len()),
    }
}

impl Sim {
    /// Like [`Sim::assert_agreement`], except that the certificate each
    /// validator records for a commit is only checked to name the proposal
    /// and its round, not compared byte for byte: the proposal headers and
    /// payloads — what is executed — must be identical everywhere. For
    /// tests that want the ordering alone; [`Sim::assert_agreement`] also
    /// holds (`tests/finality.rs`).
    pub fn assert_ordering_agreement(&self) {
        self.assert_executed_matches_committed();
        let n = self.engines.len();
        for a in 0..n {
            for b in (a + 1)..n {
                let ca = &self.committed[a];
                let cb = &self.committed[b];
                for k in 0..ca.len().min(cb.len()) {
                    assert_eq!(
                        ca[k].header, cb[k].header,
                        "validators {a} and {b} disagree at commit index {k}: rounds {} vs {}",
                        ca[k].header.round, cb[k].header.round
                    );
                    assert_eq!(
                        ca[k].payload, cb[k].payload,
                        "validators {a} and {b} committed different payloads at index {k}"
                    );
                }
            }
        }
        for (i, seq) in self.committed.iter().enumerate() {
            let mut height = 0u64;
            let mut round = 0u64;
            for c in seq {
                assert!(
                    c.header.round > round,
                    "validator {i}: rounds not increasing"
                );
                round = c.header.round;
                let expect = height + u64::from(c.header.payload_len > 0);
                assert_eq!(c.header.height, expect, "validator {i}: height skipped");
                height = expect;
                assert_eq!(
                    (c.cert.id, c.cert.round),
                    (c.header.id(), c.header.round),
                    "validator {i}: round {} recorded a certificate for something else",
                    c.header.round
                );
            }
        }
    }

    /// Each validator executed exactly its committed sequence, in order,
    /// plus at most the one proposal whose certificate is still to come.
    fn assert_executed_matches_committed(&self) {
        for (i, (ex, com)) in self.executed.iter().zip(&self.committed).enumerate() {
            assert!(
                ex.len() <= com.len() + 1,
                "validator {i}: executed {} proposals, sealed only {}",
                ex.len(),
                com.len()
            );
            for (k, c) in com.iter().enumerate() {
                assert_eq!(
                    ex.get(k),
                    Some(&c.header.id()),
                    "validator {i}: executed and sealed sequences differ at {k}"
                );
            }
        }
    }

    /// Commits at which two validators recorded different certificate
    /// bytes for the same proposal, as `(a, b, index)`.
    pub fn cert_divergences(&self) -> Vec<(usize, usize, usize)> {
        let n = self.engines.len();
        let mut out = Vec::new();
        for a in 0..n {
            for b in (a + 1)..n {
                let (ca, cb) = (&self.committed[a], &self.committed[b]);
                for k in 0..ca.len().min(cb.len()) {
                    if ca[k].cert != cb[k].cert {
                        out.push((a, b, k));
                    }
                }
            }
        }
        out
    }

    /// Panics if any two validators committed conflicting sequences, or if
    /// the same proposal committed with different bytes anywhere.
    pub fn assert_agreement(&self) {
        self.assert_executed_matches_committed();
        let n = self.engines.len();
        for a in 0..n {
            for b in (a + 1)..n {
                let ca = &self.committed[a];
                let cb = &self.committed[b];
                let m = ca.len().min(cb.len());
                for k in 0..m {
                    assert_eq!(
                        ca[k].header.id(),
                        cb[k].header.id(),
                        "validators {a} and {b} disagree at commit index {k}: rounds {} vs {}",
                        ca[k].header.round,
                        cb[k].header.round
                    );
                    assert_eq!(
                        ca[k].finality().encode(),
                        cb[k].finality().encode(),
                        "validators {a} and {b} committed different bytes at index {k}"
                    );
                }
            }
        }
        for (i, seq) in self.committed.iter().enumerate() {
            let mut height = 0u64;
            let mut round = 0u64;
            for c in seq {
                assert!(
                    c.header.round > round,
                    "validator {i}: rounds not increasing"
                );
                round = c.header.round;
                let expect = height + u64::from(c.header.payload_len > 0);
                assert_eq!(c.header.height, expect, "validator {i}: height skipped");
                height = expect;
            }
        }
    }

    /// The longest committed sequence any validator holds.
    pub fn max_committed(&self) -> usize {
        self.committed.iter().map(|c| c.len()).max().unwrap_or(0)
    }

    pub fn min_committed_among(&self, nodes: &[usize]) -> usize {
        nodes
            .iter()
            .map(|i| self.committed[*i].len())
            .min()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::Dropped;
    use super::*;

    fn busy(cfg: SimConfig) -> Sim {
        Sim::new(cfg)
    }

    #[test]
    fn four_honest_validators_commit_every_round() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(2_000);
        sim.assert_agreement();
        let n = sim.min_committed_among(&[0, 1, 2, 3]);
        assert!(n >= 50, "only {n} commits in two seconds of simulated time");
        // Every commit was a non-empty proposal and heights track them.
        let last = sim.committed[0].last().unwrap();
        assert_eq!(last.header.height, sim.committed[0].len() as u64);
        // No round timed out: no validator ever sent a timeout.
        assert!(sim
            .committed
            .iter()
            .all(|c| c.iter().all(|x| x.header.tc.is_none())));
    }

    #[test]
    fn one_validator_commits_alone() {
        let mut sim = busy(SimConfig {
            validators: 1,
            ..Default::default()
        });
        sim.run_for(500);
        sim.assert_agreement();
        assert!(sim.committed[0].len() >= 10);
    }

    #[test]
    fn idle_rounds_commit_without_executing_anything() {
        let mut sim = busy(SimConfig::default());
        sim.payloads = Payloads::Alternating;
        sim.run_for(1_000);
        sim.assert_agreement();
        let seq = &sim.committed[0];
        assert!(seq.len() >= 20);
        let non_empty = seq.iter().filter(|c| c.header.payload_len > 0).count();
        assert_eq!(seq.last().unwrap().header.height, non_empty as u64);
        assert!(seq.iter().any(|c| c.header.payload_len == 0));
    }

    #[test]
    fn one_crash_of_four_keeps_committing_and_skips_the_dead_leader() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        let crashed_at = sim.engine(1).unwrap().round();
        sim.crash(1);
        let before = sim.min_committed_among(&[0, 2, 3]);
        sim.run_for(3_000);
        sim.assert_agreement();
        let after = sim.min_committed_among(&[0, 2, 3]);
        assert!(
            after > before + 20,
            "progress stalled after the crash: {before} -> {after}"
        );
        // Rounds led by the dead validator ended in timeout certificates,
        // and the proposals after them carry the certificate.
        let dead = sim.address(1);
        let survivors = &sim.committed[0];
        assert!(survivors
            .iter()
            .all(|c| c.header.proposer != dead || c.header.round <= crashed_at));
        let tcs = survivors.iter().filter(|c| c.header.tc.is_some()).count();
        assert!(tcs >= 1);
        // Leader reputation: the dead validator's turn is skipped after it
        // times out, so the timeouts are a handful, not one in four rounds.
        let after = survivors
            .iter()
            .filter(|c| c.header.round > crashed_at)
            .count();
        assert!(
            tcs * 8 < after,
            "{tcs} timed-out rounds in {after} committed rounds after the crash"
        );
    }

    #[test]
    fn two_crashes_of_four_halt_and_a_restart_resumes() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        sim.crash(1);
        sim.crash(2);
        let stalled_at = sim.max_committed();
        sim.run_for(3_000);
        // Two of four is below quorum: nothing new can commit.
        assert!(sim.max_committed() <= stalled_at + 1);
        sim.restart(1);
        sim.run_for(4_000);
        sim.assert_agreement();
        assert!(sim.max_committed() > stalled_at + 10);
        // The restarted validator caught up through proposal requests and
        // committed the same sequence.
        assert!(sim.committed[1].len() > 10);
    }

    /// A validator that was down long enough to be benched by the others
    /// comes back from its last commit, knowing nothing of the benching,
    /// and must still catch up and commit again.
    #[test]
    fn a_benched_validator_rejoins_after_a_restart() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        sim.crash(1);
        // Within the 256 rounds of proposals the others retain: rejoining
        // from further back is block sync's job, not this test's.
        sim.run_for(1_500);
        let others = sim.min_committed_among(&[0, 2, 3]);
        assert!(others > 50);
        sim.restart(1);
        let was = sim.committed[1].len();
        sim.run_for(3_000);
        sim.assert_agreement();
        let now = sim.committed[1].len();
        assert!(
            now + 3 >= sim.min_committed_among(&[0, 2, 3]),
            "restarted validator committed {was} -> {now}, others {}",
            sim.min_committed_among(&[0, 2, 3])
        );
    }

    #[test]
    fn an_equivocating_leader_never_splits_the_chain() {
        let mut sim = busy(SimConfig::default());
        sim.equivocate(2);
        sim.run_for(4_000);
        sim.assert_agreement();
        assert!(sim.min_committed_among(&[0, 1, 3]) >= 20);
        // The liar's rounds produced no certificates; some other round's
        // proposal carries the timeout certificate that skipped it.
        let liar = sim.address(2);
        assert!(sim.committed[0].iter().all(|c| c.header.proposer != liar));
    }

    #[test]
    fn a_partition_stalls_the_minority_and_heals() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        sim.partition(&[0, 1, 2], &[3]);
        sim.run_for(2_000);
        sim.assert_agreement();
        let majority = sim.min_committed_among(&[0, 1, 2]);
        let minority = sim.committed[3].len();
        assert!(majority > minority + 10);
        sim.heal();
        sim.run_for(3_000);
        sim.assert_agreement();
        assert!(sim.committed[3].len() + 3 >= sim.min_committed_among(&[0, 1, 2]));
    }

    #[test]
    fn lossy_links_and_jitter_stay_consistent() {
        for seed in 1..=6 {
            let mut sim = busy(SimConfig {
                drop_permille: 100,
                min_delay_ms: 1,
                max_delay_ms: 40,
                seed,
                ..Default::default()
            });
            sim.run_for(6_000);
            sim.assert_agreement();
            assert!(
                sim.min_committed_among(&[0, 1, 2, 3]) >= 10,
                "seed {seed}: too little progress"
            );
        }
    }

    #[test]
    fn seven_validators_with_two_crashes() {
        let mut sim = busy(SimConfig {
            validators: 7,
            ..Default::default()
        });
        sim.run_for(300);
        sim.crash(0);
        sim.crash(4);
        sim.run_for(3_000);
        sim.assert_agreement();
        assert!(sim.min_committed_among(&[1, 2, 3, 5, 6]) >= 30);
    }

    #[test]
    fn a_stale_or_forged_message_is_dropped_not_fatal() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(200);
        let (bad_sk, _) = keypair_from_seed(b"outsider");
        let outsider = Vote::sign(&bad_sk, sim.address(0), 10_000, H256([1; 32]));
        let acts =
            sim.engines[1]
                .as_mut()
                .unwrap()
                .on_message(Message::Vote(outsider), None, sim.now_ms);
        assert!(acts.is_empty());
        sim.run_for(500);
        sim.assert_agreement();
    }

    /// Validator 3's driver vetoes every peer's proposal from some round
    /// on. It never votes for one again, the other three still form
    /// quorums so nothing times out, and it still commits what they
    /// certify — a veto withholds a vote, it does not leave the chain.
    #[test]
    fn a_vetoed_proposal_gets_no_vote_and_the_others_carry_on() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        let from = sim.engine(3).unwrap().round() + 5;
        sim.veto(3, from);
        // Votes for the rounds before the veto drain first.
        sim.run_for(300);
        assert!(sim.engine(3).unwrap().round() > from + 10);
        let foreign_before = sim.foreign_votes[3];
        let own_before = sim.votes_sent[3] - foreign_before;
        let committed_before = sim.min_committed_among(&[0, 1, 2]);
        sim.run_for(2_000);
        sim.assert_agreement();
        assert_eq!(
            sim.foreign_votes[3], foreign_before,
            "validator 3 voted for a peer's proposal it had vetoed"
        );
        // It still votes for its own proposals, which the others certify.
        assert!(sim.votes_sent[3] - sim.foreign_votes[3] > own_before);
        assert!(sim.min_committed_among(&[0, 1, 2]) > committed_before + 20);
        assert!(sim.committed[3].len() + 3 >= sim.min_committed_among(&[0, 1, 2]));
        // Three of four is a quorum: no round after the veto timed out.
        assert!(sim.committed[0]
            .iter()
            .filter(|c| c.header.round >= from)
            .all(|c| c.header.tc.is_none()));
    }

    /// A certificate that names a certified proposal but carries some other
    /// round is verified like any other and fails: it must not move this
    /// node's round, whether it arrives in a timeout or justifies a
    /// proposal. Before this was checked, one member of the set could send
    /// everyone to a round a million ahead, or extend a certified proposal
    /// on a dead fork from a round of its choosing.
    #[test]
    fn a_certificate_with_a_certified_id_and_another_round_is_refused() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        let now = sim.now_ms;
        let (key1, addr1) = (sim.keys[1], sim.address(1));
        let keys = sim.keys.clone();
        let members = sim.set.as_slice().to_vec();
        let e = sim.engines[0].as_mut().unwrap();
        let round_before = e.round();
        let high = e.high_qc().clone();
        let bogus = QuorumCert {
            round: high.round + 1_000_000,
            id: high.id,
            votes: Vec::new(),
        };

        // In a timeout from a member of the set.
        let t = Timeout::sign(&key1, addr1, round_before, bogus.clone());
        e.dropped.clear();
        let _ = e.on_message(Message::Timeout(t), None, now);
        assert_eq!(
            e.round(),
            round_before,
            "a bogus certificate moved the round"
        );
        assert!(e
            .dropped
            .iter()
            .any(|(_, why)| matches!(why, Dropped::BadCertificate(_))));

        // Justifying a proposal from that round's leader.
        let round = bogus.round + 1;
        let leader = e.leader_for(round);
        let idx = members.iter().position(|a| *a == leader).unwrap();
        let mut h = ProposalHeader {
            round,
            height: 0,
            parent_id: high.id,
            payload_hash: H256::ZERO,
            payload_len: 0,
            timestamp: 0,
            exec_height: 0,
            exec_root: H256::ZERO,
            qc: bogus,
            tc: None,
            proposer: leader,
            sig: Vec::new(),
        };
        h.sign_with(&keys[idx]);
        e.dropped.clear();
        let acts = e.on_message(
            Message::Proposal(Proposal {
                header: h,
                payload: Vec::new(),
            }),
            None,
            now,
        );
        assert_eq!(
            e.round(),
            round_before,
            "a bogus certificate moved the round"
        );
        assert!(!acts
            .iter()
            .any(|a| matches!(a, Action::Broadcast(Message::Vote(_)))));
        assert!(e
            .dropped
            .iter()
            .any(|(_, why)| matches!(why, Dropped::BadCertificate(_))));
        sim.run_for(500);
        sim.assert_agreement();
    }

    /// What a restart must remember. Without its safety record a validator
    /// restarted at the parent of the last proposal it voted for votes
    /// again in that round, for whatever the leader offers this time; with
    /// it, it does not, and the timeout it sends instead carries the
    /// certificate it held when it voted rather than the one at its
    /// executed head.
    #[test]
    fn a_restarted_validator_does_not_vote_twice_in_a_round() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        let now = sim.now_ms;
        let (voted, high) = sim.safety[3].clone().unwrap();
        let v = sim.last_vote[3].clone().unwrap();
        assert_eq!(v.round, voted);
        // Validator 3 holds what it voted for; the others may not have it
        // yet.
        let holder = sim.engines[3].as_ref().unwrap();
        let p = holder
            .proposal(&v.id)
            .expect("validator 3 holds what it voted for");
        let parent = holder.proposal(&p.header.parent_id).unwrap();
        let root = Root::from_finality(&Finality {
            header: parent.header.clone(),
            cert: p.header.qc.clone(),
        });
        // The same leader's other proposal for that round.
        let mut other = p.clone();
        other.payload = b"the leader's other proposal".to_vec();
        other.header.payload_hash = keccak256(&other.payload);
        other.header.payload_len = 1;
        other.header.height = parent.header.height + 1;
        let idx = sim
            .set
            .as_slice()
            .iter()
            .position(|a| *a == p.header.proposer)
            .unwrap();
        other.header.sign_with(&sim.keys[idx]);
        let votes = |acts: &[Action]| {
            acts.iter()
                .any(|a| matches!(a, Action::Broadcast(Message::Vote(_))))
        };

        // Amnesiac: the vote is cast a second time.
        let mut amnesiac = sim.fresh_engine(3, root.clone());
        let _ = amnesiac.take_initial_actions();
        assert_eq!(amnesiac.round(), voted);
        let acts = amnesiac.on_message(Message::Proposal(other.clone()), None, now);
        assert!(votes(&acts), "the test would not catch a double vote");

        // Restored: it is not.
        let mut restored = sim.fresh_engine(3, root);
        restored.restore_safety(voted, high.clone());
        let _ = restored.take_initial_actions();
        let acts = restored.on_message(Message::Proposal(other), None, now);
        assert!(!votes(&acts), "voted twice in round {voted}");

        // Restarted further back, its timeout still carries the certificate
        // it held when it voted, not the lower one at its executed head.
        let deep_root = Root::from_finality(&sim.committed[3].last().unwrap().finality());
        assert!(deep_root.round < high.round);
        let mut deep = sim.fresh_engine(3, deep_root);
        deep.restore_safety(voted, high.clone());
        let _ = deep.take_initial_actions();
        let acts = deep.on_tick(now + 10_000);
        let t = acts
            .iter()
            .find_map(|a| match a {
                Action::Broadcast(Message::Timeout(t)) => Some(t.clone()),
                _ => None,
            })
            .expect("the round times out");
        assert_eq!(t.high_qc.round, high.round);
        assert!(t.round >= voted);
    }

    /// One validator, one vote per round: a second vote for another
    /// proposal is equivocation, not a repeat. And votes or timeouts for
    /// rounds further ahead than proposals are kept are not buffered, so a
    /// member cannot fill memory with rounds that never come.
    #[test]
    fn vote_equivocation_and_far_future_messages_are_dropped() {
        let mut sim = busy(SimConfig::default());
        sim.run_for(300);
        let now = sim.now_ms;
        let (key1, addr1) = (sim.keys[1], sim.address(1));
        let genesis = sim.genesis_id;
        let e = sim.engines[0].as_mut().unwrap();
        let r = e.round() + 100;
        e.dropped.clear();
        let first = Vote::sign(&key1, addr1, r, H256([1; 32]));
        let second = Vote::sign(&key1, addr1, r, H256([2; 32]));
        let _ = e.on_message(Message::Vote(first.clone()), None, now);
        assert!(e.dropped.is_empty());
        let _ = e.on_message(Message::Vote(first), None, now);
        assert_eq!(e.dropped.last(), Some(&(r, Dropped::Duplicate)));
        let _ = e.on_message(Message::Vote(second), None, now);
        assert_eq!(e.dropped.last(), Some(&(r, Dropped::Equivocation)));

        let far = e.round() + 257;
        let _ = e.on_message(
            Message::Vote(Vote::sign(&key1, addr1, far, H256([3; 32]))),
            None,
            now,
        );
        assert_eq!(e.dropped.last(), Some(&(far, Dropped::FarFuture)));
        let _ = e.on_message(
            Message::Timeout(Timeout::sign(
                &key1,
                addr1,
                far,
                QuorumCert::genesis(genesis),
            )),
            None,
            now,
        );
        assert_eq!(e.dropped.last(), Some(&(far, Dropped::FarFuture)));
        sim.run_for(500);
        sim.assert_agreement();
    }

    /// A validator rebuilding its tree after a restart is served each
    /// missing proposal about once, and catches up, whatever the seed. It
    /// used to ask for the parent of every orphan it held, each answered
    /// with a stretch of sixty-four ancestors it already had: sixty times
    /// the gap, every one validated again, which under load is why a
    /// restarted validator could not catch the tip. And on some seeds it
    /// never caught up at all: it had led the round after its last commit
    /// before the crash, proposed in that round again after the restart,
    /// and then refused the certified original as equivocation, leaving
    /// its tree disconnected for good. Seed 1 alone, as the rejoin test
    /// above runs, happens not to lead that round.
    #[test]
    fn a_restarted_validator_is_served_each_missing_proposal_about_once() {
        for seed in 1..=8 {
            let mut sim = busy(SimConfig {
                seed,
                ..Default::default()
            });
            sim.run_for(300);
            sim.crash(1);
            sim.run_for(2_000);
            let gap = sim.engine(0).unwrap().round()
                - sim.committed[1].last().map(|c| c.header.round).unwrap_or(0);
            assert!(gap > 100, "seed {seed}");
            sim.restart(1);
            sim.run_for(3_000);
            sim.assert_agreement();
            assert!(
                sim.committed[1].len() + 3 >= sim.min_committed_among(&[0, 2, 3]),
                "seed {seed}: restarted validator at {} commits, others at {}",
                sim.committed[1].len(),
                sim.min_committed_among(&[0, 2, 3])
            );
            assert!(
                sim.served[1] <= 2 * gap + Engine::REQUEST_ANCESTORS as u64,
                "seed {seed}: served {} proposals for a gap of {gap} rounds",
                sim.served[1]
            );
        }
    }
}
