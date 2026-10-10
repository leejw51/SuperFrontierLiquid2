//! The safety rules at the engine's boundary, one hand-built message at a
//! time: when an engine may vote, what it commits and when, and what a
//! restart must not forget.
//!
//! These are the rules the quorum-intersection argument assumes every
//! honest validator follows. A slip in any one of them is a fork with no
//! Byzantine validator involved at all.

use super::*;

/// One vote per round, however the proposal arrives: the same proposal
/// twice is a repeat and gets no second vote; the leader's second,
/// different proposal for the round is equivocation and gets none either.
#[test]
fn an_engine_votes_once_per_round() {
    let kit = Kit::new(4);
    let mut e = kit.engine(0);
    let p1 = kit.proposal(1, 0, kit.genesis_qc(), None, b"a");
    let mut p1b = p1.clone();
    p1b.payload = b"b".to_vec();
    p1b.header.payload_hash = keccak256(b"b");
    kit.resign(&mut p1b, kit.leader(1));

    let first = e.on_message(msg(&p1), None, 0);
    assert_eq!(votes(&first).len(), 1);
    assert_eq!(votes(&first)[0].id, p1.header.id());

    let again = e.on_message(msg(&p1), None, 0);
    assert!(votes(&again).is_empty());
    assert_eq!(last_drop(&e), Some(Dropped::Duplicate));

    let other = e.on_message(msg(&p1b), None, 0);
    assert!(
        votes(&other).is_empty(),
        "voted for both of a leader's proposals"
    );
    assert_eq!(last_drop(&e), Some(Dropped::Equivocation));
    assert_eq!(e.last_voted_round(), 1);
}

/// Never a vote at or below the voting floor. An engine told it voted up to
/// round 5 is fed a perfectly justified chain from round 1: it follows the
/// certificates, but its first vote is in round 6.
#[test]
fn an_engine_never_votes_at_or_below_its_last_voted_round() {
    let kit = Kit::new(4);
    let chain = kit.chain(8);
    let mut e = kit.engine(3);
    e.restore_safety(5, kit.genesis_qc());
    let acts = feed(&mut e, &chain, 0);
    let rounds: Vec<Round> = votes(&acts).iter().map(|v| v.round).collect();
    assert_eq!(rounds, vec![6, 7, 8]);
    // It still followed the chain and committed what the chain commits.
    assert_eq!(e.round(), 8);
    assert_eq!(e.committed_round(), 6);
}

/// A round this engine has timed out in gets no vote, even when the
/// proposal turns up afterwards: having told the others it gave up, voting
/// would let a timeout certificate and a quorum certificate for the same
/// round both count this engine.
#[test]
fn a_round_the_engine_timed_out_gets_no_vote() {
    let kit = Kit::new(4);
    let mut e = kit.engine(0);
    let acts = e.on_tick(e.next_deadline_ms());
    let t = timeouts(&acts);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].round, 1);
    assert_eq!(e.last_voted_round(), 1);

    let p1 = kit.proposal(1, 0, kit.genesis_qc(), None, b"late");
    let acts = e.on_message(msg(&p1), None, 0);
    assert!(votes(&acts).is_empty(), "voted in a round it had timed out");
    assert!(e.holds(&p1.header.id()), "the proposal is still recorded");
}

