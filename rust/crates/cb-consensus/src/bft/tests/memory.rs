//! Memory. Every buffer the engine keeps is filled by messages from peers,
//! so every buffer must have a bound that no peer — and no `f` peers —
//! can push past. An engine that runs out of memory is a crashed engine,
//! and `f + 1` crashed engines halt the chain.

use super::super::engine::BufferSizes;
use super::super::sim::{Sim, SimConfig};
use super::*;

/// A member of the set floods a validator that is behind — rejoining after
/// a restart, say, and so missing the proposals the certificates it holds
/// name — with distinct, correctly signed, justified proposals for one
/// round, differing only in payload. Each one waits for a parent the
/// victim does not have. Only one per proposer per round can ever enter
/// the tree, so only one per proposer per round may wait for a parent:
/// anything more is memory the attacker chooses the size of, payloads
/// included. When the parents arrive the victim carries on normally.
#[test]
fn an_orphan_flood_from_one_member_stays_bounded() {
    let kit = Kit::new(4);
    let chain = kit.chain(6);
    let qc5 = kit.qc_for(&chain[4]);
    let attacker = 3;
    assert_ne!(kit.leader(6), attacker);
    let mut e = kit.engine(0);
    for k in 0..2_000u32 {
        let payload = format!("flood {k} {}", "x".repeat(200));
        let p = kit.proposal_by(attacker, 6, 5, qc5.clone(), None, payload.as_bytes());
        let _ = e.on_message(msg(&p), None, 0);
    }
    let sizes = e.buffer_sizes();
    assert!(
        sizes.orphans <= kit.n() && sizes.orphan_ids <= kit.n(),
        "one member parked {} proposals for one round: {sizes:?}",
        sizes.orphans
    );

    // The genuine chain arrives; the victim adopts it and votes in round 6.
    let acts = feed(&mut e, &chain, 0);
    assert!(votes(&acts).iter().any(|v| v.round == 6));
    assert_eq!(e.committed_round(), 4);
    assert_eq!(e.orphan_count(), 0);
}

/// The same flood through the timeout-certificate path: proposals for
/// round 7 justified by a genuine timeout certificate for round 6, each
/// extending the round-5 certificate. Bounded the same way.
#[test]
fn an_orphan_flood_behind_a_timeout_certificate_stays_bounded() {
    let kit = Kit::new(4);
    let chain = kit.chain(5);
    let qc5 = kit.qc_for(&chain[4]);
    let tc6 = kit.tc(6, &[qc5.clone()]);
    let attacker = 1;
    assert_ne!(kit.leader(7), attacker);
    let mut e = kit.engine(0);
    for k in 0..1_000u32 {
        let p = kit.proposal_by(
            attacker,
            7,
            5,
            qc5.clone(),
            Some(tc6.clone()),
            format!("flood {k}").as_bytes(),
        );
        let _ = e.on_message(msg(&p), None, 0);
    }
    let sizes = e.buffer_sizes();
    assert!(sizes.orphans <= kit.n(), "{sizes:?}");
}

/// The bound each buffer must respect, given where the engine is. `window`
/// is the span of rounds anything may legitimately be buffered for: from
/// the commit point to the far-future horizon.
fn assert_bounded(i: usize, e: &Engine, n: usize, retain: u64, ctx: &str) -> BufferSizes {
    let s = e.buffer_sizes();
    let window = (e.round() - e.committed_round() + retain + 2) as usize;
    let history = (retain as usize + window) * n;
    let checks: [(&str, usize, usize); 14] = [
        ("tree", s.tree, history),
        ("children", s.children, history),
        ("by_round", s.by_round, history),
        ("certified", s.certified, history),
        ("orphans", s.orphans, n * window),
        ("orphan_ids", s.orphan_ids, n * window),
        ("orphan_slots", s.orphan_slots, n * window),
        ("requested", s.requested, n * window),
        ("vote rounds", s.vote_rounds, window),
        ("voters", s.voters, n * window),
        ("votes", s.votes, n * window),
        ("timeout rounds", s.timeout_rounds, window),
        ("timeout senders", s.timeout_senders, n * window),
        ("timeouts", s.timeouts, n * window),
    ];
    for (name, got, bound) in checks {
        assert!(
            got <= bound,
            "{ctx}: validator {i}: {name} = {got} > {bound}; {s:?}"
        );
    }
    assert!(s.tcs <= window, "{ctx}: validator {i}: tcs {}", s.tcs);
    assert!(
        s.dropped <= 1_024,
        "{ctx}: validator {i}: dropped {}",
        s.dropped
    );
    assert!(
        s.pending <= 4 * n,
        "{ctx}: validator {i}: pending {}",
        s.pending
    );
    s
}

