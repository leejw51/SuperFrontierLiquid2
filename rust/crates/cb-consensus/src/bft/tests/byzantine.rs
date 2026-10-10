//! Byzantine behaviour, one adversary at a time: first against a single
//! engine with every message built by hand, so the test can say exactly
//! why each one is refused; then live, inside a simulated network, where
//! the refusals have to hold up alongside everything else going on.
//!
//! The attacker is always a member of the set, holding its own key: that
//! is the strongest position the protocol has to survive. For each attack
//! the test checks the engine refused it for the right reason, did not
//! vote, did not move its round, certificate, voting floor or commit point,
//! and did not panic.

use super::super::sim::{Sim, SimConfig};
use super::*;

fn outsider(k: u8) -> (SecretKey, Address) {
    let (sk, pk) = keypair_from_seed(&[b'o', b'u', b't', k]);
    (sk, address_from_pubkey(&pk))
}

/// Every way to fake a certificate for `(round, id)` without a quorum of
/// the set actually signing it.
fn forged_qcs(kit: &Kit, round: Round, id: H256) -> Vec<(&'static str, QuorumCert)> {
    let q = kit.set.quorum();
    let honest: Vec<usize> = (0..q).collect();
    let mut out = Vec::new();
    out.push((
        "no signatures",
        QuorumCert {
            round,
            id,
            votes: Vec::new(),
        },
    ));
    out.push((
        "one short of a quorum",
        kit.qc_by(round, id, &honest[..q - 1]),
    ));
    let mut dup = kit.qc_by(round, id, &honest[..q - 1]);
    dup.votes.push(dup.votes[0].clone());
    out.push(("a signer repeated to make up the count", dup));
    out.push((
        "signed by non-validators",
        QuorumCert {
            round,
            id,
            votes: (0..q as u8)
                .map(|k| {
                    let (sk, a) = outsider(k);
                    Vote::sign(&sk, a, round, id)
                })
                .collect(),
        },
    ));
    let mut stale_round = kit.qc_by(round - 1, id, &honest);
    stale_round.round = round;
    out.push((
        "votes from an earlier round, relabelled at the top",
        stale_round,
    ));
    let mut relabelled = kit.qc_by(round - 1, id, &honest);
    relabelled.round = round;
    for v in &mut relabelled.votes {
        v.round = round;
    }
    out.push(("votes from an earlier round, relabelled inside", relabelled));
    let mut other = kit.qc_by(round, H256([0xEE; 32]), &honest);
    other.id = id;
    out.push(("votes for another proposal", other));
    let mut garbage = kit.qc_by(round, id, &honest);
    for (k, v) in garbage.votes.iter_mut().enumerate() {
        v.sig = vec![k as u8 + 1; 65];
    }
    out.push(("garbage signatures", garbage));
    let mut short = kit.qc_by(round, id, &honest);
    for v in &mut short.votes {
        v.sig.truncate(10);
    }
    out.push(("truncated signatures", short));
    let mut wrong_voter = kit.qc_by(round, id, &honest);
    wrong_voter.votes[0].voter = kit.addr(kit.n() - 1);
    out.push(("a vote relabelled to another validator", wrong_voter));
    out.push((
        "the genesis round naming another proposal",
        QuorumCert {
            round: 0,
            id,
            votes: Vec::new(),
        },
    ));
    let mut genesis_with_votes = kit.genesis_qc();
    genesis_with_votes.votes.push(kit.vote(0, 0, kit.genesis));
    out.push((
        "the genesis certificate padded with a vote",
        genesis_with_votes,
    ));
    out
}