/// The voting rule, case by case, with genuine signatures throughout. The
/// engine has voted for P1 (round 1) and holds its certificate; a leader
/// then proposes in round 3.
///
/// * certificate for round 1, nothing for round 2: refused, since nothing
///   shows round 2 ended;
/// * with a timeout certificate for the wrong round: refused;
/// * with a timeout certificate for round 2 whose highest certificate is
///   round 1: accepted — the leader extends the highest thing anyone in
///   the quorum was locked on;
/// * extending genesis, with an authentic timeout certificate for round 2
///   in which one validator reports the round-1 certificate: refused.
///   This is the lock: a quorum's worth of timeouts proves no one in it
///   saw anything higher than what the proposal extends, and one
///   validator did. Forgery is not needed to break it; only the rule.
/// * extending genesis with a timeout certificate that reports nothing
///   higher: accepted — no one in that quorum was locked on round 1.
#[test]
fn the_voting_rule_needs_the_previous_certificate_or_a_timeout_certificate() {
    let kit = Kit::new(4);
    let p1 = kit.proposal(1, 0, kit.genesis_qc(), None, b"p1");
    let qc1 = kit.qc_for(&p1);
    let g = kit.genesis_qc();
    let setup = || {
        let mut e = kit.engine(3);
        assert_eq!(votes(&e.on_message(msg(&p1), None, 0)).len(), 1);
        e
    };

    struct Case {
        what: &'static str,
        qc: QuorumCert,
        parent_height: u64,
        tc: Option<TimeoutCert>,
        vote: bool,
    }
    let cases = [
        Case {
            what: "no timeout certificate for the skipped round",
            qc: qc1.clone(),
            parent_height: 1,
            tc: None,
            vote: false,
        },
        Case {
            what: "timeout certificate for the wrong round",
            qc: qc1.clone(),
            parent_height: 1,
            tc: Some(kit.tc(1, &[g.clone()])),
            vote: false,
        },
        Case {
            what: "extends the timeout certificate's highest certificate",
            qc: qc1.clone(),
            parent_height: 1,
            tc: Some(kit.tc(2, &[qc1.clone(), g.clone()])),
            vote: true,
        },
        Case {
            what: "extends below an authentic timeout certificate's highest",
            qc: g.clone(),
            parent_height: 0,
            tc: Some(kit.tc(2, &[g.clone(), qc1.clone(), g.clone()])),
            vote: false,
        },
        Case {
            what: "extends genesis when no one reported anything higher",
            qc: g.clone(),
            parent_height: 0,
            tc: Some(kit.tc(2, &[g.clone()])),
            vote: true,
        },
    ];
    for c in cases {
        let mut e = setup();
        let p3 = kit.proposal(3, c.parent_height, c.qc, c.tc, b"p3");
        let acts = e.on_message(msg(&p3), None, 0);
        let voted = votes(&acts).iter().any(|v| v.round == 3);
        assert_eq!(voted, c.vote, "{}", c.what);
        if !c.vote {
            assert_eq!(last_drop(&e), Some(Dropped::Unjustified), "{}", c.what);
        }
    }
}

/// A proposal must extend exactly the proposal its certificate names, and
/// its certificate must be from an earlier round. Either slip would let a
/// leader attach a real certificate to a block that forks away from it.
#[test]
fn a_proposal_must_extend_the_certificate_it_carries() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let mut e = kit.engine(0);
    let _ = feed(&mut e, &chain, 0);
    let before = snapshot(&e);

    // Carries the round-2 certificate but names round 1 as its parent.
    let mut p = kit.proposal(3, 2, kit.qc_for(&chain[1]), None, b"x");
    p.header.parent_id = chain[0].header.id();
    kit.resign(&mut p, kit.leader(3));
    let acts = e.on_message(msg(&p), None, 0);
    assert!(votes(&acts).is_empty());
    assert_eq!(last_drop(&e), Some(Dropped::Unjustified));

    // A certificate from its own round.
    let qc2 = kit.qc_for(&chain[1]);
    let p = kit.proposal(2, 2, qc2, None, b"y");
    let acts = e.on_message(msg(&p), None, 0);
    assert!(votes(&acts).is_empty());
    assert_eq!(last_drop(&e), Some(Dropped::Unjustified));
    assert_eq!(snapshot(&e), before);
}

