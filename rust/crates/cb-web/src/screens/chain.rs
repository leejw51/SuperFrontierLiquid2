//! The chain explorer.
//!
//! The rail along the foot of every screen is the permanent version of this
//! page. Giving the chain a rail rather than only a page is the decision that
//! makes the blockchain feel like a fact about the product rather than a claim
//! on a marketing page; this screen is where that fact can be inspected.
//!
//! The twenty-four-block strip is a histogram of load: each chip's fill is its
//! transaction count against the busiest block shown, so a quiet stretch and a
//! busy one are visibly different shapes rather than two identical rows.

use std::sync::Arc;

use leptos::prelude::*;

use crate::api::types::{Block, ChainTx};
use crate::config::Backend;
use crate::format;
use crate::i18n::t;
use crate::state::{use_stores, Load};
use crate::ui::{AppShell, Icon, RpcPanel};

/// How many transactions the dashboard lists.
const TX_ROWS: u32 = 60;

/// The dashboard: the chain, where everything listens, and what the chain
/// has been applying.
#[component]
pub fn ChainExplorer() -> impl IntoView {
    let stores = use_stores();
    let backend = StoredValue::new(expect_context::<Arc<Backend>>());
    let selected = RwSignal::new(None::<u64>);

    // The transaction feed. Refetched every few seconds rather than on every
    // block: at ten blocks a second a per-block fetch is a denial of service
    // on the gateway from its own dashboard.
    let txs = RwSignal::new(Vec::<ChainTx>::new());
    let txs_load = RwSignal::new(Load::Loading);
    Effect::new(move |_| {
        let _tick = stores.now_ms.get() / 3_000;
        let backend = backend.get_value();
        leptos::task::spawn_local(async move {
            let response = backend.api.chain_txs(TX_ROWS).await;
            txs_load.set(Load::from_result(&response));
            if let Ok(response) = response {
                txs.set(response.txs);
            }
        });
    });

    let chain = move || stores.chain.get();
    let blocks = move || stores.blocks.get();
    let busiest = move || {
        blocks()
            .iter()
            .map(|block| block.tx_count)
            .max()
            .unwrap_or(1)
            .max(1)
    };
    // Hoisted out of the view: a turbofish inside a `view!` attribute reads as
    // an opening tag to the macro.
    let oldest_first = move || {
        let mut ordered: Vec<Block> = blocks();
        ordered.reverse();
        ordered
    };
    let current = move || {
        let blocks = blocks();
        match selected.get() {
            Some(height) => blocks.iter().find(|block| block.height == height).cloned(),
            None => blocks.first().cloned(),
        }
    };

    view! {
        <AppShell>
            <h1 class="t-h2" style="margin-bottom:var(--sp-12)">{t("Dashboard")}</h1>
            <div class="ch-hero panel spine-chain">
                <div class="panel-head">
                    <h3>
                        {move || chain().map_or_else(
                            || t("Chain").to_string(), |chain| format!("{} {}", t("Chain"), chain.chain_id))}
                    </h3>
                    <span class=move || if chain().is_none_or(|chain| chain.synced) {
                        "badge live"
                    } else {
                        "badge warn"
                    }>
                        {move || if chain().is_none_or(|chain| chain.synced) {
                            t("Synced")
                        } else {
                            t("Not synced")
                        }}
                    </span>
                    <span class="spacer"></span>
                    <span class="t-meta">
                        {move || chain().map_or_else(
                            || "Connecting".to_string(),
                            |chain| format!(
                                "Up {} on a single sequencer with {} validators and replica sync",
                                format::duration(
                                    i64::try_from(chain.uptime_seconds).unwrap_or(0)),
                                chain.validators,
                            ),
                        )}
                    </span>
                </div>
                <div class="ch-stats">
                    <span class="stat">
                        <span class="k">{t("Block height")}</span>
                        <span
                            class="v lg"
                            class:tick-up=move || stores.block_tick.get().is_multiple_of(2)
                        >
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(), |chain| format::count(chain.height))}
                        </span>
                    </span>
                    <span class="stat">
                        <span class="k">{t("Throughput, 1 min")}</span>
                        <span class="v" style="font-size:var(--fs-xl)">
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(),
                                |chain| format::fixed(chain.tps_1m, 1))}
                            <span style="font-size:var(--fs-sm);color:var(--text-muted)">" tx/s"</span>
                        </span>
                    </span>
                    <span class="stat">
                        <span class="k">{t("Block time")}</span>
                        <span class="v" style="font-size:var(--fs-xl)">
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(), |chain| chain.block_time_ms.to_string())}
                            <span style="font-size:var(--fs-sm);color:var(--text-muted)">" ms"</span>
                        </span>
                    </span>
                    <span class="stat">
                        <span class="k">{t("Transactions")}</span>
                        <span class="v" style="font-size:var(--fs-xl)">
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(), |chain| format::count(chain.tx_count))}
                        </span>
                    </span>
                    <span class="stat">
                        <span class="k">{t("Mempool")}</span>
                        <span class="v" style="font-size:var(--fs-xl)">
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(), |chain| chain.mempool_depth.to_string())}
                        </span>
                    </span>
                    <span class="stat">
                        <span class="k">{t("State root")}</span>
                        <span class="v" style="font-size:var(--fs-base)">
                            {move || chain().map_or_else(
                                || "\u{2014}".to_string(),
                                |chain| format::hash(&chain.state_root))}
                        </span>
                    </span>
                </div>
                <div class="ch-strip">
                    <span class="t-meta" style="flex:0 0 auto">
                        {t("Last 24 blocks, by transaction count")}
                    </span>
                    <span class="blocks" style="display:flex;gap:4px;margin-left:var(--sp-16)">
                        <For
                            each=oldest_first
                            key=|block| block.height
                            let:block
                        >
                            {
                                let height = block.height;
                                let fill = (block.tx_count * 100 / busiest()).max(6);
                                view! {
                                    <button
                                        class="blk lg"
                                        style=format!("--fill:{fill}")
                                        aria-label=format!(
                                            "Block {}, {} transactions",
                                            format::count(height), block.tx_count,
                                        )
                                        on:click=move |_| selected.set(Some(height))
                                    ><i></i></button>
                                }
                            }
                        </For>
                    </span>
                </div>
            </div>

            <div class="ch-grid">
                <section class="panel spine-chain">
                    <div class="panel-head">
                        <h3>{t("Recent blocks")}</h3>
                        <span class="spacer"></span>
                        <crate::ui::ConnIndicator />
                    </div>
                    <div class="tbl-wrap">
                        // A skeleton here promised blocks that were never
                        // coming: a chain with nothing submitted to it has no
                        // blocks to list, and that is a fact rather than a wait.
                        <crate::ui::Region
                            load=Signal::derive(move || stores.blocks_load.get())
                            empty=Signal::derive(move || blocks().is_empty())
                            rows=12
                            title="No blocks yet"
                            body="This chain produces a block when there is something to put in \
                                  one. The first transaction anybody sends \u{2014} a faucet \
                                  claim, an order, a cancellation \u{2014} makes block one."
                        >
                            <table class="tbl">
                                <thead>
                                    <tr>
                                        <th class="n">{t("Height")}</th>
                                        <th class="n">{t("Transactions")}</th>
                                        <th>{t("Hash")}</th>
                                        <th>{t("State root")}</th>
                                        <th>{t("Proposer")}</th>
                                        <th class="n">{t("Age")}</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For each=blocks key=|block| block.height let:block>
                                        {
                                            let height = block.height;
                                            let at = format::parse_rfc3339(&block.at)
                                                .unwrap_or_default();
                                            view! {
                                                <tr
                                                    aria-selected=move || (selected.get()
                                                        == Some(height)).to_string()
                                                    on:click=move |_| selected.set(Some(height))
                                                >
                                                    <td class="n strong">
                                                        {format::count(block.height)}
                                                    </td>
                                                    <td class="n">{block.tx_count.to_string()}</td>
                                                    <td class="n dim">{format::hash(&block.hash)}</td>
                                                    <td class="n dim">
                                                        {or_dash(&block.state_root)}
                                                    </td>
                                                    <td class="n dim">
                                                        {or_dash(&block.proposer)}
                                                    </td>
                                                    <td class="n">
                                                        {move || format::age(
                                                            stores.now_ms.get() - at * 1_000)}
                                                    </td>
                                                </tr>
                                            }
                                        }
                                    </For>
                                </tbody>
                            </table>
                        </crate::ui::Region>
                    </div>
                </section>

                <section class="panel spine-chain">
                    <div class="panel-head">
                        <h3>
                            {move || current().map_or_else(
                                || "Block".to_string(),
                                |block| format!("Block {}", format::count(block.height)))}
                        </h3>
                        <span class="spacer"></span>
                        <button
                            class="btn btn-ghost btn-sm"
                            on:click=move |_| {
                                if let Some(block) = current() {
                                    crate::screens::landing::copy_to_clipboard(&block.hash);
                                    stores.notify(
                                        crate::state::Tone::Good,
                                        "Hash copied",
                                        block.hash.clone(),
                                    );
                                }
                            }
                        >{t("Copy hash")}</button>
                    </div>
                    <div style="padding:var(--sp-12);display:flex;flex-direction:column;gap:var(--sp-10);min-height:0;overflow:auto">
                        {move || current().map_or_else(
                            || view! {
                                <crate::ui::EmptyState
                                    title="No block selected"
                                    body="Pick a block from the list or the histogram to see what \
                                          it carried."
                                />
                            }.into_any(),
                            |block| view! { <BlockDetail block=block /> }.into_any(),
                        )}
                    </div>
                </section>
            </div>

            <section class="panel spine-chain" style="margin-top:var(--sp-16)" data-testid="tx-panel">
                <div class="panel-head">
                    <h3>{t("Recent transactions")}</h3>
                    <span class="badge pending">{move || txs.read().len().to_string()}</span>
                    <span class="spacer"></span>
                    <span class="t-meta">
                        {t("What the chain applied, newest first, from the gateway's block stream")}
                    </span>
                </div>
                <div class="tbl-wrap" style="max-height:420px">
                    <crate::ui::Region
                        load=Signal::derive(move || txs_load.get())
                        empty=Signal::derive(move || txs.read().is_empty())
                        rows=8
                        title=t("No transactions yet")
                        body="The feed fills as blocks carry transactions past the gateway. Turn                               the simulation on from the admin page, claim from the faucet, or                               place an order."
                    >
                        <table class="tbl">
                            <thead>
                                <tr>
                                    <th class="n">{t("Height")}</th>
                                    <th>{t("Kind")}</th>
                                    <th>{t("Market")}</th>
                                    <th>{t("Side")}</th>
                                    <th>{t("Sender")}</th>
                                    <th>{t("Hash")}</th>
                                    <th class="n">{t("Age")}</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || txs.get() key=|tx| tx.hash.clone() let:tx>
                                    {
                                        let at = format::parse_rfc3339(&tx.at).unwrap_or_default();
                                        let side_class = match tx.side.as_deref() {
                                            Some("buy") => "up",
                                            Some("sell") => "down",
                                            _ => "dim",
                                        };
                                        view! {
                                            <tr data-kind=tx.kind.clone()>
                                                <td class="n strong">{format::count(tx.height)}</td>
                                                <td class="strong">{tx.kind.clone()}</td>
                                                <td>{tx.market.clone().unwrap_or_else(|| "\u{2014}".to_string())}</td>
                                                <td class=side_class>
                                                    {tx.side.clone().map_or_else(
                                                        || "\u{2014}".to_string(),
                                                        |side| side.to_uppercase())}
                                                </td>
                                                <td class="n dim">{format::hash(&tx.sender)}</td>
                                                <td class="n dim">{format::hash(&tx.hash)}</td>
                                                <td class="n">
                                                    {move || format::age(stores.now_ms.get() - at * 1_000)}
                                                </td>
                                            </tr>
                                        }
                                    }
                                </For>
                            </tbody>
                        </table>
                    </crate::ui::Region>
                </div>
            </section>

            <RpcPanel />
        </AppShell>
    }
}