/// A forged certificate is refused wherever it turns up: justifying the
/// round leader's proposal, or carried in a member's timeout. The engine
/// sits at round 2 having voted; each forgery claims round 2 (or genesis)
/// for proposal P2, which is real but not yet certified. Accepting any of
/// them would move the engine to round 3 on nobody's word, and a proposal
/// built on one would extend whatever the forger liked.
#[test]
fn forged_certificates_are_refused_in_proposals_and_timeouts() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let target = chain[1].header.id();
    let forgeries = forged_qcs(&kit, 2, target);
    assert!(forgeries.len() >= 12);
    for (what, qc) in forgeries {
        assert!(
            qc.verify(&kit.set, kit.genesis).is_err(),
            "{what}: verifies"
        );
        let mut e = kit.engine(0);
        let _ = feed(&mut e, &chain, 0);
        let before = snapshot(&e);
        assert_eq!(before.0, 2);

        let round = qc.round + 1;
        let p = kit.proposal(round, 2, qc.clone(), None, b"built on a forgery");
        let acts = e.on_message(msg(&p), None, 0);
        assert!(votes(&acts).is_empty(), "{what}: voted");
        assert!(
            matches!(last_drop(&e), Some(Dropped::BadCertificate(_))),
            "{what}: dropped as {:?}",
            last_drop(&e)
        );
        assert_eq!(snapshot(&e), before, "{what}: state moved");
        assert!(!e.holds(&p.header.id()), "{what}: stored");

        let t = kit.timeout(1, e.round(), qc);
        let acts = e.on_message(Message::Timeout(t), None, 0);
        assert!(acts.is_empty(), "{what}: acted on a timeout");
        assert!(
            matches!(last_drop(&e), Some(Dropped::BadCertificate(_))),
            "{what}: timeout dropped as {:?}",
            last_drop(&e)
        );
        assert_eq!(snapshot(&e), before, "{what}: state moved");
    }
}

/// Forged timeout certificates, used the way an attacker would: to justify
/// a proposal in round 4 that extends genesis and so abandons P1, which the
/// engine voted for and holds the certificate of. Every honest validator's
/// timeout for round 3 carries that certificate, so a genuine timeout
/// certificate would refuse the fork; the forger tries to fake one that
/// reports nothing higher than genesis.
///
/// The decisive case is the rewritten one: real timeouts with their
/// embedded certificate swapped for genesis. The timeout signature binds
/// the embedded certificate's round, so the swap breaks every signature.
/// The controls at the end show the same shape with the genuine
/// certificate is refused by the voting rule, and extending P1 instead is
/// accepted: the forgery was the only thing at fault.
#[test]
fn forged_timeout_certificates_cannot_unlock_a_validator() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc1 = kit.qc_for(&chain[0]);
    let g = kit.genesis_qc();
    let q = kit.set.quorum();
    let genuine = kit.tc(3, &[qc1.clone()]);
    genuine.verify(&kit.set, kit.genesis).unwrap();

    let mut forgeries: Vec<(&str, TimeoutCert)> = Vec::new();
    forgeries.push((
        "no timeouts",
        TimeoutCert {
            round: 3,
            timeouts: Vec::new(),
        },
    ));
    let mut short = kit.tc(3, &[g.clone()]);
    short.timeouts.pop();
    forgeries.push(("one short of a quorum", short.clone()));
    let mut dup = short.clone();
    dup.timeouts.push(dup.timeouts[0].clone());
    forgeries.push(("a timeout repeated", dup));
    forgeries.push((
        "timeouts from non-validators",
        TimeoutCert {
            round: 3,
            timeouts: (0..q as u8)
                .map(|k| {
                    let (sk, a) = outsider(k);
                    Timeout::sign(&sk, a, 3, g.clone())
                })
                .collect(),
        },
    ));
    let mut mixed = kit.tc(3, &[g.clone()]);
    mixed.timeouts[0] = kit.timeout(0, 2, g.clone());
    forgeries.push(("one timeout for another round", mixed));
    let mut swapped = genuine.clone();
    for t in &mut swapped.timeouts {
        t.high_qc = g.clone();
    }
    forgeries.push((
        "genuine timeouts with a lower certificate swapped in",
        swapped,
    ));
    let mut relabelled = kit.tc(2, &[g.clone()]);
    relabelled.round = 3;
    for t in &mut relabelled.timeouts {
        t.round = 3;
    }
    forgeries.push(("timeouts for round 2 relabelled as round 3", relabelled));
    let fake_high = QuorumCert {
        round: 2,
        id: chain[1].header.id(),
        votes: vec![kit.vote(0, 2, chain[1].header.id())],
    };
    let mut embedded = kit.tc(3, &[g.clone()]);
    embedded.timeouts[0] = kit.timeout(0, 3, fake_high);
    forgeries.push(("a timeout carrying a forged certificate", embedded));

    for (what, tc) in forgeries {
        assert!(
            tc.verify(&kit.set, kit.genesis).is_err(),
            "{what}: verifies"
        );
        let mut e = kit.engine(3);
        let _ = feed(&mut e, &chain, 0);
        let before = snapshot(&e);
        let p4 = kit.proposal(4, 0, g.clone(), Some(tc), b"fork from genesis");
        let acts = e.on_message(msg(&p4), None, 0);
        assert!(votes(&acts).is_empty(), "{what}: voted for the fork");
        assert!(
            matches!(last_drop(&e), Some(Dropped::BadCertificate(_))),
            "{what}: dropped as {:?}",
            last_drop(&e)
        );
        assert_eq!(snapshot(&e), before, "{what}: state moved");
    }

    // Controls.
    let mut e = kit.engine(3);
    let _ = feed(&mut e, &chain, 0);
    let fork = kit.proposal(4, 0, g.clone(), Some(genuine.clone()), b"fork from genesis");
    assert!(votes(&e.on_message(msg(&fork), None, 0)).is_empty());
    assert_eq!(last_drop(&e), Some(Dropped::Unjustified));
    let mut e = kit.engine(3);
    let _ = feed(&mut e, &chain, 0);
    let good = kit.proposal(4, 1, qc1, Some(genuine), b"extends P1");
    assert_eq!(votes(&e.on_message(msg(&good), None, 0)).len(), 1);
}

