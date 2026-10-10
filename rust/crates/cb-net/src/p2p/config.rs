//! Peer-to-peer configuration.
//!
//! Every number the gossip layer reasons about lives here, so an operator can
//! retune the network without a rebuild and a reviewer can see the whole policy
//! on one screen. The defaults below are sized for the three-validator cluster
//! in `rust/scripts/supervisord/`, which is small enough that "connect to
//! everybody" is the right answer; they are deliberately conservative rather
//! than tuned for a large network.
//!
//! The types derive `serde::Deserialize` with `#[serde(default)]` on every
//! field, so a TOML file may set one knob and inherit the rest. That is the
//! trade-off taken here: a partial config can never fail to parse, which also
//! means a typo'd key is silently ignored — `deny_unknown_fields` is enabled to
//! claw that back, so a misspelled knob is a startup error rather than a
//! setting that quietly does nothing.

use std::net::SocketAddr;
use std::time::Duration;

/// Wire-protocol version. Bumped when the message set changes incompatibly;
/// a peer advertising a different value is rejected at handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// How a peer's behaviour moves its score, and when a bad score becomes a ban.
///
/// Scores start at [`ScoreConfig::initial`], are clamped to
/// `[floor, max]`, and a peer whose score drops to or below
/// [`ScoreConfig::ban_at_or_below`] is disconnected and refused for
/// [`ScoreConfig::ban`]. Positive behaviour earns score back, so a peer that
/// times out once during a network hiccup recovers by being useful afterwards
/// instead of being banned for a transient fault.
#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoreConfig {
    /// Score a freshly discovered peer starts with.
    pub initial: i32,
    /// Upper clamp, so a long-lived useful peer cannot bank unlimited credit
    /// and then misbehave for free.
    pub max: i32,
    /// Lower clamp. Exists only so the arithmetic cannot wrap.
    pub floor: i32,
    /// Ban threshold: score <= this and the peer is dropped and refused.
    pub ban_at_or_below: i32,
    /// How long a ban lasts. Bans expire; a permanent ban would make a
    /// restarted honest peer unreachable forever after one bad day.
    pub ban: Duration,

    /// Reward for a block body that verified and was adopted.
    pub good_block: i32,
    /// Reward for a transaction that passed static validation.
    pub good_tx: i32,
    /// Reward for answering a `getPeers` with at least one usable address.
    pub good_peers: i32,

    /// Penalty for re-announcing something we already have. Small: honest
    /// peers do this constantly, because gossip is inherently redundant.
    pub duplicate: i32,
    /// Penalty for a request that went unanswered inside the timeout.
    pub timeout: i32,
    /// Penalty for a reply we never asked for.
    pub unsolicited: i32,
    /// Penalty for exceeding a rate or buffer limit.
    pub flood: i32,
    /// Penalty for a malformed or wrongly-signed message.
    pub bad_signature: i32,
    /// Penalty for a block body that failed verification. Large, but sized so
    /// a peer is given more than one chance: `bad_block` must be crossed
    /// repeatedly to reach `ban_at_or_below` from `initial`.
    pub bad_block: i32,
    /// Penalty for speaking out of turn (a payload before the handshake, a
    /// message that violates the state machine).
    pub protocol_violation: i32,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self {
            initial: 100,
            max: 200,
            floor: -1_000,
            ban_at_or_below: 0,
            ban: Duration::from_secs(300),
            good_block: 2,
            good_tx: 1,
            good_peers: 1,
            duplicate: -1,
            timeout: -5,
            unsolicited: -10,
            flood: -25,
            bad_signature: -40,
            // 100 / 40 => the third bad block bans. "Eventually banned", not
            // "banned on the first mistake", which matters because a block can
            // fail verification for reasons that are not the sender's fault
            // (a race against our own head, a partially pruned peer).
            bad_block: -40,
            protocol_violation: -50,
        }
    }
}

