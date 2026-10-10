//! Loads the committed `genesis-perps.jsonl` and applies it. Pins:
//!   - the file actually parses with the env subset the runbook documents,
//!   - the resulting chain has the markets the runbook references,
//!   - margin accounts and insurance fund are seeded as expected.
//!
//! If you change the perps genesis or any of the seeded amounts, update the
//! assertions here. The runbook doc references these exact numbers.

use cb_genesis::{apply, GenesisSpec};
use cb_state::{Accounts, Database, FuturesMarkets, MarginAccounts, Meta, PerpMarkets};
use cb_types::{Address, AssetId, DerivId};
use std::path::PathBuf;
use tempfile::tempdir;

/// Set the env vars `genesis-perps.jsonl` references to deterministic dev
/// values. Mirrors what `make startperps` does on the shell side.
fn set_perps_env() {
    // Each var name is unique to this test so it doesn't race the existing
    // dotenv tests in this crate (cargo test runs them all in parallel).
    std::env::set_var("MYCHAINID", "1");
    std::env::set_var("MYDENOM", "ULD");
    std::env::set_var("INITIAL_AMOUNT", "1000000000000");
    std::env::set_var("MYWALLET0", "0x000000000000000000000000000000000000a0a0");
    std::env::set_var("MYWALLET1", "0x000000000000000000000000000000000000a0a1");
    std::env::set_var("MYWALLET2", "0x000000000000000000000000000000000000a0a2");
    std::env::set_var("MYWALLET3", "0x000000000000000000000000000000000000c0de");
    // The treasury is the only account allowed to mint, so it has to be an
    // address somebody holds a key for. This file used to name a vanity address
    // nobody could sign as, which meant the faucet silently could not pay out
    // on any chain booted from it.
    std::env::set_var("TREASURY", "0x00000000000000000000000000000000000c0c0d");
    std::env::set_var("ORACLE_BTC", "0x000000000000000000000000000000000000b7c0");
    std::env::set_var("ORACLE_ETH", "0x000000000000000000000000000000000000e7c0");
    std::env::set_var("DEMO_TRADER0", "0x0000000000000000000000000000000000d10000");
    std::env::set_var("DEMO_TRADER1", "0x0000000000000000000000000000000000d10001");
    std::env::set_var("DEMO_TRADER2", "0x0000000000000000000000000000000000d10002");
}

fn perps_genesis_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/cb-genesis`; the file lives at the repo
    // root, two levels up.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p.push("genesis-perps.jsonl");
    p
}

#[test]
fn genesis_perps_parses_with_runbook_env() {
    set_perps_env();
    let path = perps_genesis_path();
    assert!(
        path.exists(),
        "genesis-perps.jsonl missing at {} — committed file moved?",
        path.display()
    );
    let text = std::fs::read_to_string(&path).expect("read genesis-perps.jsonl");
    let spec = GenesisSpec::from_jsonl(&text).expect("parse genesis-perps.jsonl");
    assert_eq!(
        spec.validators.len(),
        4,
        "4 validators: a BFT quorum of three with one to spare"
    );
    assert_eq!(spec.perp_markets.len(), 2, "btc_perp + eth_perp");
    assert_eq!(spec.future_markets.len(), 1, "one dated future");
    assert_eq!(spec.margin_accounts.len(), 3, "3 demo traders");
}

#[test]
fn genesis_perps_applies_and_seeds_runbook_invariants() {
    set_perps_env();
    let text = std::fs::read_to_string(perps_genesis_path()).unwrap();
    let spec = GenesisSpec::from_jsonl(&text).unwrap();

    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    apply(&db, &spec).unwrap();

    // Chain meta.
    let m = Meta::new(&db);
    assert_eq!(m.chain_id(), 1);
    assert_eq!(m.denom(), "ULD");

    // Insurance fund: $1M = 1e12 micro-USD.
    let margins = MarginAccounts::new(&db);
    let ins = margins
        .get(&Address::INSURANCE)
        .expect("insurance fund seeded");
    assert_eq!(ins.collateral, 1_000_000_000_000);

    // Perp markets: btc_perp (50x = 5000), eth_perp (25x = 2500).
    let pms = PerpMarkets::new(&db);
    let perp0 = pms.get(&DerivId::perp(0)).expect("btc_perp");
    assert_eq!(perp0.underlying, AssetId::Btc);
    assert_eq!(perp0.max_leverage_x100, 5000);
    let perp1 = pms.get(&DerivId::perp(1)).expect("eth_perp");
    assert_eq!(perp1.underlying, AssetId::Eth);
    assert_eq!(perp1.max_leverage_x100, 2500);

    // Distinct oracle authorities.
    assert_ne!(
        perp0.oracle_authority, perp1.oracle_authority,
        "BtcPerp and EthPerp must have distinct oracle authorities"
    );

    // Dated future: eth_dated_q2, ~4M block expiry.
    let fms = FuturesMarkets::new(&db);
    let fut0 = fms.get(&DerivId::future(0)).expect("eth_dated_q2");
    assert_eq!(fut0.expiry_height, 4_000_000);
    assert_eq!(fut0.settlement_method, 0); // TWAP

    // Demo trader margin accounts: $10M each.
    let mut bytes = [0u8; 20];
    bytes[16] = 0x0d;
    bytes[17] = 0x10;
    // Last byte differs: 0x00, 0x01, 0x02 — but the env vars right-pad.
    // Simpler: use the env value we set and parse it.
    let demo0_hex = std::env::var("DEMO_TRADER0").unwrap();
    let demo0 = parse_addr(&demo0_hex);
    let acct = margins.get(&demo0).expect("demo trader 0 margin");
    assert_eq!(acct.collateral, 10_000_000_000_000);

    // Spot company + community wallet still credited.
    let accts = Accounts::new(&db);
    // Read the treasury from the same variable genesis substitutes, so the
    // assertion cannot drift from the file the way a hardcoded literal did.
    let company = parse_addr(&std::env::var("TREASURY").unwrap());
    let comp = accts.get(&company).expect("company seeded");
    assert!(comp.balance(AssetId::Usd) > 0);
    let _ = bytes; // silence unused

    // The file points validator 3's `wallet` and the `community_wallet` record
    // at the same variable, `${MYWALLET3}`, so that one address is credited
    // `INITIAL_AMOUNT` twice: once as the validator's stake, once as the
    // treasury seed. Genesis credits are additive (`wallets.rs` pins that),
    // so the shipped file funds the community wallet at exactly double what
    // its own line says. Asserted here so the doubling is a known property of
    // the file rather than a surprise on the first balance query.
    let initial: u128 = std::env::var("INITIAL_AMOUNT").unwrap().parse().unwrap();
    let community = parse_addr(&std::env::var("MYWALLET3").unwrap());
    assert_eq!(m.community_wallet_addr(), community);
    assert_eq!(
        accts.get(&community).expect("community wallet seeded").balance(AssetId::Usd),
        2 * initial,
        "validator 3's wallet and the community wallet are the same address, so it holds both credits"
    );
}

fn parse_addr(s: &str) -> Address {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).expect("hex");
    let mut a = [0u8; 20];
    a.copy_from_slice(&bytes);
    Address(a)
}
