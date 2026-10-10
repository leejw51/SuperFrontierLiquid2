//! JSONL genesis loader.
//!
//! Each non-blank, non-comment line in the genesis file is a single JSON record
//! tagged with a `kind` discriminator. Order is significant for `validator` and
//! `market` records (validators are leader-rotated by their genesis index).
//!
//! ## Environment-variable substitution
//!
//! Before JSON parsing, every `${VAR}` token in the file is replaced with the
//! value of the corresponding environment variable. A reference to an unset
//! variable is a hard error (no silent empty-string substitution). Use this
//! to keep operator-owned addresses (e.g. validator wallets) out of the
//! committed genesis file:
//!
//! ```jsonl
//! {"kind":"validator","seed":"v0","wallet":"${MYWALLET0}","weight":1,"initial_amount":"1000"}
//! ```
//!
//! Substitution is purely textual and runs once over the whole file. `${` with
//! no matching `}` is rejected.
//!
//! Supported records:
//!
//! - `{"kind":"chain","chain_id":<u32>,"denom":"ULD"?}`
//!   `denom` (optional, default `"USD"`) is the user-facing denomination name
//!   persisted in `Meta` for downstream display layers. Internal AssetId stays
//!   `Btc`/`Eth`/`Usd`; this string is metadata only.
//! - `{"kind":"validator","seed":<str>,"weight":<u64>,"initial_amount":<str>,"wallet":"0x.."?}`
//!   or
//!   `{"kind":"validator","addr":"0x..","pubkey":"0x..","weight":<u64>,"initial_amount":<str>,"wallet":"0x.."?}`
//!   `initial_amount` is the validator's starting USD balance (decimal string).
//!   `wallet` (optional) — when present, `initial_amount` is credited to this
//!   address instead of the validator's own derived address. Lets an operator
//!   run the validator key but hold funds in a separate wallet.
//! - `{"kind":"market","id":<str>,"base":<str>,"quote":<str>,"tick_size":<u128>,"min_qty":<u128>,"fee_bps":<u16>}`
//! - `{"kind":"company","address":"0x..","balances":{<asset>:<u128 string>,...}}`
//! - `{"kind":"community_wallet","address":"0x..","balances":{<asset>:<u128 string>,...}}`
//! - `{"kind":"sim","num_traders":<u32>,"seed_prefix":<str>,"balances":{<asset>:<u128 string>,...}}`
//! - `{"kind":"margin_account","address":"0x..","collateral":<u128 string>}`
//!   Pre-seeds a per-owner USD margin pool (1e6-base units, same units as
//!   `MarginAccount::collateral`). Required for demo traders on a fresh chain
//!   because `AdjustMargin` is `Unsupported` in v1 — without genesis seeding
//!   no address can place a perp order. Distinct from spot USD: spot balance
//!   is on `AccountState`, margin lives on `MarginAccounts`.
//!
//! Leading `#` lines and blank lines are skipped.

use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GenesisRecord {
    Chain(ChainRecord),
    Validator(ValidatorRecord),
    Market(MarketRecord),
    PerpMarket(PerpMarketRecord),
    FutureMarket(FutureMarketRecord),
    MarginAccount(MarginAccountRecord),
    Company(CompanyRecord),
    CommunityWallet(CommunityWalletRecord),
    Sim(SimRecord),
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChainRecord {
    pub chain_id: u32,
    /// User-facing denomination string (e.g. "USD" or "ULD"). Persisted in
    /// `Meta::denom` for display layers; does not affect on-chain accounting.
    /// Defaults to "USD" when absent.
    #[serde(default)]
    pub denom: Option<String>,
    /// Initial USD seed for the protocol-pinned insurance fund
    /// (`Address::INSURANCE`). Decimal string, micro-USD (1e6 base units).
    /// Defaults to 0 when absent — convenient for dev clusters that don't
    /// run liquidations. The trading spec recommends >= $1M for production
    /// (`docs/design/perps-trading.md` §5.5).
    #[serde(deserialize_with = "u128_str", default)]
    pub insurance_fund_seed: u128,
}

/// Either a `seed` (dev mode — addr + pubkey are derived via keccak256(seed))
/// or explicit `addr` + `pubkey` (production-style).
#[derive(Clone, Debug, Deserialize)]
pub struct ValidatorRecord {
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default)]
    pub pubkey: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u64,
    /// Starting USD balance credited at genesis. Decimal string (u128 capacity).
    /// Goes to `wallet` when set, otherwise to the validator's own address.
    #[serde(deserialize_with = "u128_str", default)]
    pub initial_amount: u128,
    /// Optional separate wallet that receives `initial_amount`. Lets the
    /// signing key and the funds-holding account be different addresses.
    #[serde(default)]
    pub wallet: Option<String>,
}