/// Twenty simulated seconds of a live four-validator network in which one
/// member, besides running an honest engine, sprays every honest validator
/// with votes and timeouts for rounds scattered around the current one and
/// far beyond it, replays of old proposals, and justified proposals of its
/// own for rounds it does not lead. Every buffer stays inside its bound at
/// the halfway mark and at the end, and none grows from one to the other
/// by more than churn and the widening of the uncommitted stretch —
/// steady state, not a leak.
#[test]
fn buffers_stay_bounded_under_a_long_spam_run() {
    let retain = 256;
    let mut sim = Sim::new(SimConfig {
        seed: 42,
        retain_rounds: retain,
        max_delay_ms: 6,
        ..Default::default()
    });
    let byz = 3;
    let key = sim.keys[byz];
    let me = sim.address(byz);
    let genesis = sim.genesis_id;
    sim.run_for(300);
    let old: Vec<Message> = sim.committed[0]
        .iter()
        .map(|c| {
            Message::Proposal(Proposal {
                header: c.header.clone(),
                payload: c.payload.clone(),
            })
        })
        .collect();
    let mut halfway: Vec<(BufferSizes, u64)> = Vec::new();
    for step in 0..5_000u64 {
        sim.run_for(4);
        let victim = (step % 3) as usize;
        let e = sim.engine(victim).unwrap();
        let (round, high) = (e.round(), e.high_qc().clone());
        let parent_height = e.proposal(&high.id).map(|p| p.header.height);
        let jitter = sim.rand() % 600;
        let near = (round + jitter).saturating_sub(300).max(1);
        let id = H256([(sim.rand() % 251) as u8; 32]);
        let mut spam = vec![
            Message::Vote(Vote::sign(&key, me, near, id)),
            Message::Vote(Vote::sign(&key, me, round + 1_000_000 + step, id)),
            Message::Timeout(Timeout::sign(
                &key,
                me,
                round + sim.rand() % (retain + 50),
                QuorumCert::genesis(genesis),
            )),
            Message::Timeout(Timeout::sign(
                &key,
                me,
                u64::MAX - step,
                QuorumCert::genesis(genesis),
            )),
            old[(step as usize) % old.len()].clone(),
        ];
        if let (Some(h), true) = (parent_height, high.round + 1 == round) {
            let payload = format!("spam {step}").into_bytes();
            let mut header = ProposalHeader {
                round,
                height: h + 1,
                parent_id: high.id,
                payload_hash: keccak256(&payload),
                payload_len: 1,
                timestamp: u64::MAX / 2,
                exec_height: 0,
                exec_root: H256::ZERO,
                qc: high,
                tc: None,
                proposer: me,
                sig: Vec::new(),
            };
            header.sign_with(&key);
            spam.push(Message::Proposal(Proposal { header, payload }));
        }
        for m in spam {
            sim.inject(victim, m, 1);
        }
        if step == 2_500 {
            for i in 0..3 {
                let e = sim.engine(i).unwrap();
                let lag = e.round() - e.committed_round();
                halfway.push((assert_bounded(i, e, 4, retain, "halfway"), lag));
            }
        }
    }
    sim.run_for(200);
    let honest = [0, 1, 2];
    super::byzantine::assert_sim_safe(&sim, &honest, "spam run");
    let state: Vec<String> = (0..4)
        .map(|i| {
            let e = sim.engine(i).unwrap();
            format!(
                "v{i}: commits {} round {} high {} committed {} voted {}",
                sim.committed[i].len(),
                e.round(),
                e.high_qc().round,
                e.committed_round(),
                e.last_voted_round()
            )
        })
        .collect();
    // Progress, loosely: the spam must not stop the chain, but this seed
    // also runs into the leader-reputation stall (`reputation.rs`, a known
    // defect), which halts commits for a stretch near the end of the run.
    assert!(
        sim.min_committed_among(&honest) > 300,
        "the spam stalled the chain: {state:?}"
    );
    for i in 0..3 {
        let e = sim.engine(i).unwrap();
        let end = assert_bounded(i, e, 4, retain, "end");
        let (mid, mid_lag) = &halfway[i];
        // What is above the commit point is kept until it commits, so a
        // wider uncommitted stretch at the end legitimately holds more: up
        // to one entry per validator per extra round.
        let slack = 4 * (e.round() - e.committed_round()).saturating_sub(*mid_lag) as usize;
        for (name, a, b) in [
            ("tree", mid.tree, end.tree),
            ("voters", mid.voters, end.voters),
            ("timeouts", mid.timeouts, end.timeouts),
            ("orphans", mid.orphans, end.orphans),
            ("certified", mid.certified, end.certified),
            ("children", mid.children, end.children),
            ("by_round", mid.by_round, end.by_round),
        ] {
            assert!(
                b <= a + a / 2 + 64 + slack,
                "validator {i}: {name} grew from {a} to {b} in the second half (slack {slack})"
            );
        }
    }
}

/// A validator partitioned away for a long stretch keeps timing out rounds
/// alone. Its buffers hold only its own timeouts in that time, one per
/// round it sat through, and once it rejoins and commits they are pruned
/// back to steady state.
#[test]
fn an_isolated_validator_prunes_after_it_rejoins() {
    let mut sim = Sim::new(SimConfig {
        seed: 7,
        ..Default::default()
    });
    sim.run_for(300);
    sim.partition(&[0, 1, 2], &[3]);
    // Long enough to time out alone many times, short enough that the
    // proposals it missed are still retained (256 rounds here): with
    // the dead validator benched the others run well over a hundred
    // rounds a second.
    sim.run_for(1_500);
    let isolated = sim.engine(3).unwrap().buffer_sizes();
    assert!(isolated.timeouts <= 64, "{isolated:?}");
    sim.heal();
    sim.run_for(3_000);
    sim.assert_agreement();
    let e = sim.engine(3).unwrap();
    assert!(e.committed_round() + 10 >= sim.engine(0).unwrap().committed_round());
    let s = assert_bounded(3, e, 4, 256, "after rejoin");
    assert!(s.timeout_rounds <= 8, "{s:?}");
    assert!(s.vote_rounds <= 8, "{s:?}");
}
