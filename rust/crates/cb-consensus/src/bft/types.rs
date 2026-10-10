//! Wire types of the BFT protocol: proposals, votes, timeouts and the two
//! certificates built from them.
//!
//! Every signed object hashes a domain-tagged, bincode-encoded preimage with
//! keccak and signs that with the validator's secp256k1 key, the same scheme
//! transactions and block headers use. Changing any preimage is a hard fork.
//!
//! # What a proposal certifies
//!
//! A proposal is an *ordering* claim: "these transactions follow the
//! transactions of the proposal `parent_id`". It carries no state root of its
//! own; execution happens after commit (ADR 0008). What it does carry is the
//! proposer's latest *executed* head (`exec_height`, `exec_root`), so every
//! validator audits every other validator's execution a few blocks after the
//! fact: a validator whose own root at `exec_height` differs refuses to vote.

use cb_crypto::{keccak256, recover, sign, SecretKey, Signature};
use cb_types::{Address, H256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A consensus round. Rounds advance on every certificate, quorum or
/// timeout, and are never reused. The leader of round `r` is
/// `validators[r % n]`.
pub type Round = u64;

const DOMAIN_PROPOSAL: &[u8] = b"CB-BFT-PROPOSAL\0";
const DOMAIN_VOTE: &[u8] = b"CB-BFT-VOTE\0";
const DOMAIN_TIMEOUT: &[u8] = b"CB-BFT-TIMEOUT\0";

/// The validator set, in genesis order, with the two thresholds derived
/// from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSet {
    validators: Vec<Address>,
}

impl ValidatorSet {
    /// An empty set is refused: a chain with nobody to vote cannot agree on
    /// anything.
    pub fn new(validators: Vec<Address>) -> Result<Self, BftError> {
        if validators.is_empty() {
            return Err(BftError::EmptyValidatorSet);
        }
        let mut seen = std::collections::HashSet::new();
        for v in &validators {
            if !seen.insert(*v) {
                return Err(BftError::DuplicateValidator(*v));
            }
        }
        Ok(Self { validators })
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    pub fn contains(&self, a: &Address) -> bool {
        self.validators.contains(a)
    }

    pub fn as_slice(&self) -> &[Address] {
        &self.validators
    }

    /// The validator entitled to propose in `round`.
    pub fn leader(&self, round: Round) -> Address {
        self.validators[(round % self.validators.len() as u64) as usize]
    }

    /// Byzantine faults tolerated: `f = (n - 1) / 3`. One validator alone
    /// tolerates none; four tolerate one; seven tolerate two.
    pub fn faults_tolerated(&self) -> usize {
        (self.validators.len() - 1) / 3
    }

    /// Votes a certificate needs: `ceil(2n / 3)`, which for `n = 3f + 1` is
    /// `2f + 1`. Two such quorums share at least `f + 1` validators, so at
    /// least one honest one, which is what makes two conflicting
    /// certificates in one round impossible.
    pub fn quorum(&self) -> usize {
        crate::quorum_count(self.validators.len())
    }

    /// Messages from distinct validators that prove at least one honest
    /// validator sent one: `f + 1`.
    pub fn honest_threshold(&self) -> usize {
        self.faults_tolerated() + 1
    }
}

/// A vote for one proposal in one round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub round: Round,
    pub id: H256,
    pub voter: Address,
    /// 65-byte secp256k1 signature over [`vote_digest`].
    pub sig: Vec<u8>,
}

pub fn vote_digest(round: Round, id: H256) -> H256 {
    let mut buf = Vec::with_capacity(DOMAIN_VOTE.len() + 8 + 32);
    buf.extend_from_slice(DOMAIN_VOTE);
    buf.extend_from_slice(&round.to_be_bytes());
    buf.extend_from_slice(&id.0);
    keccak256(&buf)
}

impl Vote {
    pub fn sign(sk: &SecretKey, voter: Address, round: Round, id: H256) -> Self {
        let sig = sign(sk, vote_digest(round, id)).expect("vote sign");
        Self {
            round,
            id,
            voter,
            sig: sig.to_vec(),
        }
    }