/// A leader that sends two proposals for its round gets at most one vote
/// from any honest engine, and a double-voting member cannot make both
/// halves certify: the first vote an engine sees from a validator in a
/// round stands, the second is filed as equivocation and counted nowhere.
///
/// Here the double voter's vote for the second proposal arrives first; the
/// honest validators split two and two. Nothing certifies, which is the
/// correct outcome: the round times out, rather than two certificates
/// forming for one round.
#[test]
fn an_equivocating_leader_and_a_double_voter_cannot_certify_twice() {
    let kit = Kit::new(4);
    let byz = kit.leader(1); // validator 1 leads round 1 and double-votes
    let a = kit.proposal(1, 0, kit.genesis_qc(), None, b"A");
    let b = kit.proposal(1, 0, kit.genesis_qc(), None, b"B");
    let (ia, ib) = (a.header.id(), b.header.id());

    let mut e = kit.engine(3);
    let acts = e.on_message(msg(&a), None, 0);
    assert_eq!(votes(&acts)[0].id, ia);
    let acts = e.on_message(msg(&b), None, 0);
    assert!(votes(&acts).is_empty());
    assert_eq!(last_drop(&e), Some(Dropped::Equivocation));

    for v in [
        kit.vote(byz, 1, ib),
        kit.vote(2, 1, ib),
        kit.vote(byz, 1, ia),
        kit.vote(0, 1, ia),
    ] {
        let _ = e.on_message(Message::Vote(v), None, 0);
    }
    assert!(e
        .dropped
        .iter()
        .any(|(r, d)| *r == 1 && *d == Dropped::Equivocation));
    assert_eq!(
        e.high_qc().round,
        0,
        "a double vote completed a certificate"
    );
    assert_eq!(e.round(), 1);
}

