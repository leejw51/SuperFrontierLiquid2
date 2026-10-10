use crate::metrics::MetricsSnapshot;
use crate::perps::{
    render_funding_table, render_insurance_panel, render_liquidations_table, render_oracle_table,
    render_positions_table, FundingRow, InsuranceSnapshot, LiquidationRow, OracleRow, PositionRow,
};
use crate::server::PruningInfo;
use serde::Serialize;

#[derive(Serialize)]
pub struct BlockSummary {
    pub height: u64,
    pub hash: String,
    pub tx_count: usize,
    pub timestamp: u64,
    pub state_root: String,
}

/// One row in the dashboard's "Perps" table. Strings throughout to keep the
/// template rendering trivial and to dodge u128's lack of a JSON-native form.
#[derive(Serialize)]
pub struct PerpSummary {
    pub market: String,
    pub underlying: String,
    /// `px_q8` units, decimal string (u128 doesn't survive JS `Number`).
    pub mark_px_q8: String,
    pub oracle_px_q8: String,
    pub last_px_q8: String,
    pub funding_rate_q9: i64,
    /// Total open interest = sum of |qty| over open positions on this market.
    pub open_interest: String,
    pub best_bid_q8: String,
    pub best_ask_q8: String,
    pub paused: bool,
    pub paused_reduce_only_only: bool,
}

#[derive(Serialize)]
pub struct IndexData {
    pub chain_id: u32,
    pub head_height: u64,
    pub head_hash: String,
    pub state_root: String,
    pub mempool_empty: bool,
    pub company_addr: String,
    pub recent_blocks: Vec<BlockSummary>,
    pub metrics: MetricsSnapshot,
    pub pruning: PruningInfo,
    /// Highest block height for which a quorum certificate is persisted in
    /// CF_QC. With voting enabled this should be `head_height - 1` (the leader
    /// proposes block H, then H's QC forms once 2/3 vote, then H+1 carries it).
    pub last_qc_height: u64,
    /// One row per registered perpetual market.
    pub perps: Vec<PerpSummary>,
    /// M14 perp panels — funding state, top positions, oracle freshness,
    /// recent liquidations, insurance fund snapshot. Populated by
    /// `server::collect_index`; defaults to empty vectors when called from
    /// tests that don't seed the DB.
    #[serde(default)]
    pub funding: Vec<FundingRow>,
    #[serde(default)]
    pub positions: Vec<PositionRow>,
    #[serde(default)]
    pub oracle: Vec<OracleRow>,
    #[serde(default)]
    pub liquidations: Vec<LiquidationRow>,
    #[serde(default)]
    pub insurance: InsuranceSnapshot,
}

