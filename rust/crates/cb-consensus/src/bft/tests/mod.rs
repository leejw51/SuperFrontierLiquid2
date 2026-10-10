//! Adversarial and property tests for the BFT engine, beyond the scenario
//! tests in `sim.rs`.
//!
//! * `quorum` — the threshold arithmetic every safety argument rests on.
//! * `safety_rules` — the voting, locking and commit rules at the engine's
//!   boundary, one hand-built message at a time.
//! * `byzantine` — each misbehaviour a member of the set can attempt, alone
//!   against one engine and live inside a simulated network.
//! * `sweep` — randomised fault schedules over many seeds, checking safety
//!   throughout and liveness once the network settles.
//! * `malformed` — arbitrary and mutated bytes through every decoder, and
//!   nonsense that decodes through a live engine.
//! * `memory` — the engine's buffers stay bounded under spam.
//!
//! The kit below builds every signed object by hand from keys the test
//! owns, which is what lets a test play any validator, honest or not.

// Test code favours explicit indexing and literal fixtures over clippy's
// preferred iterator forms; the lints below are style, not correctness.
#![allow(
    clippy::needless_range_loop,
    clippy::cloned_ref_to_slice_refs,
    clippy::type_complexity,
    clippy::useless_vec
)]

mod byzantine;
mod carried_certs;
mod finality;
mod malformed;
mod memory;
mod pacemaker;
mod quorum;
mod reputation;
mod safety_rules;
mod sweep;

use super::engine::{Action, Committed, Config, Dropped, Engine, Root};
use super::types::*;
use cb_crypto::{address_from_pubkey, keccak256, keypair_from_seed, SecretKey};
use cb_types::{Address, H256};

/// Keys, the validator set and the genesis for `n` validators, and
/// builders for every signed object the protocol exchanges.
pub(super) struct Kit {
    pub keys: Vec<SecretKey>,
    pub set: ValidatorSet,
    pub genesis: H256,
}

impl Kit {
    pub fn new(n: usize) -> Self {
        let mut keys = Vec::new();
        let mut addrs = Vec::new();
        for i in 0..n {
            let (sk, pk) = keypair_from_seed(format!("kit-{n}-{i}").as_bytes());
            keys.push(sk);
            addrs.push(address_from_pubkey(&pk));
        }
        Self {
            keys,
            set: ValidatorSet::new(addrs).unwrap(),
            genesis: keccak256(b"kit-genesis"),
        }
    }

    pub fn n(&self) -> usize {
        self.set.len()
    }

    pub fn addr(&self, i: usize) -> Address {
        self.set.as_slice()[i]
    }

    /// The plain round-robin leader's index. Unit tests never commit a
    /// timeout certificate's proposal before asking, so no one is benched.
    pub fn leader(&self, round: Round) -> usize {
        (round % self.n() as u64) as usize
    }

    pub fn config() -> Config {
        Config {
            base_timeout_ms: 1_000,
            max_timeout_ms: 16_000,
            retain_rounds: 256,
        }
    }

    /// Validator `i`'s engine with its tree at `root`, clock at zero.
    pub fn engine_at(&self, i: usize, root: Root) -> Engine {
        let mut e = Engine::new(
            Self::config(),
            self.set.clone(),
            self.addr(i),
            self.keys[i],
            self.genesis,
            root,
            keccak256,
            0,
        );
        let _ = e.take_initial_actions();
        e
    }

    pub fn engine(&self, i: usize) -> Engine {
        self.engine_at(i, Root::genesis(self.genesis))
    }

    pub fn genesis_qc(&self) -> QuorumCert {
        QuorumCert::genesis(self.genesis)
    }

    pub fn vote(&self, i: usize, round: Round, id: H256) -> Vote {
        Vote::sign(&self.keys[i], self.addr(i), round, id)
    }

    /// A certificate signed by exactly the listed validators.
    pub fn qc_by(&self, round: Round, id: H256, signers: &[usize]) -> QuorumCert {
        QuorumCert {
            round,
            id,
            votes: signers.iter().map(|i| self.vote(*i, round, id)).collect(),
        }
    }

    /// A certificate signed by the first quorum of the set.
    pub fn qc(&self, round: Round, id: H256) -> QuorumCert {
        let signers: Vec<usize> = (0..self.set.quorum()).collect();
        self.qc_by(round, id, &signers)
    }