fn default_weight() -> u64 {
    1
}

#[derive(Clone, Debug, Deserialize)]
pub struct MarketRecord {
    pub id: String,
    pub base: String,
    pub quote: String,
    #[serde(deserialize_with = "u128_loose")]
    pub tick_size: u128,
    #[serde(deserialize_with = "u128_loose")]
    pub min_qty: u128,
    pub fee_bps: u16,
}

/// Perp-market record. The `id` is a string at the genesis layer (e.g.
/// `"btc_perp"`); registration order maps to `DerivId::perp(N)` so genesis
/// alone determines the on-chain market index.
#[derive(Clone, Debug, Deserialize)]
pub struct PerpMarketRecord {
    pub id: String,
    /// Underlying spot asset (e.g. "btc", "eth").
    pub underlying: String,
    #[serde(deserialize_with = "u128_loose")]
    pub tick_size: u128,
    #[serde(deserialize_with = "u128_loose")]
    pub lot_size: u128,
    pub max_leverage_x100: u32,
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub taker_fee_bps: u16,
    pub maker_fee_bps_signed: i16,
    pub funding_interval_blocks: u32,
    pub funding_rate_cap_q9: i64,
    /// Hex-encoded address (with optional `0x` prefix). Honours `${ENV_VAR}`
    /// substitution like other genesis address fields.
    pub oracle_authority: String,
    #[serde(deserialize_with = "u128_loose", default)]
    pub max_position_notional: u128,
    #[serde(default)]
    pub paused: bool,
}

/// Dated-futures record. Same shape as `PerpMarketRecord` minus funding,
/// plus expiry / settlement fields.
#[derive(Clone, Debug, Deserialize)]
pub struct FutureMarketRecord {
    pub id: String,
    pub underlying: String,
    #[serde(deserialize_with = "u128_loose")]
    pub tick_size: u128,
    #[serde(deserialize_with = "u128_loose")]
    pub lot_size: u128,
    pub max_leverage_x100: u32,
    pub initial_margin_bps: u16,
    pub maintenance_margin_bps: u16,
    pub taker_fee_bps: u16,
    pub maker_fee_bps_signed: i16,
    pub expiry_height: u64,
    /// `0` = TWAP of last N mark snapshots; `1` = oracle spot at expiry.
    pub settlement_method: u8,
    pub settlement_window_blocks: u32,
    pub oracle_authority: String,
    #[serde(default)]
    pub paused: bool,
}

/// Pre-seed a per-owner `MarginAccount` row at genesis. `collateral` is
/// 1e6-base USD (matches `AssetId::Usd`). Demo / dev only — production
/// genesis files should leave margin entirely user-funded via `AdjustMargin`
/// once that surface ships (see `docs/design/perps-trading.md` §2).
#[derive(Clone, Debug, Deserialize)]
pub struct MarginAccountRecord {
    pub address: String,
    #[serde(deserialize_with = "u128_str")]
    pub collateral: u128,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompanyRecord {
    pub address: String,
    /// asset name → decimal string (u128).
    #[serde(deserialize_with = "balances_without_repeats")]
    pub balances: BTreeMap<String, U128Str>,
}

/// Protocol-level treasury account credited at genesis. Same shape as the
/// company record but persisted under a separate `Meta` key — both can coexist.
#[derive(Clone, Debug, Deserialize)]
pub struct CommunityWalletRecord {
    pub address: String,
    /// asset name → decimal string (u128).
    #[serde(deserialize_with = "balances_without_repeats")]
    pub balances: BTreeMap<String, U128Str>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SimRecord {
    pub num_traders: u32,
    #[serde(default = "default_sim_seed")]
    pub seed_prefix: String,
    #[serde(deserialize_with = "balances_without_repeats")]
    pub balances: BTreeMap<String, U128Str>,
}

fn default_sim_seed() -> String {
    "cb-sim-trader-".to_string()
}

/// u128 wrapper that deserializes from either a quoted string or an integer
/// (the latter is bounded by JSON's i64 range).
#[derive(Clone, Debug)]
pub struct U128Str(pub u128);

impl<'de> Deserialize<'de> for U128Str {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(U128Str(u128_loose(d)?))
    }
}

/// A `balances` object, refusing a key that appears twice.
///
/// JSON allows a repeated key and serde's map decoding keeps the last one, so
/// `{"usd":"1","usd":"2"}` would silently credit 2 and discard the 1. One of
/// those numbers is a typo and nothing downstream could tell which, so the
/// file is refused instead. Keys that differ only in case (`"usd"` and
/// `"USD"`) name the same asset too; `apply` refuses those once it resolves
/// each key to an asset.
fn balances_without_repeats<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, U128Str>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, U128Str>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of asset name to amount")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut m: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((k, v)) = m.next_entry::<String, U128Str>()? {
                if out.contains_key(&k) {
                    return Err(serde::de::Error::custom(format!(
                        "asset `{k}` appears twice in one balances object"
                    )));
                }
                out.insert(k, v);
            }
            Ok(out)
        }
    }
    d.deserialize_map(Visitor)
}

