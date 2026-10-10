//! Leader-side vote accumulator. The current leader collects votes for its
//! own proposed block (one self-vote + each follower's vote) and forms a
//! `QuorumCert` once the count crosses `quorum_count(N)`. Formed QCs are
//! persisted to `CF_QC` keyed by height so subsequent block-builds can pick
//! them up as `parent_qc` for the next header.
//!
//! Concurrency model: the pool is shared (`Arc<Mutex<_>>`) between the
//! sequencer loop (which seeds self-votes) and the RPC server (which receives
//! follower votes). Locking is contended only at vote-receive time, which is
//! O(N) per height and never hot-path.

use cb_consensus::{quorum_count, ConsensusError, QuorumCert, Vote};
use cb_state::{Database, Meta, Qcs, ValidatorInfo, Validators};
use cb_types::{Address, H256};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

/// Result of `add_vote`. Lets the caller distinguish a vote that just pushed
/// the pool to quorum (worth logging / wiring into next-block proposal) from
/// a vote that was already counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteOutcome {
    /// Vote stored, quorum not yet reached for this (view, height).
    Accepted,
    /// Vote stored AND quorum was crossed by this addition; QC was formed +
    /// persisted to `CF_QC` keyed by `height`. Idempotent: subsequent votes
    /// past the threshold still return `Accepted`.
    Quorum,
    /// Vote was already in the pool (same validator, same view+height). Not
    /// an error per se — just a no-op.
    Duplicate,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum VoteRejection {
    #[error("validator not in genesis set")]
    UnknownValidator,
    #[error("malformed signature")]
    MalformedSig,
    #[error("vote signer mismatch with claimed validator")]
    SignerMismatch,
    #[error("vote is for height {got} but head is at {head}")]
    StaleHeight { got: u64, head: u64 },
    #[error("vote is for an unknown block hash at height {0}")]
    UnknownBlock(u64),
}

impl From<ConsensusError> for VoteRejection {
    fn from(e: ConsensusError) -> Self {
        match e {
            ConsensusError::MalformedVoteSig(_) => VoteRejection::MalformedSig,
            ConsensusError::VoteSignerMismatch { .. } => VoteRejection::SignerMismatch,
            // Other variants only arise from QC verification, which we don't
            // do inside the pool. Map to MalformedSig as a safe default.
            _ => VoteRejection::MalformedSig,
        }
    }
}

/// Per-(view, height, hash) bucket. Identified by the block hash so that if
/// two proposals at the same (view, height) ever land — they shouldn't, but
/// the pool stays correct under churn — votes don't cross-pollinate.
#[derive(Default)]
struct Bucket {
    votes: Vec<Vote>,
    voters: HashSet<Address>,
    quorum_formed: bool,
}

/// Indexed by (view, height) → block_hash → bucket. Most of the time a single
/// (view, height) maps to a single block_hash, but the inner map handles the
/// (rare) case of competing proposals during a view-change race.
#[derive(Default)]
pub struct VotePool {
    buckets: BTreeMap<(u64, u64), BTreeMap<H256, Bucket>>,
}