pub fn render_index(data: &IndexData) -> String {
    let mut blocks_html = String::new();
    for (i, b) in data.recent_blocks.iter().enumerate() {
        let dt = if i + 1 < data.recent_blocks.len() {
            let prev = &data.recent_blocks[i + 1];
            format!("+{} ms", b.timestamp.saturating_sub(prev.timestamp))
        } else {
            "—".to_string()
        };
        blocks_html.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td class=\"num\">{}</td><td class=\"num\">{}</td><td class=\"dim\">{}</td><td><code>{}</code></td></tr>",
            b.height,
            short(&b.hash),
            fmt_int(b.tx_count as u64),
            b.timestamp,
            dt,
            short(&b.state_root),
        ));
    }
    let mut perps_html = String::new();
    if data.perps.is_empty() {
        perps_html
            .push_str("<tr><td colspan=\"11\" class=\"dim\">no perp markets registered</td></tr>");
    } else {
        for p in data.perps.iter() {
            perps_html.push_str(&format!(
                "<tr>\
                    <td><code>{m}</code></td>\
                    <td>{u}</td>\
                    <td class=\"num\">{mark}</td>\
                    <td class=\"num\">{oracle}</td>\
                    <td class=\"num\">{last}</td>\
                    <td class=\"num\">{rate}</td>\
                    <td class=\"num\">{oi}</td>\
                    <td class=\"num\">{bid}</td>\
                    <td class=\"num\">{ask}</td>\
                    <td>{paused}</td>\
                    <td>{ro}</td>\
                </tr>",
                m = p.market,
                u = p.underlying,
                mark = p.mark_px_q8,
                oracle = p.oracle_px_q8,
                last = p.last_px_q8,
                rate = p.funding_rate_q9,
                oi = p.open_interest,
                bid = p.best_bid_q8,
                ask = p.best_ask_q8,
                paused = if p.paused { "yes" } else { "no" },
                ro = if p.paused_reduce_only_only {
                    "yes"
                } else {
                    "no"
                },
            ));
        }
    }
    let m = &data.metrics;
    let bench_state = if m.running { "running" } else { "idle" };
    let bench_class = if m.running { "ok" } else { "off" };

    let pruning_state = if data.pruning.enabled { "on" } else { "off" };
    let pruning_class = if data.pruning.enabled { "ok" } else { "off" };
    let pruning_keep = if data.pruning.enabled {
        fmt_int(data.pruning.keep_recent)
    } else {
        "—".to_string()
    };

    let buy_pct = if m.recent_buys_per_sec + m.recent_sells_per_sec > 0.0 {
        m.recent_buys_per_sec * 100.0 / (m.recent_buys_per_sec + m.recent_sells_per_sec)
    } else {
        50.0
    };
    let btc_pct = if m.recent_btc_per_sec + m.recent_eth_per_sec > 0.0 {
        m.recent_btc_per_sec * 100.0 / (m.recent_btc_per_sec + m.recent_eth_per_sec)
    } else {
        50.0
    };

    let html = include_str!("../templates/index.html");
    html.replace("{{chain_id}}", &data.chain_id.to_string())
        .replace("{{head_height}}", &fmt_int(data.head_height))
        .replace("{{head_hash}}", &data.head_hash)
        .replace("{{state_root}}", &data.state_root)
        .replace(
            "{{mempool_status}}",
            if data.mempool_empty {
                "empty"
            } else {
                "non-empty"
            },
        )
        .replace("{{company_addr}}", &data.company_addr)
        .replace("{{blocks_rows}}", &blocks_html)
        .replace("{{bench_state}}", bench_state)
        .replace("{{bench_class}}", bench_class)
        .replace("{{recent_tps}}", &fmt_rate(m.recent_tps))
        .replace("{{avg_tps}}", &fmt_rate(m.avg_tps))
        .replace("{{elapsed}}", &fmt_secs(m.elapsed_secs))
        .replace(
            "{{orders_submitted_per_sec}}",
            &fmt_rate(m.recent_orders_submitted_per_sec),
        )
        .replace(
            "{{orders_applied_per_sec}}",
            &fmt_rate(m.recent_orders_applied_per_sec),
        )
        .replace("{{orders_total}}", &fmt_int(m.orders_applied))
        .replace("{{buys_per_sec}}", &fmt_rate(m.recent_buys_per_sec))
        .replace("{{sells_per_sec}}", &fmt_rate(m.recent_sells_per_sec))
        .replace("{{btc_per_sec}}", &fmt_rate(m.recent_btc_per_sec))
        .replace("{{eth_per_sec}}", &fmt_rate(m.recent_eth_per_sec))
        .replace("{{buy_pct}}", &format!("{:.1}", buy_pct))
        .replace("{{sell_pct}}", &format!("{:.1}", 100.0 - buy_pct))
        .replace("{{btc_pct}}", &format!("{:.1}", btc_pct))
        .replace("{{eth_pct}}", &format!("{:.1}", 100.0 - btc_pct))
        .replace("{{submitted}}", &fmt_int(m.submitted))
        .replace("{{orders_submitted}}", &fmt_int(m.orders_submitted))
        .replace("{{applied}}", &fmt_int(m.applied))
        .replace("{{rejected}}", &fmt_int(m.rejected))
        .replace("{{buys_total}}", &fmt_int(m.buys))
        .replace("{{sells_total}}", &fmt_int(m.sells))
        .replace("{{btc_total}}", &fmt_int(m.btc_orders))
        .replace("{{eth_total}}", &fmt_int(m.eth_orders))
        .replace("{{blocks}}", &fmt_int(m.blocks))
        .replace("{{block_min}}", &fmt_int(m.block_txs_min))
        .replace("{{block_max}}", &fmt_int(m.block_txs_max))
        .replace("{{block_avg}}", &format!("{:.0}", m.block_txs_avg))
        .replace("{{block_last}}", &fmt_int(m.block_txs_last))
        .replace("{{channel_depth}}", &fmt_int(m.channel_depth))
        .replace(
            "{{inflight}}",
            &fmt_int(m.submitted.saturating_sub(m.applied)),
        )
        .replace("{{pruning_state}}", pruning_state)
        .replace("{{pruning_class}}", pruning_class)
        .replace("{{pruning_keep}}", &pruning_keep)
        .replace("{{perps_rows}}", &perps_html)
        .replace("{{funding_rows}}", &render_funding_table(&data.funding))
        .replace(
            "{{positions_rows}}",
            &render_positions_table(&data.positions),
        )
        .replace("{{oracle_rows}}", &render_oracle_table(&data.oracle))
        .replace(
            "{{liquidations_rows}}",
            &render_liquidations_table(&data.liquidations),
        )
        .replace(
            "{{insurance_panel}}",
            &render_insurance_panel(&data.insurance),
        )
}

