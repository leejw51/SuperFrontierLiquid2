//! BFT consensus primitives shared across the leader, follower, and verifier paths.
//!
//! Today's flavour: a 3-validator HotStuff-lite where every block at height H
//! requires 2-of-3 votes before block H+1 carries its quorum certificate (QC).
//! With n=3 the network is **crash-fault-tolerant** for one fault (2/3 quorum
//! still forms after one validator goes down) but cannot tolerate a Byzantine
//! validator on its own — Byzantine safety relies on slashing as the economic
//! deterrent. The protocol math: standard HotStuff requires n >= 3f+1 for f
//! Byzantine faults; n=3 means f=0.
//!
//! Chained-HotStuff structure: block H contains the QC for block H-1 in its
//! `parent_qc` slot. Genesis (height 0) has no QC; the very first proposed
//! block (height 1) likewise carries no parent_qc since there's nothing yet
//! committed. Block 2 onward carries the QC proving 2/3 of validators voted on
//! its parent.

pub mod bft;

use cb_crypto::{keccak256, recover, Signature};
use cb_types::{Address, H256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Monotonically increasing view number. The leader for view V is
/// `validators[V mod N]`. Views advance on commit (steady-state) and on
/// view-change timeout (when the current leader fails to produce in time).
pub type View = u64;

/// A single validator's signature over a block hash, scoped to a (view, height).
/// Re-binding the signature to view+height prevents a vote replay across
/// different rounds: the same byte sequence cannot stand in for a vote on a
/// different block at a different height.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub view: View,
    pub height: u64,
    pub block_hash: H256,
    pub validator: Address,
    /// 65-byte secp256k1 signature over `vote_signing_bytes`.
    pub sig: Vec<u8>,
}

/// The canonical preimage a validator signs when voting. Keeping this stable
/// matters: any change becomes a hard fork.
pub fn vote_signing_bytes(view: View, height: u64, block_hash: H256) -> [u8; 32] {
    // domain || view || height || block_hash, hashed.
    let mut buf = Vec::with_capacity(8 + 8 + 8 + 32);
    buf.extend_from_slice(b"UL-VOTE\0");
    buf.extend_from_slice(&view.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
    buf.extend_from_slice(&block_hash.0);
    keccak256(&buf).0
}

impl Vote {
    /// Recover the signing address from the embedded signature. Returns the
    /// recovered address rather than `validator` — callers cross-check.
    pub fn recover_signer(&self) -> Result<Address, ConsensusError> {
        let sig = Signature::from_bytes(&self.sig)
            .map_err(|e| ConsensusError::MalformedVoteSig(e.to_string()))?;
        let digest = H256(vote_signing_bytes(self.view, self.height, self.block_hash));
        recover(&sig, digest).map_err(|e| ConsensusError::MalformedVoteSig(e.to_string()))
    }

    /// Verify the embedded signature recovers to `self.validator`. Cheap to
    /// call repeatedly; callers should also check the validator is in the set.
    pub fn verify_self_consistent(&self) -> Result<(), ConsensusError> {
        let recovered = self.recover_signer()?;
        if recovered != self.validator {
            return Err(ConsensusError::VoteSignerMismatch {
                claimed: self.validator,
                recovered,
            });
        }
        Ok(())
    }
}

/// A 2-of-N quorum certificate: a set of validator votes proving a block was
/// finalized at a (view, height). `block_hash` is the hash of the block this
/// QC certifies; `votes` are the validator signatures.
///
/// QCs are bincode-serialized into block headers' `parent_qc` slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuorumCert {
    pub view: View,
    pub height: u64,
    pub block_hash: H256,
    pub votes: Vec<Vote>,
}