/// Replays of the whole committed history — every proposal, every vote in
/// every certificate, every timeout in every timeout certificate — into
/// every live engine, a while after the fact. None of it moves anything or
/// draws a vote; the network carries on and still agrees.
#[test]
fn replayed_history_is_ignored() {
    for seed in 1..=3 {
        let mut sim = Sim::new(SimConfig {
            seed,
            ..Default::default()
        });
        sim.run_for(800);
        let mut old: Vec<Message> = Vec::new();
        for c in &sim.committed[0] {
            old.push(Message::Proposal(Proposal {
                header: c.header.clone(),
                payload: c.payload.clone(),
            }));
            old.extend(c.cert.votes.iter().cloned().map(Message::Vote));
            if let Some(tc) = &c.header.tc {
                old.extend(tc.timeouts.iter().cloned().map(Message::Timeout));
            }
        }
        assert!(old.len() > 100);
        sim.run_for(500);
        let now = sim.now_ms;
        for i in 0..4 {
            let e = sim.engine_mut(i).unwrap();
            let before = snapshot(e);
            e.dropped.clear();
            let mut acts = Vec::new();
            for m in &old {
                acts.extend(e.on_message(m.clone(), None, now));
            }
            assert!(
                acts.iter().all(|a| !matches!(
                    a,
                    Action::Broadcast(Message::Vote(_))
                        | Action::Broadcast(Message::Proposal(_))
                        | Action::Commit(_)
                )),
                "seed {seed} validator {i}: a replay drew a vote, proposal or commit"
            );
            assert_eq!(snapshot(e), before, "seed {seed} validator {i}");
            assert!(e
                .dropped
                .iter()
                .all(|(_, d)| matches!(d, Dropped::Stale | Dropped::Duplicate)));
        }
        let before = sim.min_committed_among(&[0, 1, 2, 3]);
        sim.run_for(800);
        sim.assert_agreement();
        assert!(sim.min_committed_among(&[0, 1, 2, 3]) > before + 20);
    }
}

/// Messages claiming rounds far ahead. Votes and timeouts beyond the
/// retention window are refused as far-future, including at `u64::MAX`,
/// with no arithmetic overflow; a vote right at the window's edge is still
/// buffered. Proposals claiming a far round are refused because nothing
/// justifies them: a certificate for the round before would need a quorum.
#[test]
fn far_future_claims_are_refused_without_overflow() {
    let kit = Kit::new(4);
    let mut e = kit.engine(0);
    let before = snapshot(&e);
    let retain = Kit::config().retain_rounds;
    let edge = e.round() + retain;

    let _ = e.on_message(Message::Vote(kit.vote(1, edge, H256([1; 32]))), None, 0);
    assert!(e.dropped.is_empty(), "a vote inside the window was dropped");
    for r in [edge + 1, 1 << 40, u64::MAX - 1, u64::MAX] {
        let _ = e.on_message(Message::Vote(kit.vote(1, r, H256([1; 32]))), None, 0);
        assert_eq!(last_drop(&e), Some(Dropped::FarFuture), "vote at {r}");
        let _ = e.on_message(
            Message::Timeout(kit.timeout(2, r, kit.genesis_qc())),
            None,
            0,
        );
        assert_eq!(last_drop(&e), Some(Dropped::FarFuture), "timeout at {r}");
    }
    for r in [1 << 40, u64::MAX - 1, u64::MAX] {
        // Justified by nothing.
        let p = kit.proposal(r, 0, kit.genesis_qc(), None, b"far");
        let _ = e.on_message(msg(&p), None, 0);
        assert_eq!(last_drop(&e), Some(Dropped::Unjustified), "proposal at {r}");
        // Justified by the attacker's own timeout alone.
        let tc = TimeoutCert {
            round: r - 1,
            timeouts: vec![kit.timeout(1, r - 1, kit.genesis_qc())],
        };
        let p = kit.proposal(r, 0, kit.genesis_qc(), Some(tc), b"far");
        let _ = e.on_message(msg(&p), None, 0);
        assert!(
            matches!(last_drop(&e), Some(Dropped::BadCertificate(_))),
            "proposal at {r} with a one-signature timeout certificate"
        );
        // A timeout certificate at u64::MAX would make the next round
        // overflow; it justifies nothing.
        let tc = TimeoutCert {
            round: u64::MAX,
            timeouts: vec![kit.timeout(1, u64::MAX, kit.genesis_qc())],
        };
        let p = kit.proposal(r, 0, kit.genesis_qc(), Some(tc), b"far");
        let _ = e.on_message(msg(&p), None, 0);
        assert!(matches!(
            last_drop(&e),
            Some(Dropped::Unjustified) | Some(Dropped::BadCertificate(_))
        ));
    }
    assert_eq!(snapshot(&e), before);
}