fn short(s: &str) -> String {
    // Characters, not bytes: a byte slice panics inside a multi-byte char.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 18 {
        return s.to_string();
    }
    let head: String = chars[..10].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}…{tail}")
}

fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Compact rate: 1234.5 → "1,234", 1_234_567 → "1.23 M", 12_345_678 → "12.3 M".
fn fmt_rate(v: f64) -> String {
    let abs = v.abs();
    if abs >= 1_000_000.0 {
        format!("{:.2} M", v / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1} K", v / 1_000.0)
    } else {
        format!("{:.1}", v)
    }
}

fn fmt_secs(s: f64) -> String {
    if s < 60.0 {
        format!("{:.1} s", s)
    } else if s < 3600.0 {
        format!("{:.1} min", s / 60.0)
    } else {
        format!("{:.2} h", s / 3600.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_int, short};
    use proptest::prelude::*;

    #[test]
    fn short_keeps_hex_readable() {
        let h = format!("0x{}", "ab".repeat(32));
        assert_eq!(short(&h), "0xabababab…ababab");
        assert_eq!(short("0x1234"), "0x1234");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// Any text, including multi-byte characters: no panic, and a long
        /// string keeps its first ten and last six characters.
        #[test]
        fn short_never_panics_and_keeps_both_ends(s in "\\PC{0,64}") {
            let out = short(&s);
            let chars: Vec<char> = s.chars().collect();
            if chars.len() < 18 {
                prop_assert_eq!(out, s);
            } else {
                prop_assert!(out.starts_with(&chars[..10].iter().collect::<String>()));
                prop_assert!(out.ends_with(&chars[chars.len() - 6..].iter().collect::<String>()));
            }
        }

        /// Thousands separators are cosmetic: stripping them gives the number
        /// back, and every group after the first has exactly three digits.
        #[test]
        fn fmt_int_only_adds_separators(n in any::<u64>()) {
            let s = fmt_int(n);
            prop_assert_eq!(s.replace(',', ""), n.to_string());
            let groups: Vec<&str> = s.split(',').collect();
            prop_assert!(!groups[0].is_empty() && groups[0].len() <= 3);
            for g in &groups[1..] {
                prop_assert_eq!(g.len(), 3);
            }
        }
    }
}
