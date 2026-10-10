//! Loads the committed `genesis-full.jsonl` and applies it.
//!
//! This file has one job beyond "the genesis parses": it is the **cross-check
//! that makes it safe for the market universe to be written down twice** — once
//! as data in `genesis-full.jsonl`, once as a table in `cb_types::symbols`.
//!
//! Genesis assigns `DerivId::perp(N)` by line order, so the symbol table cannot
//! know what index a contract has without repeating the ordering. Two sources of
//! truth are tolerable only while something asserts they agree; that is
//! `perp_lines_match_the_symbol_table` and `future_lines_match_the_symbol_table`
//! below. If they fail, the wire symbol `"AAPL-PERP"` and the on-chain market
//! `DerivId::perp(3)` have come apart, and orders would be routed to the wrong
//! contract.
//!
//! Lives in its own test binary so its `set_var` calls cannot race the env other
//! genesis tests set up — each integration test file is a separate process.

use std::path::PathBuf;
use tempfile::tempdir;

use cb_genesis::{apply, GenesisSpec};
use cb_state::{
    Accounts, Database, FuturesMarkets, MarginAccounts, MarketConfig, Markets, Meta, PerpMarkets,
    Validators,
};
use cb_types::symbols::{FUTURES, PERPS};
use cb_types::{Address, AssetId, DerivId, MarketId};

/// Deterministic dev values for every `${VAR}` the file references.
fn set_full_env() {
    std::env::set_var("MYCHAINID", "1");
    std::env::set_var("MYDENOM", "ULD");
    std::env::set_var("INITIAL_AMOUNT", "1000000000000");
    std::env::set_var("MYWALLET0", "0x000000000000000000000000000000000000a0a0");
    std::env::set_var("MYWALLET1", "0x000000000000000000000000000000000000a0a1");
    std::env::set_var("MYWALLET2", "0x000000000000000000000000000000000000a0a2");
    std::env::set_var("MYWALLET3", "0x000000000000000000000000000000000000c0de");
    std::env::set_var(
        "ORACLE_AUTHORITY",
        "0x0000000000000000000000000000000000020ac1",
    );
    // The treasury is the only account permitted to mint, so genesis-full names
    // it through a variable rather than a literal: the address has to be one
    // somebody holds a key for, and an earlier version used a vanity address
    // nobody could sign as, which meant the faucet could never pay out.
    std::env::set_var("TREASURY", "0x00000000000000000000000000000000000c0c0d");
    std::env::set_var("DEMO_TRADER0", "0x0000000000000000000000000000000000d10000");
    std::env::set_var("DEMO_TRADER1", "0x0000000000000000000000000000000000d10001");
    std::env::set_var("DEMO_TRADER2", "0x0000000000000000000000000000000000d10002");
}