fn u128_str<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    u128_loose(d)
}

fn u128_loose<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrInt {
        Str(String),
        Int(i64),
    }
    match StrOrInt::deserialize(d)? {
        StrOrInt::Int(n) => {
            u128::try_from(n).map_err(|_| serde::de::Error::custom("negative u128"))
        }
        StrOrInt::Str(s) => s
            .parse::<u128>()
            .map_err(|e| serde::de::Error::custom(e.to_string())),
    }
}

/// Parsed genesis: records grouped by kind, preserving order within each group.
#[derive(Clone, Debug, Default)]
pub struct GenesisSpec {
    pub chain: Option<ChainRecord>,
    pub validators: Vec<ValidatorRecord>,
    pub markets: Vec<MarketRecord>,
    pub perp_markets: Vec<PerpMarketRecord>,
    pub future_markets: Vec<FutureMarketRecord>,
    pub margin_accounts: Vec<MarginAccountRecord>,
    pub company: Option<CompanyRecord>,
    pub community_wallet: Option<CommunityWalletRecord>,
    pub sim: Option<SimRecord>,
}

impl GenesisSpec {
    /// Parse JSONL genesis text. `${VAR}` tokens on each non-comment, non-blank
    /// line are substituted from the process environment **before** JSON
    /// parsing — see module docs. Comment lines (`#`) are passed through
    /// verbatim so the file may legitimately mention `${...}` in its header.
    pub fn from_jsonl(text: &str) -> anyhow::Result<Self> {
        let mut expanded = String::with_capacity(text.len());
        for raw in text.lines() {
            let trimmed = raw.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                expanded.push_str(raw);
            } else {
                expanded.push_str(&expand_env_vars(raw)?);
            }
            expanded.push('\n');
        }
        Self::from_jsonl_no_env(&expanded)
    }

    /// Parse JSONL genesis text without performing env-var substitution.
    /// Useful in tests that want to control the input verbatim.
    pub fn from_jsonl_no_env(text: &str) -> anyhow::Result<Self> {
        let mut spec = GenesisSpec::default();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let rec: GenesisRecord = serde_json::from_str(line).map_err(|e| {
                anyhow::anyhow!("genesis line {}: {} (input: {})", lineno + 1, e, line)
            })?;
            match rec {
                GenesisRecord::Chain(c) => {
                    anyhow::ensure!(spec.chain.is_none(), "duplicate `chain` record");
                    spec.chain = Some(c);
                }
                GenesisRecord::Validator(v) => spec.validators.push(v),
                GenesisRecord::Market(m) => spec.markets.push(m),
                GenesisRecord::PerpMarket(m) => spec.perp_markets.push(m),
                GenesisRecord::FutureMarket(m) => spec.future_markets.push(m),
                GenesisRecord::MarginAccount(m) => spec.margin_accounts.push(m),
                GenesisRecord::Company(c) => {
                    anyhow::ensure!(spec.company.is_none(), "duplicate `company` record");
                    spec.company = Some(c);
                }
                GenesisRecord::CommunityWallet(c) => {
                    anyhow::ensure!(
                        spec.community_wallet.is_none(),
                        "duplicate `community_wallet` record"
                    );
                    spec.community_wallet = Some(c);
                }
                GenesisRecord::Sim(s) => {
                    anyhow::ensure!(spec.sim.is_none(), "duplicate `sim` record");
                    spec.sim = Some(s);
                }
            }
        }
        anyhow::ensure!(spec.chain.is_some(), "genesis missing `chain` record");
        anyhow::ensure!(
            !spec.validators.is_empty(),
            "genesis must define at least one validator"
        );
        Ok(spec)
    }
}