/// `f` members claiming to have timed out a round far ahead do not drag an
/// honest engine there; `f + 1` do, because then at least one of them is
/// honest and the engine should join it. Checked with seven validators,
/// where `f = 2`.
#[test]
fn f_timeouts_cannot_advance_the_round_but_f_plus_one_can() {
    let kit = Kit::new(7);
    let f = kit.set.faults_tolerated();
    let mut e = kit.engine(0);
    let target = 50;
    for i in 0..f {
        let _ = e.on_message(
            Message::Timeout(kit.timeout(6 - i, target, kit.genesis_qc())),
            None,
            0,
        );
    }
    assert_eq!(e.round(), 1, "{f} timeouts moved the round");
    assert_eq!(e.last_voted_round(), 0);
    let acts = e.on_message(
        Message::Timeout(kit.timeout(6 - f, target, kit.genesis_qc())),
        None,
        0,
    );
    assert_eq!(e.round(), target);
    assert_eq!(timeouts(&acts).len(), 1, "joined without timing out itself");
}

/// Proposals a member could send that break some rule other than the
/// certificate's: from a validator that does not lead the round, signed by
/// an outsider, relabelled to the leader, with a payload that does not
/// match its hash, a height that skips, a timestamp that goes back, or
/// forking below the lock with no timeout certificate. None draws a vote,
/// none moves the engine backwards, and the one that is a well-formed
/// proposal from the wrong member is kept (it may still be certified if
/// this engine's leader schedule is the one that is behind) but not voted
/// for.
#[test]
fn malformed_or_misattributed_proposals_draw_no_vote() {
    let kit = Kit::new(4);
    let chain = kit.chain(2);
    let qc2 = kit.qc_for(&chain[1]);
    let leader = kit.leader(3);
    let not_leader = (leader + 1) % 4;
    let base = || {
        let mut e = kit.engine(leader.wrapping_add(2) % 4);
        let _ = feed(&mut e, &chain, 0);
        e
    };
    let good = kit.proposal(3, 2, qc2.clone(), None, b"p3");

    let mut cases: Vec<(&str, Proposal, Option<Dropped>)> = Vec::new();
    cases.push((
        "from a member that does not lead the round",
        kit.proposal_by(not_leader, 3, 2, qc2.clone(), None, b"p3"),
        None,
    ));
    let (sk, a) = outsider(9);
    let mut p = good.clone();
    p.header.proposer = a;
    p.header.sign_with(&sk);
    cases.push(("signed by an outsider", p, Some(Dropped::BadSignature)));
    let mut p = good.clone();
    p.header.sign_with(&kit.keys[not_leader]);
    cases.push((
        "signed by one member, naming the leader",
        p,
        Some(Dropped::BadSignature),
    ));
    let mut p = good.clone();
    p.payload = b"swapped after signing".to_vec();
    cases.push((
        "payload swapped after signing",
        p,
        Some(Dropped::PayloadMismatch),
    ));
    let mut p = good.clone();
    p.header.payload_len = 0;
    kit.resign(&mut p, leader);
    cases.push((
        "claims to be empty but carries a payload",
        p,
        Some(Dropped::PayloadMismatch),
    ));
    let mut p = good.clone();
    p.header.height += 5;
    kit.resign(&mut p, leader);
    cases.push(("skips heights", p, Some(Dropped::BadHeight)));
    let mut p = good.clone();
    p.header.timestamp = 0;
    kit.resign(&mut p, leader);
    cases.push((
        "timestamp before its parent's",
        p,
        Some(Dropped::TimestampRegressed),
    ));
    cases.push((
        "forks below the lock with no timeout certificate",
        kit.proposal(3, 1, kit.qc_for(&chain[0]), None, b"fork"),
        Some(Dropped::Unjustified),
    ));

    for (what, p, why) in cases {
        let mut e = base();
        let before = snapshot(&e);
        let acts = e.on_message(msg(&p), None, 0);
        assert!(votes(&acts).is_empty(), "{what}: voted");
        match why {
            Some(d) => {
                assert_eq!(last_drop(&e), Some(d), "{what}");
                assert!(!e.holds(&p.header.id()), "{what}: stored");
            }
            None => assert!(e.holds(&p.header.id()), "{what}: not stored"),
        }
        // The certificate it carried is genuine and may move the round
        // forward; nothing moves back.
        let after = snapshot(&e);
        assert!(after.0 >= before.0 && after.1 >= before.1 && after.3 >= before.3);
        // And the real leader's proposal still gets its vote afterwards.
        let acts = e.on_message(msg(&good), None, 0);
        assert_eq!(
            votes(&acts).len(),
            1,
            "{what}: the genuine proposal lost its vote"
        );
    }
}