/// The lock only rises. Once an engine holds the round-5 certificate, a
/// proposal or a timeout carrying an older certificate does not lower it,
/// and a restart record naming an older round does not lower the voting
/// floor.
#[test]
fn the_high_certificate_and_voting_floor_never_go_back() {
    let kit = Kit::new(4);
    let chain = kit.chain(6);
    let mut e = kit.engine(1);
    let _ = feed(&mut e, &chain, 0);
    assert_eq!((e.high_qc().round, e.last_voted_round()), (5, 6));

    let old = kit.timeout(2, 6, kit.qc_for(&chain[0]));
    let _ = e.on_message(Message::Timeout(old), None, 0);
    assert_eq!(e.high_qc().round, 5);

    e.restore_safety(2, kit.qc_for(&chain[1]));
    assert_eq!((e.high_qc().round, e.last_voted_round()), (5, 6));
}

/// Two certificates in consecutive rounds commit the first one's proposal.
/// The certificate for the parent alone commits nothing. The commit reaches
/// the driver once the next commit names its successor, carrying the
/// certificate from that successor's header: the same bytes every validator
/// records.
#[test]
fn consecutive_certificates_commit_the_parent() {
    let kit = Kit::new(4);
    let chain = kit.chain(4);
    let mut e = kit.engine(0);
    let acts = feed(&mut e, &chain[..2], 0);
    assert!(commits(&acts).is_empty(), "committed on one certificate");
    assert_eq!(e.committed_round(), 0);
    let acts = feed(&mut e, &chain[2..3], 0);
    assert_eq!(e.committed_round(), 1);
    assert!(
        commits(&acts).is_empty(),
        "sealed before its successor was committed"
    );
    // Handed over for execution at once, without its certificate.
    let ordered: Vec<H256> = acts
        .iter()
        .filter_map(|a| match a {
            Action::Ordered(o) => Some(o.header.id()),
            _ => None,
        })
        .collect();
    assert_eq!(ordered, vec![chain[0].header.id()]);
    let acts = feed(&mut e, &chain[3..], 0);
    assert_eq!(e.committed_round(), 2);
    let c = commits(&acts);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].header.id(), chain[0].header.id());
    assert_eq!(c[0].cert, chain[1].header.qc);
    c[0].finality().verify(&kit.set, kit.genesis).unwrap();
}

/// The certificate that commits can also be formed here from votes, before
/// any proposal carries it: the commit comes one message hop sooner, and it
/// is the same commit.
#[test]
fn a_certificate_formed_from_votes_commits_too() {
    let kit = Kit::new(4);
    let chain = kit.chain(4);
    let mut e = kit.engine(3);
    let _ = feed(&mut e, &chain[..2], 0);
    let id2 = chain[1].header.id();
    for i in 0..2 {
        let _ = e.on_message(Message::Vote(kit.vote(i, 2, id2)), None, 0);
    }
    // Its own vote was the third.
    assert_eq!(e.committed_round(), 1);
    assert_eq!(e.committed_id(), chain[0].header.id());
    // Handed over with the certificate its successor carries, not the one
    // formed here from votes.
    let c = commits(&feed(&mut e, &chain[2..], 0));
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].header.id(), chain[0].header.id());
    assert_eq!(c[0].cert, chain[1].header.qc);
}

/// A certificate whose parent was certified two rounds earlier (a timeout in
/// between) commits nothing. The next consecutive pair then commits both
/// proposals at once, oldest first. Committing on a non-consecutive pair is
/// the classic chained-HotStuff bug: the round in between could hold a
/// certificate for a sibling that this node never saw.
#[test]
fn a_gap_between_certificates_defers_the_commit() {
    let kit = Kit::new(4);
    let g = kit.genesis_qc();
    let p1 = kit.proposal(1, 0, g.clone(), None, b"p1");
    let qc1 = kit.qc_for(&p1);
    let p3 = kit.proposal(3, 1, qc1.clone(), Some(kit.tc(2, &[qc1.clone(), g])), b"p3");
    let qc3 = kit.qc_for(&p3);
    let p4 = kit.proposal(4, 2, qc3.clone(), None, b"p4");
    let p5 = kit.proposal(5, 3, kit.qc_for(&p4), None, b"p5");
    let p6 = kit.proposal(6, 4, kit.qc_for(&p5), None, b"p6");

    let mut e = kit.engine(0);
    let acts = feed(&mut e, &[p1.clone(), p3.clone(), p4.clone()], 0);
    assert!(commits(&acts).is_empty(), "committed across a round gap");
    assert_eq!(e.committed_round(), 0);
    // Both commit at once; the newer one waits for its successor.
    let c = commits(&feed(&mut e, &[p5], 0));
    assert_eq!(e.committed_round(), 3);
    let ids: Vec<H256> = c.iter().map(|c| c.header.id()).collect();
    assert_eq!(ids, vec![p1.header.id()]);
    assert_eq!(c[0].cert, qc1);
    let c = commits(&feed(&mut e, &[p6], 0));
    assert_eq!(e.committed_round(), 4);
    let ids: Vec<H256> = c.iter().map(|c| c.header.id()).collect();
    assert_eq!(ids, vec![p3.header.id()]);
    assert_eq!(c[0].cert, qc3);
}

