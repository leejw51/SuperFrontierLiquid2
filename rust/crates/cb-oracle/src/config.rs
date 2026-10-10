//! The command line, and the one number that needed an argument: the cadence.
//!
//! # How often this has to publish, and why
//!
//! It is tempting to read `cb-engine/src/oracle.rs` looking for a freshness
//! window and to size the period against it. There isn't one. That module
//! enforces exactly two rules — the sender must be the market's registered
//! authority, and `submitted_height` must not be *strictly older* than the
//! stored `source_height`. Nothing on the consensus path expires an oracle
//! price, and nothing refuses to trade against a stale one. So the engine
//! never tells the oracle to hurry; a price posted an hour ago is still the
//! price every margin check uses. That is the reason this crate exists, and it
//! is also why the cadence is a judgement rather than a derivation.
//!
//! Four constraints set it:
//!
//! 1. **A mark price is only recomputed when an `OracleUpdate` lands.**
//!    `cb-block::executor::apply_oracle_update` is the sole writer of
//!    `MarkPrice`. The publish period is therefore the exact worst-case age of
//!    every mark price on the venue, and mark is what liquidation prices a
//!    position at.
//! 2. **Blocks are 100 ms** on the shipped configuration (`INTERVAL_MS` in
//!    `rust/Makefile`). A one-second period means a mark is at most ~10 blocks
//!    behind the price the simulator believes in — close enough that a trader
//!    watching the tape and the liquidation engine agree about what happened.
//! 3. **Funding ticks every 3600 blocks** (~6 minutes at 100 ms), and the
//!    premium EMA it consumes is only advanced on an oracle update. One second
//!    gives ~360 premium samples per funding interval; a ten-second period
//!    would give 36, and the EMA would be dominated by whichever handful of
//!    samples happened to land.
//! 4. **`cb-feed` calibrates its per-step volatility against one second**
//!    ([`cb_feed::DEFAULT_TICK`]). Publishing on a one-second period and
//!    stepping the feed once per publish keeps the simulated annual volatility
//!    honest; stepping a 1 s-calibrated model 10x a second would inflate
//!    realised volatility by √10 without anyone noticing.
//!
//! Hence [`DEFAULT_PERIOD_MS`] = 1000. `--period-ms` moves it, and the feed's
//! tick width moves with it (see [`OracleConfig::feed_tick`]) so constraint 4
//! keeps holding at any period.
//!
//! The cost of this cadence is 11 transactions per second on the shipped
//! genesis — about 0.1% of the node's measured throughput, and it is the
//! reason the period is not set to 100 ms "to be safe".

use anyhow::{anyhow, Context, Result};
use cb_crypto::SecretKey;
use cb_feed::Scenario;
use cb_types::Address;
use std::net::SocketAddr;
use std::time::Duration;

/// Publish period when none is given. See the module header for the argument.
pub const DEFAULT_PERIOD_MS: u64 = 1_000;

/// How many publish periods a price may age before this oracle refuses to
/// republish it.
///
/// Three, not one: a single missed feed step is a hiccup, and halting the
/// whole venue's mark price over one late tick would be its own outage. Three
/// consecutive misses is a source that has stopped, which is the case worth
/// refusing on.
pub const STALENESS_PERIODS: u32 = 3;

/// Reconnect backoff floor.
pub const DEFAULT_BACKOFF_MIN_MS: u64 = 250;
/// Reconnect backoff ceiling. A node being rebuilt is back within seconds;
/// waiting minutes to notice would make `make up` feel broken.
pub const DEFAULT_BACKOFF_MAX_MS: u64 = 5_000;
/// Per-RPC deadline. A dead TCP connection does not always fail a Cap'n Proto
/// promise promptly, and an oracle parked forever on one await is the failure
/// mode this whole crate exists to avoid.
pub const DEFAULT_RPC_TIMEOUT_MS: u64 = 5_000;

