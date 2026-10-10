//! Apply a `GenesisSpec` to a fresh database, producing block 0 state.

use crate::spec::{FutureMarketRecord, GenesisSpec, PerpMarketRecord, ValidatorRecord};
use cb_crypto::{address_from_pubkey, keypair_from_seed};
use cb_state::{
    AccountState, Accounts, Database, FundingIndices, FuturesMarkets, MarginAccounts, MarketConfig,
    Markets, Meta, PerpMarkets, ValidatorInfo, Validators,
};
use cb_types::{
    Address, AssetId, DerivId, FundingIndex, FutureMarket, MarginAccount, MarketId, PerpMarket,
    H256,
};

/// Resolve a genesis asset name (`"btc"`, `"aapl"`, …) to its `AssetId`.
///
/// Delegates to `cb-types`, which drives the mapping off `ASSET_SPECS`, so the
/// set of names genesis accepts is exactly the listed asset universe and cannot
/// drift from it. The error lists the valid names, because the most common way
/// to hit it is a typo in a hand-edited genesis file.
fn parse_asset(s: &str) -> anyhow::Result<AssetId> {
    s.parse::<AssetId>().map_err(|_| {
        let known: Vec<&str> = AssetId::ALL.iter().map(|a| a.lower_symbol()).collect();
        anyhow::anyhow!(
            "unknown asset `{s}` (expected one of: {})",
            known.join(", ")
        )
    })
}

/// Resolve a genesis spot-market id (`"btc_usd"`, `"btcusd"`, …) to its `MarketId`.
///
/// Same delegation rationale as `parse_asset`.
fn parse_market(s: &str) -> anyhow::Result<MarketId> {
    s.parse::<MarketId>().map_err(|_| {
        let known: Vec<&str> = MarketId::ALL.iter().map(|m| m.genesis_id()).collect();
        anyhow::anyhow!(
            "unknown market `{s}` (expected one of: {})",
            known.join(", ")
        )
    })
}

/// Resolve a `balances` object to assets, refusing two keys that name the
/// same asset. The parser already refuses a literally repeated key; this
/// catches the spelling variants (`"usd"` and `"USD"`), which would otherwise
/// both be credited, doubling the seed.
fn parse_balances(
    owner: &str,
    balances: &std::collections::BTreeMap<String, crate::spec::U128Str>,
) -> anyhow::Result<Vec<(AssetId, u128)>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(balances.len());
    for (name, amt) in balances {
        let asset = parse_asset(name)?;
        anyhow::ensure!(
            seen.insert(asset),
            "`{owner}` balances name {asset} twice (as `{name}` and another spelling)"
        );
        out.push((asset, amt.0));
    }
    Ok(out)
}

/// Add a genesis credit, refusing one that would overflow the balance.
///
/// Credits to one address accumulate across records (a validator wallet can
/// also be the community wallet), so a sum can overflow even when every
/// number in the file fits. That is a malformed genesis, reported as one,
/// rather than a panic halfway through writing block 0.
fn credit(
    state: &mut AccountState,
    asset: AssetId,
    amount: u128,
    whose: &str,
) -> anyhow::Result<()> {
    state.add(asset, amount).map_err(|e| {
        anyhow::anyhow!("genesis credit of {amount} {asset} to {whose} overflows: {e:?}")
    })
}

fn parse_address(s: &str) -> anyhow::Result<Address> {
    let stripped = s.trim_start_matches("0x");
    let bytes = hex::decode(stripped)?;
    anyhow::ensure!(bytes.len() == 20, "address must be 20 bytes");
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(Address(out))
}

fn parse_pubkey(s: &str) -> anyhow::Result<Vec<u8>> {
    let stripped = s.trim_start_matches("0x");
    let bytes = hex::decode(stripped)?;
    anyhow::ensure!(
        bytes.len() == 33 || bytes.len() == 65,
        "pubkey must be 33 (compressed) or 65 (uncompressed) bytes, got {}",
        bytes.len()
    );
    Ok(bytes)
}

