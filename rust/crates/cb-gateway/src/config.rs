//! Command-line configuration.
//!
//! Every knob the gateway has is here, and nothing here is read anywhere but
//! `main.rs`. That is the composition-root rule in practice: a handler that
//! wanted a configuration value would have to be given it, which is exactly
//! the pressure that keeps configuration from leaking into business logic.

use clap::{Parser, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Which application-storage backend to wire in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StoreKind {
    /// Volatile, in-process. Fast to start and loses everything on restart.
    Memory,
    /// SQLite on disk.
    Sqlite,
}

/// The browser-facing gateway.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "cb-gateway",
    about = "SuperFrontierLiquid browser-facing gateway"
)]
pub struct Config {
    /// Address to serve HTTP and WebSocket on.
    #[arg(long, default_value = "127.0.0.1:8080", env = "CB_GATEWAY_LISTEN")]
    pub listen: SocketAddr,

    /// The sequencer's Cap'n Proto RPC address.
    #[arg(long, default_value = "127.0.0.1:9000", env = "CB_GATEWAY_NODE_RPC")]
    pub node_rpc: SocketAddr,

    /// Which application store to use.
    #[arg(long, value_enum, default_value_t = StoreKind::Memory, env = "CB_GATEWAY_STORE")]
    pub store: StoreKind,