pub fn load(path: &std::path::Path) -> anyhow::Result<GenesisSpec> {
    // Best-effort: if there's a `.env` next to the genesis file, load it
    // before substitution. Real env vars take precedence (see `env::apply_dotenv`).
    if let Some(dir) = path.parent() {
        let dotenv = dir.join(".env");
        crate::env::apply_dotenv(&dotenv)?;
    }
    let text = std::fs::read_to_string(path)?;
    GenesisSpec::from_jsonl(&text)
}

/// Replace every `${VAR}` token in `text` with `std::env::var(VAR)`.
/// Hard-errors on:
///   - a `${` with no closing `}`,
///   - an empty variable name (`${}`),
///   - a referenced variable that is not set in the environment.
///
/// No default-value syntax (e.g. `${VAR:-default}`) — keep it boring.
pub fn expand_env_vars(text: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow::anyhow!("genesis: unterminated `${{` — no matching `}}`"))?;
        let name = &after[..end];
        anyhow::ensure!(!name.is_empty(), "genesis: empty env var name in `${{}}`");
        let value = std::env::var(name).map_err(|_| {
            anyhow::anyhow!("genesis: env var `{name}` is not set (referenced as `${{{name}}}`)")
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment line
{"kind":"chain","chain_id":1}

{"kind":"validator","seed":"cb-validator-0","weight":1,"initial_amount":"1000"}
{"kind":"validator","seed":"cb-validator-1","weight":2,"initial_amount":"2000","wallet":"0x00000000000000000000000000000000000000aa"}
{"kind":"market","id":"btc_usd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"100"}}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"42","btc":"7"}}
"#;

    #[test]
    fn parse_jsonl_skips_blanks_and_comments() {
        let spec = GenesisSpec::from_jsonl(SAMPLE).unwrap();
        assert_eq!(spec.chain.unwrap().chain_id, 1);
        assert_eq!(spec.validators.len(), 2);
        assert_eq!(spec.validators[0].seed.as_deref(), Some("cb-validator-0"));
        assert_eq!(spec.validators[0].wallet, None);
        assert_eq!(spec.validators[1].weight, 2);
        assert_eq!(spec.validators[1].initial_amount, 2000);
        assert_eq!(
            spec.validators[1].wallet.as_deref(),
            Some("0x00000000000000000000000000000000000000aa")
        );
        assert_eq!(spec.markets.len(), 1);
        assert_eq!(spec.company.unwrap().balances.get("usd").unwrap().0, 100);
        let cw = spec.community_wallet.expect("community_wallet parsed");
        assert_eq!(cw.address, "0x000000000000000000000000000000000000c0de");
        assert_eq!(cw.balances.get("usd").unwrap().0, 42);
        assert_eq!(cw.balances.get("btc").unwrap().0, 7);
    }

    #[test]
    fn duplicate_community_wallet_record_fails() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"1"}}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"2"}}
"#;
        let err = GenesisSpec::from_jsonl(s).unwrap_err();
        assert!(err.to_string().contains("duplicate `community_wallet`"));
    }

    #[test]
    fn validator_wallet_field_optional_default_none() {
        let line = r#"{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}"#;
        let rec: GenesisRecord = serde_json::from_str(line).unwrap();
        match rec {
            GenesisRecord::Validator(v) => assert!(v.wallet.is_none()),
            _ => panic!("wrong record kind"),
        }
    }

    #[test]
    fn validator_explicit_addr_form() {
        let line = r#"{"kind":"validator","addr":"0x01020304050607080910111213141516171819ff","pubkey":"0xdead","weight":1,"initial_amount":"5"}"#;
        let rec: GenesisRecord = serde_json::from_str(line).unwrap();
        match rec {
            GenesisRecord::Validator(v) => {
                assert_eq!(
                    v.addr.as_deref(),
                    Some("0x01020304050607080910111213141516171819ff")
                );
                assert_eq!(v.pubkey.as_deref(), Some("0xdead"));
                assert_eq!(v.initial_amount, 5);
            }
            _ => panic!("wrong record kind"),
        }
    }

    #[test]
    fn missing_chain_record_fails() {
        let s = r#"{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}"#;
        let err = GenesisSpec::from_jsonl(s).unwrap_err();
        assert!(err.to_string().contains("missing `chain`"));
    }

    #[test]
    fn empty_validator_set_fails() {
        let s = r#"{"kind":"chain","chain_id":1}"#;
        let err = GenesisSpec::from_jsonl(s).unwrap_err();
        assert!(err.to_string().contains("at least one validator"));
    }

    #[test]
    fn duplicate_chain_record_fails() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"chain","chain_id":2}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
