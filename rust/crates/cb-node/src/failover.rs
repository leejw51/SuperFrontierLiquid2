//! Who leads next when the leader stops, and when it is safe to take over.
//!
//! # The rule
//!
//! A block proposed in view `V` is valid only if it is signed by
//! `validators[V % N]` — round robin, checked by every node in
//! `cb_net::apply_remote_block`. So skipping a dead leader is not a
//! negotiation: it is arithmetic. Advance the view, and the next validator in
//! the set is the one whose blocks everyone will accept.
//!
//! What has to be agreed is *when*, and that is what the two conditions below
//! are for.
//!
//! # Condition one: the leader is gone
//!
//! Not "no blocks for a while": a chain with nothing to do produces no blocks
//! at all, because the builder refuses to make an empty one, so an idle
//! leader and a dead one look the same from a follower. What separates them is
//! asking. The follower's timer only prompts the question; the answer is the
//! leader's block stream breaking, or its `chainInfo` going unanswered. A
//! leader that is alive and answering but has stopped producing is not
//! covered here — that needs the followers to agree among themselves.
//!
//! # Condition two: this node can safely build on what it has
//!
//! There is no rollback in this codebase, so a candidate must only build on a
//! tip the rest of the network can still agree with.
//!
//! A certificate for block N travels in block N+1's header, so a follower that
//! is up to date always holds one uncertified block at the tip: head is N+1,
//! the highest certificate is N. Requiring head to equal the last certificate
//! would therefore block every takeover on a live chain, which is what the
//! first version of this did.
//!
//! One uncertified block is safe here, and two is not. One leader leads a
//! view, views only go forward, and a crashed validator stops rather than
//! lying, so at most one block can exist at height N+1 and it is the one this
//! node holds. A candidate that is two or more above the last certificate has
//! taken more on trust than the rest of the network can be assumed to have
//! seen, and it waits instead.
//!
//! That is the crash-fault assumption stated as a rule. A validator that
//! equivocates rather than stopping breaks it, and nothing here defends
//! against that.
//!
//! This is crash-fault failover. It assumes validators stop, not lie: a
//! quorum of three tolerates one silent node and no Byzantine one.

use cb_state::{Database, Meta, Validators};
use cb_types::Address;
use std::time::{Duration, Instant};

/// Why a node did not take over, for the operator who asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotLeading {
    /// The leader is producing blocks.
    LeaderIsAlive,
    /// The chain has no validator set, so there is nobody to rotate to.
    NoValidatorSet,
    /// Another validator leads the next view.
    SomeoneElseLeadsNextView,
    /// This node is more than one block ahead of the highest certificate it
    /// holds, so it cannot assume the rest of the network has what it has.
    TooFarAboveLastQuorum { head: u64, last_qc: u64 },
}

/// What a follower should do when its view timer fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Take over: set the view to `view` and start producing blocks.
    Promote { view: u64, head: u64 },
    /// Stay a follower.
    Stay(NotLeading),
}

/// Tracks the last time this node applied a block.
pub struct ViewTimer {
    silence: Duration,
    last_progress: Instant,
    last_height: u64,
}

impl ViewTimer {
    pub fn new(silence: Duration, head: u64) -> Self {
        Self {
            silence,
            last_progress: Instant::now(),
            last_height: head,
        }
    }

    /// Call whenever a block is applied.
    pub fn saw_block(&mut self, height: u64) {
        if height > self.last_height {
            self.last_height = height;
            self.last_progress = Instant::now();
        }
    }

    /// The leader answered a liveness probe. It is alive and idle, which on a
    /// chain that refuses to build empty blocks is the ordinary state.
    pub fn saw_leader(&mut self) {
        self.last_progress = Instant::now();
    }

    pub fn is_silent(&self) -> bool {
        self.last_progress.elapsed() >= self.silence
    }

    pub fn silence(&self) -> Duration {
        self.silence
    }
}

/// Decide whether `me` should take over, given the chain's current state.
///
/// Pure: it reads the database and a clock verdict and returns a decision, so
/// the rule can be tested without a network.
pub fn decide(db: &Database, me: Address, leader_is_silent: bool) -> Decision {
    if !leader_is_silent {
        return Decision::Stay(NotLeading::LeaderIsAlive);
    }
    let validators = Validators::new(db);
    if validators.count() == 0 {
        return Decision::Stay(NotLeading::NoValidatorSet);
    }
    let next_view = Meta::new(db).view() + 1;
    match validators.leader_for_view(next_view) {
        Some(v) if v.addr == me => {}
        _ => return Decision::Stay(NotLeading::SomeoneElseLeadsNextView),
    }
    let head = Meta::new(db).head_height();
    let last_qc = Meta::new(db).last_qc_height();
    if head > last_qc + 1 {
        return Decision::Stay(NotLeading::TooFarAboveLastQuorum { head, last_qc });
    }
    Decision::Promote {
        view: next_view,
        head,
    }
}