/// What a restart must remember, at every position a validator can be in
/// when it goes down: for each of four validators and each round 1..=5 of
/// a chain, the validator follows the chain to that round (voting, or
/// leading when the round is its own), goes down, and comes back at
/// genesis with its safety record. The round's leader then equivocates:
/// the restarted validator must vote for nothing at or below the round,
/// and if it led the round it must not propose again. An amnesiac copy
/// without the record is run alongside to prove the scenario would catch
/// a double vote.
#[test]
fn a_restored_safety_record_prevents_a_double_vote_at_every_position() {
    let kit = Kit::new(4);
    let chain = kit.chain(5);
    for me in 0..4 {
        for k in 1..=5u64 {
            let mut e = kit.engine(me);
            let acts = feed(&mut e, &chain[..k as usize], 0);
            assert!(
                votes(&acts).iter().any(|v| v.round == k),
                "me={me} k={k}: setup never voted in round {k}"
            );
            let record = (e.last_voted_round(), e.high_qc().clone());
            assert_eq!(record.0, k);

            // The leader of round k equivocates.
            let mut alt = chain[k as usize - 1].clone();
            alt.payload = format!("alt r{k}").into_bytes();
            alt.header.payload_hash = keccak256(&alt.payload);
            kit.resign(&mut alt, kit.leader(k));
            let mut replay = chain[..k as usize - 1].to_vec();
            replay.push(alt);

            let mut restored = kit.engine(me);
            restored.restore_safety(record.0, record.1.clone());
            let mut acts = feed(&mut restored, &replay, 0);
            if kit.leader(restored.round()) == me {
                acts.extend(restored.propose(b"again".to_vec(), 1, 0, 0, H256::ZERO, 0));
            }
            assert!(
                votes(&acts).iter().all(|v| v.round > k),
                "me={me} k={k}: voted again at or below round {k}"
            );
            assert!(
                proposals(&acts).iter().all(|p| p.header.round > k),
                "me={me} k={k}: proposed again at or below round {k}"
            );

            let mut amnesiac = kit.engine(me);
            let acts = feed(&mut amnesiac, &replay, 0);
            assert!(
                votes(&acts).iter().any(|v| v.round == k),
                "me={me} k={k}: the amnesiac control did not double-vote, so the test proves nothing"
            );
        }
    }
}

