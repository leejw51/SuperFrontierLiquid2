//! Quorum arithmetic for every set size from one to sixteen.
//!
//! Every safety argument in the engine reduces to one fact: two quorums
//! share at least `f + 1` validators, so at least one honest validator, who
//! never votes twice in a round. Every liveness argument reduces to
//! another: the honest validators alone make a quorum. These tests check
//! both facts exhaustively over subsets, not just by formula, and check that
//! the code that counts signatures stops at exactly the threshold.

use super::*;
use crate::quorum_count;
use proptest::prelude::*;

/// Every subset of `0..n` with exactly `k` members, as bitmasks.
fn subsets(n: usize, k: usize) -> Vec<u32> {
    (0u32..(1 << n))
        .filter(|m| m.count_ones() as usize == k)
        .collect()
}

/// Any two quorums intersect in at least `f + 1` validators — checked over
/// every pair of quorum-sized subsets, not derived — and the bound is tight
/// when `n = 3f + 1`. If this failed for some `n`, two certificates for two
/// different proposals in one round would be possible with only `f` liars,
/// and the chain could fork.
#[test]
fn two_quorums_share_an_honest_validator_for_every_n_up_to_16() {
    for n in 1..=16usize {
        let q = quorum_count(n);
        let f = (n - 1) / 3;
        let quorums = subsets(n, q);
        let mut min_overlap = usize::MAX;
        for (i, a) in quorums.iter().enumerate() {
            for b in &quorums[i..] {
                min_overlap = min_overlap.min((a & b).count_ones() as usize);
            }
        }
        assert!(
            min_overlap > f,
            "n={n}: two quorums of {q} can share only {min_overlap} validators, f={f}"
        );
        assert_eq!(min_overlap, 2 * q - n, "n={n}: overlap formula");
        if n % 3 == 1 {
            assert_eq!(min_overlap, f + 1, "n={n}: bound should be tight at 3f+1");
        }
    }
}

/// The honest validators alone form a quorum, so `f` silent or lying
/// validators can never stop the others from certifying. And every quorum
/// holds at least `f + 1` honest members, so `f + 1` matching timeouts is
/// the least evidence that an honest validator gave up.
#[test]
fn honest_validators_alone_make_a_quorum_for_every_n_up_to_16() {
    for n in 1..=16usize {
        let kit = Kit::new(n);
        let (q, f) = (kit.set.quorum(), kit.set.faults_tolerated());
        assert_eq!(q, quorum_count(n));
        assert_eq!(f, (n - 1) / 3);
        assert_eq!(kit.set.honest_threshold(), f + 1);
        assert!(
            q <= n - f,
            "n={n}: honest {} cannot reach quorum {q}",
            n - f
        );
        assert!(
            q - f > f,
            "n={n}: a quorum minus the liars is not f+1 honest"
        );
        // f is the largest tolerable: one more and the honest alone fall
        // short of a quorum, or two quorums could share only liars.
        let f1 = f + 1;
        assert!(q > n - f1 || 2 * q - n <= f1, "n={n}: f is not maximal");
    }
}

/// Signatures one short of a quorum never certify, for proposals or for
/// timeouts, at every set size; exactly a quorum does; and a quorum padded
/// out with a repeated signer or an outsider is refused. The boundary is
/// where an off-by-one would let `f` liars plus `f` honest validators
/// certify on their own.
#[test]
fn one_signature_short_of_a_quorum_never_certifies() {
    let (outsider, pk) = keypair_from_seed(b"not-a-validator");
    let outsider_addr = address_from_pubkey(&pk);
    for n in 1..=16usize {
        let kit = Kit::new(n);
        let q = kit.set.quorum();
        let id = H256([n as u8; 32]);
        let signers: Vec<usize> = (0..q).collect();

        let full = kit.qc_by(7, id, &signers);
        full.verify(&kit.set, kit.genesis).unwrap();
        let short = kit.qc_by(7, id, &signers[..q - 1]);
        assert_eq!(
            short.verify(&kit.set, kit.genesis),
            Err(BftError::BelowQuorum {
                got: q - 1,
                need: q
            }),
            "n={n}"
        );
        if q >= 2 {
            let mut dup = short.clone();
            dup.votes.push(dup.votes[0].clone());
            assert!(
                matches!(
                    dup.verify(&kit.set, kit.genesis),
                    Err(BftError::DuplicateVoter(_))
                ),
                "n={n}"
            );
        }
        let mut foreign = short.clone();
        foreign
            .votes
            .push(Vote::sign(&outsider, outsider_addr, 7, id));
        assert!(
            matches!(
                foreign.verify(&kit.set, kit.genesis),
                Err(BftError::VoterNotInSet(_))
            ),
            "n={n}"
        );

        let g = kit.genesis_qc();
        let tc = TimeoutCert {
            round: 9,
            timeouts: signers
                .iter()
                .map(|i| kit.timeout(*i, 9, g.clone()))
                .collect(),
        };
        tc.verify(&kit.set, kit.genesis).unwrap();
        let mut short_tc = tc.clone();
        short_tc.timeouts.pop();
        assert_eq!(
            short_tc.verify(&kit.set, kit.genesis),
            Err(BftError::BelowQuorum {
                got: q - 1,
                need: q
            }),
            "n={n}"
        );
        if q >= 2 {
            let mut dup_tc = short_tc.clone();
            dup_tc.timeouts.push(dup_tc.timeouts[0].clone());
            assert!(
                matches!(
                    dup_tc.verify(&kit.set, kit.genesis),
                    Err(BftError::DuplicateVoter(_))
                ),
                "n={n}"
            );
        }
    }
}

/// The engine itself certifies at exactly a quorum of votes: `q - 1`
/// distinct votes leave the round where it was, the `q`-th moves it on,
/// and a repeat of any of the first `q - 1` never counts twice. Checked
/// for every set size.
#[test]
fn the_engine_certifies_at_exactly_a_quorum_of_votes() {
    for n in 1..=16usize {
        let kit = Kit::new(n);
        let q = kit.set.quorum();
        let me = n - 1;
        let mut e = kit.engine(me);
        let id = H256([0xAB; 32]);
        assert_eq!(e.round(), 1);
        for i in 0..q - 1 {
            let _ = e.on_message(Message::Vote(kit.vote(i, 1, id)), None, 0);
            // A repeat is not a second voice.
            let _ = e.on_message(Message::Vote(kit.vote(i, 1, id)), None, 0);
        }
        assert_eq!(
            (e.round(), e.high_qc().round),
            (1, 0),
            "n={n}: certified early"
        );
        let last = if q - 1 == me { me } else { q - 1 };
        let _ = e.on_message(Message::Vote(kit.vote(last, 1, id)), None, 0);
        assert_eq!(
            (e.round(), e.high_qc().round),
            (2, 1),
            "n={n}: quorum ignored"
        );
        assert_eq!(e.high_qc().votes.len(), q);
        e.high_qc().verify(&kit.set, kit.genesis).unwrap();
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// For a random set size and a random subset of signers, a certificate
    /// verifies exactly when the subset reaches a quorum.
    #[test]
    fn a_certificate_verifies_iff_its_signers_reach_quorum(
        n in 1usize..=16,
        mask in any::<u16>(),
        round in 1u64..1_000_000,
    ) {
        let kit = Kit::new(n);
        let signers: Vec<usize> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
        let qc = kit.qc_by(round, H256([3; 32]), &signers);
        let ok = qc.verify(&kit.set, kit.genesis).is_ok();
        prop_assert_eq!(ok, signers.len() >= kit.set.quorum());
    }
}