/// Votes that are not what they claim: from outside the set, relabelled
/// to a member, or with a signature that does not parse. All refused
/// before they touch any tally.
#[test]
fn votes_that_misrepresent_their_voter_are_refused() {
    let kit = Kit::new(4);
    let mut e = kit.engine(0);
    let id = H256([5; 32]);
    let (sk, a) = outsider(1);
    let _ = e.on_message(Message::Vote(Vote::sign(&sk, a, 1, id)), None, 0);
    assert_eq!(last_drop(&e), Some(Dropped::NotInSet));
    let mut v = Vote::sign(&sk, a, 1, id);
    v.voter = kit.addr(2);
    let _ = e.on_message(Message::Vote(v), None, 0);
    assert_eq!(last_drop(&e), Some(Dropped::BadSignature));
    let mut v = kit.vote(1, 1, id);
    v.voter = kit.addr(2);
    let _ = e.on_message(Message::Vote(v), None, 0);
    assert_eq!(last_drop(&e), Some(Dropped::BadSignature));
    let mut v = kit.vote(1, 1, id);
    v.sig = vec![0xFF; 65];
    let _ = e.on_message(Message::Vote(v), None, 0);
    assert_eq!(last_drop(&e), Some(Dropped::BadSignature));
    // None of those counted: two genuine votes are still one short.
    for i in [1, 2] {
        let _ = e.on_message(Message::Vote(kit.vote(i, 1, id)), None, 0);
    }
    assert_eq!(e.high_qc().round, 0);
}

// ----- live -----------------------------------------------------------

/// Check a finished simulation: agreement, and no invariant violation by
/// any validator in `honest`, and never two certificates for one round.
pub(super) fn assert_sim_safe(sim: &Sim, honest: &[usize], ctx: &str) {
    // Ordering agreement and certificate bytes: see `finality.rs`.
    sim.assert_agreement();
    for (i, what) in &sim.violations {
        assert!(
            !(honest.contains(i) || what.starts_with("two certificates")),
            "{ctx}: validator {i}: {what}"
        );
    }
}

/// An equivocating leader over many seeds and seven validators, with a
/// colluding double voter at seven and two double voters at ten: the double
/// voters see both halves of every split and vote for both. With `f`
/// Byzantine members at most, one half can certify but never both, and
/// every honest validator commits the same sequence.
#[test]
fn equivocation_with_colluding_double_voters_never_forks() {
    let runs: &[(usize, &[usize], &[usize], std::ops::RangeInclusive<u64>)] = &[
        (4, &[2], &[], 1..=6),
        (4, &[], &[1], 1..=4),
        (7, &[2], &[5], 1..=4),
        (10, &[2], &[5, 7], 1..=2),
    ];
    for (n, equivocators, double_voters, seeds) in runs {
        for seed in seeds.clone() {
            let mut sim = Sim::new(SimConfig {
                validators: *n,
                seed,
                max_delay_ms: 8,
                ..Default::default()
            });
            for i in *equivocators {
                sim.equivocate(*i);
            }
            for i in *double_voters {
                sim.double_vote(*i);
            }
            sim.run_for(3_000);
            let honest: Vec<usize> = (0..*n)
                .filter(|i| !equivocators.contains(i) && !double_voters.contains(i))
                .collect();
            let ctx = format!("n={n} seed={seed}");
            assert_sim_safe(&sim, &honest, &ctx);
            // At ten, the double votes that arrive first leave some honest
            // validators a vote short of the certified half, and those
            // rounds time out with backoff until leader reputation benches
            // the equivocator: slower, still live.
            let floor = if *n >= 10 { 10 } else { 20 };
            assert!(
                sim.min_committed_among(&honest) >= floor,
                "{ctx}: only {} commits",
                sim.min_committed_among(&honest)
            );
        }
    }
}