/// Resolve a `ValidatorRecord` into the `(addr, pubkey)` pair persisted in
/// `CF_VALIDATORS`. Two forms are supported:
///
/// 1. `seed` — derive keypair via `keypair_from_seed`. Used for dev clusters
///    where addresses don't matter; the seed itself is recorded as part of the
///    validator's identity off-chain (e.g. in supervisord configs).
/// 2. `addr` + `pubkey` — explicit production-style entry. Both must be
///    provided; the pubkey is verified to derive to the claimed address.
fn resolve_validator(rec: &ValidatorRecord) -> anyhow::Result<(Address, Vec<u8>)> {
    match (&rec.seed, &rec.addr, &rec.pubkey) {
        (Some(seed), None, None) => {
            let (_sk, pk) = keypair_from_seed(seed.as_bytes());
            Ok((address_from_pubkey(&pk), pk.serialize().to_vec()))
        }
        (None, Some(addr_str), Some(pk_str)) => {
            let addr = parse_address(addr_str)?;
            let pk_bytes = parse_pubkey(pk_str)?;
            // Verify the explicit pubkey actually derives to the claimed addr.
            let pk = cb_crypto::PublicKey::from_slice(&pk_bytes)
                .map_err(|e| anyhow::anyhow!("invalid pubkey bytes: {e}"))?;
            anyhow::ensure!(
                address_from_pubkey(&pk) == addr,
                "explicit validator pubkey does not derive to claimed address {addr}"
            );
            Ok((addr, pk_bytes))
        }
        _ => anyhow::bail!(
            "validator record must have either `seed` or both `addr` + `pubkey` (got: \
             seed={:?}, addr={:?}, pubkey={:?})",
            rec.seed,
            rec.addr,
            rec.pubkey
        ),
    }
}

/// Validate + persist perp markets in JSONL line order. Each record gets a
/// stable `DerivId::perp(N)` and a zeroed `FundingIndex` row. We hard-error
/// on duplicate ids inside the genesis file so operators can't silently lose
/// a market by typoing a name.
fn apply_perp_markets(db: &Database, recs: &[PerpMarketRecord]) -> anyhow::Result<()> {
    let pms = PerpMarkets::new(db);
    let funding = FundingIndices::new(db);
    let mut seen_ids = std::collections::HashSet::new();
    anyhow::ensure!(
        recs.len() <= u16::MAX as usize,
        "too many perp_market records (max {} per chain)",
        u16::MAX
    );
    for (i, r) in recs.iter().enumerate() {
        anyhow::ensure!(
            seen_ids.insert(r.id.clone()),
            "duplicate perp_market id `{}` in genesis",
            r.id
        );
        let did = DerivId::perp(i as u16);
        let pm = PerpMarket {
            id: did,
            underlying: parse_asset(&r.underlying)?,
            tick_size: r.tick_size,
            lot_size: r.lot_size,
            max_leverage_x100: r.max_leverage_x100,
            initial_margin_bps: r.initial_margin_bps,
            maintenance_margin_bps: r.maintenance_margin_bps,
            taker_fee_bps: r.taker_fee_bps,
            maker_fee_bps_signed: r.maker_fee_bps_signed,
            funding_interval_blocks: r.funding_interval_blocks,
            funding_rate_cap_q9: r.funding_rate_cap_q9,
            oracle_authority: parse_address(&r.oracle_authority)?,
            max_position_notional: r.max_position_notional,
            paused: r.paused,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        };
        pms.put(&pm);
        funding.put(&FundingIndex::new(did));
    }
    Ok(())
}