    pub fn qc_for(&self, p: &Proposal) -> QuorumCert {
        self.qc(p.header.round, p.header.id())
    }

    pub fn timeout(&self, i: usize, round: Round, high_qc: QuorumCert) -> Timeout {
        Timeout::sign(&self.keys[i], self.addr(i), round, high_qc)
    }

    /// A timeout certificate from the first quorum, each carrying the
    /// matching certificate from `high_qcs` (the last one repeats).
    pub fn tc(&self, round: Round, high_qcs: &[QuorumCert]) -> TimeoutCert {
        let timeouts = (0..self.set.quorum())
            .map(|i| {
                let qc = high_qcs[i.min(high_qcs.len() - 1)].clone();
                self.timeout(i, round, qc)
            })
            .collect();
        TimeoutCert { round, timeouts }
    }

    /// A proposal signed by validator `by`, extending the proposal `qc`
    /// names at `parent_height`.
    #[allow(clippy::too_many_arguments)]
    pub fn proposal_by(
        &self,
        by: usize,
        round: Round,
        parent_height: u64,
        qc: QuorumCert,
        tc: Option<TimeoutCert>,
        payload: &[u8],
    ) -> Proposal {
        let len = u32::from(!payload.is_empty());
        let mut header = ProposalHeader {
            round,
            height: parent_height + u64::from(len),
            parent_id: qc.id,
            payload_hash: if payload.is_empty() {
                H256::ZERO
            } else {
                keccak256(payload)
            },
            payload_len: len,
            timestamp: round,
            exec_height: 0,
            exec_root: H256::ZERO,
            qc,
            tc,
            proposer: self.addr(by),
            sig: Vec::new(),
        };
        header.sign_with(&self.keys[by]);
        Proposal {
            header,
            payload: payload.to_vec(),
        }
    }

    /// The round's leader's proposal.
    pub fn proposal(
        &self,
        round: Round,
        parent_height: u64,
        qc: QuorumCert,
        tc: Option<TimeoutCert>,
        payload: &[u8],
    ) -> Proposal {
        self.proposal_by(self.leader(round), round, parent_height, qc, tc, payload)
    }

    /// Re-sign a header after a test changed it, as `by`.
    pub fn resign(&self, p: &mut Proposal, by: usize) {
        p.header.sign_with(&self.keys[by]);
    }

    /// Rounds 1..=k, each extending the one before with a full
    /// certificate, all non-empty.
    pub fn chain(&self, k: u64) -> Vec<Proposal> {
        let mut out: Vec<Proposal> = Vec::new();
        for r in 1..=k {
            let (qc, h) = match out.last() {
                Some(p) => (self.qc_for(p), p.header.height),
                None => (self.genesis_qc(), 0),
            };
            out.push(self.proposal(r, h, qc, None, format!("chain r{r}").as_bytes()));
        }
        out
    }
}

pub(super) fn msg(p: &Proposal) -> Message {
    Message::Proposal(p.clone())
}

pub(super) fn votes(acts: &[Action]) -> Vec<Vote> {
    acts.iter()
        .filter_map(|a| match a {
            Action::Broadcast(Message::Vote(v)) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn timeouts(acts: &[Action]) -> Vec<Timeout> {
    acts.iter()
        .filter_map(|a| match a {
            Action::Broadcast(Message::Timeout(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn proposals(acts: &[Action]) -> Vec<Proposal> {
    acts.iter()
        .filter_map(|a| match a {
            Action::Broadcast(Message::Proposal(p)) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn commits(acts: &[Action]) -> Vec<Committed> {
    acts.iter()
        .flat_map(|a| match a {
            Action::Commit(list) => list.clone(),
            _ => Vec::new(),
        })
        .collect()
}

/// Feed `ps` in order and collect every action.
pub(super) fn feed(e: &mut Engine, ps: &[Proposal], now: u64) -> Vec<Action> {
    let mut out = Vec::new();
    for p in ps {
        out.extend(e.on_message(msg(p), None, now));
    }
    out
}

/// The engine's observable safety state, to compare before and after an
/// attack.
pub(super) fn snapshot(e: &Engine) -> (Round, Round, Round, Round, H256) {
    (
        e.round(),
        e.high_qc().round,
        e.committed_round(),
        e.last_voted_round(),
        e.committed_id(),
    )
}

pub(super) fn last_drop(e: &Engine) -> Option<Dropped> {
    e.dropped.last().map(|(_, d)| d.clone())
}