    /// Path to the SQLite database, when `--store sqlite`.
    #[arg(
        long,
        default_value = "cb-gateway.sqlite",
        env = "CB_GATEWAY_SQLITE_PATH"
    )]
    pub sqlite_path: PathBuf,

    /// Browser origins allowed to call the API. Repeat the flag for several;
    /// `*` allows any, which is appropriate for a local simulator and for
    /// nothing else.
    #[arg(
        long = "cors-origin",
        env = "CB_GATEWAY_CORS_ORIGINS",
        value_delimiter = ','
    )]
    pub cors_origins: Vec<String>,

    /// The secret every account's trading key is derived from.
    ///
    /// A trading key is `HKDF-SHA256(server_secret, login_address)` and is
    /// never stored. **Rotating this value orphans every trading account**:
    /// the same login address would derive a different on-chain account, with
    /// none of the old one's money. There is deliberately no default.
    #[arg(long, env = "CB_GATEWAY_SERVER_SECRET")]
    pub server_secret: String,

    /// Start even though the server secret no longer derives the trading
    /// accounts this deployment has been using.
    ///
    /// The gateway records a fingerprint of the derivation the first time it
    /// starts and refuses to start when that fingerprint changes, because the
    /// change is silent and total: every existing account's money stays on
    /// chain at an address nothing can reach any more, `GET /account` answers
    /// `200` with an empty balance, and every resting order becomes
    /// uncancellable. Pass this flag only when the orphaning is intended —
    /// a scratch chain, a fresh database — and read the warning it prints.
    #[arg(
        long,
        env = "CB_GATEWAY_ALLOW_SECRET_ROTATION",
        default_value_t = false
    )]
    pub allow_server_secret_rotation: bool,

    /// Mount `POST /auth/dev-login`, which accepts a mnemonic over the wire.
    ///
    /// Off by default, and refused when the listen address is not loopback.
    /// It exists so the bot simulator and the integration suite can sign in
    /// without implementing client-side signing.
    #[arg(long, default_value_t = false, env = "CB_GATEWAY_DEV_LOGIN")]
    pub dev_login: bool,

    /// Login addresses that hold the operator role. Repeat the flag for
    /// several, or pass a comma-separated list.
    ///
    /// This is the *supported* way to name an operator. An address is
    /// cryptographically controlled: holding it means holding a key. A handle
    /// is a self-chosen string that the first account to log in can claim, so
    /// binding the role to one hands the whole admin surface to whoever
    /// registers first.
    #[arg(
        long = "admin-address",
        env = "CB_GATEWAY_ADMIN_ADDRESSES",
        value_delimiter = ','
    )]
    pub admin_addresses: Vec<String>,

    /// Handles that hold the operator role. Repeat the flag for several.
    ///
    /// **Refused unless `--insecure-admin-by-handle` is also passed**, because
    /// handles are first-come and self-chosen; see
    /// [`ConfigError::AdminHandleWithoutOptIn`].
    #[arg(
        long = "admin-handle",
        env = "CB_GATEWAY_ADMIN_HANDLES",
        value_delimiter = ','
    )]
    pub admin_handles: Vec<String>,

    /// Let `--admin-handle` take effect.
    ///
    /// The convenience this buys is real — the integration suite and the local
    /// `make gateway` workflow sign in with a mnemonic and pick the handle
    /// `admin` — and it is why the flag exists at all rather than the handle
    /// path simply being deleted. The cost is that on a fresh store the
    /// operator role belongs to whoever logs in first. Never pass it anywhere
    /// a stranger can reach the gateway.
    #[arg(
        long,
        default_value_t = false,
        env = "CB_GATEWAY_INSECURE_ADMIN_BY_HANDLE"
    )]
    pub insecure_admin_by_handle: bool,

    /// Hex-encoded secp256k1 secret key of the chain's treasury signer, which
    /// is the only account allowed to mint via `Deposit`.
    #[arg(long, env = "CB_GATEWAY_FAUCET_TREASURY_KEY")]
    pub faucet_treasury_key: String,

    /// Seconds between faucet claims.
    #[arg(long, default_value_t = 3_600, env = "CB_GATEWAY_FAUCET_COOLDOWN")]
    pub faucet_cooldown_seconds: u64,

    /// What one claim grants, as `ASSET:AMOUNT` pairs such as
    /// `USD:100000,BTC:1`.
    #[arg(
        long,
        default_value = "USD:100000,BTC:1",
        env = "CB_GATEWAY_FAUCET_GRANT",
        value_delimiter = ','
    )]
    pub faucet_grant: Vec<String>,

    /// How many faucet claims one account may ever make. Unlimited by default.
    #[arg(long, env = "CB_GATEWAY_FAUCET_MAX_CLAIMS")]
    pub faucet_max_claims: Option<u64>,

    /// How long a session stays valid, in hours.
    #[arg(long, default_value_t = 24, env = "CB_GATEWAY_SESSION_TTL_HOURS")]
    pub session_ttl_hours: i64,

    /// Assumed block interval, used to turn block heights into timestamps.
    #[arg(long, default_value_t = 100, env = "CB_GATEWAY_BLOCK_TIME_MS")]
    pub block_time_ms: u64,

    /// How long `POST /orders` waits for the block carrying the order before
    /// answering `pending`.
    #[arg(long, default_value_t = 2_000, env = "CB_GATEWAY_ORDER_CONFIRM_MS")]
    pub order_confirm_ms: u64,

    /// Mutating requests allowed per session, per minute.
    #[arg(long, default_value_t = 120, env = "CB_GATEWAY_RATE_PER_SESSION")]
    pub rate_limit_per_session: u32,

    /// Mutating requests allowed per client address, per minute.
    #[arg(long, default_value_t = 300, env = "CB_GATEWAY_RATE_PER_IP")]
    pub rate_limit_per_ip: u32,

    /// Peer addresses whose `x-forwarded-for` header the gateway believes.
    ///
    /// Empty by default: the header is ignored and the real TCP peer is used
    /// for rate limiting and session attribution. Name the reverse proxy here
    /// when there is one, and only then.
    #[arg(
        long = "trusted-proxy",
        env = "CB_GATEWAY_TRUSTED_PROXIES",
        value_delimiter = ','
    )]
    pub trusted_proxies: Vec<String>,

    /// Messages that may queue for one WebSocket client before it is dropped.
    #[arg(long, default_value_t = 256, env = "CB_GATEWAY_WS_BUFFER")]
    pub ws_send_buffer: usize,

    /// Seconds a WebSocket connection may stay silent before it is closed.
    #[arg(long, default_value_t = 90, env = "CB_GATEWAY_WS_IDLE_SECONDS")]
    pub ws_idle_seconds: u64,

    /// Seconds between leaderboard recomputations.
    #[arg(long, default_value_t = 60, env = "CB_GATEWAY_LEADERBOARD_INTERVAL")]
    pub leaderboard_interval_seconds: u64,

    /// Seconds between market-catalogue refreshes.
    #[arg(long, default_value_t = 30, env = "CB_GATEWAY_MARKET_REFRESH")]
    pub market_refresh_seconds: u64,

    /// Block height to begin streaming from. `1` replays the chain, which is
    /// what gives a fresh gateway its candle history.
    #[arg(long, default_value_t = 1, env = "CB_GATEWAY_START_HEIGHT")]
    pub start_height: u64,

    /// The `cb-sim` binary the admin page's simulation switch launches.
    ///
    /// Absent, the gateway attaches no harness: `GET /admin/sim` reports the
    /// switch off and `POST` refuses to turn it on. This is a simulator's
    /// control, so it has to be asked for.
    #[arg(long, env = "CB_GATEWAY_SIM_BIN")]
    pub sim_bin: Option<PathBuf>,

    /// The position of the simulation switch at start-up: `off`, `on` or
    /// `max`. Applied once the sequencer link is up.
    #[arg(long, default_value = "off", env = "CB_GATEWAY_SIM_MODE")]
    pub sim_mode: String,

    /// Where the simulation workers write their logs.
    #[arg(long, default_value = "data", env = "CB_GATEWAY_SIM_LOG_DIR")]
    pub sim_log_dir: PathBuf,

    /// Seed the first simulation worker signs with.
    #[arg(long, default_value_t = 0xC0FFEE, env = "CB_GATEWAY_SIM_SEED")]
    pub sim_seed: u64,

    /// The oracle's scenario-control URL, such as `http://127.0.0.1:8171`.
    /// A scenario change on `/admin/sim` is forwarded there, so the operator's
    /// choice reprices every derivative market rather than being remembered
    /// in one process.
    #[arg(long, env = "CB_GATEWAY_ORACLE_CONTROL")]
    pub oracle_control: Option<String>,

    /// Seed for the simulated market data feed, so a deployment's prices are
    /// reproducible.
    #[arg(long, default_value_t = 42, env = "CB_GATEWAY_FEED_SEED")]
    pub feed_seed: u64,

    /// Log filter, in `tracing_subscriber` syntax.
    #[arg(long, default_value = "info,cb_gateway=debug", env = "RUST_LOG")]
    pub log_filter: String,
}

