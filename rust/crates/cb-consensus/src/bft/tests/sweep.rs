//! The randomised sweep: for each seed, a validator set of four, seven or
//! ten, up to `f` of them faulty in a way the seed picks — crashing and
//! restarting with their safety record, equivocating as leader, voting
//! for everything, both, or restarting with their disk wiped — and a
//! schedule of network phases that partition arbitrary groups, lose,
//! duplicate, replay, reorder and delay messages. Then the network settles
//! (global stabilisation time): partitions heal, loss stops, every crashed
//! validator comes back.
//!
//! Safety is checked after every phase, not only at the end: committed
//! sequences are prefixes of one another byte for byte, no round or height
//! ever commits two different proposals across all validators, commit
//! rounds strictly increase and heights never go back, no validator that
//! is not Byzantine ever votes or proposes twice in a round, no round ever
//! has two certificates, and no engine's round, lock, commit point or
//! voting floor goes backwards.
//!
//! Liveness is checked after stabilisation: every validator that is not
//! Byzantine — the restarted ones included — commits at least
//! [`K`] new proposals within [`LIVENESS_MS`] of simulated time and
//! [`LIVENESS_ROUNDS`] rounds.
//!
//! Every run is reproducible from its `(n, seed)`; a failure message names
//! both. `SWEEP_DEBUG=1` prints each validator's state every simulated
//! second while a run waits for liveness.

use super::super::sim::{Payloads, Sim, SimConfig};
use super::byzantine::assert_sim_safe;
use super::*;
use std::collections::HashMap;

/// New commits every non-Byzantine validator must make after the network
/// settles.
const K: usize = 5;
/// Simulated time allowed for them. Most runs settle in under two seconds.
/// The allowance dates from when a validator restarted from its executed
/// head forgot who was standing aside and could stall the chain until a
/// bench ran out (up to about thirteen seconds); that is fixed
/// (`reputation.rs`), and a validator restarted with its disk wiped now
/// costs a few seconds at most. Kept generous so a regression shows as a
/// miss with its recovery time rather than as noise.
const LIVENESS_MS: u64 = 20_000;
/// Rounds allowed for them, counted on the most advanced honest engine:
/// the first bench stretch plus room for timer backoff.
const LIVENESS_ROUNDS: u64 = 600;
/// How long a run that missed the liveness window is followed further, to
/// tell a transient stall from a permanent one.
const RECOVERY_LIMIT_MS: u64 = 300_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Honest,
    /// Crashes and restarts with its safety record: an honest validator
    /// that is sometimes down.
    Crashy,
    Equivocator,
    DoubleVoter,
    EquivocatingDoubleVoter,
    /// Crashes and restarts without its safety record.
    Amnesiac,
}

impl Role {
    fn byzantine(self) -> bool {
        !matches!(self, Role::Honest | Role::Crashy)
    }
    fn crashes(self) -> bool {
        matches!(self, Role::Crashy | Role::Amnesiac)
    }
}

#[derive(Debug, Default)]
#[allow(dead_code)] // `commits` is read only through `Debug` in failure reports.
pub(super) struct Outcome {
    pub ms_to_live: u64,
    pub rounds_to_live: u64,
    pub commits: usize,
    pub faulty: usize,
    /// Commits whose recorded certificate differs between two validators.
    /// Must be zero (`finality.rs`); the sweep fails otherwise.
    pub cert_divergences: usize,
    /// Double votes the invariant log caught from validators restarted
    /// without their safety record: evidence the check can fire.
    pub amnesiac_double_votes: usize,
    /// Why liveness failed, if it did.
    pub failure: Option<String>,
}

fn pick<T: Copy>(sim: &mut Sim, xs: &[T]) -> T {
    xs[(sim.rand() % xs.len() as u64) as usize]
}