"#;
        let err = GenesisSpec::from_jsonl(s).unwrap_err();
        assert!(err.to_string().contains("duplicate `chain`"));
    }

    #[test]
    fn unknown_fields_are_ignored_rather_than_rejected() {
        let s = r#"{"kind":"chain","chain_id":1,"oops":true}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}"#;
        // serde_json ignores unknown fields unless a record opts into
        // `deny_unknown_fields`. Pinned, with the parsed values checked, so a
        // future flip of that attribute fails here rather than in a
        // deployment whose genesis file carried an annotation nobody
        // re-validated.
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(spec.chain.unwrap().chain_id, 1);
        assert_eq!(spec.validators.len(), 1);
        assert_eq!(spec.validators[0].initial_amount, 1);
    }

    // ---------------- denom on chain record ----------------

    #[test]
    fn chain_denom_defaults_to_none_when_absent() {
        let s = r#"{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(spec.chain.unwrap().denom, None);
    }

    #[test]
    fn chain_denom_is_parsed_when_present() {
        let s = r#"{"kind":"chain","chain_id":1,"denom":"ULD"}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(spec.chain.unwrap().denom.as_deref(), Some("ULD"));
    }

    // ---------------- env-var substitution ----------------

    #[test]
    fn expand_env_substitutes_a_single_var() {
        // SAFETY: tests in the same crate may run in parallel and share env;
        // use a unique name per test to avoid races.
        std::env::set_var("UL_GENESIS_TEST_VAR_A", "hello");
        let out = expand_env_vars("a=${UL_GENESIS_TEST_VAR_A}").unwrap();
        assert_eq!(out, "a=hello");
    }

    #[test]
    fn expand_env_substitutes_multiple_vars() {
        std::env::set_var("UL_GENESIS_TEST_VAR_X", "X1");
        std::env::set_var("UL_GENESIS_TEST_VAR_Y", "Y2");
        let out =
            expand_env_vars("${UL_GENESIS_TEST_VAR_X}-${UL_GENESIS_TEST_VAR_Y}-tail").unwrap();
        assert_eq!(out, "X1-Y2-tail");
    }

    #[test]
    fn expand_env_passthrough_when_no_tokens() {
        let out = expand_env_vars(r#"{"kind":"chain","chain_id":1}"#).unwrap();
        assert_eq!(out, r#"{"kind":"chain","chain_id":1}"#);
    }

    #[test]
    fn expand_env_missing_var_is_an_error() {
        std::env::remove_var("UL_GENESIS_TEST_DEFINITELY_UNSET");
        let err = expand_env_vars("${UL_GENESIS_TEST_DEFINITELY_UNSET}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("UL_GENESIS_TEST_DEFINITELY_UNSET"), "{msg}");
        assert!(msg.contains("not set"), "{msg}");
    }

    #[test]
    fn expand_env_unterminated_is_an_error() {
        let err = expand_env_vars("trailing ${UNCLOSED").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn expand_env_empty_name_is_an_error() {
        let err = expand_env_vars("oops ${} empty").unwrap_err();
        assert!(err.to_string().contains("empty env var name"), "{err}");
    }

    #[test]
    fn from_jsonl_expands_env_vars_in_validator_wallet() {
        std::env::set_var(
            "UL_GENESIS_TEST_WALLET",
            "0x00000000000000000000000000000000000000aa",
        );
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","wallet":"${UL_GENESIS_TEST_WALLET}","weight":1,"initial_amount":"1"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(
            spec.validators[0].wallet.as_deref(),
            Some("0x00000000000000000000000000000000000000aa")
        );
    }

    #[test]
    fn from_jsonl_no_env_skips_substitution() {
        // The verbatim parser must not touch `${...}` tokens — useful when a
        // test wants to assert behaviour with the raw text intact.
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","wallet":"${UL_GENESIS_NEVER_SET}","weight":1,"initial_amount":"1"}
"#;
        let spec = GenesisSpec::from_jsonl_no_env(s).unwrap();
        assert_eq!(
            spec.validators[0].wallet.as_deref(),
            Some("${UL_GENESIS_NEVER_SET}")
        );
    }

    #[test]
    fn from_jsonl_does_not_substitute_inside_comments() {
        // `${UL_GENESIS_DOC_VAR}` appears literally in a `#` comment line
        // explaining the syntax. The parser must skip it: only the JSON
        // payload's `${...}` tokens get expanded.
        std::env::set_var("UL_GENESIS_DOC_REAL_VAR", "0xdead");
        let s = r#"
# This file uses ${UL_GENESIS_DOC_VAR} as an example. Do not require it set.
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","wallet":"${UL_GENESIS_DOC_REAL_VAR}","weight":1,"initial_amount":"1"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(
            spec.validators[0].wallet.as_deref(),
            Some("0xdead"),
            "JSON line var should be substituted",
        );
    }

    // ---------------- margin_account record ----------------

    #[test]
    fn margin_account_record_parses() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000beef","collateral":"500000000"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000cafe","collateral":"123"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        assert_eq!(spec.margin_accounts.len(), 2);
        assert_eq!(spec.margin_accounts[0].collateral, 500_000_000);
        assert_eq!(spec.margin_accounts[1].collateral, 123);
        assert_eq!(
            spec.margin_accounts[0].address,
            "0x000000000000000000000000000000000000beef"
        );
    }

    // ---------------- amounts: what the u128 codec refuses ----------------

    fn with_company_usd(usd_json: &str) -> anyhow::Result<GenesisSpec> {
        GenesisSpec::from_jsonl_no_env(&format!(
            r#"{{"kind":"chain","chain_id":1}}
{{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}}
{{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{{"usd":{usd_json}}}}}"#
        ))
    }

    #[test]
    fn a_negative_balance_is_refused_whether_written_as_a_number_or_a_string() {
        // On-chain amounts are unsigned. A negative genesis balance has no
        // representation, so it must be an error, never a wrap to 2^128 - 5.
        let err = with_company_usd("-5").unwrap_err().to_string();
        assert!(err.contains("negative u128"), "{err}");
        assert!(with_company_usd("\"-5\"").is_err());
        assert!(
            with_company_usd("\"-0\"").is_err(),
            "a signed zero is still a sign"
        );
    }

    #[test]
    fn a_fractional_empty_or_oversized_balance_is_refused() {
        assert!(
            with_company_usd("\"1.5\"").is_err(),
            "base units have no fraction"
        );
        assert!(with_company_usd("1.5").is_err(), "nor as a float");
        assert!(with_company_usd("\"\"").is_err(), "empty is not zero");
        assert!(with_company_usd("\" 1\"").is_err(), "no whitespace");
        assert!(
            with_company_usd("\"340282366920938463463374607431768211456\"").is_err(),
            "u128::MAX + 1"
        );
        assert!(
            with_company_usd("18446744073709551616000").is_err(),
            "an integer literal beyond i64 has no loose form; write it as a string"
        );
        // The boundary itself is representable.
        let spec = with_company_usd("\"340282366920938463463374607431768211455\"").unwrap();
        assert_eq!(spec.company.unwrap().balances["usd"].0, u128::MAX);
    }

    #[test]
    fn a_negative_validator_stake_or_insurance_seed_is_refused() {
        let s = r#"{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":-1}"#;
        let err = GenesisSpec::from_jsonl_no_env(s).unwrap_err().to_string();
        assert!(err.contains("negative u128"), "{err}");
        let s = r#"{"kind":"chain","chain_id":1,"insurance_fund_seed":"-1"}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}"#;
        assert!(GenesisSpec::from_jsonl_no_env(s).is_err());
    }

    #[test]
    fn a_balances_object_naming_an_asset_twice_is_refused() {
        // serde would keep the second value and drop the first without a word.
        let err = with_company_usd("\"1\",\"usd\":\"2\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("appears twice"), "{err}");
        // The same asset under two spellings is caught at `apply`, where the
        // keys resolve to assets; see `build.rs`.
        let spec = with_company_usd("\"1\",\"USD\":\"2\"").unwrap();
        assert_eq!(spec.company.unwrap().balances.len(), 2);
    }
}
