//! Certificates and vetoes that ride along with a proposal.
//!
//! A proposal its quorum certificate already justifies may still carry a
//! timeout certificate, and the engine processes it either way: it moves
//! the round, is trusted byte for byte afterwards and strikes a leader's
//! reputation. Each test here plays a member that forges one, and checks
//! that the forgery goes nowhere. The last checks that a veto given to a
//! proposal that arrived before its parent still stands when the parent
//! turns up.

use super::*;

/// A timeout certificate with no timeouts in it: signed by nobody.
fn forged(round: Round) -> TimeoutCert {
    TimeoutCert {
        round,
        timeouts: Vec::new(),
    }
}

#[test]
fn a_forged_tc_on_a_qc_justified_proposal_is_refused() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc2 = kit.qc_for(&chain[1]);
    let mut e = kit.engine(0);
    let _ = feed(&mut e, &chain, 0);
    let before = e.round();

    // Validator 3 leads round 3 and wraps a forged certificate for round 2.
    let q = kit.proposal_by(3, 3, 2, qc2, Some(forged(2)), b"carrier");
    let acts = e.on_message(msg(&q), None, 0);
    assert!(
        votes(&acts).is_empty(),
        "voted for a proposal carrying a forged TC"
    );
    assert_eq!(e.round(), before, "an unverified TC moved the round");
}

#[test]
fn a_tc_for_a_round_other_than_the_previous_one_is_refused() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc2 = kit.qc_for(&chain[1]);
    let mut e = kit.engine(0);
    let _ = feed(&mut e, &chain, 0);

    // A certificate for round 1002 on a round-3 proposal: the jump the
    // attack needed, even before asking whether it verifies.
    let q = kit.proposal_by(3, 3, 2, qc2, Some(forged(1002)), b"carrier");
    let acts = e.on_message(msg(&q), None, 0);
    assert!(votes(&acts).is_empty());
    assert!(e.round() < 1000, "jumped to round {}", e.round());
}

#[test]
fn a_forged_tc_cannot_justify_a_fork_below_the_lock() {
    // The whole attack, end to end: jump the victim with a carried TC, then
    // extend the certified grandparent to abandon its lock.
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc1 = kit.qc_for(&chain[0]);
    let qc2 = kit.qc_for(&chain[1]);
    let mut e = kit.engine(0);
    let _ = feed(&mut e, &chain, 0);

    let carrier = kit.proposal_by(3, 3, 2, qc2, Some(forged(1002)), b"carrier");
    let _ = e.on_message(msg(&carrier), None, 0);
    let fork = kit.proposal_by(3, 1003, 1, qc1, Some(forged(1002)), b"fork");
    let acts = e.on_message(msg(&fork), None, 0);
    assert!(votes(&acts).is_empty(), "voted for a fork below its lock");
    assert!(e.round() < 1000, "jumped to round {}", e.round());
    assert!(e.committed_round() < 3, "committed past the lock");
}

#[test]
fn a_forged_tc_strikes_nobody() {
    let kit = Kit::new(4);
    let chain = kit.chain(3);
    let qc3 = kit.qc_for(&chain[2]);
    let mut e = kit.engine(2);
    let _ = feed(&mut e, &chain, 0);

    // Round 4's leader blames round 5's round-robin leader, validator 1,
    // for a timeout that never happened.
    let p4 = kit.proposal(4, 3, qc3, Some(forged(5)), b"r4");
    let _ = e.on_message(msg(&p4), None, 0);
    for i in [0usize, 1, 3] {
        let _ = e.on_message(Message::Vote(kit.vote(i, 4, p4.header.id())), None, 0);
    }
    assert!(
        !e.standing_aside(7).contains(&kit.addr(1)),
        "a forged TC benched an honest validator"
    );
}

#[test]
fn a_genuine_tc_on_a_qc_justified_proposal_is_still_accepted() {
    // The fix must not refuse what honest validators can send: a verified
    // TC for the previous round alongside a certificate that also
    // justifies the proposal.
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc2 = kit.qc_for(&chain[1]);
    let mut e = kit.engine(1);
    let _ = feed(&mut e, &chain, 0);
    let tc2 = kit.tc(2, &[qc2.clone(), qc2.clone(), qc2.clone()]);
    let q = kit.proposal(3, 2, qc2, Some(tc2), b"r3");
    let acts = e.on_message(msg(&q), None, 0);
    assert_eq!(votes(&acts).len(), 1);
}

#[test]
fn a_veto_survives_the_wait_for_the_parent() {
    let kit = Kit::new(4);
    let chain = kit.chain(3);
    let mut e = kit.engine(1);
    let _ = feed(&mut e, &chain[..1], 0);

    // Round 3 arrives before round 2, and the driver vetoes it.
    let _ = e.on_message(msg(&chain[2]), Some("executed root differs"), 0);
    assert_eq!(e.orphan_count(), 1);
    let acts = e.on_message(msg(&chain[1]), None, 0);
    let vs = votes(&acts);
    assert!(
        !vs.iter().any(|v| v.id == chain[2].header.id()),
        "voted for a proposal the driver vetoed"
    );
}

#[test]
fn an_unvetoed_orphan_is_still_voted_for() {
    let kit = Kit::new(4);
    let chain = kit.chain(3);
    let mut e = kit.engine(1);
    let _ = feed(&mut e, &chain[..1], 0);
    let _ = e.on_message(msg(&chain[2]), None, 0);
    let acts = e.on_message(msg(&chain[1]), None, 0);
    assert!(votes(&acts).iter().any(|v| v.id == chain[2].header.id()));
}