/// Safety across the whole network at this instant.
fn check_safety(sim: &Sim, roles: &[Role], ctx: &str, verify_proofs: bool) {
    // Byte-for-byte prefix agreement, increasing rounds, stepping heights,
    // and the invariant log for everyone whose disk is intact.
    let intact: Vec<usize> = (0..roles.len())
        .filter(|i| roles[*i] != Role::Amnesiac)
        .collect();
    assert_sim_safe(sim, &intact, ctx);
    // No round and no non-empty height commits two different proposals,
    // anywhere.
    let mut by_round: HashMap<Round, H256> = HashMap::new();
    let mut by_height: HashMap<u64, H256> = HashMap::new();
    for (i, seq) in sim.committed.iter().enumerate() {
        let mut last_height = 0;
        for c in seq {
            let id = c.header.id();
            let r = *by_round.entry(c.header.round).or_insert(id);
            assert_eq!(r, id, "{ctx}: round {} committed twice", c.header.round);
            if c.header.payload_len > 0 {
                let h = *by_height.entry(c.header.height).or_insert(id);
                assert_eq!(h, id, "{ctx}: height {} committed twice", c.header.height);
            }
            assert!(
                c.header.height >= last_height,
                "{ctx}: validator {i} height went back"
            );
            last_height = c.header.height;
        }
    }
    // Every commit carries a finality proof any observer accepts. The
    // longest sequence is enough: the others are byte-for-byte prefixes.
    if verify_proofs {
        let longest = sim.committed.iter().max_by_key(|c| c.len()).unwrap();
        for c in longest {
            c.finality()
                .verify(&sim.set, sim.genesis_id)
                .unwrap_or_else(|e| {
                    panic!("{ctx}: round {}: bad finality proof: {e}", c.header.round)
                });
        }
    }
}

