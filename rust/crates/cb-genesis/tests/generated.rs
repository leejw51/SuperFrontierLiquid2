//! Genesis over generated specs, and every class of invalid spec refused.
//!
//! The hand-written tests in this crate each check one record kind against
//! one fixture. Here the builder is driven by randomly generated *valid* specs
//! (any number of validators with or without wallets, company and community
//! wallets with arbitrary balances across the asset universe, margin accounts,
//! a sim population, derivative markets), and the resulting chain is checked
//! against an independent model of what the spec asked for.
//!
//! What the group proves:
//! - every generated valid spec loads and applies;
//! - every credited address holds exactly the sum of the credits the spec
//!   directs to it, asset by asset, including when several records credit
//!   the same address (the model adds them up; so must the builder);
//! - margin accounts, the validator set and the derivative markets land as
//!   specified, in line order;
//! - applying the same spec twice yields the same state root;
//! - each class of invalid spec, one mutation at a time on a valid base, is
//!   refused with an error and never a panic.

use cb_crypto::{address_from_pubkey, keypair_from_seed};
use cb_genesis::{apply, GenesisSpec};
use cb_state::{Accounts, Database, MarginAccounts, PerpMarkets, Validators};
use cb_types::{Address, AssetId, DerivId};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap};
use tempfile::tempdir;

fn hex_addr(a: &Address) -> String {
    format!("0x{}", hex::encode(a.0))
}

fn seeded(seed: &str) -> Address {
    let (_, pk) = keypair_from_seed(seed.as_bytes());
    address_from_pubkey(&pk)
}

/// An address no seed derives to and no other record uses: the high byte
/// marks the role, the rest is the index.
fn fixed(role: u8, i: u32) -> Address {
    let mut a = [0u8; 20];
    a[0] = role;
    a[16..].copy_from_slice(&i.to_be_bytes());
    Address(a)
}

#[derive(Debug, Clone)]
struct Gen {
    validators: Vec<(Option<u8>, u64)>,
    company: BTreeMap<usize, u64>,
    community: BTreeMap<usize, u64>,
    margins: Vec<u64>,
    sim_traders: u32,
    sim: BTreeMap<usize, u64>,
    perps: usize,
    salt: u32,
}

fn balances() -> impl Strategy<Value = BTreeMap<usize, u64>> {
    proptest::collection::btree_map(0..AssetId::ALL.len(), any::<u64>(), 0..=AssetId::ALL.len())
}

fn gen() -> impl Strategy<Value = Gen> {
    (
        // A wallet index points at one of three shared wallets, so several
        // validators routinely credit the same address.
        proptest::collection::vec((proptest::option::of(0u8..3), any::<u64>()), 1..6),
        balances(),
        balances(),
        proptest::collection::vec(any::<u64>(), 0..4),
        0u32..4,
        balances(),
        0usize..=cb_types::PERPS.len(),
        any::<u32>(),
    )
        .prop_map(
            |(validators, company, community, margins, sim_traders, sim, perps, salt)| Gen {
                validators,
                company,
                community,
                margins,
                sim_traders,
                sim,
                perps,
                salt,
            },
        )
}

fn balances_json(b: &BTreeMap<usize, u64>) -> String {
    let items: Vec<String> = b
        .iter()
        .map(|(i, v)| format!("\"{}\":\"{v}\"", AssetId::ALL[*i].lower_symbol()))
        .collect();
    format!("{{{}}}", items.join(","))
}

impl Gen {
    fn validator_seed(&self, i: usize) -> String {
        format!("gen-{}-validator-{i}", self.salt)
    }

    fn sim_prefix(&self) -> String {
        format!("gen-{}-sim-", self.salt)
    }