/// Publish oracle prices for every derivative market on a SuperFrontierLiquid
/// node.
#[derive(Debug, Clone, clap::Parser)]
#[command(name = "cb-oracle", version, about, long_about = None)]
pub struct Cli {
    /// Node Cap'n Proto RPC address.
    #[arg(long, default_value = "127.0.0.1:7700")]
    pub rpc: String,

    /// Oracle authority secret key, `0x`-prefixed or bare hex.
    ///
    /// Read from the environment by default so it never appears in `ps`.
    #[arg(long, env = "ORACLE_AUTHORITY_KEY", hide_env_values = true)]
    pub authority_key: String,

    /// Publish period in milliseconds. See the module docs for the default.
    #[arg(long, default_value_t = DEFAULT_PERIOD_MS)]
    pub period_ms: u64,

    /// Master seed for the simulated price paths. The same seed replays the
    /// same market, which is what makes a liquidation reproducible.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Opening scenario: `calm`, `trend_up`, `trend_down`, `crash`, `squeeze`
    /// or `chop`.
    #[arg(long, default_value = "calm")]
    pub scenario: String,

    /// Refuse to publish a price older than this many milliseconds. Defaults
    /// to [`STALENESS_PERIODS`] times the publish period.
    #[arg(long)]
    pub max_price_age_ms: Option<u64>,

    /// Serve the scenario-control API on this address. Off unless given.
    #[arg(long)]
    pub control_listen: Option<SocketAddr>,

    /// Chain id to sign with. Read from the node's `chainInfo` when omitted,
    /// which is what you want unless you are deliberately testing replay
    /// rejection.
    #[arg(long)]
    pub chain_id: Option<u32>,

    /// Stop after this many publish ticks. For tests and demos; unset means
    /// run until killed.
    #[arg(long)]
    pub max_ticks: Option<u64>,

    /// Per-RPC deadline in milliseconds.
    #[arg(long, default_value_t = DEFAULT_RPC_TIMEOUT_MS)]
    pub rpc_timeout_ms: u64,
}

/// A validated [`Cli`]: parsed key, resolved address, resolved durations.
#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// Where the node listens.
    pub rpc: SocketAddr,
    /// The signing key.
    pub key: SecretKey,
    /// The address that key controls, which must match each market's
    /// `oracle_authority`.
    pub authority: Address,
    /// How often to publish.
    pub period: Duration,
    /// Master seed for the price paths.
    pub seed: u64,
    /// Opening scenario.
    pub scenario: Scenario,
    /// Oldest price this oracle will publish.
    pub max_price_age: Duration,
    /// Scenario-control listen address, when enabled.
    pub control_listen: Option<SocketAddr>,
    /// Chain id override, or `None` to read it from the node.
    pub chain_id: Option<u32>,
    /// Tick budget, or `None` for unbounded.
    pub max_ticks: Option<u64>,
    /// Per-RPC deadline.
    pub rpc_timeout: Duration,
}

impl OracleConfig {
    /// Validate a parsed command line.
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let rpc: SocketAddr = cli
            .rpc
            .parse()
            .with_context(|| format!("parse --rpc `{}` as host:port", cli.rpc))?;