impl VotePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `vote` and form + persist a QC if the pool now has ≥ quorum.
    /// Returns the outcome. `db` is needed only to write CF_QC on quorum;
    /// the validator set is read inside this call too so rotations land
    /// consistently.
    ///
    /// This is [`VotePool::verify`] followed by [`VotePool::add_verified`].
    /// A caller that shares the pool behind a lock should call
    /// [`add_vote_shared`] instead, which runs the signature recovery before
    /// taking the lock.
    pub fn add_vote(&mut self, db: &Database, vote: Vote) -> Result<VoteOutcome, VoteRejection> {
        let verified = Self::verify(db, vote)?;
        Ok(self.add_verified(db, verified))
    }

    /// The checks that need no pool state: the signature recovers to the
    /// claimed validator, and that validator is in the set. This is the
    /// expensive part of accepting a vote (one secp256k1 recovery), so it
    /// runs before a shared pool is locked.
    pub fn verify(db: &Database, vote: Vote) -> Result<VerifiedVote, VoteRejection> {
        // Pre-validation: signer must recover to claimed validator AND be in
        // the genesis-pinned validator set.
        vote.verify_self_consistent().map_err(VoteRejection::from)?;
        let validators = Validators::new(db).list();
        if validators.iter().all(|v| v.addr != vote.validator) {
            return Err(VoteRejection::UnknownValidator);
        }
        Ok(VerifiedVote { vote, validators })
    }

    /// Record a vote [`VotePool::verify`] accepted, forming, checking and
    /// persisting the certificate if it completes a quorum.
    pub fn add_verified(&mut self, db: &Database, verified: VerifiedVote) -> VoteOutcome {
        match self.insert_verified(verified) {
            Inserted::Duplicate => VoteOutcome::Duplicate,
            Inserted::Accepted => VoteOutcome::Accepted,
            Inserted::Quorum { qc, set, quorum } => persist_qc(db, &qc, &set, quorum),
        }
    }

    /// The bookkeeping half: pool state only, no signature work and no I/O.
    fn insert_verified(&mut self, verified: VerifiedVote) -> Inserted {
        let VerifiedVote { vote, validators } = verified;
        let key = (vote.view, vote.height);
        let by_hash = self.buckets.entry(key).or_default();
        let bucket = by_hash.entry(vote.block_hash).or_default();

        if !bucket.voters.insert(vote.validator) {
            return Inserted::Duplicate;
        }
        bucket.votes.push(vote.clone());

        let n = validators.len();
        let q = quorum_count(n);
        if !bucket.quorum_formed && bucket.votes.len() >= q {
            bucket.quorum_formed = true;
            let qc = QuorumCert {
                view: vote.view,
                height: vote.height,
                block_hash: vote.block_hash,
                votes: bucket.votes.clone(),
            };
            let set: Vec<Address> = validators.iter().map(|v: &ValidatorInfo| v.addr).collect();
            return Inserted::Quorum { qc, set, quorum: q };
        }
        Inserted::Accepted
    }

    /// Drop any pool entries for heights below `keep_above` to keep memory
    /// bounded. Called whenever a new QC is persisted: anything below the new
    /// finalized height is no longer interesting.
    pub fn prune_below(&mut self, keep_above: u64) {
        self.buckets.retain(|(_view, h), _| *h > keep_above);
    }

    /// Diagnostic — number of distinct (view, height, hash) buckets currently
    /// holding votes. Useful for dashboards.
    pub fn bucket_count(&self) -> usize {
        self.buckets.values().map(|m| m.len()).sum()
    }
}

pub type SharedVotePool = Arc<Mutex<VotePool>>;

/// A vote whose signature and membership [`VotePool::verify`] has checked,
/// with the validator set it was checked against.
pub struct VerifiedVote {
    vote: Vote,
    validators: Vec<ValidatorInfo>,
}

impl VerifiedVote {
    pub fn vote(&self) -> &Vote {
        &self.vote
    }
}

enum Inserted {
    Accepted,
    Duplicate,
    Quorum {
        qc: QuorumCert,
        set: Vec<Address>,
        quorum: usize,
    },
}

/// Check a freshly formed certificate and persist it. The check re-verifies
/// every vote's signature, which is a programming-error tripwire — each vote
/// was verified on the way in — so a failure is logged and not persisted.
fn persist_qc(db: &Database, qc: &QuorumCert, set: &[Address], quorum: usize) -> VoteOutcome {
    if let Err(e) = qc.verify(set, quorum) {
        tracing::error!(?e, "newly-formed QC failed verify; not persisting");
        return VoteOutcome::Accepted;
    }
    write_qc(db, qc);
    VoteOutcome::Quorum
}

fn write_qc(db: &Database, qc: &QuorumCert) {
    Qcs::new(db).put(qc.height, &qc.encode());
    // Bump Meta so dashboards / verifiers can read finality state in O(1)
    // instead of scanning CF_QC.
    let meta = Meta::new(db);
    if qc.height > meta.last_qc_height() {
        meta.set_last_qc_height(qc.height);
    }
}