    /// The signer, which must equal `voter` and be in the set. Callers check
    /// both; this only does the recovery.
    pub fn signer(&self) -> Result<Address, BftError> {
        recover_addr(&self.sig, vote_digest(self.round, self.id))
    }
}

/// A quorum of votes for one proposal: the proposal is *certified*.
///
/// Round 0 is the genesis certificate: no votes, `id` is the chain's genesis
/// identity, and it verifies by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuorumCert {
    pub round: Round,
    pub id: H256,
    pub votes: Vec<Vote>,
}

impl QuorumCert {
    pub fn genesis(id: H256) -> Self {
        Self {
            round: 0,
            id,
            votes: Vec::new(),
        }
    }

    pub fn is_genesis(&self) -> bool {
        self.round == 0
    }

    /// Every vote is for `(round, id)`, signed by a distinct member of the
    /// set, and there are at least `quorum` of them.
    pub fn verify(&self, set: &ValidatorSet, genesis_id: H256) -> Result<(), BftError> {
        if self.is_genesis() {
            return if self.id == genesis_id && self.votes.is_empty() {
                Ok(())
            } else {
                Err(BftError::BadGenesisCert)
            };
        }
        if self.votes.len() < set.quorum() {
            return Err(BftError::BelowQuorum {
                got: self.votes.len(),
                need: set.quorum(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for v in &self.votes {
            if v.round != self.round || v.id != self.id {
                return Err(BftError::VoteForOtherProposal);
            }
            let signer = v.signer()?;
            if signer != v.voter || !set.contains(&signer) {
                return Err(BftError::VoterNotInSet(v.voter));
            }
            if !seen.insert(signer) {
                return Err(BftError::DuplicateVoter(signer));
            }
        }
        Ok(())
    }
}

/// A validator gave up on `round` without a certificate. It carries the
/// highest certificate the sender holds, so the leader who collects a quorum
/// of these knows the highest certified proposal it must extend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeout {
    pub round: Round,
    pub high_qc: QuorumCert,
    pub voter: Address,
    /// Signature over [`timeout_digest`], which binds `round` and
    /// `high_qc.round` but not the certificate's votes.
    pub sig: Vec<u8>,
}

pub fn timeout_digest(round: Round, high_qc_round: Round) -> H256 {
    let mut buf = Vec::with_capacity(DOMAIN_TIMEOUT.len() + 16);
    buf.extend_from_slice(DOMAIN_TIMEOUT);
    buf.extend_from_slice(&round.to_be_bytes());
    buf.extend_from_slice(&high_qc_round.to_be_bytes());
    keccak256(&buf)
}

impl Timeout {
    pub fn sign(sk: &SecretKey, voter: Address, round: Round, high_qc: QuorumCert) -> Self {
        let sig = sign(sk, timeout_digest(round, high_qc.round)).expect("timeout sign");
        Self {
            round,
            high_qc,
            voter,
            sig: sig.to_vec(),
        }
    }

    pub fn signer(&self) -> Result<Address, BftError> {
        recover_addr(&self.sig, timeout_digest(self.round, self.high_qc.round))
    }
}

/// A quorum of timeouts for one round: the round is over without a
/// certificate, and the next leader may propose on top of the highest
/// certificate any of them carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutCert {
    pub round: Round,
    pub timeouts: Vec<Timeout>,
}

impl TimeoutCert {
    /// Every timeout is for `round`, from a distinct validator, its embedded
    /// certificate verifies, and there are at least `quorum` of them.
    pub fn verify(&self, set: &ValidatorSet, genesis_id: H256) -> Result<(), BftError> {
        if self.timeouts.len() < set.quorum() {
            return Err(BftError::BelowQuorum {
                got: self.timeouts.len(),
                need: set.quorum(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for t in &self.timeouts {
            if t.round != self.round {
                return Err(BftError::TimeoutForOtherRound);
            }
            let signer = t.signer()?;
            if signer != t.voter || !set.contains(&signer) {
                return Err(BftError::VoterNotInSet(t.voter));
            }
            if !seen.insert(signer) {
                return Err(BftError::DuplicateVoter(signer));
            }
            t.high_qc.verify(set, genesis_id)?;
        }
        Ok(())
    }

    /// The highest certificate round any member of the quorum carried. A
    /// proposal justified by this certificate must extend a certificate at
    /// least this high.
    pub fn high_qc_round(&self) -> Round {
        self.timeouts
            .iter()
            .map(|t| t.high_qc.round)
            .max()
            .unwrap_or(0)
    }

    /// The highest certificate itself.
    pub fn high_qc(&self) -> Option<&QuorumCert> {
        self.timeouts
            .iter()
            .map(|t| &t.high_qc)
            .max_by_key(|q| q.round)
    }
}

/// What a proposal commits to, signed by the round's leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalHeader {
    pub round: Round,
    /// Number of executed blocks on this proposal's chain, itself included
    /// when it is not empty. An empty proposal keeps its parent's height.
    pub height: u64,
    /// The proposal this one extends: `qc.id`, repeated so the tree can be
    /// walked without opening the certificate.
    pub parent_id: H256,
    /// Merkle-style root of the payload's transactions, or zero when empty.
    pub payload_hash: H256,
    /// Number of transactions in the payload.
    pub payload_len: u32,
    /// Milliseconds since the epoch on the proposer's clock, never below the
    /// parent's.
    pub timestamp: u64,
    /// The proposer's latest executed height and its state root, for the
    /// audit described in the module docs.
    pub exec_height: u64,
    pub exec_root: H256,
    /// Certifies `parent_id`.
    pub qc: QuorumCert,
    /// Present when `round != qc.round + 1`: proves the rounds in between
    /// ended without a certificate.
    pub tc: Option<TimeoutCert>,
    pub proposer: Address,
    /// Signature over [`ProposalHeader::digest`] of the header with an empty
    /// signature.
    pub sig: Vec<u8>,
}

impl ProposalHeader {
    /// The identity of the proposal: what votes name and what the child's
    /// certificate certifies. Covers every field but the signature.
    pub fn id(&self) -> H256 {
        let mut h = self.clone();
        h.sig.clear();
        let encoded = bincode::serialize(&h).expect("ProposalHeader encode");
        let mut buf = Vec::with_capacity(DOMAIN_PROPOSAL.len() + encoded.len());
        buf.extend_from_slice(DOMAIN_PROPOSAL);
        buf.extend_from_slice(&encoded);
        keccak256(&buf)
    }

    pub fn sign_with(&mut self, sk: &SecretKey) {
        self.sig.clear();
        let sig = sign(sk, self.id()).expect("proposal sign");
        self.sig = sig.to_vec();
    }

    pub fn signer(&self) -> Result<Address, BftError> {
        recover_addr(&self.sig, self.id())
    }

    pub fn is_empty(&self) -> bool {
        self.payload_len == 0
    }
}

/// A proposal with its payload. The payload is opaque to the protocol: the
/// node hashes it with the same function that produced `payload_hash`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub header: ProposalHeader,
    pub payload: Vec<u8>,
}

/// Proof, carried in an executed block, that its transactions were ordered
/// by a quorum: the proposal header and the certificate for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finality {
    pub header: ProposalHeader,
    pub cert: QuorumCert,
}

impl Finality {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Finality encode")
    }

    pub fn decode(b: &[u8]) -> Result<Self, BftError> {
        bincode::deserialize(b).map_err(|e| BftError::Malformed(e.to_string()))
    }

    /// The certificate names this header, the header is signed by a member
    /// of the set, and the certificate is a quorum of the set.
    ///
    /// Which member led the round is not checked here: the leader schedule
    /// depends on the committed history (see `Engine::leader_for`), which a
    /// verifier of one block does not have. It does not need it: a quorum
    /// of validators voted for this proposal, and honest validators vote
    /// only for the round's leader.
    pub fn verify(&self, set: &ValidatorSet, genesis_id: H256) -> Result<(), BftError> {
        let id = self.header.id();
        if self.cert.id != id || self.cert.round != self.header.round {
            return Err(BftError::CertForOtherProposal);
        }
        let signer = self.header.signer()?;
        if signer != self.header.proposer || !set.contains(&signer) {
            return Err(BftError::WrongProposer {
                round: self.header.round,
                expected: self.header.proposer,
                got: signer,
            });
        }
        self.cert.verify(set, genesis_id)
    }
}

/// Everything that travels between validators.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Proposal(Proposal),
    Vote(Vote),
    Timeout(Timeout),
    /// Ask whoever hears it for the proposal with this id, and for up to
    /// `ancestors` of its parents. A validator rebuilding its tree walks
    /// back from a certified proposal to its commit point; one hop per
    /// round trip would lose that race against the peers' pruning.
    ProposalRequest {
        id: H256,
        ancestors: u32,
    },
    /// Transactions one validator admitted, for the others' mempools. Not
    /// part of the ordering protocol — the engine ignores it — but carried
    /// on the same channel because it is the cheapest way for a
    /// transaction that reached one validator to reach the one that
    /// proposes next: one message per batch, no announce-and-fetch round
    /// trips. Each entry is a `SignedTx`'s bincode; the receiver verifies
    /// every one as if a client had submitted it.
    TxBatch {
        txs: Vec<Vec<u8>>,
    },
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Message encode")
    }