/// Record the view this node is about to lead, so the blocks it signs carry
/// it and every follower checks the signature against the right validator.
///
/// Called at the moment the node starts producing, never at the moment it
/// decides to. Writing it on the decision alone left a node whose view was
/// ahead of the chain's: it then computed the next leader from a view nobody
/// else was in, and pointed at the wrong validator for as long as the
/// database survived.
pub fn take_view(db: &Database, view: u64) {
    Meta::new(db).set_view(view);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_state::{ValidatorInfo, Validators};
    use cb_types::H256;
    use tempfile::tempdir;

    fn addr(n: u8) -> Address {
        Address([n; 20])
    }

    fn db_with(view: u64, head: u64, last_qc: u64, n: u8) -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let set: Vec<ValidatorInfo> = (1..=n)
            .map(|i| ValidatorInfo {
                addr: addr(i),
                pubkey: vec![i; 33],
                weight: 1,
            })
            .collect();
        Validators::new(&db).put_set(&set);
        Meta::new(&db).set_view(view);
        Meta::new(&db).set_head(head, H256([9; 32]), H256([8; 32]));
        Meta::new(&db).set_last_qc_height(last_qc);
        (dir, db)
    }

    #[test]
    fn a_live_leader_is_left_alone() {
        let (_d, db) = db_with(0, 5, 5, 3);
        assert_eq!(
            decide(&db, addr(2), false),
            Decision::Stay(NotLeading::LeaderIsAlive)
        );
    }

    #[test]
    fn the_next_validator_in_the_set_takes_over() {
        // View 0 belongs to validator 1; view 1 to validator 2.
        let (_d, db) = db_with(0, 5, 5, 3);
        assert_eq!(
            decide(&db, addr(2), true),
            Decision::Promote { view: 1, head: 5 }
        );
        // And nobody else does.
        for other in [1u8, 3] {
            assert_eq!(
                decide(&db, addr(other), true),
                Decision::Stay(NotLeading::SomeoneElseLeadsNextView),
                "validator {other} must not promote into view 1"
            );
        }
    }

    #[test]
    fn rotation_wraps_so_a_dead_successor_is_skipped_too() {
        // The chain already moved to view 2 because view 1's leader was also
        // silent; view 3 comes back round to validator 1.
        let (_d, db) = db_with(2, 9, 9, 3);
        assert_eq!(
            decide(&db, addr(1), true),
            Decision::Promote { view: 3, head: 9 }
        );
    }

    /// The safety rule: a candidate holding a block nobody certified must not
    /// build on it.
    #[test]
    fn one_uncertified_block_may_lead_and_two_may_not() {
        // The ordinary state of a follower on a live chain: the certificate
        // for block 6 is still travelling in block 7's header.
        let (_d, db) = db_with(0, 6, 5, 3);
        assert_eq!(
            decide(&db, addr(2), true),
            Decision::Promote { view: 1, head: 6 },
            "one uncertified block at the tip is the normal case, not a hazard"
        );

        // Two is more than the rest of the network can be assumed to hold.
        let (_d2, db2) = db_with(0, 7, 5, 3);
        assert_eq!(
            decide(&db2, addr(2), true),
            Decision::Stay(NotLeading::TooFarAboveLastQuorum {
                head: 7,
                last_qc: 5
            })
        );
        // The certificate catches up and it may lead again.
        Meta::new(&db2).set_last_qc_height(6);
        assert_eq!(
            decide(&db2, addr(2), true),
            Decision::Promote { view: 1, head: 7 }
        );
    }

    #[test]
    fn a_chain_with_no_validator_set_never_rotates() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Meta::new(&db).set_head(5, H256([1; 32]), H256([2; 32]));
        assert_eq!(
            decide(&db, addr(1), true),
            Decision::Stay(NotLeading::NoValidatorSet)
        );
    }

    #[test]
    fn the_timer_only_restarts_on_a_new_height() {
        let mut t = ViewTimer::new(Duration::from_millis(40), 3);
        t.saw_block(3); // not new
        std::thread::sleep(Duration::from_millis(60));
        assert!(t.is_silent(), "a repeat of the same height is not progress");
        t.saw_block(4);
        assert!(!t.is_silent());
    }
}