        let key = parse_secret_key(&cli.authority_key)?;
        let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key);
        let authority = cb_crypto::address_from_pubkey(&public);

        if cli.period_ms == 0 {
            return Err(anyhow!("--period-ms must be greater than zero"));
        }
        let period = Duration::from_millis(cli.period_ms);

        let scenario = Scenario::parse(&cli.scenario).ok_or_else(|| {
            anyhow!(
                "unknown --scenario `{}` (expected one of: {})",
                cli.scenario,
                Scenario::ALL
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let max_price_age = match cli.max_price_age_ms {
            Some(ms) => Duration::from_millis(ms),
            None => period * STALENESS_PERIODS,
        };

        Ok(Self {
            rpc,
            key,
            authority,
            period,
            seed: cli.seed,
            scenario,
            max_price_age,
            control_listen: cli.control_listen,
            chain_id: cli.chain_id,
            max_ticks: cli.max_ticks,
            rpc_timeout: Duration::from_millis(cli.rpc_timeout_ms),
        })
    }

    /// Simulated time per feed step.
    ///
    /// One step per publish, so the feed's volatility calibration and the
    /// publish cadence stay the same number. See the module header, point 4.
    pub fn feed_tick(&self) -> Duration {
        self.period
    }
}

/// Parse a `0x`-prefixed or bare 32-byte hex secret key.
///
/// Errors name the problem without echoing the key: a secret in a log line is
/// a secret in a log file.
pub fn parse_secret_key(s: &str) -> Result<SecretKey> {
    let body = s.trim();
    let body = body.strip_prefix("0x").unwrap_or(body);
    let bytes = hex::decode(body).map_err(|_| {
        anyhow!("the oracle authority key is not valid hex (expected 64 hex characters)")
    })?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "the oracle authority key must be 32 bytes, got {}",
            bytes.len()
        ));
    }
    SecretKey::from_slice(&bytes)
        .map_err(|_| anyhow!("the oracle authority key is not a valid secp256k1 secret"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// The key published in `rust/.env.example` for `ORACLE_AUTHORITY`, and
    /// the address it is supposed to control. Both are dev values committed on
    /// purpose; nothing of value may ever be held by them.
    const EXAMPLE_KEY: &str = "0x943f7fb2f31e4f9a82252e317c82fb35408191017f64100b7ce1485d21e56730";
    const EXAMPLE_ADDR: &str = "0x79c1bef33668fb267546509399179d3dd98b45da";

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["cb-oracle", "--authority-key", EXAMPLE_KEY];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn env_example_key_controls_the_env_example_authority() {
        // If this ever fails, `make oracle` posts updates the executor
        // rejects with `notAuthorized` and every derivative market stays dead.
        let cfg = OracleConfig::from_cli(cli(&[])).expect("config");
        assert_eq!(format!("0x{}", hex::encode(cfg.authority.0)), EXAMPLE_ADDR);
    }

    #[test]
    fn default_period_is_one_second_and_drives_the_feed_tick() {
        let cfg = OracleConfig::from_cli(cli(&[])).expect("config");
        assert_eq!(cfg.period, Duration::from_millis(DEFAULT_PERIOD_MS));
        assert_eq!(cfg.feed_tick(), cfg.period);
    }

    #[test]
    fn staleness_defaults_to_three_periods_and_follows_the_period() {
        let cfg = OracleConfig::from_cli(cli(&["--period-ms", "250"])).expect("config");
        assert_eq!(cfg.max_price_age, Duration::from_millis(750));
        let cfg = OracleConfig::from_cli(cli(&["--max-price-age-ms", "10"])).expect("config");
        assert_eq!(cfg.max_price_age, Duration::from_millis(10));
    }

    #[test]
    fn zero_period_is_refused() {
        assert!(OracleConfig::from_cli(cli(&["--period-ms", "0"])).is_err());
    }

    #[test]
    fn every_scenario_name_parses_and_an_unknown_one_lists_the_choices() {
        for s in Scenario::ALL {
            let cfg = OracleConfig::from_cli(cli(&["--scenario", s.as_str()])).expect("config");
            assert_eq!(cfg.scenario, s);
        }
        let err = OracleConfig::from_cli(cli(&["--scenario", "moon"]))
            .expect_err("unknown scenario")
            .to_string();
        assert!(err.contains("crash"), "{err}");
    }

    #[test]
    fn a_bad_key_is_refused_without_echoing_it() {
        let err = parse_secret_key("0xdeadbeef").expect_err("too short");
        assert!(err.to_string().contains("32 bytes"));
        assert!(!err.to_string().contains("deadbeef"));
    }
}
