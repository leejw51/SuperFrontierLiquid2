//! Fee-stress scenario (M15).
//!
//! 50 traders, 1 perp market, 1000 blocks, ~100 orders/block (50 trader
//! place-orders + ~50 maker quotes per block). The market goes through
//! mild oscillations so a small fraction of accounts cross maintenance
//! margin and get liquidated, depositing liquidation fees into the
//! insurance fund. Traders are sized so that every liquidation is solvent
//! (i.e. trader collateral fully covers the fee), so insurance is never
//! drained — it only grows.
//!
//! Output metrics:
//!   * `liquidations_triggered` / `liquidations_succeeded`
//!   * `insurance_start_micro_usd`, `insurance_end_micro_usd`
//!   * `insurance_growth_micro_usd` = end - start
//!   * `keeper_balance_end_micro_usd` (proxy for keeper bounty share)
//!   * `block_insurance_snapshots` len
//!
//! Asserts (smoke test): insurance balance is monotonically non-decreasing
//! across all block snapshots. This is the property the user asked for —
//! "monotonic insurance growth from fee share" — and pins the executor's
//! behaviour that liquidation fees flow to insurance and never out.

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
pub struct FeeStressMetrics {
    pub blocks: u64,
    pub txs_submitted: u64,
    pub txs_accepted: u64,
    pub txs_rejected: u64,
    pub liquidations_triggered: u64,
    pub liquidations_succeeded: u64,
    pub insurance_start_micro_usd: u128,
    pub insurance_end_micro_usd: u128,
    pub insurance_growth_micro_usd: u128,
    pub keeper_balance_end_micro_usd: u128,
    /// Per-block insurance balance — used by the smoke test to verify
    /// monotonic non-decreasing growth.
    pub block_insurance_snapshots: Vec<u128>,
    pub final_state_root: H256,
}

#[derive(Clone, Debug)]
pub struct FeeStressConfig {
    pub num_traders: usize,
    pub blocks: u64,
    /// How many trader-order attempts per block. Each attempt produces one
    /// trader order + one maker quote, so the actual tx count is roughly
    /// 2x this.
    pub orders_per_block: u32,
    pub rng_seed: u64,
    pub starting_collateral: u128,
    pub insurance_seed: u128,
    pub funding_interval_blocks: u32,
    /// Every N blocks, apply a small +/- oracle move to trigger a few
    /// liquidations (insurance income).
    pub oracle_move_interval: u64,
    pub oracle_move_bps: u32,
}

impl Default for FeeStressConfig {
    fn default() -> Self {
        Self {
            num_traders: 50,
            blocks: 1_000,
            orders_per_block: 50,
            rng_seed: 0xFEE57E55,
            starting_collateral: 100_000 * MICRO,
            insurance_seed: 50_000 * MICRO,
            funding_interval_blocks: 100,
            oracle_move_interval: 50,
            oracle_move_bps: 75,
        }
    }
}