/// Why a configuration value could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The treasury key is not 32 bytes of hex, or not a valid scalar.
    #[error("faucet treasury key: {0}")]
    TreasuryKey(String),
    /// A `--trusted-proxy` value is not an IP address.
    #[error("--trusted-proxy `{0}` is not an IP address")]
    TrustedProxy(String),
    /// A faucet grant could not be parsed.
    #[error("faucet grant `{0}`: {1}")]
    Grant(String, String),
    /// `--sim-mode` is not one of the three positions.
    #[error("--sim-mode `{0}` is not one of off, on, max")]
    SimMode(String),
    /// `--dev-login` was asked for on a non-loopback bind.
    #[error(
        "--dev-login accepts a mnemonic over the wire and will not be mounted on {0}; \
         bind to a loopback address or drop the flag"
    )]
    DevLoginExposed(String),
    /// An `--admin-address` is not a valid, checksummed address.
    #[error("--admin-address `{0}`: {1}")]
    AdminAddress(String, String),
    /// `--admin-handle` (or `CB_GATEWAY_ADMIN_HANDLES`) was configured without
    /// the explicit opt-in.
    #[error(
        "--admin-handle (or CB_GATEWAY_ADMIN_HANDLES) binds the operator role to a self-chosen \
         handle, and handles are first-come: on a fresh store whoever logs in first as `{0}` \
         becomes the operator. Use --admin-address 0x<login address> instead, which is bound to \
         a key nobody else holds. If you really want the handle behaviour for a local run, pass \
         --insecure-admin-by-handle as well"
    )]
    AdminHandleWithoutOptIn(String),
}

