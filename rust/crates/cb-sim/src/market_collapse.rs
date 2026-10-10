//! Market collapse scenario (M15).
//!
//! Long-tail bear market: oracle drifts -5% per 100 blocks for 10 epochs,
//! producing a 50% total drop. 60 traders open mixed-leverage positions
//! during a 50-block warmup; epochs 1..10 each apply one large oracle step
//! at the epoch boundary, followed by a sweep of liquidation triggers.
//!
//! This is the long-tail counterpart to `perp_shock`: instead of a single
//! 30% candle, we test gradual deterioration over ~1000 blocks and record
//! per-epoch metrics. Documents whether insurance breaches in the metric
//! `insurance_breached` (true if `insurance_min == 0`).
//!
//! Output metrics:
//!   * Per-epoch: `liquidation_rate` = succeeded / triggered.
//!   * Per-epoch: `insurance_after_micro_usd` (snapshot at epoch end).
//!   * `insurance_breached` flag.
//!   * Determinism: `final_state_root`.

// The scenario loops walk several parallel per-trader and per-market arrays
// by one index; iterating one of them and indexing the rest is the shape the
// lint objects to and the clearest way to write it.
#![allow(clippy::needless_range_loop)]

use cb_block::{apply_tx_at, BlockState, ExecCtx};
use cb_crypto::{address_from_pubkey, keypair_from_seed, SecretKey};
use cb_state::{Database, MarginAccounts, Meta, PerpMarkets};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, DerivId, MarginAccount, PerpMarket, Side, Signed, Tif, H256};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const Q8: u128 = 100_000_000;
const MICRO: u128 = 1_000_000;

#[derive(Clone, Debug, Default)]
pub struct EpochSnapshot {
    pub epoch: u32,
    pub block_at_end: u64,
    pub triggered: u64,
    pub succeeded: u64,
    pub insurance_after_micro_usd: u128,
    pub oracle_px_q8: u128,
}

#[derive(Clone, Debug, Default)]
pub struct MarketCollapseMetrics {
    pub blocks: u64,
    pub txs_submitted: u64,
    pub txs_accepted: u64,
    pub txs_rejected: u64,
    pub liquidations_triggered: u64,
    pub liquidations_succeeded: u64,
    pub insurance_start_micro_usd: u128,
    pub insurance_end_micro_usd: u128,
    pub insurance_min_micro_usd: u128,
    pub insurance_breached: bool,
    pub epochs: Vec<EpochSnapshot>,
    pub final_state_root: H256,
}

#[derive(Clone, Debug)]
pub struct MarketCollapseConfig {
    pub num_traders: usize,
    pub blocks: u64,
    pub warmup_blocks: u64,
    pub epoch_blocks: u64,
    pub num_epochs: u32,
    /// Per-epoch drop in basis points (500 = 5%).
    pub epoch_drop_bps: u32,
    pub rng_seed: u64,
    pub starting_collateral: u128,
    pub insurance_seed: u128,
    pub funding_interval_blocks: u32,
}

impl Default for MarketCollapseConfig {
    fn default() -> Self {
        Self {
            num_traders: 60,
            blocks: 1_050,
            warmup_blocks: 50,
            epoch_blocks: 100,
            num_epochs: 10,
            epoch_drop_bps: 500,
            rng_seed: 0xC011A5EC0DE_u64,
            starting_collateral: 5_000 * MICRO,
            insurance_seed: 1_000_000 * MICRO,
            funding_interval_blocks: 50,
        }
    }
}