/// Truncates an identifier, or shows an em dash when the field is not carried.
fn or_dash(value: &str) -> String {
    if value.is_empty() {
        "\u{2014}".to_string()
    } else {
        format::hash(value)
    }
}

/// One block's header and the transactions it carried.
#[component]
fn BlockDetail(
    /// The block.
    block: Block,
) -> impl IntoView {
    let at = format::parse_rfc3339(&block.at).map_or_else(|| block.at.clone(), format::clock);

    view! {
        <div class="ticket" style="padding:0;gap:var(--sp-6)">
            <div class="row"><span class="k">"Hash"</span><span class="v">{format::hash(&block.hash)}</span></div>
            // The `chain` channel carries height, hash, transaction count and
            // time, and nothing else. A block that arrived over the socket
            // rather than from `/chain/blocks` therefore has no state root and
            // no proposer, and says so rather than showing an empty cell that
            // reads as a missing value.
            <div class="row">
                <span class="k">{t("State root")}</span>
                <span class="v">{or_dash(&block.state_root)}</span>
            </div>
            <div class="row">
                <span class="k">"Proposer"</span>
                <span class="v">{or_dash(&block.proposer)}</span>
            </div>
            <div class="row">
                <span class="k">{t("Transactions")}</span>
                <span class="v">{block.tx_count.to_string()}</span>
            </div>
            <div class="row"><span class="k">"Timestamp"</span><span class="v">{format!("{at} UTC")}</span></div>
        </div>
        <hr style="border:0;border-top:1px solid var(--border-subtle)" />
        <span class="t-meta">"What this block carried"</span>
        // One number, because one number is all there is. The 60/20/20 split
        // that used to sit here was arithmetic on `tx_count` and nothing else:
        // a block holding one limit order reported "0 order transactions",
        // under a note promising nothing had been invented.
        <div style="padding:var(--sp-8) 0">
            {if block.tx_count == 0 {
                view! {
                    <crate::ui::EmptyState
                        title="An empty block"
                        body="Nothing was submitted in this hundred milliseconds. The chain \
                              produces a block either way, which is why the height keeps \
                              climbing on a quiet market."
                    />
                }.into_any()
            } else {
                view! {
                    <div class="txlist">
                        <div class="tx">
                            <span class="badge pending">"On chain"</span>
                            <span class="d">
                                {if block.tx_count == 1 {
                                    "1 transaction".to_string()
                                } else {
                                    format!(
                                        "{} transactions",
                                        format::count(u64::from(block.tx_count)))
                                }}
                            </span>
                            <span class="num h">{format::hash(&block.hash)}</span>
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
        <p class="t-meta" style="line-height:var(--lh-snug)">
            <Icon name="i-info" size=11 style="display:inline;vertical-align:-1px" />
            " The contract's "<code>"/chain/blocks"</code>" carries block headers, not the \
             transactions inside them. That count is the header's own, and what kind of \
             transactions they were is not something this client can know, so it does not say."
        </p>
    }
}
