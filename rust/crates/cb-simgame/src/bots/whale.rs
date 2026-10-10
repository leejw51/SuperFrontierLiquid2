//! The whale: occasional large market orders that move the book.
//!
//! Its job is to create the conditions the rest of the population cannot:
//! a single order that sweeps several price levels, leaves the market maker
//! with real inventory, and pushes somebody's margin. Between strikes it does
//! nothing at all, which is deliberate — a whale that trades every block is
//! just a large noise trader, and the book adapts to it.

use cb_tx::TxBody;
use cb_types::{Side, Tif};
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use super::{client_id, on_tick, place_perp, Bot, BotKind, Observation};
use crate::universe::{bump_bps, NOTIONAL_DEN};

/// Parameters for one whale.
#[derive(Clone, Copy, Debug)]
pub struct WhaleParams {
    /// Blocks between strikes.
    pub interval_blocks: u64,
    /// Notional per strike as a multiple of collateral, times 100.
    pub size_x100: u128,
    /// How far through the book the order is priced, in basis points.
    pub sweep_bps: i64,
}

impl Default for WhaleParams {
    fn default() -> Self {
        Self {
            interval_blocks: 23,
            size_x100: 250,
            sweep_bps: 150,
        }
    }
}

/// A bot that occasionally trades size.
pub struct Whale {
    label: String,
    params: WhaleParams,
    tag: u8,
    counter: u64,
    phase: u64,
}

impl Whale {
    /// Build a whale. `phase` staggers strikes so a cohort of whales does not
    /// all fire on the same block.
    pub fn new(label: String, tag: u8, phase: u64, params: WhaleParams) -> Self {
        Self {
            label,
            params,
            tag,
            counter: 0,
            phase,
        }
    }

    fn next_id(&mut self) -> cb_types::H256 {
        self.counter += 1;
        client_id(self.tag, self.counter)
    }
}

impl Bot for Whale {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::Whale
    }

    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        let interval = self.params.interval_blocks.max(1);
        if (obs.height + self.phase) % interval != 0 {
            return Vec::new();
        }
        let candidates: Vec<(cb_types::DerivId, u128, u128)> = obs
            .tradeable()
            .map(|m| (m.id, m.reference_q8(), m.tick))
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let pick = rng.gen_range(0..candidates.len());
        let (market, reference, tick) = candidates[pick];

        // Prefer to unwind an existing position rather than pile on: a whale
        // that only ever adds ends up as the whole market's open interest.
        let current = obs.me.signed_qty(market);
        let side = if current.unsigned_abs() > 0 && rng.gen_bool(0.5) {
            if current > 0 {
                Side::Sell
            } else {
                Side::Buy
            }
        } else if rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };

        let notional_micro = obs.me.collateral.saturating_mul(self.params.size_x100) / 100;
        let qty = (notional_micro.saturating_mul(NOTIONAL_DEN) / reference.max(1)).max(1);
        let sweep = if side == Side::Buy {
            self.params.sweep_bps
        } else {
            -self.params.sweep_bps
        };
        let price = on_tick(bump_bps(reference, sweep), tick);
        let cid = self.next_id();
        vec![place_perp(
            market,
            side,
            price,
            qty,
            Tif::Ioc,
            false,
            false,
            cid,
        )]
    }
}
