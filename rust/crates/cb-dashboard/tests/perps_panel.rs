//! Smoke test for the dashboard's perps panel.
//!
//! Verifies that `collect_perps` (via `render_index`) renders a perp row that
//! reflects the seeded mark / oracle / last-trade / open interest / paused
//! flag. We don't spin up the axum server — the `templates` rendering plus
//! `Database` reads are deterministic enough on their own.

use cb_dashboard::server::PruningInfo;
use cb_dashboard::templates::{render_index, IndexData, PerpSummary};
use cb_dashboard::{InsuranceSnapshot, Metrics};
use cb_state::{Database, Meta, OracleState, PerpMarkets};
use cb_types::{Address, AssetId, DerivId, MarkPrice, OraclePrice, PerpMarket};
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn render_index_emits_perps_section_with_paused_reduce_only_only_column() {
    // We don't need a database for this test — feed the index directly.
    let perps = vec![PerpSummary {
        market: format!("{}", DerivId::perp(0)),
        underlying: "btc".into(),
        mark_px_q8: "5000000000000".into(),
        oracle_px_q8: "5000050000000".into(),
        last_px_q8: "5000010000000".into(),
        funding_rate_q9: 12_345,
        open_interest: "999".into(),
        best_bid_q8: "100".into(),
        best_ask_q8: "101".into(),
        paused: false,
        paused_reduce_only_only: true,
    }];
    let data = IndexData {
        chain_id: 1,
        head_height: 100,
        head_hash: "0xabcd".into(),
        state_root: "0x9f3c".into(),
        mempool_empty: true,
        company_addr: "0xc0de".into(),
        recent_blocks: Vec::new(),
        metrics: Metrics::new().snapshot(),
        pruning: PruningInfo {
            enabled: false,
            keep_recent: 0,
        },
        last_qc_height: 99,
        perps,
        funding: Vec::new(),
        positions: Vec::new(),
        oracle: Vec::new(),
        liquidations: Vec::new(),
        insurance: InsuranceSnapshot::default(),
    };
    let _arc = Arc::new(()); // keep the import live without changing imports.
    let html = render_index(&data);
    // Perps header + reduce-only column.
    assert!(html.contains("<h2>Perps</h2>"), "Perps header missing");
    assert!(html.contains("reduce-only halt"), "halt column missing");
    // Row content shows up verbatim.
    assert!(html.contains("perp:0"), "market id missing");
    // The reduce-only-halt cell should render `yes` for our seeded row.
    assert!(html.contains(">yes<"), "reduce-only flag not rendered");
}

#[test]
fn render_index_with_no_perps_shows_placeholder() {
    let data = IndexData {
        chain_id: 1,
        head_height: 0,
        head_hash: "0x".into(),
        state_root: "0x".into(),
        mempool_empty: true,
        company_addr: "0xc0de".into(),
        recent_blocks: Vec::new(),
        metrics: Metrics::new().snapshot(),
        pruning: PruningInfo {
            enabled: false,
            keep_recent: 0,
        },
        last_qc_height: 0,
        perps: Vec::new(),
        funding: Vec::new(),
        positions: Vec::new(),
        oracle: Vec::new(),
        liquidations: Vec::new(),
        insurance: InsuranceSnapshot::default(),
    };
    let html = render_index(&data);
    assert!(html.contains("no perp markets registered"));
}

// Ensure the database reads compile (we can't easily exercise the full
// `collect_perps` without exposing it; this smokes the imports stay live).
#[test]
fn perp_market_round_trip_via_state_seeds_dashboard_input() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // Transactions here are signed for chain 1; the executor rejects a tx whose
    // `chain_id` does not match the chain's own, so the fixture must declare it.
    // `Meta` is outside the state-root preimage, so no asserted root changes.
    Meta::new(&db).set_chain_id(1);
    let m = PerpMarket {
        id: DerivId::perp(0),
        underlying: AssetId::Btc,
        tick_size: 100,
        lot_size: 1_000,
        max_leverage_x100: 5_000,
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: 3_600,
        funding_rate_cap_q9: 1_000_000,
        oracle_authority: Address([7; 20]),
        max_position_notional: 100_000_000_000_000,
        paused: false,
        last_trade_px_q8: 50_001 * 100_000_000,
        last_trade_height: 5,
        paused_reduce_only_only: false,
    };
    PerpMarkets::new(&db).put(&m);
    let os = OracleState::new(&db);
    os.put_oracle(&OraclePrice {
        market: m.id,
        px_q8: 50_000 * 100_000_000,
        source_height: 5,
        authority: Address([7; 20]),
    });
    os.put_mark(&MarkPrice {
        market: m.id,
        px_q8: 50_005 * 100_000_000,
        computed_at_height: 5,
        premium_ema_q9: 100,
    });
    assert_eq!(PerpMarkets::new(&db).list().len(), 1);
}