fn apply_future_markets(db: &Database, recs: &[FutureMarketRecord]) -> anyhow::Result<()> {
    let fms = FuturesMarkets::new(db);
    let mut seen_ids = std::collections::HashSet::new();
    anyhow::ensure!(
        recs.len() <= u16::MAX as usize,
        "too many future_market records (max {} per chain)",
        u16::MAX
    );
    for (i, r) in recs.iter().enumerate() {
        anyhow::ensure!(
            seen_ids.insert(r.id.clone()),
            "duplicate future_market id `{}` in genesis",
            r.id
        );
        anyhow::ensure!(
            matches!(r.settlement_method, 0 | 1),
            "future_market `{}` settlement_method must be 0 (TWAP) or 1 (oracle), got {}",
            r.id,
            r.settlement_method
        );
        let did = DerivId::future(i as u16);
        let fm = FutureMarket {
            id: did,
            underlying: parse_asset(&r.underlying)?,
            tick_size: r.tick_size,
            lot_size: r.lot_size,
            max_leverage_x100: r.max_leverage_x100,
            initial_margin_bps: r.initial_margin_bps,
            maintenance_margin_bps: r.maintenance_margin_bps,
            taker_fee_bps: r.taker_fee_bps,
            maker_fee_bps_signed: r.maker_fee_bps_signed,
            expiry_height: r.expiry_height,
            settlement_method: r.settlement_method,
            settlement_window_blocks: r.settlement_window_blocks,
            oracle_authority: parse_address(&r.oracle_authority)?,
            paused: r.paused,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        };
        fms.put(&fm);
    }
    Ok(())
}