pub(super) fn scenario(n: usize, seed: u64, phases: usize) -> Outcome {
    let mut sim = Sim::new(SimConfig {
        validators: n,
        seed: seed * 1_000 + n as u64,
        retain_rounds: 4_096,
        ..Default::default()
    });
    sim.payloads = pick(
        &mut sim,
        &[Payloads::Busy, Payloads::Idle, Payloads::Alternating],
    );
    let f = (n - 1) / 3;
    // Usually the full bound; sometimes fewer.
    let faulty = if sim.rand() % 5 == 0 {
        (sim.rand() % (f as u64 + 1)) as usize
    } else {
        f
    };
    let mut order: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (sim.rand() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    let mut roles = vec![Role::Honest; n];
    for &i in &order[..faulty] {
        roles[i] = pick(
            &mut sim,
            &[
                Role::Crashy,
                Role::Crashy,
                Role::Equivocator,
                Role::DoubleVoter,
                Role::EquivocatingDoubleVoter,
                Role::Amnesiac,
            ],
        );
        match roles[i] {
            Role::Equivocator => sim.equivocate(i),
            Role::DoubleVoter => sim.double_vote(i),
            Role::EquivocatingDoubleVoter => {
                sim.equivocate(i);
                sim.double_vote(i);
            }
            _ => {}
        }
    }
    let ctx = |phase: &str| format!("n={n} seed={seed} roles={roles:?} phase {phase}");

    for phase in 0..phases {
        sim.cfg.drop_permille = pick(&mut sim, &[0, 0, 50, 150, 300]);
        sim.cfg.dup_permille = pick(&mut sim, &[0, 0, 50, 200]);
        sim.cfg.replay_permille = pick(&mut sim, &[0, 0, 20, 100]);
        sim.cfg.slow_permille = pick(&mut sim, &[0, 0, 50, 150]);
        sim.cfg.max_delay_ms = pick(&mut sim, &[3, 5, 15, 40]);
        sim.heal();
        if sim.rand() % 3 == 0 {
            let side: Vec<bool> = (0..n).map(|_| sim.rand() % 2 == 0).collect();
            let a: Vec<usize> = (0..n).filter(|i| side[*i]).collect();
            let b: Vec<usize> = (0..n).filter(|i| !side[*i]).collect();
            sim.partition(&a, &b);
        }
        for i in 0..n {
            if roles[i].crashes() && sim.rand() % 3 == 0 {
                match (sim.is_up(i), roles[i]) {
                    (true, _) => sim.crash(i),
                    (false, Role::Amnesiac) => sim.restart_amnesiac(i),
                    (false, _) => sim.restart(i),
                }
            }
        }
        let dur = 100 + sim.rand() % 500;
        sim.run_for(dur);
        check_safety(&sim, &roles, &ctx(&phase.to_string()), false);
    }

    // Stabilisation.
    sim.heal();
    sim.cfg.drop_permille = 0;
    sim.cfg.dup_permille = 0;
    sim.cfg.replay_permille = 0;
    sim.cfg.slow_permille = 0;
    sim.cfg.max_delay_ms = 5;
    for i in 0..n {
        if !sim.is_up(i) {
            if roles[i] == Role::Amnesiac {
                sim.restart_amnesiac(i);
            } else {
                sim.restart(i);
            }
        }
    }
    let live: Vec<usize> = (0..n).filter(|i| !roles[*i].byzantine()).collect();
    let base: Vec<usize> = live.iter().map(|i| sim.committed[*i].len()).collect();
    let top_round = |sim: &Sim| {
        live.iter()
            .map(|i| sim.engine(*i).unwrap().round())
            .max()
            .unwrap()
    };
    let (t0, r0) = (sim.now_ms, top_round(&sim));
    let done = |sim: &Sim| {
        live.iter()
            .zip(&base)
            .all(|(i, b)| sim.committed[*i].len() >= b + K)
    };
    let debug = std::env::var("SWEEP_DEBUG").is_ok();
    while !done(&sim) && sim.now_ms - t0 < LIVENESS_MS {
        sim.run_for(50);
        if debug && (sim.now_ms - t0) % 1000 == 0 {
            let st: Vec<String> = (0..n)
                .map(|i| match sim.engine(i) {
                    Some(e) => format!(
                        "{i}:{:?} c{} r{} hq{} cr{} lv{} beh{} orph{} req{}",
                        roles[i],
                        sim.committed[i].len(),
                        e.round(),
                        e.high_qc().round,
                        e.committed_round(),
                        e.last_voted_round(),
                        e.behind_by(),
                        e.orphan_count(),
                        e.outstanding_requests()
                    ),
                    None => format!("{i}:down"),
                })
                .collect();
            eprintln!(
                "n={n} seed={seed} t+{}: {}",
                sim.now_ms - t0,
                st.join(" | ")
            );
        }
    }
    let out = Outcome {
        ms_to_live: sim.now_ms - t0,
        rounds_to_live: top_round(&sim) - r0,
        commits: sim.max_committed(),
        faulty,
        cert_divergences: sim.cert_divergences().len(),
        failure: None,
        amnesiac_double_votes: sim
            .violations
            .iter()
            .filter(|(i, what)| roles[*i] == Role::Amnesiac && what.starts_with("voted twice"))
            .count(),
    };
    check_safety(&sim, &roles, &ctx("after stabilisation"), true);
    let mut failure = None;
    if !done(&sim) || out.rounds_to_live > LIVENESS_ROUNDS {
        let progress: Vec<(usize, usize)> = live
            .iter()
            .zip(&base)
            .map(|(i, b)| (*i, sim.committed[*i].len() - b))
            .collect();
        let commit_points: Vec<Round> = live
            .iter()
            .map(|i| sim.engine(*i).unwrap().committed_round())
            .collect();
        // How long it really takes, if it recovers at all: a stall that
        // outlasts the window is either transient or for good, and the
        // two are different defects.
        while !done(&sim) && sim.now_ms - t0 < RECOVERY_LIMIT_MS {
            sim.run_for(500);
        }
        check_safety(&sim, &roles, &ctx("after the extended wait"), false);
        let recovered = if done(&sim) {
            format!("recovered after {} ms", sim.now_ms - t0)
        } else {
            format!("still stalled at {RECOVERY_LIMIT_MS} ms")
        };
        failure = Some(format!(
            "{}: not live within {LIVENESS_MS} ms / {LIVENESS_ROUNDS} rounds \
             ({} ms, {} rounds); new commits {progress:?}; commit rounds {commit_points:?}; {recovered}",
            ctx("liveness"),
            out.ms_to_live,
            out.rounds_to_live
        ));
    }
    Outcome { failure, ..out }
}

/// Run every seed. A safety violation panics at once with its seed; a
/// liveness failure is collected, the sweep carries on, and the test fails
/// at the end listing every seed that missed, so one run shows the whole
/// picture.
pub(super) fn sweep(n: usize, seeds: std::ops::RangeInclusive<u64>, phases: usize) {
    let (mut worst_ms, mut worst_rounds, mut runs, mut faulty) = (0, 0, 0, 0);
    let (mut divergent_runs, mut amnesiac_double_votes) = (0, 0);
    let mut failures = Vec::new();
    for seed in seeds {
        let o = scenario(n, seed, phases);
        if let Some(f) = o.failure {
            failures.push(f);
            runs += 1;
            continue;
        }
        worst_ms = worst_ms.max(o.ms_to_live);
        worst_rounds = worst_rounds.max(o.rounds_to_live);
        faulty += o.faulty;
        divergent_runs += usize::from(o.cert_divergences > 0);
        amnesiac_double_votes += o.amnesiac_double_votes;
        runs += 1;
    }
    eprintln!(
        "sweep n={n}: {runs} seeds, {faulty} faulty validators in the live ones, \
         worst recovery {worst_ms} ms / {worst_rounds} rounds after stabilisation; \
         {divergent_runs} runs recorded differing certificates for some commit; \
         {amnesiac_double_votes} double votes caught from validators restarted without their record; \
         {} liveness failures",
        failures.len()
    );
    assert_eq!(
        divergent_runs, 0,
        "honest validators recorded different certificates for a commit"
    );
    assert!(
        failures.is_empty(),
        "{} of {runs} runs missed liveness:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Four validators (`f = 1`), 150 seeds, eight fault phases each.
#[test]
fn sweep_four_validators() {
    sweep(4, 1..=150, 8);
}

/// Seven validators (`f = 2`), 50 seeds.
#[test]
fn sweep_seven_validators() {
    sweep(7, 1..=50, 8);
}

/// Ten validators (`f = 3`), 25 seeds.
#[test]
fn sweep_ten_validators() {
    sweep(10, 1..=25, 8);
}

/// The long sweep: hundreds of seeds, more phases. Run with
/// `cargo test -p cb-consensus --release -- --ignored sweep_long`.
#[test]
#[ignore = "long: run with --ignored"]
fn sweep_long_four_validators() {
    sweep(4, 151..=2_150, 12);
}

/// Run with `cargo test -p cb-consensus --release -- --ignored sweep_long`.
#[test]
#[ignore = "long: run with --ignored"]
fn sweep_long_seven_validators() {
    sweep(7, 51..=650, 12);
}

/// Run with `cargo test -p cb-consensus --release -- --ignored sweep_long`.
#[test]
#[ignore = "long: run with --ignored"]
fn sweep_long_ten_validators() {
    sweep(10, 26..=325, 12);
}

/// Reproduce one run of the sweep:
/// `SWEEP_N=4 SWEEP_SEED=184 SWEEP_DEBUG=1 cargo test -p cb-consensus --release -- --ignored sweep_one --nocapture`.
#[test]
#[ignore = "reproduces one sweep run named by SWEEP_N and SWEEP_SEED"]
fn sweep_one() {
    let var = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
    let (Some(n), Some(seed)) = (var("SWEEP_N"), var("SWEEP_SEED")) else {
        return;
    };
    let phases = var("SWEEP_PHASES").unwrap_or(12) as usize;
    let o = scenario(n as usize, seed, phases);
    eprintln!("{o:?}");
}