pub fn run_fee_stress(cfg: &FeeStressConfig) -> FeeStressMetrics {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open db");
    // This scenario builds its own throwaway chain, so it has to declare a chain
    // id: the executor rejects any tx whose `chain_id` does not match the
    // chain's own, and every tx signed below uses 1. `Meta` is outside the
    // state-root preimage, so this does not affect the scenario's replay-
    // determinism assertions.
    Meta::new(&db).set_chain_id(1);

    let (oracle_sk, oracle_pk) = keypair_from_seed(b"fee-stress-oracle");
    let oracle_authority = address_from_pubkey(&oracle_pk);

    PerpMarkets::new(&db).put(&PerpMarket {
        id: DerivId::perp(0),
        underlying: AssetId::Btc,
        tick_size: 1,
        lot_size: 1,
        max_leverage_x100: 5_000,
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
    }
    let mut traders: Vec<Trader> = (0..cfg.num_traders)
        .map(|i| {
            let (sk, pk) = keypair_from_seed(format!("fee-stress-{i}").as_bytes());
            let addr = address_from_pubkey(&pk);
            MarginAccounts::new(&db).put(&MarginAccount {
                owner: addr,
                collateral: cfg.starting_collateral,
                unrealized_pnl: Signed::ZERO,
                bad_debt: 0,
            });
            Trader { sk, addr, nonce: 0 }
        })
        .collect();

    let (keeper_sk, keeper_pk) = keypair_from_seed(b"fee-stress-keeper");
    let keeper = address_from_pubkey(&keeper_pk);
    MarginAccounts::new(&db).put(&MarginAccount {
        owner: keeper,
        collateral: 0,
        unrealized_pnl: Signed::ZERO,
        bad_debt: 0,
    });

    let (maker_sk, maker_pk) = keypair_from_seed(b"fee-stress-maker");
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

    let mut metrics = FeeStressMetrics {
        insurance_start_micro_usd: cfg.insurance_seed,
        insurance_end_micro_usd: cfg.insurance_seed,
        block_insurance_snapshots: Vec::with_capacity(cfg.blocks as usize),
        ..Default::default()
    };

    let mut rng = StdRng::seed_from_u64(cfg.rng_seed);

    for h in 2..(2 + cfg.blocks) {
        let mut txs: Vec<VerifiedTx> = Vec::new();

        // Periodic small oracle move — produces a few liquidations.
        if h % cfg.oracle_move_interval == 0 {
            let direction_up = rng.gen_bool(0.5);
            let delta = oracle_px.saturating_mul(cfg.oracle_move_bps as u128) / 10_000;
            oracle_px = if direction_up {
                oracle_px + delta
            } else {
                oracle_px.saturating_sub(delta).max(1)
            };
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

        // Generate ~orders_per_block trader orders. Each pairs with a maker
        // quote at the same price for a clean fill. Random side. Quantity
        // is small enough that traders survive routine moves but a meaningful
        // fraction fall to maintenance margin during periodic oracle moves.
        for _ in 0..cfg.orders_per_block {
            let ti = rng.gen_range(0..traders.len());
            let side = if rng.gen_bool(0.5) {
                Side::Buy
            } else {
                Side::Sell
            };
            let cur = oracle_px;
            // Tight spread: oracle ± 0.05%.
            let jitter = cur / 2000;
            let price = if side == Side::Buy {
                cur + jitter
            } else {
                cur.saturating_sub(jitter).max(1)
            };
            // Small qty: 0.001..0.01 BTC.
            let qty = rng.gen_range(100_000u128..1_000_000u128);

            let maker_side = if side == Side::Buy {
                Side::Sell
            } else {
                Side::Buy
            };
            txs.push(sign(
                &maker_sk,
                maker_nonce,
                TxBody::PlacePerpOrder {
                    market,
                    side: maker_side,
                    price_q8: price,
                    qty,
                    tif: Tif::Gtc,
                    post_only: true,
                    reduce_only: false,
                    client_order_id: H256([(maker_nonce as u8).wrapping_add(0xF0); 32]),
                },
            ));
            maker_nonce += 1;

            let trader = &mut traders[ti];
            txs.push(sign(
                &trader.sk,
                trader.nonce,
                TxBody::PlacePerpOrder {
                    market,
                    side,
                    price_q8: price,
                    qty,
                    tif: Tif::Ioc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: H256([(ti as u8).wrapping_add(1); 32]),
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

        // Keeper sweep right after an oracle move — small chance of catching
        // a liquidatable account whose margin slipped under maintenance.
        if h % cfg.oracle_move_interval == 0 {
            for trader in &traders {
                let owner = trader.addr;
                metrics.liquidations_triggered += 1;
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
                }
                keeper_nonce += 1;
            }
        }

        let ins = MarginAccounts::new(&db)
            .get(&Address::INSURANCE)
            .map(|m| m.collateral)
            .unwrap_or(0);
        metrics.insurance_end_micro_usd = ins;
        metrics.block_insurance_snapshots.push(ins);
    }

    metrics.insurance_growth_micro_usd = metrics
        .insurance_end_micro_usd
        .saturating_sub(metrics.insurance_start_micro_usd);
    metrics.keeper_balance_end_micro_usd = MarginAccounts::new(&db)
        .get(&keeper)
        .map(|m| m.collateral)
        .unwrap_or(0);

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

    fn small_cfg(seed: u64) -> FeeStressConfig {
        FeeStressConfig {
            num_traders: 12,
            blocks: 60,
            orders_per_block: 6,
            rng_seed: seed,
            starting_collateral: 100_000 * MICRO,
            insurance_seed: 50_000 * MICRO,
            funding_interval_blocks: 30,
            oracle_move_interval: 25,
            oracle_move_bps: 75,
        }
    }

    #[test]
    fn fee_stress_smoke() {
        let m = run_fee_stress(&small_cfg(0xFEE5));
        assert_eq!(m.blocks, 60);
        assert_eq!(m.block_insurance_snapshots.len(), 60);

        // Critical property: insurance is monotonically non-decreasing —
        // liquidation fees flow IN, but never OUT (because traders are
        // funded so generously every liq is solvent in this scenario).
        let mut prev = m.insurance_start_micro_usd;
        for (i, snap) in m.block_insurance_snapshots.iter().enumerate() {
            assert!(
                *snap >= prev,
                "insurance dropped from {} to {} at block index {}",
                prev,
                snap,
                i
            );
            prev = *snap;
        }
        // End >= start.
        assert!(m.insurance_end_micro_usd >= m.insurance_start_micro_usd);
        assert_ne!(m.final_state_root, H256::ZERO);
    }

    #[test]
    fn fee_stress_replay_is_deterministic() {
        let cfg = small_cfg(0xCAB00D1E);
        let a = run_fee_stress(&cfg);
        let b = run_fee_stress(&cfg);
        assert_eq!(a.final_state_root, b.final_state_root);
        assert_eq!(a.txs_accepted, b.txs_accepted);
        assert_eq!(a.txs_rejected, b.txs_rejected);
        assert_eq!(a.liquidations_triggered, b.liquidations_triggered);
        assert_eq!(a.insurance_end_micro_usd, b.insurance_end_micro_usd);
        assert_eq!(a.insurance_growth_micro_usd, b.insurance_growth_micro_usd);
        assert_eq!(
            a.keeper_balance_end_micro_usd,
            b.keeper_balance_end_micro_usd
        );
        assert_eq!(a.block_insurance_snapshots, b.block_insurance_snapshots);
    }
}