impl QuorumCert {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("QuorumCert encode")
    }
    pub fn decode(b: &[u8]) -> Result<Self, ConsensusError> {
        bincode::deserialize(b).map_err(|e| ConsensusError::MalformedQc(e.to_string()))
    }

    /// Verify the QC: every vote is internally consistent, all votes share the
    /// same (view, height, block_hash), validators are unique, and the count
    /// reaches the supplied quorum threshold.
    ///
    /// `validator_set` is the canonical (addr, weight) list at this height.
    /// Today we count validators (not weights) so quorum = `ceil(2 * N / 3)`.
    pub fn verify(
        &self,
        validator_set: &[Address],
        quorum_count: usize,
    ) -> Result<(), ConsensusError> {
        if self.votes.len() < quorum_count {
            return Err(ConsensusError::QcBelowQuorum {
                got: self.votes.len(),
                need: quorum_count,
            });
        }
        let mut seen = std::collections::HashSet::new();
        for vote in &self.votes {
            if vote.view != self.view {
                return Err(ConsensusError::QcVoteViewMismatch {
                    qc: self.view,
                    vote: vote.view,
                });
            }
            if vote.height != self.height {
                return Err(ConsensusError::QcVoteHeightMismatch {
                    qc: self.height,
                    vote: vote.height,
                });
            }
            if vote.block_hash != self.block_hash {
                return Err(ConsensusError::QcVoteHashMismatch);
            }
            vote.verify_self_consistent()?;
            if !validator_set.contains(&vote.validator) {
                return Err(ConsensusError::VoterNotInSet(vote.validator));
            }
            if !seen.insert(vote.validator) {
                return Err(ConsensusError::DuplicateVoter(vote.validator));
            }
        }
        Ok(())
    }
}

/// Standard 2/3 quorum threshold rounded up: `quorum_count(3) == 2`,
/// `quorum_count(4) == 3`, `quorum_count(7) == 5`.
pub fn quorum_count(n: usize) -> usize {
    n.saturating_mul(2).div_ceil(3)
}

/// View-change message: when a validator's view timer fires (no commit in time),
/// it broadcasts `NewView(v+1)` to nudge the next leader. The next leader takes
/// over once it has 2/3 of these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewView {
    pub new_view: View,
    pub last_committed_height: u64,
    pub last_committed_qc: Option<QuorumCert>,
    pub validator: Address,
    /// 65-byte secp256k1 over `new_view_signing_bytes`.
    pub sig: Vec<u8>,
}

pub fn new_view_signing_bytes(new_view: View, last_committed_height: u64) -> [u8; 32] {
    let mut buf = Vec::with_capacity(10 + 8 + 8);
    buf.extend_from_slice(b"UL-VIEWCHG\0");
    buf.extend_from_slice(&new_view.to_be_bytes());
    buf.extend_from_slice(&last_committed_height.to_be_bytes());
    keccak256(&buf).0
}

#[derive(Debug, Error)]
pub enum ConsensusError {
    #[error("malformed vote signature: {0}")]
    MalformedVoteSig(String),
    #[error("vote signer mismatch: claimed {claimed}, recovered {recovered}")]
    VoteSignerMismatch {
        claimed: Address,
        recovered: Address,
    },
    #[error("malformed QC bytes: {0}")]
    MalformedQc(String),
    #[error("QC below quorum: got {got} votes, need {need}")]
    QcBelowQuorum { got: usize, need: usize },
    #[error("QC vote view {vote} != QC view {qc}")]
    QcVoteViewMismatch { qc: View, vote: View },
    #[error("QC vote height {vote} != QC height {qc}")]
    QcVoteHeightMismatch { qc: u64, vote: u64 },
    #[error("QC vote block_hash differs from QC block_hash")]
    QcVoteHashMismatch,
    #[error("voter {0} not in validator set")]
    VoterNotInSet(Address),
    #[error("duplicate voter {0} in QC")]
    DuplicateVoter(Address),
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_crypto::{address_from_pubkey, generate_keypair, sign};

    fn make_vote(
        sk: &cb_crypto::SecretKey,
        addr: Address,
        view: View,
        height: u64,
        hash: H256,
    ) -> Vote {
        let preimage = H256(vote_signing_bytes(view, height, hash));
        let sig = sign(sk, preimage).unwrap().to_vec();
        Vote {
            view,
            height,
            block_hash: hash,
            validator: addr,
            sig,
        }
    }

    #[test]
    fn vote_recovers_to_signer() {
        let (sk, pk) = generate_keypair();
        let addr = address_from_pubkey(&pk);
        let v = make_vote(&sk, addr, 7, 42, H256([1; 32]));
        v.verify_self_consistent().unwrap();
    }

    #[test]
    fn vote_with_wrong_claimed_validator_rejected() {
        let (sk, pk) = generate_keypair();
        let real_addr = address_from_pubkey(&pk);
        let mut v = make_vote(&sk, real_addr, 7, 42, H256([1; 32]));
        v.validator = Address([0xee; 20]);
        let err = v.verify_self_consistent().unwrap_err();
        assert!(matches!(err, ConsensusError::VoteSignerMismatch { .. }));
    }