    pub fn decode(b: &[u8]) -> Result<Self, BftError> {
        bincode::deserialize(b).map_err(|e| BftError::Malformed(e.to_string()))
    }
}

fn recover_addr(sig: &[u8], digest: H256) -> Result<Address, BftError> {
    let sig = Signature::from_bytes(sig).map_err(|e| BftError::MalformedSig(e.to_string()))?;
    recover(&sig, digest).map_err(|e| BftError::MalformedSig(e.to_string()))
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BftError {
    #[error("validator set is empty")]
    EmptyValidatorSet,
    #[error("validator {0} listed twice")]
    DuplicateValidator(Address),
    #[error("malformed signature: {0}")]
    MalformedSig(String),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("genesis certificate does not name the genesis")]
    BadGenesisCert,
    #[error("below quorum: {got} of {need}")]
    BelowQuorum { got: usize, need: usize },
    #[error("vote names a different proposal than the certificate")]
    VoteForOtherProposal,
    #[error("timeout names a different round than the certificate")]
    TimeoutForOtherRound,
    #[error("{0} is not in the validator set, or the signature is not its")]
    VoterNotInSet(Address),
    #[error("{0} appears twice")]
    DuplicateVoter(Address),
    #[error("certificate names a different proposal")]
    CertForOtherProposal,
    #[error("round {round} belongs to {expected}, proposal is from {got}")]
    WrongProposer {
        round: Round,
        expected: Address,
        got: Address,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_crypto::{address_from_pubkey, generate_keypair};

    fn keys(n: usize) -> (Vec<SecretKey>, ValidatorSet) {
        let mut sks = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..n {
            let (sk, pk) = generate_keypair();
            sks.push(sk);
            addrs.push(address_from_pubkey(&pk));
        }
        (sks, ValidatorSet::new(addrs).unwrap())
    }

    #[test]
    fn thresholds_follow_three_f_plus_one() {
        let (_, one) = keys(1);
        assert_eq!((one.faults_tolerated(), one.quorum()), (0, 1));
        let (_, three) = keys(3);
        assert_eq!((three.faults_tolerated(), three.quorum()), (0, 2));
        let (_, four) = keys(4);
        assert_eq!((four.faults_tolerated(), four.quorum()), (1, 3));
        let (_, seven) = keys(7);
        assert_eq!((seven.faults_tolerated(), seven.quorum()), (2, 5));
        // Two quorums always share an honest validator.
        for n in 1..=13 {
            let (_, set) = keys(n);
            assert!(2 * set.quorum() > n + set.faults_tolerated(), "n={n}");
            assert!(set.quorum() <= n - set.faults_tolerated(), "n={n}");
        }
    }

    #[test]
    fn quorum_cert_verifies_and_rejects_short_or_foreign_votes() {
        let (sks, set) = keys(4);
        let id = H256([3; 32]);
        let votes: Vec<Vote> = sks
            .iter()
            .zip(set.as_slice())
            .take(3)
            .map(|(sk, a)| Vote::sign(sk, *a, 5, id))
            .collect();
        let qc = QuorumCert {
            round: 5,
            id,
            votes: votes.clone(),
        };
        qc.verify(&set, H256::ZERO).unwrap();

        let short = QuorumCert {
            round: 5,
            id,
            votes: votes[..2].to_vec(),
        };
        assert!(matches!(
            short.verify(&set, H256::ZERO),
            Err(BftError::BelowQuorum { got: 2, need: 3 })
        ));

        let (outsider, pk) = generate_keypair();
        let mut foreign = votes[..2].to_vec();
        foreign.push(Vote::sign(&outsider, address_from_pubkey(&pk), 5, id));
        let qc = QuorumCert {
            round: 5,
            id,
            votes: foreign,
        };
        assert!(matches!(
            qc.verify(&set, H256::ZERO),
            Err(BftError::VoterNotInSet(_))
        ));

        let mut dup = votes[..2].to_vec();
        dup.push(votes[0].clone());
        let qc = QuorumCert {
            round: 5,
            id,
            votes: dup,
        };
        assert!(matches!(
            qc.verify(&set, H256::ZERO),
            Err(BftError::DuplicateVoter(_))
        ));
    }

    #[test]
    fn a_vote_relabelled_to_another_validator_is_caught() {
        let (sks, set) = keys(4);
        let mut v = Vote::sign(&sks[0], set.as_slice()[0], 1, H256([1; 32]));
        v.voter = set.as_slice()[1];
        let qc = QuorumCert {
            round: 1,
            id: H256([1; 32]),
            votes: vec![
                v,
                Vote::sign(&sks[2], set.as_slice()[2], 1, H256([1; 32])),
                Vote::sign(&sks[3], set.as_slice()[3], 1, H256([1; 32])),
            ],
        };
        assert!(matches!(
            qc.verify(&set, H256::ZERO),
            Err(BftError::VoterNotInSet(_))
        ));
    }

    #[test]
    fn genesis_cert_only_verifies_for_the_genesis_id() {
        let (_, set) = keys(4);
        let g = H256([9; 32]);
        QuorumCert::genesis(g).verify(&set, g).unwrap();
        assert!(QuorumCert::genesis(H256([8; 32])).verify(&set, g).is_err());
    }

    #[test]
    fn proposal_id_ignores_signature_and_signer_recovers() {
        let (sks, set) = keys(4);
        let mut h = ProposalHeader {
            round: 1,
            height: 1,
            parent_id: H256::ZERO,
            payload_hash: H256([7; 32]),
            payload_len: 3,
            timestamp: 1,
            exec_height: 0,
            exec_root: H256::ZERO,
            qc: QuorumCert::genesis(H256::ZERO),
            tc: None,
            proposer: set.leader(1),
            sig: Vec::new(),
        };
        let before = h.id();
        h.sign_with(&sks[1]);
        assert_eq!(h.id(), before);
        assert_eq!(h.signer().unwrap(), set.leader(1));
        h.payload_len = 4;
        assert_ne!(h.id(), before);
    }

    #[test]
    fn timeout_cert_reports_highest_embedded_qc() {
        let (sks, set) = keys(4);
        let g = H256::ZERO;
        let id = H256([2; 32]);
        let votes: Vec<Vote> = sks
            .iter()
            .zip(set.as_slice())
            .take(3)
            .map(|(sk, a)| Vote::sign(sk, *a, 3, id))
            .collect();
        let qc3 = QuorumCert {
            round: 3,
            id,
            votes,
        };
        let timeouts = vec![
            Timeout::sign(&sks[0], set.as_slice()[0], 4, QuorumCert::genesis(g)),
            Timeout::sign(&sks[1], set.as_slice()[1], 4, qc3.clone()),
            Timeout::sign(&sks[2], set.as_slice()[2], 4, QuorumCert::genesis(g)),
        ];
        let tc = TimeoutCert { round: 4, timeouts };
        tc.verify(&set, g).unwrap();
        assert_eq!(tc.high_qc_round(), 3);
        assert_eq!(tc.high_qc().unwrap(), &qc3);
    }

    #[test]
    fn message_roundtrips() {
        let (sks, set) = keys(4);
        let v = Vote::sign(&sks[0], set.as_slice()[0], 9, H256([1; 32]));
        let m = Message::Vote(v);
        assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        assert!(Message::decode(b"garbage").is_err());
    }
}
