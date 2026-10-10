//! The arbitrageur: trades the gap between the book and the index.
//!
//! On a healthy perpetual this is the bot that holds funding near zero:
//! when the book trades above the index it sells, when it trades below it
//! buys, and the premium that funding is computed from stays small.
//!
//! ## Why it watches the book midpoint, not the mark
//!
//! The chain's mark price is computed from `(oracle, mid, last_trade)` — but
//! the executor calls `compute_mark_price` with `mid = None` on every path,
//! so today's mark never sees the book at all and tracks the oracle almost
//! exactly. An arbitrageur trading "mark versus index" would therefore have
//! no signal to trade. Comparing the book midpoint against the oracle is the
//! spread that actually exists, and is what a real arbitrageur would trade.
//! See `docs/qa-report.md` for the mark-price finding.

use cb_tx::TxBody;
use cb_types::{Side, Tif};
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use super::{client_id, on_tick, place_perp, Bot, BotKind, Observation};
use crate::universe::{bump_bps, NOTIONAL_DEN};

/// Parameters for one arbitrageur.
#[derive(Clone, Copy, Debug)]
pub struct ArbitrageurParams {
    /// Premium, in basis points, that must exist before the bot trades.
    pub entry_bps: u128,
    /// Target notional per trade as a multiple of collateral, times 100.
    pub size_x100: u128,
    /// Cap on the absolute position the bot will hold, in base units.
    pub max_inventory: u128,
}

impl Default for ArbitrageurParams {
    fn default() -> Self {
        Self {
            entry_bps: 15,
            size_x100: 60,
            max_inventory: 60_000_000,
        }
    }
}

/// A premium-fading bot.
pub struct Arbitrageur {
    label: String,
    params: ArbitrageurParams,
    tag: u8,
    counter: u64,
}

impl Arbitrageur {
    /// Build an arbitrageur.
    pub fn new(label: String, tag: u8, params: ArbitrageurParams) -> Self {
        Self {
            label,
            params,
            tag,
            counter: 0,
        }
    }

    fn next_id(&mut self) -> cb_types::H256 {
        self.counter += 1;
        client_id(self.tag, self.counter)
    }
}

impl Bot for Arbitrageur {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::Arbitrageur
    }

    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        let mut out = Vec::new();
        let markets: Vec<(cb_types::DerivId, u128, u128, u128)> = obs
            .tradeable()
            .filter_map(|m| m.mid_q8().map(|mid| (m.id, mid, m.oracle_q8, m.tick)))
            .collect();

        for (market, mid, index, tick) in markets {
            if index == 0 {
                continue;
            }
            let (premium_bps, rich) = if mid > index {
                ((mid - index).saturating_mul(10_000) / index, true)
            } else {
                ((index - mid).saturating_mul(10_000) / index, false)
            };
            if premium_bps < self.params.entry_bps {
                continue;
            }
            // Fade the premium: sell when the book is rich, buy when cheap.
            let side = if rich { Side::Sell } else { Side::Buy };
            let current = obs.me.signed_qty(market);
            let would_be_longer = side == Side::Buy;
            if would_be_longer && current > self.params.max_inventory as i128 {
                continue;
            }
            if !would_be_longer && current < -(self.params.max_inventory as i128) {
                continue;
            }
            if !rng.gen_bool(0.7) {
                continue;
            }

            let notional_micro = obs.me.collateral.saturating_mul(self.params.size_x100) / 100;
            let qty = (notional_micro.saturating_mul(NOTIONAL_DEN) / index.max(1)).max(1);
            if qty < 100_000 {
                continue;
            }
            // Price just inside the index so the trade only happens while the
            // premium is genuinely there.
            let cross = if side == Side::Buy { 5i64 } else { -5 };
            let price = on_tick(bump_bps(index, cross), tick);
            let cid = self.next_id();
            out.push(place_perp(
                market,
                side,
                price,
                qty,
                Tif::Ioc,
                false,
                false,
                cid,
            ));
        }
        out
    }
}
