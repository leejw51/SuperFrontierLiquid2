//! Peer behaviour classification and the score it moves.
//!
//! Scoring exists so that a peer which wastes our time stops being retried.
//! The design choice worth naming is that the *classification* lives here and
//! the *numbers* live in [`ScoreConfig`], so an operator can retune aggression
//! without a code change and a reader can see that no threshold is invented at
//! its call site.
//!
//! Two things are deliberately scored differently:
//!
//! * A peer that serves a block which fails re-execution is heavily penalised
//!   — that is either corruption or malice, and either way its blocks are
//!   worthless to us.
//! * A peer that cannot serve a range because it has pruned those heights is
//!   barely penalised. It is being honest about what it has; punishing it
//!   would make archival topology a prerequisite for participation.
//!
//! Positive behaviour restores score. A peer that times out during a network
//! hiccup and then serves ten good blocks ends up ahead, which is the point:
//! the ban is for peers that are persistently useless, not for peers that had
//! one bad minute.

use crate::p2p::config::ScoreConfig;

/// Something a peer did that we have an opinion about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// Served a block body that verified and was adopted.
    GoodBlock,
    /// Served a transaction that passed static validation.
    GoodTx,
    /// Answered a `getPeers` with at least one usable address.
    GoodPeers,
    /// Announced or served something we already had.
    Duplicate,
    /// Did not answer a request inside the timeout.
    Timeout,
    /// Sent a body we never requested.
    Unsolicited,
    /// Exceeded a rate limit or overflowed a bounded buffer.
    Flood,
    /// Sent a malformed signature or an unverifiable identity.
    BadSignature,
    /// Served a block that failed verification — a bad state root, a bad
    /// proposer signature, a body that would not decode.
    BadBlock,
    /// Broke the message state machine.
    ProtocolViolation,
    /// Could not serve a requested range (pruned, or still catching up).
    /// Scored as a duplicate-weight nuisance, not as misbehaviour.
    UnservedRange,
}

impl Behaviour {
    /// The score delta this behaviour carries under `cfg`.
    pub fn delta(&self, cfg: &ScoreConfig) -> i32 {
        match self {
            Behaviour::GoodBlock => cfg.good_block,
            Behaviour::GoodTx => cfg.good_tx,
            Behaviour::GoodPeers => cfg.good_peers,
            Behaviour::Duplicate | Behaviour::UnservedRange => cfg.duplicate,
            Behaviour::Timeout => cfg.timeout,
            Behaviour::Unsolicited => cfg.unsolicited,
            Behaviour::Flood => cfg.flood,
            Behaviour::BadSignature => cfg.bad_signature,
            Behaviour::BadBlock => cfg.bad_block,
            Behaviour::ProtocolViolation => cfg.protocol_violation,
        }
    }

    /// Whether this behaviour is normal-but-unhelpful rather than evidence of
    /// misbehaviour.
    ///
    /// Benign behaviour costs a peer reputation but is never, on its own, a
    /// reason to ban it. Gossip is redundant by construction, so a peer that
    /// is merely late or merely pruned would otherwise accumulate its way to a
    /// ban just by participating — which is exactly what happened the first
    /// time this ran against a real cluster: a node catching up re-announced
    /// every historical block it adopted, and the peer it was catching up from
    /// banned it for the duplicates.
    pub fn is_benign(&self) -> bool {
        matches!(self, Behaviour::Duplicate | Behaviour::UnservedRange)
    }

    /// Whether this behaviour is one the operator should see in the log at
    /// warn level. Duplicates are the normal cost of gossip and would drown
    /// the log if they were surfaced.
    pub fn is_notable(&self) -> bool {
        !matches!(self, Behaviour::Duplicate | Behaviour::UnservedRange) && self.is_negative_kind()
    }

    fn is_negative_kind(&self) -> bool {
        !matches!(
            self,
            Behaviour::GoodBlock | Behaviour::GoodTx | Behaviour::GoodPeers
        )
    }
}

/// Apply `behaviour` to `score`, clamped to the configured range.
pub fn apply(score: i32, behaviour: Behaviour, cfg: &ScoreConfig) -> i32 {
    score
        .saturating_add(behaviour.delta(cfg))
        .clamp(cfg.floor, cfg.max)
}

/// Whether a score has reached the ban threshold.
pub fn should_ban(score: i32, cfg: &ScoreConfig) -> bool {
    score <= cfg.ban_at_or_below
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_blocks_take_more_than_one_strike_to_ban() {
        let cfg = ScoreConfig::default();
        let mut s = cfg.initial;
        let mut strikes = 0;
        while !should_ban(s, &cfg) {
            s = apply(s, Behaviour::BadBlock, &cfg);
            strikes += 1;
            assert!(strikes < 100, "ban threshold is unreachable");
        }
        assert!(
            strikes >= 2,
            "a single bad block should not ban; took {strikes}"
        );
    }

    #[test]
    fn good_behaviour_recovers_from_a_timeout() {
        let cfg = ScoreConfig::default();
        let after_timeout = apply(cfg.initial, Behaviour::Timeout, &cfg);
        assert!(after_timeout < cfg.initial);
        let mut s = after_timeout;
        for _ in 0..3 {
            s = apply(s, Behaviour::GoodBlock, &cfg);
        }
        assert!(
            s >= cfg.initial,
            "a peer that is useful afterwards should recover: {s} < {}",
            cfg.initial
        );
    }

    #[test]
    fn score_is_clamped_at_both_ends() {
        let cfg = ScoreConfig::default();
        let mut s = cfg.initial;
        for _ in 0..1_000 {
            s = apply(s, Behaviour::GoodBlock, &cfg);
        }
        assert_eq!(s, cfg.max);
        for _ in 0..10_000 {
            s = apply(s, Behaviour::BadBlock, &cfg);
        }
        assert_eq!(s, cfg.floor);
    }

    #[test]
    fn benign_behaviour_can_never_ban_on_its_own() {
        assert!(Behaviour::Duplicate.is_benign());
        assert!(Behaviour::UnservedRange.is_benign());
        assert!(!Behaviour::BadBlock.is_benign());
        assert!(!Behaviour::Timeout.is_benign());
    }

    #[test]
    fn a_pruned_peer_is_barely_penalised() {
        let cfg = ScoreConfig::default();
        assert_eq!(
            Behaviour::UnservedRange.delta(&cfg),
            Behaviour::Duplicate.delta(&cfg)
        );
        assert!(Behaviour::UnservedRange.delta(&cfg) > Behaviour::BadBlock.delta(&cfg));
    }
}