pub fn apply(db: &Database, spec: &GenesisSpec) -> anyhow::Result<H256> {
    let chain = spec
        .chain
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("genesis missing chain record"))?;
    let meta = Meta::new(db);
    meta.set_chain_id(chain.chain_id);
    if let Some(d) = &chain.denom {
        meta.set_denom(d);
    }

    // Seed the insurance fund (M4). Goes onto a `MarginAccount` rather than a
    // spot balance because it's a USD-denominated pool that the liquidation
    // path expects under the `MarginAccounts` CF (see
    // `docs/design/perps-trading.md` §5.5 and the `apply_bad_debt_absorption`
    // path in `cb-block::executor`). Zero is the dev default; production
    // genesis files should set this to ≥ 1e12 micro-USD ($1M).
    if chain.insurance_fund_seed > 0 {
        MarginAccounts::new(db).put(&MarginAccount {
            owner: Address::INSURANCE,
            collateral: chain.insurance_fund_seed,
            unrealized_pnl: cb_types::Signed::ZERO,
            bad_debt: 0,
        });
    }

    // A spot market listed twice would take whichever line came last, with
    // nothing to say the first was ever there. Refused, like every other
    // record kind; `"btc_usd"` and `"btcusd"` are the same market, so the
    // check is on the resolved id, not the spelling.
    let markets = Markets::new(db);
    let mut seen_markets = std::collections::HashSet::new();
    for m in &spec.markets {
        let id = parse_market(&m.id)?;
        anyhow::ensure!(
            seen_markets.insert(id),
            "spot market `{}` is listed twice in genesis",
            m.id
        );
        markets.put(&MarketConfig {
            id,
            base: parse_asset(&m.base)?,
            quote: parse_asset(&m.quote)?,
            tick_size: m.tick_size,
            min_qty: m.min_qty,
            fee_bps: m.fee_bps,
        });
    }

    // Derivative markets. The Nth `perp_market` record gets `DerivId::perp(N)`,
    // the Nth `future_market` gets `DerivId::future(N)` — registration order is
    // the JSONL line order, same convention as validators today. Each perp also
    // gets a zeroed `FundingIndex` row so funding-tick handlers (M2) can read
    // current state without a per-market init hop.
    apply_perp_markets(db, &spec.perp_markets)?;
    apply_future_markets(db, &spec.future_markets)?;

    // Pre-seed `MarginAccount` rows. Each entry credits the address with
    // `collateral` USD on the `MarginAccounts` CF — distinct from the spot
    // `Accounts` CF. Demo-only on a fresh chain because `AdjustMargin` is
    // currently `Unsupported` (see executor.rs); without these seeds no
    // address can place a perp order. Hard-error on duplicate addresses
    // and on collisions with the protocol-pinned insurance fund address.
    {
        let margins = MarginAccounts::new(db);
        let mut seen = std::collections::HashSet::new();
        for r in &spec.margin_accounts {
            let addr = parse_address(&r.address)?;
            anyhow::ensure!(
                addr != Address::INSURANCE,
                "margin_account address `{}` collides with protocol insurance fund",
                r.address
            );
            anyhow::ensure!(
                seen.insert(addr),
                "duplicate margin_account address `{}` in genesis",
                r.address
            );
            margins.put(&MarginAccount {
                owner: addr,
                collateral: r.collateral,
                unrealized_pnl: cb_types::Signed::ZERO,
                bad_debt: 0,
            });
        }
    }

    let accts = Accounts::new(db);
    if let Some(c) = &spec.company {
        let addr = parse_address(&c.address)?;
        let mut state = AccountState::default();
        for (asset, amt) in parse_balances(&c.address, &c.balances)? {
            credit(&mut state, asset, amt, &c.address)?;
        }
        accts.put(&addr, &state);
        meta.set_company_addr(addr);
    }

    if let Some(c) = &spec.community_wallet {
        let addr = parse_address(&c.address)?;
        if let Some(co) = &spec.company {
            anyhow::ensure!(
                parse_address(&co.address)? != addr,
                "community_wallet address must differ from company address"
            );
        }
        let mut state = accts.get(&addr).unwrap_or_default();
        for (asset, amt) in parse_balances(&c.address, &c.balances)? {
            credit(&mut state, asset, amt, &c.address)?;
        }
        accts.put(&addr, &state);
        meta.set_community_wallet_addr(addr);
    }

    // BFT validator set: resolve each record into (addr, pubkey), persist into
    // CF_VALIDATORS, and credit the validator's `initial_amount` of USD. The
    // credit goes to the validator's `wallet` field if set, otherwise to its
    // own address. Order is preserved — validators[0] is the leader for view 0.
    anyhow::ensure!(
        !spec.validators.is_empty(),
        "genesis must define at least one validator"
    );
    let mut infos: Vec<ValidatorInfo> = Vec::with_capacity(spec.validators.len());
    let mut seen = std::collections::HashSet::new();
    for v in &spec.validators {
        let (addr, pubkey) = resolve_validator(v)?;
        anyhow::ensure!(
            seen.insert(addr),
            "duplicate validator address {addr} in genesis"
        );
        infos.push(ValidatorInfo {
            addr,
            pubkey,
            weight: v.weight,
        });
        if v.initial_amount > 0 {
            let credit_to = match &v.wallet {
                Some(w) => parse_address(w)?,
                None => addr,
            };
            let mut s = accts.get(&credit_to).unwrap_or_default();
            credit(
                &mut s,
                AssetId::Usd,
                v.initial_amount,
                "validator initial_amount",
            )?;
            accts.put(&credit_to, &s);
        }
    }
    Validators::new(db).put_set(&infos);
    meta.set_view(0);
    meta.set_proposer_addr(infos[0].addr);

    if let Some(sim) = &spec.sim {
        for i in 0..sim.num_traders {
            let seed = format!("{}{i}", sim.seed_prefix);
            let (_, pk) = keypair_from_seed(seed.as_bytes());
            let trader_addr = address_from_pubkey(&pk);
            // Additive, like every other genesis credit. A fresh state here
            // used to overwrite whatever an earlier record had credited to the
            // same address, so a validator wallet that happened to be a sim
            // trader lost its stake without a word.
            let mut s = accts.get(&trader_addr).unwrap_or_default();
            for (asset, amt) in parse_balances("sim", &sim.balances)? {
                credit(&mut s, asset, amt, "sim trader")?;
            }
            accts.put(&trader_addr, &s);
        }
    }

    // The genesis root hashes every committed row once; each block after it
    // chains the hash of what that block changed (`cb_state::state_hash`).
    let state_root = cb_state::genesis_state_hash(db)?;
    meta.set_state_hash_scheme(cb_state::STATE_HASH_SCHEME);
    meta.set_head(0, H256::ZERO, state_root);
    Ok(state_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::GenesisSpec;
    use tempfile::tempdir;

    const SPEC_JSONL: &str = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"cb-validator-0","weight":1,"initial_amount":"100000"}
{"kind":"validator","seed":"cb-validator-1","weight":1,"initial_amount":"100000"}
{"kind":"validator","seed":"cb-validator-2","weight":1,"initial_amount":"100000"}
{"kind":"market","id":"btc_usd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":0}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"1000"}}
"#;

    fn three_validator_spec() -> GenesisSpec {
        GenesisSpec::from_jsonl(SPEC_JSONL).unwrap()
    }

    #[test]
    fn apply_persists_three_validators_and_credits_initial_amount() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &three_validator_spec()).unwrap();

        let v = Validators::new(&db);
        assert_eq!(v.count(), 3);
        let list = v.list();

        let accts = Accounts::new(&db);
        for info in &list {
            let s = accts.get(&info.addr).unwrap();
            assert_eq!(
                s.balance(AssetId::Usd),
                100_000,
                "validator {} should have its initial_amount in USD",
                info.addr
            );
        }

        let meta = Meta::new(&db);
        assert_eq!(meta.view(), 0);
        assert_eq!(meta.proposer_addr(), list[0].addr);
        assert_eq!(meta.head_height(), 0);
    }

    #[test]
    fn apply_rejects_duplicate_validator_addresses() {
        let mut spec = three_validator_spec();
        spec.validators[1].seed = Some("cb-validator-0".into());
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("duplicate validator address"));
    }

    #[test]
    fn explicit_validator_with_mismatched_pubkey_rejected() {
        let line = r#"{"kind":"validator","addr":"0x000000000000000000000000000000000000aaaa","pubkey":"0x020000000000000000000000000000000000000000000000000000000000000001","weight":1,"initial_amount":"0"}"#;
        let rec: crate::spec::GenesisRecord = serde_json::from_str(line).unwrap();
        let v = match rec {
            crate::spec::GenesisRecord::Validator(v) => v,
            _ => panic!("wrong"),
        };
        let err = resolve_validator(&v).unwrap_err();
        assert!(err.to_string().contains("does not derive"));
    }

    #[test]
    fn validator_initial_amount_zero_does_not_create_balance_entry() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"0"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();
        let info = Validators::new(&db).get_by_index(0).unwrap();
        // No balance was credited but the validator is still in the set.
        let acct = Accounts::new(&db).get(&info.addr).unwrap_or_default();
        assert_eq!(acct.balance(AssetId::Usd), 0);
    }

    #[test]
    fn validator_wallet_field_redirects_initial_amount() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","wallet":"0x00000000000000000000000000000000000000aa","weight":1,"initial_amount":"777"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();

        let info = Validators::new(&db).get_by_index(0).unwrap();
        let accts = Accounts::new(&db);
        // Validator's own address has no balance — credit went to the wallet.
        let own = accts.get(&info.addr).unwrap_or_default();
        assert_eq!(own.balance(AssetId::Usd), 0);

        let mut wallet_bytes = [0u8; 20];
        wallet_bytes[19] = 0xaa;
        let wallet = Address(wallet_bytes);
        let w = accts.get(&wallet).unwrap();
        assert_eq!(w.balance(AssetId::Usd), 777);
    }

    #[test]
    fn validator_without_wallet_credits_own_address() {
        // Regression: omitting `wallet` keeps the existing default.
        apply(
            &Database::open(tempdir().unwrap().path()).unwrap(),
            &three_validator_spec(),
        )
        .unwrap();
        // Already covered by apply_persists_three_validators_and_credits_initial_amount;
        // this just pins that the default-None path still compiles.
    }

    #[test]
    fn invalid_validator_wallet_address_rejected() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","wallet":"0xnothex","weight":1,"initial_amount":"1"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("hex") || msg.contains("Invalid character") || msg.contains("address"),
            "unexpected error: {msg}",
        );
    }

    #[test]
    fn community_wallet_credited_and_meta_set() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0de","balances":{"usd":"5000","btc":"3"}}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();

        let mut bytes = [0u8; 20];
        bytes[18] = 0xc0;
        bytes[19] = 0xde;
        let cw_addr = Address(bytes);

        let meta = Meta::new(&db);
        assert_eq!(meta.community_wallet_addr(), cw_addr);

        let st = Accounts::new(&db).get(&cw_addr).unwrap();
        assert_eq!(st.balance(AssetId::Usd), 5000);
        assert_eq!(st.balance(AssetId::Btc), 3);
    }

    #[test]
    fn community_wallet_collides_with_company_rejected() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"company","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"1"}}
{"kind":"community_wallet","address":"0x000000000000000000000000000000000000c0c0","balances":{"usd":"1"}}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("must differ from company"));
    }

    #[test]
    fn community_wallet_absent_leaves_meta_zero() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &three_validator_spec()).unwrap();
        // SPEC_JSONL has no community_wallet record — meta should remain zero.
        assert_eq!(Meta::new(&db).community_wallet_addr(), Address::ZERO);
    }

    // ---------------- derivative markets ----------------

    const SPEC_WITH_PERP: &str = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"perp_market","id":"btc_perp","underlying":"btc","tick_size":"100","lot_size":"1000","max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"0x000000000000000000000000000000000000a0a0","max_position_notional":"100000000000000","paused":false}
{"kind":"perp_market","id":"eth_perp","underlying":"eth","tick_size":"100","lot_size":"1000","max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"0x000000000000000000000000000000000000a0a0","max_position_notional":"100000000000000","paused":false}
{"kind":"future_market","id":"btc_2026q1","underlying":"btc","tick_size":"100","lot_size":"1000","max_leverage_x100":2500,"initial_margin_bps":400,"maintenance_margin_bps":200,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"expiry_height":2592000,"settlement_method":0,"settlement_window_blocks":600,"oracle_authority":"0x000000000000000000000000000000000000a0a0","paused":false}
"#;

    #[test]
    fn applies_perp_and_future_markets_in_registration_order() {
        let spec = GenesisSpec::from_jsonl(SPEC_WITH_PERP).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();

        let pms = cb_state::PerpMarkets::new(&db);
        let list = pms.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, cb_types::DerivId::perp(0));
        assert_eq!(list[0].underlying, cb_types::AssetId::Btc);
        assert_eq!(list[1].id, cb_types::DerivId::perp(1));
        assert_eq!(list[1].underlying, cb_types::AssetId::Eth);

        // Funding index pre-seeded with zeros so M2 funding-tick handlers can
        // read state without a per-market init hop.
        let funding = cb_state::FundingIndices::new(&db);
        let f0 = funding.get(&cb_types::DerivId::perp(0)).unwrap();
        assert_eq!(f0.cumulative, cb_types::Signed::ZERO);
        assert_eq!(f0.last_tick_height, 0);
        assert_eq!(f0.rate_q9, 0);

        let fms = cb_state::FuturesMarkets::new(&db);
        let f_list = fms.list();
        assert_eq!(f_list.len(), 1);
        assert_eq!(f_list[0].id, cb_types::DerivId::future(0));
        assert_eq!(f_list[0].expiry_height, 2_592_000);
        assert_eq!(f_list[0].final_settlement_px_q8, None);
    }

    #[test]
    fn duplicate_perp_market_id_rejected() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"perp_market","id":"btc_perp","underlying":"btc","tick_size":"1","lot_size":"1","max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"0x000000000000000000000000000000000000a0a0"}
{"kind":"perp_market","id":"btc_perp","underlying":"eth","tick_size":"1","lot_size":"1","max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"0x000000000000000000000000000000000000a0a0"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("duplicate perp_market id"));
    }

    #[test]
    fn future_market_invalid_settlement_method_rejected() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"future_market","id":"btc_2026q1","underlying":"btc","tick_size":"1","lot_size":"1","max_leverage_x100":2500,"initial_margin_bps":400,"maintenance_margin_bps":200,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"expiry_height":1,"settlement_method":7,"settlement_window_blocks":1,"oracle_authority":"0x000000000000000000000000000000000000a0a0"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("settlement_method"));
    }

    #[test]
    fn empty_perp_section_keeps_genesis_clean() {
        // Regression: a chain with no derivative markets must still apply
        // cleanly and produce a state root identical to the pre-perp shape
        // (modulo the M1 widening).
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let r1 = apply(&db, &three_validator_spec()).unwrap();
        // Reproducible: same spec applied to a fresh DB yields the same root.
        let dir2 = tempdir().unwrap();
        let db2 = Database::open(dir2.path()).unwrap();
        let r2 = apply(&db2, &three_validator_spec()).unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn perp_market_field_order_in_jsonl_does_not_affect_root() {
        // Reproducibility: the spec gathers `perp_market` records in JSONL
        // line order; applying the same spec twice yields the same state
        // root. We don't rely on JSON field order within a record (serde does
        // by-name parsing), so reorder the fields and re-check.
        let alt = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"perp_market","oracle_authority":"0x000000000000000000000000000000000000a0a0","id":"btc_perp","underlying":"btc","funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"tick_size":"100","lot_size":"1000","max_position_notional":"100000000000000","paused":false}
"#;
        let canonical = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v0","weight":1,"initial_amount":"1"}
{"kind":"perp_market","id":"btc_perp","underlying":"btc","tick_size":"100","lot_size":"1000","max_leverage_x100":5000,"initial_margin_bps":200,"maintenance_margin_bps":100,"taker_fee_bps":3,"maker_fee_bps_signed":-1,"funding_interval_blocks":3600,"funding_rate_cap_q9":1000000,"oracle_authority":"0x000000000000000000000000000000000000a0a0","max_position_notional":"100000000000000","paused":false}
"#;
        let dir1 = tempdir().unwrap();
        let db1 = Database::open(dir1.path()).unwrap();
        let r1 = apply(&db1, &GenesisSpec::from_jsonl(alt).unwrap()).unwrap();
        let dir2 = tempdir().unwrap();
        let db2 = Database::open(dir2.path()).unwrap();
        let r2 = apply(&db2, &GenesisSpec::from_jsonl(canonical).unwrap()).unwrap();
        assert_eq!(r1, r2);
    }

    // ---------------- margin_account seeding ----------------

    #[test]
    fn margin_account_records_seed_collateral() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000beef","collateral":"500000000"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000cafe","collateral":"42"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();

        let margins = MarginAccounts::new(&db);
        let mut beef = [0u8; 20];
        beef[18] = 0xbe;
        beef[19] = 0xef;
        let mut cafe = [0u8; 20];
        cafe[18] = 0xca;
        cafe[19] = 0xfe;
        let beef_acct = margins.get(&Address(beef)).unwrap();
        assert_eq!(beef_acct.collateral, 500_000_000);
        assert_eq!(beef_acct.bad_debt, 0);
        let cafe_acct = margins.get(&Address(cafe)).unwrap();
        assert_eq!(cafe_acct.collateral, 42);
    }

    #[test]
    fn duplicate_margin_account_address_rejected() {
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000beef","collateral":"1"}
{"kind":"margin_account","address":"0x000000000000000000000000000000000000beef","collateral":"2"}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("duplicate margin_account"));
    }

    #[test]
    fn margin_account_collision_with_insurance_fund_rejected() {
        // `Address::INSURANCE` is the protocol-pinned slot; genesis must not
        // overwrite it with a user-controlled record.
        let insurance_hex = format!("0x{}", hex::encode(Address::INSURANCE.0));
        let s = format!(
            r#"
{{"kind":"chain","chain_id":1,"insurance_fund_seed":"100"}}
{{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}}
{{"kind":"margin_account","address":"{insurance_hex}","collateral":"1"}}
"#
        );
        let spec = GenesisSpec::from_jsonl(&s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("insurance fund"));
    }

    // ---------------- derivative records: the checks before the first write ----------------

    fn perp_record(id: &str, underlying: &str, oracle: &str) -> crate::spec::PerpMarketRecord {
        crate::spec::PerpMarketRecord {
            id: id.into(),
            underlying: underlying.into(),
            tick_size: 1,
            lot_size: 1,
            max_leverage_x100: 5_000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: 3_600,
            funding_rate_cap_q9: 1_000_000,
            oracle_authority: oracle.into(),
            max_position_notional: 0,
            paused: false,
        }
    }

    const ORACLE: &str = "0x000000000000000000000000000000000000b7c0";

    #[test]
    fn more_perp_markets_than_a_u16_can_index_are_refused_before_anything_is_written() {
        // `DerivId::perp(N)` carries N in a u16. One record past the cap would
        // wrap onto index 0 and silently alias the first market; the cap is
        // checked before the loop so no row lands.
        let mut spec = three_validator_spec();
        spec.perp_markets = (0..=u16::MAX as usize)
            .map(|i| perp_record(&format!("perp_{i}"), "btc", ORACLE))
            .collect();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err();
        assert!(err.to_string().contains("too many perp_market"), "{err}");
        assert!(
            PerpMarkets::new(&db).get(&DerivId::perp(0)).is_none(),
            "the cap is checked before the first market is written"
        );
    }

    #[test]
    fn exactly_u16_max_perp_markets_are_allowed() {
        // The boundary itself: the last record is `perp(65534)`, still in range.
        let mut spec = three_validator_spec();
        spec.perp_markets = (0..u16::MAX as usize)
            .map(|i| perp_record(&format!("perp_{i}"), "btc", ORACLE))
            .collect();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        apply(&db, &spec).unwrap();
        assert!(PerpMarkets::new(&db)
            .get(&DerivId::perp(u16::MAX - 1))
            .is_some());
        assert!(PerpMarkets::new(&db)
            .get(&DerivId::perp(u16::MAX))
            .is_none());
    }

    #[test]
    fn a_perp_on_an_unknown_underlying_is_refused_and_the_error_lists_the_assets() {
        let mut spec = three_validator_spec();
        spec.perp_markets = vec![perp_record("doge_perp", "doge", ORACLE)];
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let err = apply(&db, &spec).unwrap_err().to_string();
        assert!(err.contains("unknown asset `doge`"), "{err}");
        assert!(err.contains("btc") && err.contains("spy"), "{err}");
    }

    #[test]
    fn a_perp_with_a_malformed_oracle_authority_is_refused() {
        for (oracle, why) in [
            ("0x1234", "too short"),
            ("0x000000000000000000000000000000000000b7c0ff", "too long"),
            ("0xzz00000000000000000000000000000000000000", "not hex"),
            ("", "empty"),
        ] {
            let mut spec = three_validator_spec();
            spec.perp_markets = vec![perp_record("btc_perp", "btc", oracle)];
            let dir = tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            assert!(
                apply(&db, &spec).is_err(),
                "oracle `{oracle}` ({why}) was accepted"
            );
        }
    }

    #[test]
    fn a_spot_market_listed_twice_is_refused() {
        // Before this check the second line silently won (fee_bps 20 stored).
        for second in ["btc_usd", "btcusd", "BTC_USD"] {
            let s = format!(
                r#"
{{"kind":"chain","chain_id":1}}
{{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}}
{{"kind":"market","id":"btc_usd","base":"btc","quote":"usd","tick_size":1,"min_qty":1,"fee_bps":10}}
{{"kind":"market","id":"{second}","base":"btc","quote":"usd","tick_size":2,"min_qty":2,"fee_bps":20}}
"#
            );
            let spec = GenesisSpec::from_jsonl(&s).unwrap();
            let dir = tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            let err = apply(&db, &spec).unwrap_err().to_string();
            assert!(err.contains("listed twice"), "`{second}`: {err}");
        }
    }

    #[test]
    fn two_spellings_of_one_asset_in_a_balances_object_are_refused() {
        for kind in ["company", "community_wallet"] {
            let s = format!(
                r#"
{{"kind":"chain","chain_id":1}}
{{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}}
{{"kind":"{kind}","address":"0x000000000000000000000000000000000000c0c0","balances":{{"usd":"1","USD":"2"}}}}
"#
            );
            let spec = GenesisSpec::from_jsonl(&s).unwrap();
            let dir = tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            let err = apply(&db, &spec).unwrap_err().to_string();
            assert!(err.contains("twice"), "{kind}: {err}");
        }
        let s = r#"
{"kind":"chain","chain_id":1}
{"kind":"validator","seed":"v","weight":1,"initial_amount":"1"}
{"kind":"sim","num_traders":2,"balances":{"btc":"1","Btc":"1"}}
"#;
        let spec = GenesisSpec::from_jsonl(s).unwrap();
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        assert!(apply(&db, &spec).unwrap_err().to_string().contains("twice"));
    }
}