/// Who holds the operator role, resolved from configuration.
///
/// Addresses are the supported binding; handles survive only behind
/// [`Config::insecure_admin_by_handle`].
#[derive(Debug, Clone, Default)]
pub struct AdminPolicy {
    /// Login addresses that hold the operator role.
    pub addresses: Vec<cb_types::Address>,
    /// Handles that hold it, which is empty unless the opt-in was passed.
    pub handles: Vec<String>,
}

impl AdminPolicy {
    /// Whether the handle path is in use, which is worth a loud warning.
    pub fn uses_handles(&self) -> bool {
        !self.handles.is_empty()
    }
}

impl Config {
    /// Whether the listen address is loopback.
    ///
    /// The development login is refused off loopback, because sending a
    /// mnemonic to a server is exactly what the real flow avoids and the
    /// affordance is meant to be hard to turn on by accident.
    pub fn is_loopback(&self) -> bool {
        self.listen.ip().is_loopback()
    }

    /// Whether the development login may be mounted, and why not if not.
    /// The simulation switch's start-up position.
    pub fn initial_sim_mode(&self) -> Result<crate::sim::SimMode, ConfigError> {
        crate::sim::SimMode::parse(&self.sim_mode)
            .ok_or_else(|| ConfigError::SimMode(self.sim_mode.clone()))
    }

    /// Whether the development login may be mounted: asked for, and bound to
    /// a loopback address.
    pub fn dev_login_allowed(&self) -> Result<bool, ConfigError> {
        if !self.dev_login {
            return Ok(false);
        }
        if !self.is_loopback() {
            return Err(ConfigError::DevLoginExposed(self.listen.to_string()));
        }
        Ok(true)
    }

    /// Resolve who holds the operator role.
    ///
    /// Refuses at startup — rather than silently dropping the role — when
    /// handles were configured without the opt-in, so the failure is a message
    /// naming the flag to use and not an admin surface nobody can reach.
    pub fn admin_policy(&self) -> Result<AdminPolicy, ConfigError> {
        let mut addresses = Vec::with_capacity(self.admin_addresses.len());
        for raw in &self.admin_addresses {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let address = crate::auth::eth::parse_checked_address(raw)
                .map_err(|e| ConfigError::AdminAddress(raw.to_string(), e.to_string()))?;
            addresses.push(address);
        }

        let handles: Vec<String> = self
            .admin_handles
            .iter()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .collect();
        if !handles.is_empty() && !self.insecure_admin_by_handle {
            return Err(ConfigError::AdminHandleWithoutOptIn(handles.join(", ")));
        }

        Ok(AdminPolicy { addresses, handles })
    }

    /// The proxies whose forwarded headers may be believed.
    pub fn trusted_proxy_set(
        &self,
    ) -> Result<std::collections::HashSet<std::net::IpAddr>, ConfigError> {
        let mut set = std::collections::HashSet::new();
        for raw in &self.trusted_proxies {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let ip = raw
                .parse::<std::net::IpAddr>()
                .map_err(|_| ConfigError::TrustedProxy(raw.to_string()))?;
            set.insert(ip);
        }
        Ok(set)
    }

    /// Decode the treasury signing key.
    pub fn treasury_key(&self) -> Result<cb_crypto::SecretKey, ConfigError> {
        let hex = self.faucet_treasury_key.trim().trim_start_matches("0x");
        let bytes = hex::decode(hex).map_err(|e| ConfigError::TreasuryKey(e.to_string()))?;
        cb_crypto::SecretKey::from_slice(&bytes)
            .map_err(|e| ConfigError::TreasuryKey(e.to_string()))
    }