fn full_genesis_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/cb-genesis`; the file lives two up.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // rust/
    p.push("genesis-full.jsonl");
    p
}

fn load_spec() -> GenesisSpec {
    set_full_env();
    let path = full_genesis_path();
    assert!(
        path.exists(),
        "genesis-full.jsonl missing at {} — committed file moved?",
        path.display()
    );
    let text = std::fs::read_to_string(&path).expect("read genesis-full.jsonl");
    GenesisSpec::from_jsonl(&text).expect("parse genesis-full.jsonl")
}

fn applied_db(dir: &tempfile::TempDir) -> Database {
    let spec = load_spec();
    let db = Database::open(dir.path()).expect("open db");
    apply(&db, &spec).expect("apply genesis-full.jsonl");
    db
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn genesis_full_parses_and_declares_the_whole_universe() {
    let spec = load_spec();
    assert_eq!(
        spec.validators.len(),
        4,
        "4 validators: a BFT quorum of three with one to spare"
    );
    assert_eq!(
        spec.markets.len(),
        MarketId::ALL.len(),
        "every listed spot market must appear exactly once"
    );
    assert_eq!(spec.perp_markets.len(), PERPS.len(), "one perp per listing");
    assert_eq!(
        spec.future_markets.len(),
        FUTURES.len(),
        "one line per dated future"
    );
    assert!(
        spec.future_markets.len() >= 2,
        "the universe needs at least two dated futures"
    );
    assert_eq!(spec.margin_accounts.len(), 3, "3 demo traders");
    assert!(spec.sim.is_some(), "seeded simulation accounts");
    assert!(spec.company.is_some());
    assert!(spec.community_wallet.is_some());
}

#[test]
fn every_spot_market_is_listed_exactly_once_and_quotes_usd() {
    let spec = load_spec();
    let mut seen: Vec<MarketId> = Vec::new();
    for m in &spec.markets {
        let id: MarketId =
            m.id.parse()
                .unwrap_or_else(|e| panic!("market `{}`: {e}", m.id));
        assert!(!seen.contains(&id), "spot market {id} listed twice");
        seen.push(id);

        let base: AssetId = m
            .base
            .parse()
            .unwrap_or_else(|e| panic!("base `{}`: {e}", m.base));
        let quote: AssetId = m
            .quote
            .parse()
            .unwrap_or_else(|e| panic!("quote `{}`: {e}", m.quote));
        assert_eq!(base, id.base(), "{id} base disagrees with the MarketId");
        assert_eq!(quote, AssetId::Usd, "{id} must be USD-quoted");
        assert!(m.tick_size > 0, "{id} tick_size must be positive");
        assert!(m.min_qty > 0, "{id} min_qty must be positive");
    }
    seen.sort_by_key(|m| m.as_u8());
    assert_eq!(seen, MarketId::ALL.to_vec());
}

// ---------------------------------------------------------------------------
// The cross-check: genesis line order vs. the symbol table
// ---------------------------------------------------------------------------

#[test]
fn perp_lines_match_the_symbol_table() {
    let spec = load_spec();
    for (i, (line, spec_row)) in spec.perp_markets.iter().zip(PERPS.iter()).enumerate() {
        let underlying: AssetId = line
            .underlying
            .parse()
            .unwrap_or_else(|e| panic!("perp line {i} underlying `{}`: {e}", line.underlying));
        assert_eq!(
            underlying, spec_row.underlying,
            "perp line {i} (`{}`) is {underlying} but {} is registered at DerivId::perp({i})",
            line.id, spec_row.symbol
        );
        assert_eq!(
            spec_row.index as usize, i,
            "{} claims index {} but sits on line {i}",
            spec_row.symbol, spec_row.index
        );
        assert_eq!(
            line.max_leverage_x100, spec_row.risk.max_leverage_x100,
            "{} leverage: genesis says {}, table says {}",
            spec_row.symbol, line.max_leverage_x100, spec_row.risk.max_leverage_x100
        );
        assert_eq!(
            line.initial_margin_bps, spec_row.risk.initial_margin_bps,
            "{} initial margin",
            spec_row.symbol
        );
        assert_eq!(
            line.maintenance_margin_bps, spec_row.risk.maintenance_margin_bps,
            "{} maintenance margin",
            spec_row.symbol
        );
    }
}

#[test]
fn future_lines_match_the_symbol_table() {
    let spec = load_spec();
    for (i, (line, spec_row)) in spec.future_markets.iter().zip(FUTURES.iter()).enumerate() {
        let underlying: AssetId = line
            .underlying
            .parse()
            .unwrap_or_else(|e| panic!("future line {i} underlying `{}`: {e}", line.underlying));
        assert_eq!(
            underlying, spec_row.underlying,
            "future line {i} (`{}`) is {underlying} but {} is registered at DerivId::future({i})",
            line.id, spec_row.symbol
        );
        assert_eq!(spec_row.index as usize, i);
        assert_eq!(
            line.max_leverage_x100, spec_row.risk.max_leverage_x100,
            "{} leverage",
            spec_row.symbol
        );
        assert_eq!(
            line.initial_margin_bps, spec_row.risk.initial_margin_bps,
            "{} initial margin",
            spec_row.symbol
        );
        assert_eq!(
            line.maintenance_margin_bps, spec_row.risk.maintenance_margin_bps,
            "{} maintenance margin",
            spec_row.symbol
        );
    }
}

#[test]
fn futures_expire_at_distinct_increasing_heights() {
    let spec = load_spec();
    let heights: Vec<u64> = spec
        .future_markets
        .iter()
        .map(|f| f.expiry_height)
        .collect();
    let mut sorted = heights.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        heights, sorted,
        "futures must be declared in increasing expiry order, and no two may share an expiry"
    );
    // A dev cluster at one-second blocks must not race an expiry on day one.
    assert!(
        heights[0] > 1_000_000,
        "the nearest expiry ({}) is too close to genesis for a dev cluster",
        heights[0]
    );
}

#[test]
fn dated_futures_are_crypto_only() {
    // `docs/PLAN.md` §3: equity underlyings list spot and perpetual only.
    let spec = load_spec();
    for f in &spec.future_markets {
        let underlying: AssetId = f.underlying.parse().unwrap();
        assert!(
            !underlying.is_equity(),
            "`{}` lists a dated future on the equity {underlying}",
            f.id
        );
    }
}

#[test]
fn every_equity_perp_declares_lower_leverage_than_every_crypto_perp() {
    let spec = load_spec();
    let mut equity_max = 0;
    let mut crypto_min = u32::MAX;
    for p in &spec.perp_markets {
        let underlying: AssetId = p.underlying.parse().unwrap();
        if underlying.is_equity() {
            equity_max = equity_max.max(p.max_leverage_x100);
        } else {
            crypto_min = crypto_min.min(p.max_leverage_x100);
        }
    }
    assert!(
        equity_max > 0 && crypto_min < u32::MAX,
        "both classes present"
    );
    assert!(
        equity_max < crypto_min,
        "equity cap {equity_max} must stay below the lowest crypto cap {crypto_min}"
    );
}

#[test]
fn every_market_has_the_single_oracle_authority() {
    let spec = load_spec();
    let expected = "0x0000000000000000000000000000000000020ac1";
    for p in &spec.perp_markets {
        assert_eq!(p.oracle_authority, expected, "perp `{}`", p.id);
    }
    for f in &spec.future_markets {
        assert_eq!(f.oracle_authority, expected, "future `{}`", f.id);
    }
}

// ---------------------------------------------------------------------------
// Applying it
// ---------------------------------------------------------------------------

#[test]
fn genesis_full_applies_and_seeds_every_market() {
    let dir = tempdir().unwrap();
    let db = applied_db(&dir);

    let meta = Meta::new(&db);
    assert_eq!(meta.chain_id(), 1);
    assert_eq!(meta.denom(), "ULD");
    assert_eq!(meta.head_height(), 0);
    assert_eq!(Validators::new(&db).count(), 4);

    // Spot markets: every `MarketId` resolves to a row whose base/quote agree.
    let markets = Markets::new(&db);
    for id in MarketId::ALL {
        let cfg: MarketConfig = markets
            .get(id)
            .unwrap_or_else(|| panic!("{id} not seeded by genesis-full"));
        assert_eq!(cfg.id, id);
        assert_eq!(cfg.base, id.base());
        assert_eq!(cfg.quote, AssetId::Usd);
    }

    // Perps: every entry in the symbol table resolves to a live market with the
    // declared underlying.
    let perps = PerpMarkets::new(&db);
    for row in PERPS {
        let m = perps
            .get(&row.deriv_id())
            .unwrap_or_else(|| panic!("{} ({:?}) not seeded", row.symbol, row.deriv_id()));
        assert_eq!(m.underlying, row.underlying, "{}", row.symbol);
        assert_eq!(
            m.max_leverage_x100, row.risk.max_leverage_x100,
            "{}",
            row.symbol
        );
        assert!(!m.paused, "{} should list unpaused", row.symbol);
    }
    // Nothing beyond the declared set exists.
    assert!(
        perps.get(&DerivId::perp(PERPS.len() as u16)).is_none(),
        "genesis registered more perps than the symbol table declares"
    );

    let futures = FuturesMarkets::new(&db);
    for row in FUTURES {
        let m = futures
            .get(&row.deriv_id())
            .unwrap_or_else(|| panic!("{} not seeded", row.symbol));
        assert_eq!(m.underlying, row.underlying, "{}", row.symbol);
        assert_eq!(
            m.max_leverage_x100, row.risk.max_leverage_x100,
            "{}",
            row.symbol
        );
        assert!(
            m.final_settlement_px_q8.is_none(),
            "{} settles later",
            row.symbol
        );
    }
    assert!(
        futures
            .get(&DerivId::future(FUTURES.len() as u16))
            .is_none(),
        "genesis registered more futures than the symbol table declares"
    );
}

#[test]
fn genesis_full_seeds_insurance_fund_and_demo_margin() {
    let dir = tempdir().unwrap();
    let db = applied_db(&dir);
    let margins = MarginAccounts::new(&db);

    // $5M insurance fund — larger than the perps genesis, because this chain
    // lists eleven derivative markets rather than three.
    let ins = margins
        .get(&Address::INSURANCE)
        .expect("insurance fund seeded");
    assert_eq!(ins.collateral, 5_000_000_000_000);

    for hex in [
        "0x0000000000000000000000000000000000d10000",
        "0x0000000000000000000000000000000000d10001",
        "0x0000000000000000000000000000000000d10002",
    ] {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hex::decode(hex.trim_start_matches("0x")).unwrap());
        let acct = margins
            .get(&Address(bytes))
            .unwrap_or_else(|| panic!("{hex} margin account missing"));
        assert_eq!(acct.collateral, 10_000_000_000_000, "{hex}");
    }
}

#[test]
fn simulation_accounts_are_funded_on_every_asset() {
    // A bot must be able to quote both sides of any market in the universe
    // without a faucet round-trip, which means a non-zero balance on every
    // asset — including the ones added when the universe widened.
    let dir = tempdir().unwrap();
    let db = applied_db(&dir);
    let spec = load_spec();
    let sim = spec.sim.expect("sim record");
    let accts = Accounts::new(&db);

    let (_sk, pk) = cb_crypto::keypair_from_seed(format!("{}0", sim.seed_prefix).as_bytes());
    let trader = cb_crypto::address_from_pubkey(&pk);
    let state = accts.get(&trader).expect("sim trader 0 funded");

    for asset in AssetId::ALL {
        assert!(
            state.balance(asset) > 0,
            "sim trader has no {asset} — it cannot quote the {asset} markets"
        );
    }

    // The amounts are per-asset base units, so equal human quantities are very
    // different integers. Pin a couple so a decimal slip in the file is caught.
    assert_eq!(state.balance(AssetId::Btc), 100 * AssetId::Btc.one_whole());
    assert_eq!(
        state.balance(AssetId::Sol),
        1_000 * AssetId::Sol.one_whole()
    );
    assert_eq!(
        state.balance(AssetId::Aapl),
        1_000 * AssetId::Aapl.one_whole()
    );
    assert_eq!(
        state.balance(AssetId::Usd),
        10_000_000 * AssetId::Usd.one_whole()
    );
}

#[test]
fn company_treasury_holds_every_asset() {
    let dir = tempdir().unwrap();
    let db = applied_db(&dir);
    let meta = Meta::new(&db);
    let company = meta.company_addr();
    let state = Accounts::new(&db).get(&company).expect("company funded");
    for asset in AssetId::ALL {
        assert!(state.balance(asset) > 0, "treasury holds no {asset}");
    }
}

// ---------------------------------------------------------------------------
// The older files must keep working, unchanged
// ---------------------------------------------------------------------------

#[test]
fn the_older_genesis_files_still_declare_exactly_what_they_did() {
    // `genesis.jsonl` and `genesis-perps.jsonl` are pinned by their own tests
    // and by deployed clusters. Widening the universe must not have touched
    // them — assert their shape from here too, so a careless edit to either is
    // caught even if someone edits the test that owns them.
    set_full_env();
    // The perps file uses per-market oracle vars; supply them.
    std::env::set_var("ORACLE_BTC", "0x000000000000000000000000000000000000b7c0");
    std::env::set_var("ORACLE_ETH", "0x000000000000000000000000000000000000e7c0");

    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();

    let base = GenesisSpec::from_jsonl(
        &std::fs::read_to_string(root.join("genesis.jsonl")).expect("genesis.jsonl"),
    )
    .expect("genesis.jsonl parses");
    assert_eq!(base.markets.len(), 2, "genesis.jsonl: btc_usd + eth_usd");
    assert_eq!(base.perp_markets.len(), 1, "genesis.jsonl: btc_perp only");
    assert_eq!(base.future_markets.len(), 1);

    let perps = GenesisSpec::from_jsonl(
        &std::fs::read_to_string(root.join("genesis-perps.jsonl")).expect("genesis-perps.jsonl"),
    )
    .expect("genesis-perps.jsonl parses");
    assert_eq!(perps.markets.len(), 2);
    assert_eq!(
        perps.perp_markets.len(),
        2,
        "genesis-perps.jsonl: btc + eth perps"
    );
    assert_eq!(perps.future_markets.len(), 1);

    // And both still put BTC-PERP at perp(0) / ETH-PERP at perp(1), which is
    // what lets the symbol table pin those indices.
    assert_eq!(perps.perp_markets[0].underlying, "btc");
    assert_eq!(perps.perp_markets[1].underlying, "eth");
    assert_eq!(base.perp_markets[0].underlying, "btc");
}