    fn jsonl(&self) -> String {
        let mut out =
            vec![r#"{"kind":"chain","chain_id":9,"insurance_fund_seed":"1000"}"#.to_string()];
        for (i, (wallet, amount)) in self.validators.iter().enumerate() {
            let wallet = match wallet {
                Some(w) => format!(r#","wallet":"{}""#, hex_addr(&fixed(0xaa, *w as u32))),
                None => String::new(),
            };
            out.push(format!(
                r#"{{"kind":"validator","seed":"{}","weight":1,"initial_amount":"{amount}"{wallet}}}"#,
                self.validator_seed(i)
            ));
        }
        out.push(format!(
            r#"{{"kind":"company","address":"{}","balances":{}}}"#,
            hex_addr(&fixed(0xc0, 0)),
            balances_json(&self.company)
        ));
        // The community wallet is shared wallet 0, so it also receives any
        // validator credit pointed there.
        out.push(format!(
            r#"{{"kind":"community_wallet","address":"{}","balances":{}}}"#,
            hex_addr(&fixed(0xaa, 0)),
            balances_json(&self.community)
        ));
        for (i, c) in self.margins.iter().enumerate() {
            out.push(format!(
                r#"{{"kind":"margin_account","address":"{}","collateral":"{c}"}}"#,
                hex_addr(&fixed(0xd0, i as u32))
            ));
        }
        for p in &cb_types::PERPS[..self.perps] {
            out.push(format!(
                r#"{{"kind":"perp_market","id":"{}","underlying":"{}","tick_size":1,"lot_size":1,"max_leverage_x100":{},"initial_margin_bps":{},"maintenance_margin_bps":{},"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"{}"}}"#,
                p.symbol.to_ascii_lowercase(),
                p.underlying.lower_symbol(),
                p.risk.max_leverage_x100,
                p.risk.initial_margin_bps,
                p.risk.maintenance_margin_bps,
                hex_addr(&fixed(0x0a, 0))
            ));
        }
        out.push(format!(
            r#"{{"kind":"sim","num_traders":{},"seed_prefix":"{}","balances":{}}}"#,
            self.sim_traders,
            self.sim_prefix(),
            balances_json(&self.sim)
        ));
        out.join("\n")
    }

    /// The model: every credit the spec directs, summed per address and asset.
    fn expected(&self) -> HashMap<Address, BTreeMap<AssetId, u128>> {
        let mut m: HashMap<Address, BTreeMap<AssetId, u128>> = HashMap::new();
        let mut add = |a: Address, asset: AssetId, v: u128| {
            if v > 0 {
                *m.entry(a).or_default().entry(asset).or_default() += v;
            }
        };
        for (i, v) in &self.company {
            add(fixed(0xc0, 0), AssetId::ALL[*i], *v as u128);
        }
        for (i, v) in &self.community {
            add(fixed(0xaa, 0), AssetId::ALL[*i], *v as u128);
        }
        for (i, (wallet, amount)) in self.validators.iter().enumerate() {
            let to = match wallet {
                Some(w) => fixed(0xaa, *w as u32),
                None => seeded(&self.validator_seed(i)),
            };
            add(to, AssetId::Usd, *amount as u128);
        }
        for t in 0..self.sim_traders {
            let a = seeded(&format!("{}{t}", self.sim_prefix()));
            for (i, v) in &self.sim {
                add(a, AssetId::ALL[*i], *v as u128);
            }
        }
        m
    }
}

fn applied(text: &str) -> (tempfile::TempDir, Database, cb_types::H256) {
    let spec =
        GenesisSpec::from_jsonl_no_env(text).unwrap_or_else(|e| panic!("parse: {e}\n{text}"));
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let root = apply(&db, &spec).unwrap_or_else(|e| panic!("apply: {e}\n{text}"));
    (dir, db, root)
}

proptest! {
    // Each case opens a RocksDB; 48 cases is about a second.
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn a_generated_valid_spec_applies_and_credits_exactly_what_it_says(g in gen()) {
        let text = g.jsonl();
        let (_dir, db, root) = applied(&text);
        let accts = Accounts::new(&db);
        for (addr, assets) in g.expected() {
            let state = accts.get(&addr).unwrap_or_default();
            for asset in AssetId::ALL {
                prop_assert_eq!(
                    state.balance(asset),
                    assets.get(&asset).copied().unwrap_or(0),
                    "{} {}", addr, asset
                );
            }
        }

        let v = Validators::new(&db);
        prop_assert_eq!(v.count() as usize, g.validators.len());
        for (i, info) in v.list().iter().enumerate() {
            prop_assert_eq!(info.addr, seeded(&g.validator_seed(i)), "validator order");
        }

        let margins = MarginAccounts::new(&db);
        for (i, c) in g.margins.iter().enumerate() {
            let m = margins.get(&fixed(0xd0, i as u32)).expect("margin account seeded");
            prop_assert_eq!(m.collateral, *c as u128);
        }
        prop_assert_eq!(margins.get(&Address::INSURANCE).unwrap().collateral, 1000);

        let perps = PerpMarkets::new(&db);
        for (i, p) in cb_types::PERPS.iter().enumerate() {
            let got = perps.get(&DerivId::perp(i as u16));
            prop_assert_eq!(got.is_some(), i < g.perps);
            if let Some(m) = got {
                prop_assert_eq!(m.underlying, p.underlying, "line order is index order");
            }
        }

        // Deterministic: the same file applied again gives the same root.
        let (_dir2, _db2, root2) = applied(&text);
        prop_assert_eq!(root, root2);
    }
}

// ----------------------------------------------------- invalid classes --

const BASE: &str = r#"{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"inv-v0","weight":1,"initial_amount":"10"}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"1"}}
{"kind":"market","id":"btc_usd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}"#;

const PERP: &str = r#"{"kind":"perp_market","id":"p","underlying":"btc","tick_size":1,"lot_size":1,"max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1,"oracle_authority":"0x000000000000000000000000000000000000b7c0"}"#;
const FUT: &str = r#"{"kind":"future_market","id":"f","underlying":"eth","tick_size":1,"lot_size":1,"max_leverage_x100":1000,"initial_margin_bps":1000,"maintenance_margin_bps":500,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"expiry_height":100,"settlement_method":0,"settlement_window_blocks":10,"oracle_authority":"0x000000000000000000000000000000000000e7c0"}"#;

/// Parse and apply; `Err` from either stage counts as refused.
fn outcome(text: &str) -> Result<(), String> {
    let spec = GenesisSpec::from_jsonl_no_env(text).map_err(|e| format!("parse: {e}"))?;
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    apply(&db, &spec)
        .map(|_| ())
        .map_err(|e| format!("apply: {e}"))
}

#[test]
fn the_base_spec_for_the_invalid_classes_is_itself_valid() {
    outcome(BASE).unwrap();
    outcome(&format!("{BASE}\n{PERP}\n{FUT}")).unwrap();
}

#[test]
fn every_class_of_invalid_spec_is_refused() {
    let without_chain = BASE.lines().skip(1).collect::<Vec<_>>().join("\n");
    let only_chain = BASE.lines().next().unwrap().to_string();
    let with = |extra: &str| format!("{BASE}\n{extra}");
    let big = u128::MAX;
    let cases: Vec<(&str, String)> = vec![
        ("no chain record", without_chain),
        ("no validator", only_chain),
        (
            "a second chain record",
            with(r#"{"kind":"chain","chain_id":2}"#),
        ),
        (
            "an unknown record kind",
            with(r#"{"kind":"validatr","seed":"x"}"#),
        ),
        ("a line that is not JSON", with("{kind:chain")),
        (
            "the same validator twice",
            with(r#"{"kind":"validator","seed":"inv-v0","weight":1}"#),
        ),
        (
            "a validator with neither seed nor keys",
            with(r#"{"kind":"validator","weight":1}"#),
        ),
        (
            "a validator with a seed and an address",
            with(
                r#"{"kind":"validator","seed":"s","addr":"0x000000000000000000000000000000000000abcd","weight":1}"#,
            ),
        ),
        (
            "an explicit pubkey that is not a point",
            with(
                r#"{"kind":"validator","addr":"0x000000000000000000000000000000000000abcd","pubkey":"0x02ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","weight":1}"#,
            ),
        ),
        (
            "a validator wallet that is not an address",
            with(
                r#"{"kind":"validator","seed":"w","wallet":"0x12","weight":1,"initial_amount":"1"}"#,
            ),
        ),
        (
            "a negative stake",
            with(r#"{"kind":"validator","seed":"n","weight":1,"initial_amount":-1}"#),
        ),
        (
            "a second company",
            with(
                r#"{"kind":"company","address":"0x000000000000000000000000000000000000c0c1","balances":{}}"#,
            ),
        ),
        (
            "a community wallet at the company address",
            with(
                r#"{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0c0","balances":{}}"#,
            ),
        ),
        (
            "an unknown asset in a balance",
            BASE.replace(r#""usd":"1""#, r#""doge":"1""#),
        ),
        (
            "a fractional balance",
            BASE.replace(r#""usd":"1""#, r#""usd":"1.5""#),
        ),
        (
            "a negative balance",
            BASE.replace(r#""usd":"1""#, r#""usd":"-1""#),
        ),
        (
            "an asset twice in one balances object",
            BASE.replace(r#""usd":"1""#, r#""usd":"1","usd":"2""#),
        ),
        (
            "an asset under two spellings",
            BASE.replace(r#""usd":"1""#, r#""usd":"1","USD":"2""#),
        ),
        (
            "credits to one address that overflow",
            with(&format!(
                r#"{{"kind":"validator","seed":"o","wallet":"0x000000000000000000000000000000000000c0c0","weight":1,"initial_amount":"{big}"}}"#
            )),
        ),
        (
            "an unknown spot market",
            BASE.replace("btc_usd", "doge_usd"),
        ),
        (
            "a spot market listed twice",
            with(
                r#"{"kind":"market","id":"btcusd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}"#,
            ),
        ),
        ("a perp listed twice", format!("{BASE}\n{PERP}\n{PERP}")),
        (
            "a perp on an unknown underlying",
            with(&PERP.replace(r#""underlying":"btc""#, r#""underlying":"doge""#)),
        ),
        (
            "a perp with a malformed oracle",
            with(&PERP.replace("0x000000000000000000000000000000000000b7c0", "0xb7c0")),
        ),
        (
            "a perp missing a field",
            with(&PERP.replace(r#""lot_size":1,"#, "")),
        ),
        ("a future listed twice", format!("{BASE}\n{FUT}\n{FUT}")),
        (
            "a future with an unknown settlement method",
            with(&FUT.replace(r#""settlement_method":0"#, r#""settlement_method":2"#)),
        ),
        (
            "a margin account listed twice",
            with(
                r#"{"kind":"margin_account","address":"0x00000000000000000000000000000000000000d0","collateral":"1"}
{"kind":"margin_account","address":"0x00000000000000000000000000000000000000d0","collateral":"2"}"#,
            ),
        ),
        (
            "a margin account on the insurance fund",
            with(&format!(
                r#"{{"kind":"margin_account","address":"{}","collateral":"1"}}"#,
                hex_addr(&Address::INSURANCE)
            )),
        ),
        (
            "a second sim record",
            with(
                r#"{"kind":"sim","num_traders":1,"balances":{}}
{"kind":"sim","num_traders":1,"balances":{}}"#,
            ),
        ),
        (
            "an unresolved variable",
            with(r#"{"kind":"company","address":"${NOT_SET_ANYWHERE}","balances":{}}"#),
        ),
    ];
    let mut accepted = Vec::new();
    for (why, text) in &cases {
        let res = std::panic::catch_unwind(|| outcome(text));
        match res {
            Ok(Err(_)) => {}
            Ok(Ok(())) => accepted.push(format!("accepted: {why}")),
            Err(_) => accepted.push(format!("PANICKED: {why}")),
        }
    }
    assert!(accepted.is_empty(), "{accepted:#?}");
}

#[test]
fn a_validator_wallet_that_is_also_a_sim_trader_keeps_both_credits() {
    // Genesis credits are additive everywhere. The sim population used to be
    // written with a fresh state, which erased an earlier credit to the same
    // address.
    let trader = seeded("overlap-sim-0");
    let text = format!(
        r#"{{"kind":"chain","chain_id":1}}
{{"kind":"validator","seed":"overlap-v","wallet":"{}","weight":1,"initial_amount":"70"}}
{{"kind":"sim","num_traders":1,"seed_prefix":"overlap-sim-","balances":{{"usd":"30","btc":"5"}}}}"#,
        hex_addr(&trader)
    );
    let (_dir, db, _) = applied(&text);
    let s = Accounts::new(&db).get(&trader).unwrap();
    assert_eq!(s.balance(AssetId::Usd), 100);
    assert_eq!(s.balance(AssetId::Btc), 5);
}
