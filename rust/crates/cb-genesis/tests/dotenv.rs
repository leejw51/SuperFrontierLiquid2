//! Integration test: full `cb_genesis::load(path)` flow with a sibling
//! `.env` file. Confirms that:
//!   - `${MYWALLET0..3}` and `${INITIAL_AMOUNT}` are substituted from the
//!     `.env` next to genesis.jsonl,
//!   - `chain.denom = "ULD"` propagates into `Meta::denom`,
//!   - validator wallets and community_wallet are credited the dotenv amount,
//!   - real env vars override `.env` values.
//!
//! All three tests touch the *same* `MYWALLET*` keys, so we serialize them
//! against each other with a process-local mutex. Other test binaries in
//! the workspace don't touch these names.

use cb_genesis::{apply, load};
use cb_state::{Accounts, Database, Meta};
use cb_types::{Address, AssetId};
use std::io::Write;
use std::sync::Mutex;
use tempfile::tempdir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const GENESIS_JSONL: &str = r#"
{"kind":"chain","chain_id":42,"denom":"ULD"}
{"kind":"validator","seed":"v0","wallet":"${MYWALLET0}","weight":1,"initial_amount":"${INITIAL_AMOUNT}"}
{"kind":"validator","seed":"v1","wallet":"${MYWALLET1}","weight":1,"initial_amount":"${INITIAL_AMOUNT}"}
{"kind":"validator","seed":"v2","wallet":"${MYWALLET2}","weight":1,"initial_amount":"${INITIAL_AMOUNT}"}
{"kind":"community_wallet","address":"${MYWALLET3}","balances":{"usd":"${INITIAL_AMOUNT}"}}
"#;

const DOTENV: &str = r#"
# Sample .env for the dotenv integration test.
MYWALLET0=0x00000000000000000000000000000000000a0001
MYWALLET1=0x00000000000000000000000000000000000a0002
MYWALLET2=0x00000000000000000000000000000000000a0003
MYWALLET3=0x000000000000000000000000000000000000c0de
INITIAL_AMOUNT=1000000000000
"#;

fn write_file(path: &std::path::Path, contents: &str) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}

fn addr_from_hex(hex_str: &str) -> Address {
    let bytes = hex::decode(hex_str.trim_start_matches("0x")).unwrap();
    let mut a = [0u8; 20];
    a.copy_from_slice(&bytes);
    Address(a)
}

fn clear_test_vars() {
    for k in [
        "MYWALLET0",
        "MYWALLET1",
        "MYWALLET2",
        "MYWALLET3",
        "INITIAL_AMOUNT",
    ] {
        std::env::remove_var(k);
    }
}

#[test]
fn load_with_sibling_dotenv_substitutes_and_applies() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_test_vars();

    let dir = tempdir().unwrap();
    let genesis_path = dir.path().join("genesis.jsonl");
    let dotenv_path = dir.path().join(".env");
    write_file(&genesis_path, GENESIS_JSONL);
    write_file(&dotenv_path, DOTENV);

    let spec = load(&genesis_path).expect("load with .env");

    // chain.denom from the genesis line.
    assert_eq!(spec.chain.as_ref().unwrap().denom.as_deref(), Some("ULD"));
    // validator wallets came from .env.
    assert_eq!(
        spec.validators[0].wallet.as_deref(),
        Some("0x00000000000000000000000000000000000a0001")
    );
    assert_eq!(spec.validators.len(), 3);
    for v in &spec.validators {
        assert_eq!(v.initial_amount, 1_000_000_000_000);
    }
    let cw = spec.community_wallet.as_ref().unwrap();
    assert_eq!(cw.address, "0x000000000000000000000000000000000000c0de");
    assert_eq!(cw.balances.get("usd").unwrap().0, 1_000_000_000_000);

    // Now apply and assert the on-chain effects.
    let db_dir = tempdir().unwrap();
    let db = Database::open(db_dir.path()).unwrap();
    apply(&db, &spec).unwrap();

    let m = Meta::new(&db);
    assert_eq!(m.chain_id(), 42);
    assert_eq!(m.denom(), "ULD", "Meta::denom must reflect chain.denom");

    // Each validator wallet (the address from MYWALLET{0,1,2}, NOT the
    // validator's own derived address) holds 1B ULD raw.
    let accts = Accounts::new(&db);
    for hex_addr in [
        "0x00000000000000000000000000000000000a0001",
        "0x00000000000000000000000000000000000a0002",
        "0x00000000000000000000000000000000000a0003",
    ] {
        let a = addr_from_hex(hex_addr);
        let s = accts.get(&a).expect("wallet credited");
        assert_eq!(
            s.balance(AssetId::Usd),
            1_000_000_000_000,
            "{hex_addr}: per-validator wallet must hold INITIAL_AMOUNT"
        );
    }

    // community_wallet at MYWALLET3 also holds INITIAL_AMOUNT.
    let cw_addr = addr_from_hex("0x000000000000000000000000000000000000c0de");
    let cw_state = accts.get(&cw_addr).expect("community_wallet credited");
    assert_eq!(cw_state.balance(AssetId::Usd), 1_000_000_000_000);
    assert_eq!(m.community_wallet_addr(), cw_addr);
}

#[test]
fn real_env_var_overrides_dotenv_value() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_test_vars();

    // Sentinel: set MYWALLET0 in the real env BEFORE load(). The .env
    // attempt to set it to the default value must be a no-op, so the
    // resulting validator wallet matches the env override.
    let override_addr = "0x0000000000000000000000000000000000000099";
    std::env::set_var("MYWALLET0", override_addr);

    let dir = tempdir().unwrap();
    let genesis_path = dir.path().join("genesis.jsonl");
    let dotenv_path = dir.path().join(".env");
    write_file(&genesis_path, GENESIS_JSONL);
    write_file(&dotenv_path, DOTENV);

    let spec = load(&genesis_path).expect("load with .env + override");
    assert_eq!(
        spec.validators[0].wallet.as_deref(),
        Some(override_addr),
        "real env var must beat .env",
    );
    // The other vars still came from .env.
    assert_eq!(
        spec.validators[1].wallet.as_deref(),
        Some("0x00000000000000000000000000000000000a0002"),
    );
}

#[test]
fn missing_dotenv_and_unset_var_yields_clear_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    // No .env, no real env. Substitution should fail with a message that
    // names the missing variable.
    std::env::remove_var("UL_DOTENV_INT_NOT_SET");
    let dir = tempdir().unwrap();
    let genesis_path = dir.path().join("genesis.jsonl");
    write_file(
        &genesis_path,
        r#"{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","wallet":"${UL_DOTENV_INT_NOT_SET}","weight":1,"initial_amount":"1"}
"#,
    );
    let err = load(&genesis_path).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("UL_DOTENV_INT_NOT_SET"), "{msg}");
    assert!(msg.contains("not set"), "{msg}");
}