/// The restarted leader: validator 1 leads round 1, proposes and votes,
/// goes down and comes back at genesis with its record. It is prompted to
/// lead round 1 again and must not propose; it then accepts its own
/// original proposal from a peer — not refusing it as a conflict with a
/// proposal it never made — follows the chain, and when its next turn
/// comes (round 5) it proposes normally. Before the fix a restarted leader
/// proposed a second time and then wedged on the certified original.
#[test]
fn a_restarted_leader_neither_reproposes_nor_wedges() {
    let kit = Kit::new(4);
    let me = kit.leader(1);
    let mut e = kit.engine(me);
    let acts = e.propose(b"original".to_vec(), 1, 1, 0, H256::ZERO, 0);
    let original = proposals(&acts).pop().expect("leader proposes");
    assert_eq!(votes(&acts).len(), 1);
    let record = (e.last_voted_round(), e.high_qc().clone());

    let mut r = Engine::new(
        Kit::config(),
        kit.set.clone(),
        kit.addr(me),
        kit.keys[me],
        kit.genesis,
        Root::genesis(kit.genesis),
        keccak256,
        0,
    );
    r.restore_safety(record.0, record.1);
    let init = r.take_initial_actions();
    assert!(init
        .iter()
        .any(|a| matches!(a, Action::ReadyToPropose { round: 1, .. })));
    let acts = r.propose(b"second thoughts".to_vec(), 1, 1, 0, H256::ZERO, 0);
    assert!(
        acts.is_empty(),
        "a restarted leader proposed twice in round 1"
    );

    // The rest of the network certified the original.
    let qc1 = kit.qc_for(&original);
    let p2 = kit.proposal(2, 1, qc1, None, b"p2");
    let p3 = kit.proposal(3, 2, kit.qc_for(&p2), None, b"p3");
    let p4 = kit.proposal(4, 3, kit.qc_for(&p3), None, b"p4");
    let acts = feed(&mut r, &[original.clone(), p2, p3, p4.clone()], 0);
    assert!(votes(&acts).iter().all(|v| v.round > 1));
    assert!(r.holds(&original.header.id()));
    assert_eq!(r.committed_round(), 2);
    // Round 4's certificate forms from the others' votes and its own.
    let id4 = p4.header.id();
    let mut acts = Vec::new();
    for i in [0, 2] {
        acts.extend(r.on_message(Message::Vote(kit.vote(i, 4, id4)), None, 0));
    }
    assert_eq!(r.round(), 5);
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::ReadyToPropose { round: 5, .. })));
    let acts = r.propose(b"round five".to_vec(), 1, 5, 0, H256::ZERO, 0);
    let p5 = proposals(&acts);
    assert_eq!(p5.len(), 1, "the restarted leader wedged");
    assert_eq!(p5[0].header.parent_id, id4);
}

/// Restarted after timing out a round it never voted in: validator 2
/// votes in rounds 1 and 2, sees round 2 certified by votes before the
/// round-3 proposal arrives, and times round 3 out. Restarted at genesis
/// with its record, the late round-3 proposal gets no vote, and the
/// timeout it repeats carries the round-2 certificate it held, not the
/// genesis certificate at its executed head. An amnesiac control votes.
#[test]
fn a_restored_timeout_is_not_followed_by_a_vote_in_that_round() {
    let kit = Kit::new(4);
    let chain = kit.chain(3);
    let mut e = kit.engine(2);
    let _ = feed(&mut e, &chain[..2], 0);
    let id2 = chain[1].header.id();
    for i in [0, 1] {
        let _ = e.on_message(Message::Vote(kit.vote(i, 2, id2)), None, 0);
    }
    assert_eq!(
        (e.round(), e.high_qc().round, e.last_voted_round()),
        (3, 2, 2)
    );
    let t = timeouts(&e.on_tick(e.next_deadline_ms()));
    assert_eq!(t.len(), 1);
    assert_eq!((t[0].round, t[0].high_qc.round), (3, 2));
    let record = (e.last_voted_round(), e.high_qc().clone());
    assert_eq!(record.0, 3);

    let mut r = kit.engine(2);
    r.restore_safety(record.0, record.1.clone());
    let acts = feed(&mut r, &chain, 0);
    assert!(
        votes(&acts).iter().all(|v| v.round > 3),
        "voted in a round it had timed out before the restart"
    );
    let acts = r.on_tick(r.next_deadline_ms());
    let t = timeouts(&acts);
    assert!(!t.is_empty());
    assert!(t.iter().all(|t| t.high_qc.round >= 2));

    let mut amnesiac = kit.engine(2);
    let acts = feed(&mut amnesiac, &chain, 0);
    assert!(
        votes(&acts).iter().any(|v| v.round == 3),
        "the amnesiac control did not vote, so the test proves nothing"
    );
}
