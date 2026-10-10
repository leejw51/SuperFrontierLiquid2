//! The pacemaker after message loss: validators left in different rounds
//! must be able to converge once the network delivers again.

use super::*;

/// Round 1 times out. Validator 3 hears the others' timeouts, joins, forms
/// the timeout certificate and moves to round 2. Validator 0 timed out
/// round 1 too, but every timeout from the others was lost on the way to
/// it, so it is still in round 1. The network now delivers everything.
///
/// Validator 3's timer in round 2 fires. Its own round-2 timeout is no use
/// to validator 0 — one timeout for a round ahead is short of the `f + 1`
/// that would make it join. What moves validator 0 is the round-1 timeouts
/// validator 3 holds, which it now passes on with its own. Before this the
/// two stayed in rounds 1 and 2 for ever, each re-sending its own timeout
/// (sweep `n=4 seed=184`: four honest validators frozen across rounds 46,
/// 47 and 48 for the whole twenty seconds after the network settled).
#[test]
fn a_validator_left_a_round_behind_by_lost_timeouts_catches_up() {
    let kit = Kit::new(4);
    let g = kit.genesis_qc();
    let mut behind = kit.engine(0);
    let own = timeouts(&behind.on_tick(behind.next_deadline_ms()));
    assert_eq!(own.len(), 1);
    assert_eq!(behind.round(), 1);

    let mut ahead = kit.engine(3);
    let mut acts = Vec::new();
    for i in [1, 2] {
        acts.extend(ahead.on_message(Message::Timeout(kit.timeout(i, 1, g.clone())), None, 0));
    }
    assert_eq!(ahead.round(), 2, "the certificate for round 1 did not form");

    // Round 2's timer fires on the validator ahead; everything it sends
    // reaches the one behind.
    let sent = ahead.on_tick(ahead.next_deadline_ms());
    let sent = timeouts(&sent);
    assert!(sent.iter().any(|t| t.round == 2 && t.voter == kit.addr(3)));
    for t in sent {
        let _ = behind.on_message(Message::Timeout(t), None, 10);
    }
    assert_eq!(
        behind.round(),
        2,
        "the validator behind is still stuck in round 1"
    );

    // And the same holds a second time round: the repeat carries them too.
    let again = timeouts(&ahead.on_tick(ahead.next_deadline_ms()));
    assert!(again.iter().filter(|t| t.round == 1).count() >= kit.set.quorum());
}