/// Add `vote` to a shared pool, with every signature recovery done outside
/// the lock.
///
/// # Why
///
/// `submitVote` used to lock the pool and then call [`VotePool::add_vote`],
/// which recovers the vote's signer — about 60 µs of secp256k1 — and, on
/// the vote that completes a quorum, re-verifies every vote in the
/// certificate. The RPC server runs one worker thread per core, so every
/// other vote, and the leader's own self-vote on the block-producing thread,
/// queued behind that work. Now the lock covers only the bucket update; the
/// vote's recovery comes before it and the certificate's check after it.
///
/// Persisting the certificate re-takes the lock so two certificates formed
/// on two threads cannot interleave the read-compare-write of
/// `last_qc_height`.
pub fn add_vote_shared(
    pool: &SharedVotePool,
    db: &Database,
    vote: Vote,
) -> Result<VoteOutcome, VoteRejection> {
    let verified = VotePool::verify(db, vote)?;
    let inserted = pool
        .lock()
        .expect("vote pool poisoned")
        .insert_verified(verified);
    Ok(match inserted {
        Inserted::Duplicate => VoteOutcome::Duplicate,
        Inserted::Accepted => VoteOutcome::Accepted,
        Inserted::Quorum { qc, set, quorum } => {
            if let Err(e) = qc.verify(&set, quorum) {
                tracing::error!(?e, "newly-formed QC failed verify; not persisting");
                return Ok(VoteOutcome::Accepted);
            }
            let _guard = pool.lock().expect("vote pool poisoned");
            write_qc(db, &qc);
            VoteOutcome::Quorum
        }
    })
}