pub fn run_market_collapse(cfg: &MarketCollapseConfig) -> MarketCollapseMetrics {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open db");
    // This scenario builds its own throwaway chain, so it has to declare a chain
    // id: the executor rejects any tx whose `chain_id` does not match the
    // chain's own, and every tx signed below uses 1. `Meta` is outside the
    // state-root preimage, so this does not affect the scenario's replay-
    // determinism assertions.
    Meta::new(&db).set_chain_id(1);

    let (oracle_sk, oracle_pk) = keypair_from_seed(b"market-collapse-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    // Single market is enough — long-tail is about price path, not breadth.
    PerpMarkets::new(&db).put(&PerpMarket {
        id: DerivId::perp(0),
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000, // 50x cap; per-trader uses subset
        initial_margin_bps: 200,
        maintenance_margin_bps: 100,
        taker_fee_bps: 3,
        maker_fee_bps_signed: -1,
        funding_interval_blocks: cfg.funding_interval_blocks,
        funding_rate_cap_q9: 5_000_000,
        oracle_authority,
        max_position_notional: u128::MAX,
        paused: false,
        last_trade_px_q8: 0,
        last_trade_height: 0,
        paused_reduce_only_only: false,
    });
    let market = DerivId::perp(0);

    MarginAccounts::new(&db).put(&MarginAccount {
        owner: Address::INSURANCE,
        collateral: cfg.insurance_seed,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    struct Trader {
        sk: SecretKey,
        addr: Address,
        nonce: u64,
        /// 10..=50 inclusive. Used to size the warmup open.
        leverage_x: u32,
    }
    let mut rng_init = StdRng::seed_from_u64(cfg.rng_seed.wrapping_add(1));
    let mut traders: Vec<Trader> = (0..cfg.num_traders)
        .map(|i| {
            let (sk, pk) = keypair_from_seed(format!("market-collapse-{i}").as_bytes());
            let addr = address_from_pubkey(&pk);
            MarginAccounts::new(&db).put(&MarginAccount {
                owner: addr,
                collateral: cfg.starting_collateral,
                unrealized_pnl: Signed::ZERO,
                bad_debt: 0,
            });
            // Mixed leverage 10x..50x in 10x increments — deterministic via
            // the dedicated init RNG.
            let leverage_x = 10 + (rng_init.gen_range(0u32..5) * 10);
            Trader {
                sk,
                addr,
                nonce: 0,
                leverage_x,
            }
        })
        .collect();

    let (keeper_sk, keeper_pk) = keypair_from_seed(b"market-collapse-keeper");
    let keeper = address_from_pubkey(&keeper_pk);
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: keeper,
        collateral: 0,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    let (maker_sk, maker_pk) = keypair_from_seed(b"market-collapse-maker");
    let maker_addr = address_from_pubkey(&maker_pk);
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: maker_addr,
        collateral: 1_000_000_000 * MICRO,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });
    let mut maker_nonce: u64 = 0;

    let mut oracle_nonce: u64 = 0;
    let mut keeper_nonce: u64 = 0;
    let mut oracle_px: u128 = 50_000 * Q8;

    // Bootstrap oracle.
    {
        let mut state = BlockState::new(&db);
        let v = sign(
            &oracle_sk,
            oracle_nonce,
            TxBody::OracleUpdate {
                market,
                px_q8: oracle_px,
                submitted_height: 1,
            },
        );
        apply_tx_at(&mut state, &ctx_at(1, Address::ZERO), &v, 0).unwrap();
        oracle_nonce += 1;
        state.commit().unwrap();
    }

    let mut metrics = MarketCollapseMetrics {
        insurance_start_micro_usd: cfg.insurance_seed,
        insurance_end_micro_usd: cfg.insurance_seed,
        insurance_min_micro_usd: cfg.insurance_seed,
        ..Default::default()
    };

    let mut rng = StdRng::seed_from_u64(cfg.rng_seed);

    // Warmup: traders open longs (the bear market punishes them). Mixed
    // leverage drives mixed liquidation timing.
    for h in 2..(2 + cfg.warmup_blocks) {
        let mut txs: Vec<VerifiedTx> = Vec::new();
        for ti in 0..traders.len() {
            // Stagger: each trader opens at most once during warmup.
            if traders[ti].nonce >= 1 {
                continue;
            }
            if !rng.gen_bool(0.30) {
                continue;
            }
            // Quantity from leverage: notional ≈ collateral * leverage.
            // qty (in base 1e8 units) = notional_usd_micro * Q8 / px_q8 / MICRO.
            // Use a reduced-size open so initial-margin checks pass.
            let notional_micro =
                (cfg.starting_collateral).saturating_mul(traders[ti].leverage_x as u128) / 4; // 25% of theoretical max → comfortably above IM
            let qty = notional_micro.saturating_mul(Q8) / oracle_px.max(1) / MICRO;
            if qty == 0 {
                continue;
            }
            let price = oracle_px + (oracle_px / 200);

            // Maker shorts at the same price for a clean fill.
            txs.push(sign(
                &maker_sk,
                maker_nonce,
                TxBody::PlacePerpOrder {
                    market,
                    side: Side::Sell,
                    price_q8: price,
                    qty,
                    tif: Tif::Gtc,
                    post_only: true,
                    reduce_only: false,
                    client_order_id: H256([(maker_nonce as u8).wrapping_add(0xC0); 32]),
                },
            ));
            maker_nonce += 1;

            let trader = &mut traders[ti];
            txs.push(sign(
                &trader.sk,
                trader.nonce,
                TxBody::PlacePerpOrder {
                    market,
                    side: Side::Buy,
                    price_q8: price,
                    qty,
                    tif: Tif::Ioc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([(ti as u8).wrapping_add(0x10); 32]),
                },
            ));
            trader.nonce += 1;
        }

        if h % cfg.funding_interval_blocks as u64 == 0 {
            txs.push(sign(
                &keeper_sk,
                keeper_nonce,
                TxBody::FundingTick { market },
            ));
            keeper_nonce += 1;
        }

        let mut state = BlockState::new(&db);
        let proposer = if h % cfg.funding_interval_blocks as u64 == 0 {
            keeper
        } else {
            Address::ZERO
        };
        for (i, v) in txs.iter().enumerate() {
            metrics.txs_submitted += 1;
            match apply_tx_at(&mut state, &ctx_at(h, proposer), v, i as u32) {
                Ok(()) => metrics.txs_accepted += 1,
                Err(_) => metrics.txs_rejected += 1,
            }
        }
        let _ = state.commit().unwrap();
        metrics.blocks += 1;

        let ins = MarginAccounts::new(&db)
            .get(&Address::INSURANCE)
            .map(|m| m.collateral)
            .unwrap_or(0);
        metrics.insurance_end_micro_usd = ins;
        metrics.insurance_min_micro_usd = metrics.insurance_min_micro_usd.min(ins);
    }

    // Epochs: each epoch starts with a single -5% oracle step at the very
    // first block of the epoch, then runs for `epoch_blocks` with keeper
    // sweeps every block.
    let epoch_start = 2 + cfg.warmup_blocks;
    for e in 0..cfg.num_epochs {
        let epoch_first_block = epoch_start + (e as u64) * cfg.epoch_blocks;
        let epoch_last_block = epoch_first_block + cfg.epoch_blocks - 1;
        let mut epoch_triggered = 0u64;
        let mut epoch_succeeded = 0u64;

        for h in epoch_first_block..=epoch_last_block {
            let mut txs: Vec<VerifiedTx> = Vec::new();
            // Oracle step at epoch boundary.
            if h == epoch_first_block {
                oracle_px = oracle_px.saturating_mul(10_000 - cfg.epoch_drop_bps as u128) / 10_000;
                oracle_px = oracle_px.max(1);
                txs.push(sign(
                    &oracle_sk,
                    oracle_nonce,
                    TxBody::OracleUpdate {
                        market,
                        px_q8: oracle_px,
                        submitted_height: h,
                    },
                ));
                oracle_nonce += 1;
            }

            if h % cfg.funding_interval_blocks as u64 == 0 {
                txs.push(sign(
                    &keeper_sk,
                    keeper_nonce,
                    TxBody::FundingTick { market },
                ));
                keeper_nonce += 1;
            }

            let mut state = BlockState::new(&db);
            let proposer = if h % cfg.funding_interval_blocks as u64 == 0 {
                keeper
            } else {
                Address::ZERO
            };
            for (i, v) in txs.iter().enumerate() {
                metrics.txs_submitted += 1;
                match apply_tx_at(&mut state, &ctx_at(h, proposer), v, i as u32) {
                    Ok(()) => metrics.txs_accepted += 1,
                    Err(_) => metrics.txs_rejected += 1,
                }
            }
            let _ = state.commit().unwrap();
            metrics.blocks += 1;

            // Keeper sweep — every block within the epoch, after the price
            // has been written.
            for ti in 0..traders.len() {
                let owner = traders[ti].addr;
                metrics.liquidations_triggered += 1;
                epoch_triggered += 1;
                let v = sign(
                    &keeper_sk,
                    keeper_nonce,
                    TxBody::LiquidationTrigger { owner, market },
                );
                let mut s = BlockState::new(&db);
                let ok = apply_tx_at(&mut s, &ctx_at(h, keeper), &v, 0).is_ok();
                if ok {
                    s.commit().unwrap();
                    metrics.liquidations_succeeded += 1;
                    epoch_succeeded += 1;
                }
                keeper_nonce += 1;
            }

            let ins = MarginAccounts::new(&db)
                .get(&Address::INSURANCE)
                .map(|m| m.collateral)
                .unwrap_or(0);
            metrics.insurance_end_micro_usd = ins;
            metrics.insurance_min_micro_usd = metrics.insurance_min_micro_usd.min(ins);
        }

        let ins_after = metrics.insurance_end_micro_usd;
        metrics.epochs.push(EpochSnapshot {
            epoch: e,
            block_at_end: epoch_last_block,
            triggered: epoch_triggered,
            succeeded: epoch_succeeded,
            insurance_after_micro_usd: ins_after,
            oracle_px_q8: oracle_px,
        });
    }

    metrics.insurance_breached = metrics.insurance_min_micro_usd == 0;

    // A fingerprint of the whole committed state at the end of the run. The
    // chain root would not do: this harness commits blocks without advancing
    // the head, so its roots never chain. Hashing every committed row is the
    // same computation genesis uses for the first root.
    metrics.final_state_root = cb_state::genesis_state_hash(&db).expect("state hash");
    metrics
}

fn sign(sk: &SecretKey, nonce: u64, body: TxBody) -> VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body,
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

fn ctx_at(height: u64, proposer: Address) -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        block_height: height,
        proposer_addr: proposer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_cfg(seed: u64) -> MarketCollapseConfig {
        MarketCollapseConfig {
            num_traders: 12,
            blocks: 0, // unused; warmup + epochs determine block count
            warmup_blocks: 20,
            epoch_blocks: 20,
            num_epochs: 5,
            epoch_drop_bps: 500,
            rng_seed: seed,
            starting_collateral: 5_000 * MICRO,
            insurance_seed: 100_000 * MICRO,
            funding_interval_blocks: 25,
        }
    }

    #[test]
    fn market_collapse_smoke() {
        let m = run_market_collapse(&small_cfg(0xFEEDFACE));
        // Warmup blocks + 5 epochs of 20 blocks each = 120 total blocks.
        assert_eq!(m.blocks, 20 + 5 * 20);
        assert_eq!(m.epochs.len(), 5);
        // Insurance never goes "negative" (u128).
        assert!(m.insurance_min_micro_usd <= m.insurance_start_micro_usd);
        assert!(m.insurance_end_micro_usd <= m.insurance_start_micro_usd);
        // We attempted at least N triggers per epoch.
        for e in &m.epochs {
            assert!(e.triggered > 0, "epoch {} had zero triggers", e.epoch);
        }
        assert_ne!(m.final_state_root, H256::ZERO);
    }

    #[test]
    fn market_collapse_replay_is_deterministic() {
        let cfg = small_cfg(0xBADCAFE);
        let a = run_market_collapse(&cfg);
        let b = run_market_collapse(&cfg);
        assert_eq!(a.final_state_root, b.final_state_root);
        assert_eq!(a.txs_accepted, b.txs_accepted);
        assert_eq!(a.txs_rejected, b.txs_rejected);
        assert_eq!(a.liquidations_triggered, b.liquidations_triggered);
        assert_eq!(a.liquidations_succeeded, b.liquidations_succeeded);
        assert_eq!(a.insurance_end_micro_usd, b.insurance_end_micro_usd);
        assert_eq!(a.insurance_min_micro_usd, b.insurance_min_micro_usd);
        assert_eq!(a.epochs.len(), b.epochs.len());
        for (x, y) in a.epochs.iter().zip(b.epochs.iter()) {
            assert_eq!(x.triggered, y.triggered);
            assert_eq!(x.succeeded, y.succeeded);
            assert_eq!(x.insurance_after_micro_usd, y.insurance_after_micro_usd);
            assert_eq!(x.oracle_px_q8, y.oracle_px_q8);
        }
    }
}
