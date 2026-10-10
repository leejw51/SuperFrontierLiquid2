//! The momentum trader: leveraged, trend-following, and expected to blow up.
//!
//! It holds an exponential moving average of the mark and takes leveraged
//! positions in the direction of the deviation. When the scenario engine runs
//! a crash, the momentum bots that are long get liquidated — and that is the
//! point of them. Liquidation is where value quietly leaks (fees, penalties,
//! the insurance fund, bad debt), so a population that never gets liquidated
//! never exercises the code most likely to be wrong.
//!
//! The bot deliberately does *not* manage risk well. It sizes off collateral
//! rather than equity, so a losing position keeps its notional target and it
//! rides the position down.

use cb_tx::TxBody;
use cb_types::{DerivId, Side, Tif};
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use super::{client_id, on_tick, place_perp, Bot, BotKind, Observation};
use crate::universe::{bump_bps, NOTIONAL_DEN};

/// Parameters for one momentum trader.
#[derive(Clone, Copy, Debug)]
pub struct MomentumParams {
    /// EMA smoothing, as a percentage weight on the new sample.
    pub ema_alpha_pct: u128,
    /// Deviation from the EMA, in basis points, that counts as a signal.
    pub trigger_bps: u128,
    /// Target notional as a multiple of collateral, times 100.
    pub leverage_x100: u128,
    /// How aggressively the taker prices through the book, in basis points.
    pub cross_bps: i64,
    /// Blocks to wait between position changes.
    pub cooldown_blocks: u64,
}

impl Default for MomentumParams {
    fn default() -> Self {
        Self {
            ema_alpha_pct: 20,
            trigger_bps: 20,
            leverage_x100: 500,
            cross_bps: 40,
            cooldown_blocks: 3,
        }
    }
}

/// A leveraged trend follower.
pub struct Momentum {
    label: String,
    params: MomentumParams,
    tag: u8,
    counter: u64,
    /// Per-market EMA of the mark price, in q8. Indexed alongside
    /// `Observation::markets`, which the world builds in a fixed order.
    ema_q8: Vec<(DerivId, u128)>,
    last_action_height: u64,
}

impl Momentum {
    /// Build a momentum trader.
    pub fn new(label: String, tag: u8, params: MomentumParams) -> Self {
        Self {
            label,
            params,
            tag,
            counter: 0,
            ema_q8: Vec::new(),
            last_action_height: 0,
        }
    }

    fn next_id(&mut self) -> cb_types::H256 {
        self.counter += 1;
        client_id(self.tag, self.counter)
    }

    fn update_ema(&mut self, market: DerivId, price_q8: u128) -> u128 {
        let alpha = self.params.ema_alpha_pct.min(100);
        match self.ema_q8.iter_mut().find(|(m, _)| *m == market) {
            Some((_, ema)) => {
                *ema = (*ema * (100 - alpha) + price_q8 * alpha) / 100;
                *ema
            }
            None => {
                self.ema_q8.push((market, price_q8));
                price_q8
            }
        }
    }
}

impl Bot for Momentum {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::Momentum
    }

    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        let mut out = Vec::new();
        let markets: Vec<(DerivId, u128, u128, u128)> = obs
            .tradeable()
            .map(|m| (m.id, m.mark_q8, m.tick, m.reference_q8()))
            .collect();

        for (market, mark, tick, reference) in &markets {
            let ema = self.update_ema(*market, *mark);
            if obs.height.saturating_sub(self.last_action_height) < self.params.cooldown_blocks {
                continue;
            }
            if ema == 0 || *mark == 0 {
                continue;
            }
            let deviation_bps = if *mark > ema {
                (mark - ema).saturating_mul(10_000) / ema
            } else {
                (ema - mark).saturating_mul(10_000) / ema
            };
            let flat = obs.me.signed_qty(*market) == 0;
            // A momentum trader wants to be in the market. If nothing has
            // triggered but the bot is flat, it takes a position anyway now
            // and then — otherwise a calm regime leaves the book with no
            // leveraged risk for a later crash to act on, and the
            // liquidation paths never run.
            let opportunistic = flat && rng.gen_bool(0.12);
            if deviation_bps < self.params.trigger_bps && !opportunistic {
                continue;
            }
            // Only act on some signals, so a whole cohort of momentum bots
            // does not move in lockstep every block.
            if !opportunistic && !rng.gen_bool(0.5) {
                continue;
            }

            let want_long = *mark > ema;
            let side = if want_long { Side::Buy } else { Side::Sell };
            let target_notional_micro =
                obs.me.collateral.saturating_mul(self.params.leverage_x100) / 100;
            // qty = notional * NOTIONAL_DEN / price
            let target_qty =
                target_notional_micro.saturating_mul(NOTIONAL_DEN) / (*reference).max(1);
            let current = obs.me.signed_qty(*market);
            let want = if want_long {
                target_qty as i128
            } else {
                -(target_qty as i128)
            };
            let delta = want - current;
            if delta == 0 {
                continue;
            }
            // Trade at most a third of the gap per action, so the bot builds
            // into a position rather than teleporting into it.
            let qty = (delta.unsigned_abs() / 3).max(1);
            if qty < 100_000 {
                continue;
            }
            let cross = if side == Side::Buy {
                self.params.cross_bps
            } else {
                -self.params.cross_bps
            };
            let price = on_tick(bump_bps(*reference, cross), *tick);
            let cid = self.next_id();
            out.push(place_perp(
                *market,
                side,
                price,
                qty,
                Tif::Ioc,
                false,
                false,
                cid,
            ));
            self.last_action_height = obs.height;
        }
        out
    }
}