pub fn new_shared() -> SharedVotePool {
    Arc::new(Mutex::new(VotePool::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_consensus::vote_signing_bytes;
    use cb_crypto::{address_from_pubkey, generate_keypair, sign};
    use cb_state::ValidatorInfo;
    use cb_types::Address;
    use tempfile::tempdir;

    fn setup_three_validators(db: &Database) -> Vec<(cb_crypto::SecretKey, Address)> {
        let mut keys = Vec::new();
        let mut infos: Vec<ValidatorInfo> = Vec::new();
        for _ in 0..3 {
            let (sk, pk) = generate_keypair();
            let addr = address_from_pubkey(&pk);
            infos.push(ValidatorInfo {
                addr,
                pubkey: pk.serialize().to_vec(),
                weight: 1,
            });
            keys.push((sk, addr));
        }
        Validators::new(db).put_set(&infos);
        keys
    }

    fn make_vote(
        sk: &cb_crypto::SecretKey,
        addr: Address,
        view: u64,
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
    fn second_vote_forms_qc_two_of_three() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let hash = H256([7; 32]);

        let mut pool = VotePool::new();
        let v0 = make_vote(&keys[0].0, keys[0].1, 0, 1, hash);
        let v1 = make_vote(&keys[1].0, keys[1].1, 0, 1, hash);

        assert_eq!(pool.add_vote(&db, v0).unwrap(), VoteOutcome::Accepted);
        assert!(Qcs::new(&db).get(1).is_none());

        assert_eq!(pool.add_vote(&db, v1).unwrap(), VoteOutcome::Quorum);
        let qc_bytes = Qcs::new(&db).get(1).expect("QC persisted on quorum");
        let qc = QuorumCert::decode(&qc_bytes).unwrap();
        assert_eq!(qc.height, 1);
        assert_eq!(qc.votes.len(), 2);
    }

    #[test]
    fn third_vote_after_quorum_is_accepted_not_duplicate() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let hash = H256([7; 32]);

        let mut pool = VotePool::new();
        for (sk, addr) in &keys {
            let v = make_vote(sk, *addr, 0, 1, hash);
            pool.add_vote(&db, v).unwrap();
        }
        // QC was formed at vote #2; vote #3 just adds to the bucket.
        assert!(Qcs::new(&db).get(1).is_some());
    }

    #[test]
    fn duplicate_vote_from_same_validator_is_idempotent() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let hash = H256([7; 32]);
        let mut pool = VotePool::new();
        let v = make_vote(&keys[0].0, keys[0].1, 0, 1, hash);
        assert_eq!(
            pool.add_vote(&db, v.clone()).unwrap(),
            VoteOutcome::Accepted
        );
        assert_eq!(pool.add_vote(&db, v).unwrap(), VoteOutcome::Duplicate);
        assert!(Qcs::new(&db).get(1).is_none());
    }

    #[test]
    fn vote_from_outside_validator_set_is_rejected() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let _keys = setup_three_validators(&db);
        let (sk, pk) = generate_keypair();
        let outsider = address_from_pubkey(&pk);
        let v = make_vote(&sk, outsider, 0, 1, H256([7; 32]));
        let mut pool = VotePool::new();
        assert!(matches!(
            pool.add_vote(&db, v),
            Err(VoteRejection::UnknownValidator)
        ));
    }

    #[test]
    fn vote_with_signer_mismatch_is_rejected() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let mut v = make_vote(&keys[0].0, keys[0].1, 0, 1, H256([7; 32]));
        // Tamper with claimed validator while keeping the sig from key[0].
        v.validator = keys[1].1;
        let mut pool = VotePool::new();
        assert!(matches!(
            pool.add_vote(&db, v),
            Err(VoteRejection::SignerMismatch)
        ));
    }

    /// A bad vote is refused by the signature check alone, without the
    /// pool's lock: a thread holding the lock does not delay the refusal.
    #[test]
    fn a_bad_vote_is_refused_while_another_thread_holds_the_pool() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let pool = new_shared();
        let mut v = make_vote(&keys[0].0, keys[0].1, 0, 1, H256([7; 32]));
        v.validator = keys[1].1; // signer mismatch
        let guard = pool.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let pool = pool.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let _ = tx.send(add_vote_shared(&pool, &db, v));
            });
        }
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("refused without waiting for the lock");
        assert!(matches!(got, Err(VoteRejection::SignerMismatch)));
        drop(guard);
        assert_eq!(pool.lock().unwrap().bucket_count(), 0, "nothing stored");
    }

    /// The shared path forms, persists and reports a certificate exactly as
    /// `add_vote` does, and treats a repeat as a duplicate.
    #[test]
    fn the_shared_path_forms_the_same_certificate() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let hash = H256([7; 32]);
        let pool = new_shared();
        let v0 = make_vote(&keys[0].0, keys[0].1, 0, 1, hash);
        let v1 = make_vote(&keys[1].0, keys[1].1, 0, 1, hash);
        assert_eq!(
            add_vote_shared(&pool, &db, v0.clone()).unwrap(),
            VoteOutcome::Accepted
        );
        assert_eq!(
            add_vote_shared(&pool, &db, v0).unwrap(),
            VoteOutcome::Duplicate
        );
        assert!(Qcs::new(&db).get(1).is_none());
        assert_eq!(
            add_vote_shared(&pool, &db, v1).unwrap(),
            VoteOutcome::Quorum
        );
        let qc = QuorumCert::decode(&Qcs::new(&db).get(1).unwrap()).unwrap();
        assert_eq!(qc.votes.len(), 2);
        assert_eq!(Meta::new(&db).last_qc_height(), 1);
        // Outsiders are refused on the shared path too.
        let (sk, pk) = generate_keypair();
        let outsider = make_vote(&sk, address_from_pubkey(&pk), 0, 1, hash);
        assert!(matches!(
            add_vote_shared(&pool, &db, outsider),
            Err(VoteRejection::UnknownValidator)
        ));
    }

    /// `verify` is pure: a refused vote leaves no trace and an accepted one
    /// is only stored by `add_verified`.
    #[test]
    fn verify_does_not_touch_the_pool() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let keys = setup_three_validators(&db);
        let mut pool = VotePool::new();
        let v = make_vote(&keys[0].0, keys[0].1, 0, 1, H256([7; 32]));
        let mut bad = v.clone();
        bad.sig = vec![0u8; 65];
        assert!(VotePool::verify(&db, bad).is_err());
        let ok = VotePool::verify(&db, v).unwrap();
        assert_eq!(ok.vote().height, 1);
        assert_eq!(pool.bucket_count(), 0);
        assert_eq!(pool.add_verified(&db, ok), VoteOutcome::Accepted);
        assert_eq!(pool.bucket_count(), 1);
    }
}