    #[test]
    fn quorum_count_two_of_three() {
        assert_eq!(quorum_count(3), 2);
        assert_eq!(quorum_count(4), 3);
        assert_eq!(quorum_count(5), 4);
        assert_eq!(quorum_count(7), 5);
        assert_eq!(quorum_count(0), 0);
        assert_eq!(quorum_count(1), 1);
    }

    fn three_validators() -> Vec<(cb_crypto::SecretKey, Address)> {
        (0..3)
            .map(|_| {
                let (sk, pk) = generate_keypair();
                (sk, address_from_pubkey(&pk))
            })
            .collect()
    }

    #[test]
    fn qc_verify_two_of_three_passes() {
        let vals = three_validators();
        let set: Vec<Address> = vals.iter().map(|(_, a)| *a).collect();
        let hash = H256([7; 32]);
        let votes = vec![
            make_vote(&vals[0].0, vals[0].1, 1, 5, hash),
            make_vote(&vals[1].0, vals[1].1, 1, 5, hash),
        ];
        let qc = QuorumCert {
            view: 1,
            height: 5,
            block_hash: hash,
            votes,
        };
        qc.verify(&set, quorum_count(3)).unwrap();
    }

    #[test]
    fn qc_below_quorum_rejected() {
        let vals = three_validators();
        let set: Vec<Address> = vals.iter().map(|(_, a)| *a).collect();
        let hash = H256([7; 32]);
        let qc = QuorumCert {
            view: 1,
            height: 5,
            block_hash: hash,
            votes: vec![make_vote(&vals[0].0, vals[0].1, 1, 5, hash)],
        };
        let err = qc.verify(&set, quorum_count(3)).unwrap_err();
        assert!(matches!(
            err,
            ConsensusError::QcBelowQuorum { got: 1, need: 2 }
        ));
    }

    #[test]
    fn qc_with_voter_outside_set_rejected() {
        let vals = three_validators();
        let outsider = generate_keypair();
        let outsider_addr = address_from_pubkey(&outsider.1);
        let set: Vec<Address> = vals.iter().map(|(_, a)| *a).collect();
        let hash = H256([7; 32]);
        let qc = QuorumCert {
            view: 1,
            height: 5,
            block_hash: hash,
            votes: vec![
                make_vote(&vals[0].0, vals[0].1, 1, 5, hash),
                make_vote(&outsider.0, outsider_addr, 1, 5, hash),
            ],
        };
        let err = qc.verify(&set, quorum_count(3)).unwrap_err();
        assert!(matches!(err, ConsensusError::VoterNotInSet(_)));
    }

    #[test]
    fn qc_with_duplicate_voter_rejected() {
        let vals = three_validators();
        let set: Vec<Address> = vals.iter().map(|(_, a)| *a).collect();
        let hash = H256([7; 32]);
        let qc = QuorumCert {
            view: 1,
            height: 5,
            block_hash: hash,
            votes: vec![
                make_vote(&vals[0].0, vals[0].1, 1, 5, hash),
                make_vote(&vals[0].0, vals[0].1, 1, 5, hash),
            ],
        };
        let err = qc.verify(&set, quorum_count(3)).unwrap_err();
        assert!(matches!(err, ConsensusError::DuplicateVoter(_)));
    }

    #[test]
    fn qc_encode_decode_roundtrip() {
        let vals = three_validators();
        let hash = H256([7; 32]);
        let qc = QuorumCert {
            view: 1,
            height: 5,
            block_hash: hash,
            votes: vec![
                make_vote(&vals[0].0, vals[0].1, 1, 5, hash),
                make_vote(&vals[1].0, vals[1].1, 1, 5, hash),
            ],
        };
        let bytes = qc.encode();
        let parsed = QuorumCert::decode(&bytes).unwrap();
        assert_eq!(qc, parsed);
    }

    #[test]
    fn vote_signing_bytes_changes_with_view_height_or_hash() {
        let a = vote_signing_bytes(1, 5, H256([1; 32]));
        let b = vote_signing_bytes(2, 5, H256([1; 32]));
        let c = vote_signing_bytes(1, 6, H256([1; 32]));
        let d = vote_signing_bytes(1, 5, H256([2; 32]));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }
}