    /// Parse `ASSET:AMOUNT` pairs into the faucet's grant list.
    pub fn faucet_grants(&self) -> Result<Vec<crate::faucet::Grant>, ConfigError> {
        let mut grants = Vec::with_capacity(self.faucet_grant.len());
        for raw in &self.faucet_grant {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (asset, amount) = raw
                .split_once(':')
                .ok_or_else(|| ConfigError::Grant(raw.into(), "expected ASSET:AMOUNT".into()))?;
            let asset = crate::symbols::parse_asset(asset).ok_or_else(|| {
                ConfigError::Grant(raw.into(), format!("unknown asset `{asset}`"))
            })?;
            let amount = crate::decimal::parse_scaled(amount, asset.decimals() as u32)
                .map_err(|e| ConfigError::Grant(raw.into(), e.to_string()))?;
            grants.push(crate::faucet::Grant { asset, amount });
        }
        Ok(grants)
    }

    /// The faucet configuration this `Config` describes.
    pub fn faucet_config(&self) -> Result<crate::faucet::FaucetConfig, ConfigError> {
        Ok(crate::faucet::FaucetConfig {
            enabled: true,
            cooldown_seconds: self.faucet_cooldown_seconds,
            grants: self.faucet_grants()?,
            max_claims_per_user: self.faucet_max_claims,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::AssetId;

    /// Build a config from flags, supplying the two required ones only when
    /// the caller has not overridden them (clap rejects a repeated flag).
    fn parse(extra: &[&str]) -> Config {
        let mut args = vec!["cb-gateway"];
        if !extra.contains(&"--server-secret") {
            args.extend_from_slice(&["--server-secret", "test"]);
        }
        if !extra.contains(&"--faucet-treasury-key") {
            args.extend_from_slice(&[
                "--faucet-treasury-key",
                "0101010101010101010101010101010101010101010101010101010101010101",
            ]);
        }
        args.extend_from_slice(extra);
        Config::try_parse_from(args).expect("parse")
    }

    #[test]
    fn the_defaults_are_a_working_local_simulator() {
        let config = parse(&[]);
        assert_eq!(config.listen.port(), 8080);
        assert_eq!(config.node_rpc.port(), 9000);
        assert_eq!(config.store, StoreKind::Memory);
        assert_eq!(config.faucet_cooldown_seconds, 3_600);
        assert_eq!(config.start_height, 1, "there is no block zero");
    }

    #[test]
    fn the_treasury_key_decodes_with_or_without_the_prefix() {
        assert!(parse(&[]).treasury_key().is_ok());
        let prefixed = parse(&[
            "--faucet-treasury-key",
            "0x0202020202020202020202020202020202020202020202020202020202020202",
        ]);
        assert!(prefixed.treasury_key().is_ok());
    }

    #[test]
    fn a_malformed_treasury_key_is_rejected_at_startup() {
        let bad = parse(&["--faucet-treasury-key", "not-hex"]);
        assert!(matches!(
            bad.treasury_key(),
            Err(ConfigError::TreasuryKey(_))
        ));
        let short = parse(&["--faucet-treasury-key", "0011"]);
        assert!(short.treasury_key().is_err());
    }

    #[test]
    fn faucet_grants_parse_into_chain_units() {
        let config = parse(&["--faucet-grant", "USD:100000,BTC:1.5"]);
        let grants = config.faucet_grants().expect("parse");
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].asset, AssetId::Usd);
        assert_eq!(grants[0].amount, 100_000_000_000);
        assert_eq!(grants[1].asset, AssetId::Btc);
        assert_eq!(grants[1].amount, 150_000_000);
    }

    #[test]
    fn a_grant_finer_than_the_asset_is_a_startup_error_not_a_truncation() {
        let config = parse(&["--faucet-grant", "USD:1.0000001"]);
        assert!(matches!(
            config.faucet_grants(),
            Err(ConfigError::Grant(..))
        ));
    }

    #[test]
    fn a_grant_naming_an_unknown_asset_is_rejected() {
        let config = parse(&["--faucet-grant", "DOGE:1"]);
        assert!(config.faucet_grants().is_err());
        let malformed = parse(&["--faucet-grant", "USD"]);
        assert!(malformed.faucet_grants().is_err());
    }

    #[test]
    fn the_development_login_is_off_by_default() {
        assert!(!parse(&[]).dev_login);
        assert!(!parse(&[]).dev_login_allowed().expect("allowed"));
    }

    #[test]
    fn the_development_login_is_refused_on_a_non_loopback_bind() {
        let exposed = parse(&["--dev-login", "--listen", "0.0.0.0:8080"]);
        assert!(matches!(
            exposed.dev_login_allowed(),
            Err(ConfigError::DevLoginExposed(_))
        ));

        let local = parse(&["--dev-login", "--listen", "127.0.0.1:8080"]);
        assert!(local.dev_login_allowed().expect("loopback is fine"));
    }

    #[test]
    fn admin_handles_accept_a_comma_list() {
        let config = parse(&[
            "--admin-handle",
            "operator,second_operator",
            "--insecure-admin-by-handle",
        ]);
        assert_eq!(config.admin_handles.len(), 2);
    }

    #[test]
    fn an_admin_handle_without_the_opt_in_is_a_startup_error() {
        // The whole point of the refusal: the role must not be silently lost,
        // and the message has to name the flag that replaces it.
        let config = parse(&["--admin-handle", "admin"]);
        let error = config.admin_policy().expect_err("handles need the opt-in");
        assert!(matches!(error, ConfigError::AdminHandleWithoutOptIn(_)));
        let text = error.to_string();
        assert!(text.contains("--admin-address"), "{text}");
        assert!(text.contains("--insecure-admin-by-handle"), "{text}");
    }

    #[test]
    fn an_admin_handle_with_the_opt_in_still_works() {
        let config = parse(&["--admin-handle", "admin", "--insecure-admin-by-handle"]);
        let policy = config.admin_policy().expect("opted in");
        assert_eq!(policy.handles, vec!["admin".to_string()]);
        assert!(policy.uses_handles());
        assert!(policy.addresses.is_empty());
    }

    #[test]
    fn admin_addresses_need_no_opt_in_and_are_checksum_validated() {
        let config = parse(&[
            "--admin-address",
            "0x9858EfFD232B4033E47d90003D41EC34EcaEda94",
        ]);
        let policy = config
            .admin_policy()
            .expect("addresses are the supported path");
        assert_eq!(policy.addresses.len(), 1);
        assert!(!policy.uses_handles());

        // A mixed-case address whose EIP-55 checksum does not validate is a
        // typo, and an operator address that is off by one nibble would grant
        // the role to nobody.
        let typo = parse(&[
            "--admin-address",
            "0x9858effd232B4033E47d90003D41EC34EcaEda94",
        ]);
        assert!(matches!(
            typo.admin_policy(),
            Err(ConfigError::AdminAddress(..))
        ));
    }

    #[test]
    fn no_admin_configuration_at_all_is_allowed() {
        let policy = parse(&[]).admin_policy().expect("no operator is a choice");
        assert!(policy.addresses.is_empty());
        assert!(policy.handles.is_empty());
    }

    #[test]
    fn no_proxy_is_trusted_by_default_and_a_bad_one_is_a_startup_error() {
        assert!(parse(&[])
            .trusted_proxy_set()
            .expect("empty is fine")
            .is_empty());
        let configured = parse(&["--trusted-proxy", "10.0.0.1,::1"]);
        assert_eq!(configured.trusted_proxy_set().expect("parse").len(), 2);
        assert!(matches!(
            parse(&["--trusted-proxy", "not-an-ip"]).trusted_proxy_set(),
            Err(ConfigError::TrustedProxy(_))
        ));
    }

    #[test]
    fn cors_origins_accept_a_repeated_flag_and_a_comma_list() {
        let config = parse(&[
            "--cors-origin",
            "http://localhost:5173,http://localhost:3000",
        ]);
        assert_eq!(config.cors_origins.len(), 2);
    }
}
