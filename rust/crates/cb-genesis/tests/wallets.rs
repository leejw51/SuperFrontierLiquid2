//! Coverage for the newly-added `wallet` field on validators and the
//! `community_wallet` record. These tests pin invariants the parallel agents
//! depend on:
//!
//! - Per-validator `wallet` redirect is *additive* if multiple validators
//!   point at the same wallet — i.e. the second credit adds to the first
//!   rather than overwriting it. Easy to break with a careless `put` over
//!   `accts.get(...).unwrap_or_default()`.
//! - `company` and `community_wallet` records can coexist; both balances
//!   land at genesis with their own balances and `Meta` keys are independent.
//! - Genesis `Meta` survives a DB reopen (RocksDB durability sanity).

use cb_genesis::{apply, GenesisSpec};
use cb_state::{Accounts, Database, Meta};
use cb_types::{Address, AssetId};
use tempfile::tempdir;

fn addr_with_last(byte: u8) -> Address {
    let mut a = [0u8; 20];
    a[19] = byte;
    Address(a)
}

#[test]
fn shared_wallet_across_validators_credits_additively() {
    // Two validators both name the same `wallet` — the wallet's USD balance
    // must be the sum of both `initial_amount`s (300 + 700 = 1000), not the
    // last-write-wins value.
    let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","wallet":"0x00000000000000000000000000000000000000aa","weight":1,"initial_amount":"300"}
{"kind":"validator","seed":"v1","wallet":"0x00000000000000000000000000000000000000aa","weight":1,"initial_amount":"700"}
"#;
    let spec = GenesisSpec::from_jsonl(s).unwrap();
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    apply(&db, &spec).unwrap();

    let wallet = addr_with_last(0xaa);
    let st = Accounts::new(&db).get(&wallet).unwrap();
    assert_eq!(
        st.balance(AssetId::Usd),
        1_000,
        "shared wallet must accumulate both validators' initial_amounts",
    );
}

#[test]
fn shared_wallet_overlapping_with_one_validator_own_addr() {
    // Validator 0 has a wallet pointing at validator 1's own address (no
    // `wallet` set on validator 1). Both credits should still land on the
    // same address additively.
    let v1_addr_hex = {
        let (_, pk) = cb_crypto::keypair_from_seed(b"v-shared-1");
        let a = cb_crypto::address_from_pubkey(&pk);
        format!("0x{}", hex::encode(a.0))
    };

    let s = format!(
        r#"
{{"kind":"chain","chain_id":1}}
{{"kind":"validator","seed":"v-shared-0","wallet":"{}","weight":1,"initial_amount":"100"}}
{{"kind":"validator","seed":"v-shared-1","weight":1,"initial_amount":"50"}}
"#,
        v1_addr_hex
    );
    let spec = GenesisSpec::from_jsonl(&s).unwrap();
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    apply(&db, &spec).unwrap();

    let (_, pk) = cb_crypto::keypair_from_seed(b"v-shared-1");
    let v1_addr = cb_crypto::address_from_pubkey(&pk);
    let st = Accounts::new(&db).get(&v1_addr).unwrap();
    assert_eq!(
        st.balance(AssetId::Usd),
        150,
        "validator 1's own addr must hold its own credit (50) plus the redirect from v0 (100)",
    );
}

#[test]
fn company_and_community_wallet_coexist_with_distinct_balances() {
    // Both records exist with different addresses, different USD balances,
    // and different additional asset balances. After `apply`, both meta
    // pointers are set, both account states are independent, and neither
    // overwrites the other.
    let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"111","btc":"22"}}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"333","eth":"44"}}
"#;
    let spec = GenesisSpec::from_jsonl(s).unwrap();
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    apply(&db, &spec).unwrap();

    let mut co = [0u8; 20];
    co[18] = 0xc0;
    co[19] = 0xc0;
    let company = Address(co);
    let mut cw = [0u8; 20];
    cw[18] = 0xc0;
    cw[19] = 0xde;
    let community = Address(cw);

    let meta = Meta::new(&db);
    assert_eq!(meta.company_addr(), company);
    assert_eq!(meta.community_wallet_addr(), community);
    assert_ne!(
        meta.company_addr(),
        meta.community_wallet_addr(),
        "company and community wallet meta keys must be independent",
    );

    let accts = Accounts::new(&db);
    let co_state = accts.get(&company).unwrap();
    let cw_state = accts.get(&community).unwrap();
    assert_eq!(co_state.balance(AssetId::Usd), 111);
    assert_eq!(co_state.balance(AssetId::Btc), 22);
    assert_eq!(co_state.balance(AssetId::Eth), 0);
    assert_eq!(cw_state.balance(AssetId::Usd), 333);
    assert_eq!(cw_state.balance(AssetId::Eth), 44);
    assert_eq!(cw_state.balance(AssetId::Btc), 0);
}

#[test]
fn community_wallet_meta_pointer_survives_db_reopen() {
    // Apply genesis with a community wallet, drop the Database handle, reopen
    // the same path, assert the pointer + balance are still there. Pins that
    // the genesis writes are flushed (not just buffered in the WriteBatch).
    let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"4242"}}
"#;
    let spec = GenesisSpec::from_jsonl(s).unwrap();
    let dir = tempdir().unwrap();
    {
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();
    } // drop DB

    let db = Database::open(dir.path()).unwrap();
    let mut cw = [0u8; 20];
    cw[18] = 0xc0;
    cw[19] = 0xde;
    let community = Address(cw);
    assert_eq!(Meta::new(&db).community_wallet_addr(), community);
    assert_eq!(
        Accounts::new(&db)
            .get(&community)
            .unwrap()
            .balance(AssetId::Usd),
        4242,
    );
}