/// Everything the gossip engine needs to know about how it should behave.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct P2pConfig {
    /// Address this node listens on for inbound peer connections. `None`
    /// means outbound-only: the node still gossips, but cannot be dialed.
    pub listen: Option<SocketAddr>,
    /// Addresses dialed at startup to bootstrap discovery.
    pub seeds: Vec<SocketAddr>,

    /// Hard cap on simultaneously connected peers. Reached, the node stops
    /// dialing and refuses inbound connections.
    pub max_peers: usize,
    /// Number of outbound connections the node tries to maintain. Kept below
    /// `max_peers` so there is always room for inbound peers behind NAT.
    pub target_outbound: usize,
    /// Hard cap on remembered peer addresses, connected or not. Bounds the
    /// memory an adversary can make us spend by spamming `peers` messages.
    pub max_known_peers: usize,
    /// Most addresses returned in one `peers` reply.
    pub peers_per_response: usize,

    /// Most block bodies served in one `blockRange` reply. Bounds the work a
    /// single request can ask of us.
    pub max_range_blocks: u64,
    /// Whether announced blocks are fetched and applied. A validator sets
    /// this off: its blocks come from its own execution of the committed
    /// sequence, and a body it did not ask for is nothing to fetch.
    pub fetch_blocks: bool,
    /// Most out-of-order block bodies buffered while waiting for the gap
    /// below them to be filled. Overflow scores the offending peer as a flood.
    pub max_pending_blocks: usize,
    /// How many gossip hashes to remember for deduplication, per kind. Older
    /// entries are forgotten first; forgetting one costs a redundant fetch,
    /// never a correctness failure.
    pub seen_cache: usize,

    /// How long a `getBlock` / `getTx` / `getBlockRange` may go unanswered
    /// before the peer is scored down and the request is retried elsewhere.
    pub request_timeout: Duration,
    /// Interval between `ping`s on an otherwise idle connection.
    pub ping_interval: Duration,
    /// A peer silent for this long is dropped.
    pub peer_timeout: Duration,
    /// Interval between `getPeers` rounds.
    pub discovery_interval: Duration,

    /// Most inbound messages accepted from one peer per
    /// [`Self::rate_window`] before it is scored as a flood.
    pub max_messages_per_window: u32,
    /// Width of the rate-limit window.
    pub rate_window: Duration,

    /// Scoring and banning policy.
    pub score: ScoreConfig,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            listen: None,
            seeds: Vec::new(),
            max_peers: 32,
            target_outbound: 8,
            max_known_peers: 512,
            peers_per_response: 16,
            max_range_blocks: 128,
            fetch_blocks: true,
            max_pending_blocks: 256,
            seen_cache: 8_192,
            request_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
            peer_timeout: Duration::from_secs(60),
            discovery_interval: Duration::from_secs(30),
            max_messages_per_window: 2_000,
            rate_window: Duration::from_secs(1),
            score: ScoreConfig::default(),
        }
    }
}

impl P2pConfig {
    /// Parse a TOML document into a config, inheriting defaults for anything
    /// the document does not mention.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let mut cfg: P2pConfig =
            toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.normalize();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reconcile knobs that constrain each other.
    ///
    /// An operator who writes `max_peers = 4` and nothing else means "four
    /// peers", not "a startup error about `target_outbound`, which I did not
    /// set". Clamping the dependent knobs is friendlier than refusing, and it
    /// is still deterministic — `validate` stays strict for configurations
    /// built in code, where an inconsistency is a bug rather than a shorthand.
    pub fn normalize(&mut self) {
        self.target_outbound = self.target_outbound.min(self.max_peers);
        self.max_known_peers = self.max_known_peers.max(self.max_peers);
        self.max_pending_blocks = self.max_pending_blocks.max(self.max_range_blocks as usize);
    }

    /// Load a config from a TOML file on disk.
    pub fn from_toml_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.display().to_string(), e.to_string()))?;
        Self::from_toml_str(&text)
    }

    /// Reject configurations that cannot produce a working node, at startup,
    /// rather than letting them degrade into a silently disconnected node.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_peers == 0 {
            return Err(ConfigError::Invalid("max_peers must be > 0".into()));
        }
        if self.target_outbound > self.max_peers {
            return Err(ConfigError::Invalid(
                "target_outbound must not exceed max_peers".into(),
            ));
        }
        if self.max_known_peers < self.max_peers {
            return Err(ConfigError::Invalid(
                "max_known_peers must be at least max_peers".into(),
            ));
        }
        if self.max_range_blocks == 0 {
            return Err(ConfigError::Invalid("max_range_blocks must be > 0".into()));
        }
        if (self.max_range_blocks as usize) > self.max_pending_blocks {
            return Err(ConfigError::Invalid(
                "max_range_blocks must not exceed max_pending_blocks, else an honest peer's                  range reply overflows the buffer and is scored as a flood"
                    .into(),
            ));
        }
        if self.seen_cache == 0 {
            return Err(ConfigError::Invalid("seen_cache must be > 0".into()));
        }
        if self.score.ban_at_or_below >= self.score.initial {
            return Err(ConfigError::Invalid(
                "score.ban_at_or_below must be below score.initial, else every new peer is \
                 banned on sight"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("reading p2p config {0}: {1}")]
    Io(String, String),
    /// The file was not valid TOML, or had an unknown key.
    #[error("parsing p2p config: {0}")]
    Parse(String),
    /// The values parsed but cannot describe a working node.
    #[error("invalid p2p config: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_toml_inherits_defaults() {
        let cfg = P2pConfig::from_toml_str("max_peers = 4\n").expect("parse");
        assert_eq!(cfg.max_peers, 4);
        assert_eq!(
            cfg.target_outbound, 4,
            "dependent knobs are clamped, not rejected"
        );
        assert_eq!(cfg.score.initial, ScoreConfig::default().initial);
    }

    #[test]
    fn unknown_key_is_a_startup_error() {
        let err = P2pConfig::from_toml_str("max_peerz = 4\n").expect_err("should reject");
        assert!(matches!(err, ConfigError::Parse(_)), "{err}");
    }

    #[test]
    fn durations_parse_from_toml() {
        let cfg = P2pConfig::from_toml_str(
            "request_timeout = { secs = 2, nanos = 0 }\n\
             [score]\n\
             ban = { secs = 30, nanos = 0 }\n",
        )
        .expect("parse");
        assert_eq!(cfg.request_timeout, Duration::from_secs(2));
        assert_eq!(cfg.score.ban, Duration::from_secs(30));
    }

    #[test]
    fn a_config_that_bans_every_new_peer_is_rejected() {
        let err = P2pConfig::from_toml_str("[score]\nban_at_or_below = 100\n")
            .expect_err("should reject");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
    }
}
