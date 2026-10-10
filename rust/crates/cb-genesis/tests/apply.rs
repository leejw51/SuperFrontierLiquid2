use cb_genesis::{apply, GenesisSpec};
use cb_state::{Accounts, Database, Meta, Validators};
use cb_types::{Address, AssetId};
use tempfile::tempdir;

const SPEC_JSONL: &str = r#"
{"kind":"chain","chain_id":7}
{"kind":"validator","seed":"cb-validator-0","wallet":"0x00000000000000000000000000000000000a0001","weight":1,"initial_amount":"500"}
{"kind":"validator","seed":"cb-validator-1","weight":1,"initial_amount":"500"}
{"kind":"validator","seed":"cb-validator-2","weight":1,"initial_amount":"500"}
{"kind":"market","id":"btc_usd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}
{"kind":"market","id":"eth_usd","base":"eth","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"btc":"100","eth":"200","usd":"300"}}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"9000"}}
"#;

#[test]
fn applying_genesis_seeds_company_chain_id_validators_and_initial_amounts() {
    let spec = GenesisSpec::from_jsonl(SPEC_JSONL).unwrap();
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let state_root = apply(&db, &spec).unwrap();
    assert_ne!(state_root.0, [0u8; 32]);

    let m = Meta::new(&db);
    assert_eq!(m.chain_id(), 7);
    assert_eq!(m.head_height(), 0);
    assert_eq!(m.view(), 0);

    let accts = Accounts::new(&db);
    let mut a = [0u8; 20];
    a[18] = 0xc0;
    a[19] = 0xc0;
    let st = accts.get(&Address(a)).unwrap();
    assert_eq!(st.balance(AssetId::Btc), 100);
    assert_eq!(st.balance(AssetId::Eth), 200);
    assert_eq!(st.balance(AssetId::Usd), 300);
    assert_eq!(m.company_addr(), Address(a));

    // community_wallet: balance and meta pointer.
    let mut cw = [0u8; 20];
    cw[18] = 0xc0;
    cw[19] = 0xde;
    let cw_addr = Address(cw);
    assert_eq!(m.community_wallet_addr(), cw_addr);
    let cw_state = accts.get(&cw_addr).unwrap();
    assert_eq!(cw_state.balance(AssetId::Usd), 9000);

    let v = Validators::new(&db);
    assert_eq!(v.count(), 3);
    let list = v.list();
    assert_eq!(m.proposer_addr(), list[0].addr);

    // Validator 0 has a wallet — its initial_amount goes to the wallet, not its own addr.
    let mut wallet0 = [0u8; 20];
    wallet0[17] = 0x0a;
    wallet0[19] = 0x01;
    let v0_wallet = Address(wallet0);
    assert_eq!(
        accts.get(&v0_wallet).unwrap().balance(AssetId::Usd),
        500,
        "validator 0's initial_amount should be credited to its `wallet` field"
    );
    let v0_own = accts.get(&list[0].addr).unwrap_or_default();
    assert_eq!(
        v0_own.balance(AssetId::Usd),
        0,
        "validator 0's own address should have no USD when a wallet is set"
    );

    // Validators 1 and 2 have no wallet — initial_amount goes to their own address.
    for info in &list[1..] {
        assert_eq!(info.pubkey.len(), 33);
        let s = accts.get(&info.addr).unwrap();
        assert_eq!(
            s.balance(AssetId::Usd),
            500,
            "validator {} without wallet should hold its initial_amount",
            info.addr
        );
    }
}
