//! Leader reputation after a restart.
//!
//! The leader schedule skips validators whose rounds ended in a timeout
//! certificate. It is a fold along the chain a proposal extends (ADR 0008,
//! decision 8), so every validator judging a proposal reaches the same
//! verdict; a validator restarted from its executed head starts that fold
//! from the snapshot it wrote to disk at its commit point.

use super::super::sim::{Sim, SimConfig};

/// Validator 1 dies and the others bench it once its timed-out rounds
/// commit. `restart_v2` then has validator 2 crash and come back straight
/// away (safety record intact, network perfect, validator 1 still dead).
/// Returns the new commits of the three live validators over the next
/// three seconds, the least of them.
fn after_bench(seed: u64, restart_v2: bool) -> usize {
    after_bench_with(seed, restart_v2, true, 3_000)
}

/// As [`after_bench`]; `keep_snapshot: false` restarts validator 2 without
/// the reputation snapshot on its disk, and the commits are counted over
/// `ms`.
fn after_bench_with(seed: u64, restart_v2: bool, keep_snapshot: bool, ms: u64) -> usize {
    let mut sim = Sim::new(SimConfig {
        seed,
        ..Default::default()
    });
    sim.run_for(300);
    sim.crash(1);
    sim.run_for(1_500);
    let dead = sim.address(1);
    for i in [0, 2, 3] {
        let e = sim.engine(i).unwrap();
        let r = e.round();
        assert!(
            e.standing_aside(r).contains(&dead) && e.leader_for(r) != dead,
            "seed {seed}: validator {i} has not benched the dead validator"
        );
    }
    if restart_v2 {
        if !keep_snapshot {
            sim.reputation[2] = None;
        }
        sim.crash(2);
        sim.run_for(20);
        sim.restart(2);
    }
    let before: Vec<usize> = [0, 2, 3].iter().map(|i| sim.committed[*i].len()).collect();
    sim.run_for(ms);
    sim.assert_agreement();
    [0, 2, 3]
        .iter()
        .zip(before)
        .map(|(i, b)| sim.committed[*i].len() - b)
        .min()
        .unwrap()
}

/// The control: with the dead validator benched, the other three commit
/// steadily.
#[test]
fn three_validators_commit_steadily_once_the_dead_one_is_benched() {
    for seed in 1..=3 {
        let n = after_bench(seed, false);
        assert!(n >= 100, "seed {seed}: only {n} commits in three seconds");
    }
}

/// One of the three live validators restarts. Before the schedule was a
/// fold along the extended chain started from a snapshot on disk, it came
/// back with an empty standing-aside list, believed the dead validator
/// still led every fourth round and refused to vote for whoever the others
/// thought led it. Its vote is needed — three of four is the quorum — so
/// those rounds timed out and nothing committed until the bench ran out
/// (128 rounds, doubling with each repeat, up to 8,192). The sweep found
/// it with all four validators honest after the network settled (`n=4
/// seed=67`: 123 rounds, 12.9 s simulated, to five commits).
#[test]
fn a_restarted_validator_agrees_on_the_leader_schedule() {
    for seed in 1..=3 {
        let n = after_bench(seed, true);
        assert!(
            n >= 100,
            "seed {seed}: only {n} commits in three seconds after the restart"
        );
    }
}

/// The same restart with the snapshot lost (a wiped disk, or a catch-up by
/// block sync past rounds it never saw). The restarted validator does not
/// know the dead one is standing aside and disagrees about most rounds;
/// its vote is needed, so they time out. A disagreement costs rounds, not
/// the chain: two timeouts in a row and the next round is the plain round
/// robin's, which everyone agrees on (`FALLBACK_AFTER`); the chain moves,
/// and when the plain round robin reaches the dead validator its timeout
/// lands on the chain and benches it in every schedule. It can come apart
/// again when the restarted validator's shorter bench runs out, and mends
/// the same way, until the timeouts it never saw are older than
/// `REPUTATION_WINDOW`. Measured: commits resume within one to five
/// simulated seconds, then run at close to the full rate.
#[test]
fn a_validator_restarted_without_its_snapshot_recovers() {
    for seed in 1..=3 {
        let n = after_bench_with(seed, true, false, 8_000);
        assert!(
            n >= 200,
            "seed {seed}: only {n} commits in eight seconds after the restart"
        );
    }
}
