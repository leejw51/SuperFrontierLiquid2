//! Finality proofs must be the same bytes on every validator.
//!
//! The driver seals each committed proposal's header and certificate into
//! the executed block's `parent_qc` slot, which is part of the block
//! header's signed preimage (ADR 0008, decision 5). If two validators
//! record different certificates for one proposal, they seal different
//! blocks at the same height, and every block hash from there on differs.

use super::*;

/// Two honest validators, one proposal, two different recorded
/// certificates.
///
/// P1 is proposed in round 1. P2 (round 2) extends it carrying P1's
/// certificate as P2's leader formed it, from validators {0, 1, 2}. Round 2
/// then ends both ways at once: validator A collects a quorum of votes for
/// P2, while the round-3 leader, not having seen them, times out with a
/// quorum and proposes P3 extending P1 with P1's certificate as *it* formed
/// it, from validators {1, 2, 3}, plus the round-2 timeout certificate.
/// Rounds 4 and 5 follow P3 normally.
///
/// A commits P1 the moment P2 is certified, recording P2's copy of the
/// certificate. B never sees P2 certified and commits P1 later, as an
/// ancestor of P3, recording P3's copy. Both are valid quorums for P1; they
/// are different bytes. Nothing here is Byzantine: it takes vote arrival
/// order and one slow round.
#[test]
fn every_validator_records_the_same_certificate_for_a_commit() {
    let kit = Kit::new(4);
    let g = kit.genesis_qc();
    let p1 = kit.proposal(1, 0, g.clone(), None, b"p1");
    let id1 = p1.header.id();
    let qc1_x = kit.qc_by(1, id1, &[0, 1, 2]);
    let qc1_y = kit.qc_by(1, id1, &[1, 2, 3]);
    let p2 = kit.proposal(2, 1, qc1_x.clone(), None, b"p2");
    let tc2 = kit.tc(2, &[qc1_y.clone()]);
    let p3 = kit.proposal(3, 1, qc1_y.clone(), Some(tc2), b"p3");
    let p4 = kit.proposal(4, 2, kit.qc_for(&p3), None, b"p4");
    let p5 = kit.proposal(5, 3, kit.qc_for(&p4), None, b"p5");

    // A: sees P2 certified by votes, then the rest of the chain.
    let mut a = kit.engine(0);
    let mut a_commits = commits(&feed(&mut a, &[p1.clone(), p2.clone()], 0));
    for i in [1, 2] {
        a_commits.extend(commits(&a.on_message(
            Message::Vote(kit.vote(i, 2, p2.header.id())),
            None,
            0,
        )));
    }
    a_commits.extend(commits(&feed(
        &mut a,
        &[p3.clone(), p4.clone(), p5.clone()],
        0,
    )));

    // B: never sees P2's certificate.
    let mut b = kit.engine(3);
    let b_commits = commits(&feed(&mut b, &[p1, p2, p3, p4, p5], 0));

    let ids = |c: &[Committed]| c.iter().map(|c| c.header.id()).collect::<Vec<_>>();
    assert_eq!(ids(&a_commits), ids(&b_commits), "ordering diverged");
    assert_eq!(a_commits[0].header.id(), id1);
    for c in a_commits.iter().chain(&b_commits) {
        c.finality().verify(&kit.set, kit.genesis).unwrap();
    }
    for (x, y) in a_commits.iter().zip(&b_commits) {
        assert_eq!(
            x.finality().encode(),
            y.finality().encode(),
            "round {}: A recorded the certificate signed by {:?}, B the one signed by {:?}",
            x.header.round,
            x.cert.votes.iter().map(|v| v.voter).collect::<Vec<_>>(),
            y.cert.votes.iter().map(|v| v.voter).collect::<Vec<_>>(),
        );
    }
}