/// One member of a live four-validator network also runs a forger that,
/// every few milliseconds, sends some honest validator one of: a proposal
/// on a forged certificate, a timeout carrying a certificate with a bogus
/// round, a proposal forking to genesis on a one-signature timeout
/// certificate, far-future votes and timeouts, a relabelled vote with a
/// garbage signature, a replay of a proposal the victim already holds, or a
/// justified proposal for a round it does not lead. Three seconds of that,
/// and the honest validators agree, keep committing, and none panics.
#[test]
fn a_forging_member_cannot_disturb_a_live_network() {
    for seed in 1..=3 {
        let mut sim = Sim::new(SimConfig {
            seed,
            ..Default::default()
        });
        let byz = 3;
        let key = sim.keys[byz];
        let me = sim.address(byz);
        let genesis = sim.genesis_id;
        sim.run_for(200);
        let start = sim.min_committed_among(&[0, 1, 2]);
        let mut forged_sent = 0;
        for step in 0..1_000u64 {
            sim.run_for(3);
            let victim = (sim.rand() % 3) as usize;
            let kind = sim.rand() % 8;
            let e = sim.engine(victim).unwrap();
            let (round, high) = (e.round(), e.high_qc().clone());
            let parent = e.proposal(&high.id);
            let height = parent.as_ref().map(|p| p.header.height).unwrap_or(0);
            let mk =
                |round: Round, qc: QuorumCert, tc: Option<TimeoutCert>, height: u64| -> Message {
                    let mut h = ProposalHeader {
                        round,
                        height: height + 1,
                        parent_id: qc.id,
                        payload_hash: keccak256(b"forged"),
                        payload_len: 1,
                        timestamp: 0,
                        exec_height: 0,
                        exec_root: H256::ZERO,
                        qc,
                        tc,
                        proposer: me,
                        sig: Vec::new(),
                    };
                    h.sign_with(&key);
                    Message::Proposal(Proposal {
                        header: h,
                        payload: b"forged".to_vec(),
                    })
                };
            let m = match kind {
                0 => {
                    let forged = QuorumCert {
                        round: round.saturating_sub(1).max(1),
                        id: H256([step as u8; 32]),
                        votes: vec![Vote::sign(&key, me, round, H256([step as u8; 32]))],
                    };
                    mk(forged.round + 1, forged, None, height)
                }
                1 => {
                    let bogus = QuorumCert {
                        round: high.round + 1_000_000,
                        ..high.clone()
                    };
                    Message::Timeout(Timeout::sign(&key, me, round, bogus))
                }
                2 => {
                    let tc = TimeoutCert {
                        round: round - 1,
                        timeouts: vec![Timeout::sign(
                            &key,
                            me,
                            round - 1,
                            QuorumCert::genesis(genesis),
                        )],
                    };
                    mk(round, QuorumCert::genesis(genesis), Some(tc), 0)
                }
                3 => Message::Vote(Vote::sign(&key, me, round + 10_000 + step, H256([1; 32]))),
                4 => Message::Timeout(Timeout::sign(
                    &key,
                    me,
                    u64::MAX - step,
                    QuorumCert::genesis(genesis),
                )),
                5 => {
                    let mut v = Vote::sign(&key, me, round, H256([2; 32]));
                    v.voter = sim.address(victim);
                    v.sig[3] ^= 0xFF;
                    Message::Vote(v)
                }
                6 => match parent {
                    Some(p) => Message::Proposal(p),
                    None => continue,
                },
                _ => {
                    if high.round + 1 != round {
                        continue;
                    }
                    mk(round, high.clone(), None, height)
                }
            };
            sim.inject(victim, m, 1);
            forged_sent += 1;
        }
        assert!(forged_sent > 700, "only {forged_sent} forgeries sent");
        let ctx = format!("seed {seed}");
        assert_sim_safe(&sim, &[0, 1, 2], &ctx);
        let end = sim.min_committed_among(&[0, 1, 2]);
        assert!(
            end > start + 50,
            "{ctx}: {start} -> {end} commits under attack"
        );
    }
}
