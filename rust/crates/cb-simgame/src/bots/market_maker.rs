//! The market maker: the bot that makes a lone human's order book non-empty.
//!
//! It quotes both sides around the book's reference price, widens as its
//! inventory grows, and re-quotes when the price walks away from its resting
//! orders. Of the whole population this one matters most — without a
//! two-sided book nothing else in the simulation can trade, liquidations have
//! nothing to close against, and a human who joins stares at an empty ladder.
//!
//! ## Inventory skew
//!
//! A maker that quotes symmetrically accumulates whatever the market is
//! selling until it is the market's entire risk. This one shifts its mid down
//! when it is long and up when it is short, in proportion to inventory
//! against a target size, so it prices itself out of growing a position it
//! already has. That is also what keeps it solvent through a crash, which
//! matters: a bankrupt market maker means an empty book exactly when the
//! liquidation path most needs one.
//!
//! ## Re-quoting
//!
//! Cancelling and replacing every block would churn the book and make the
//! matcher's price-time priority meaningless. Instead an order is cancelled
//! only when the reference price has moved past it by more than the requote
//! threshold, or when it has rested longer than `max_age_blocks`.

use cb_tx::TxBody;
use cb_types::{DerivId, Side, Tif};
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use super::{client_id, on_tick, place_perp, Bot, BotKind, Observation};
use crate::universe::bump_bps;

/// Parameters for one market maker.
#[derive(Clone, Copy, Debug)]
pub struct MarketMakerParams {
    /// Half-spread at zero inventory, in basis points.
    pub base_half_spread_bps: i64,
    /// Extra half-spread per unit of inventory ratio, in basis points. At an
    /// inventory equal to `target_inventory` the quote widens by this much.
    pub inventory_widen_bps: i64,
    /// How far the mid is skewed at full inventory, in basis points.
    pub inventory_skew_bps: i64,
    /// Inventory level, in base units, that counts as "full".
    pub target_inventory: u128,
    /// Quantity per quote, in base units.
    pub quote_qty: u128,
    /// Cancel and replace when the reference price has moved this far past a
    /// resting quote, in basis points.
    pub requote_bps: u128,
    /// Cancel a quote that has rested longer than this many blocks.
    pub max_age_blocks: u64,
}

impl Default for MarketMakerParams {
    fn default() -> Self {
        Self {
            base_half_spread_bps: 8,
            inventory_widen_bps: 12,
            inventory_skew_bps: 20,
            target_inventory: 300_000_000, // 3 base units
            quote_qty: 40_000_000,         // 0.4 of a base unit
            requote_bps: 25,
            max_age_blocks: 12,
        }
    }
}

/// A two-sided quoting bot.
pub struct MarketMaker {
    label: String,
    params: MarketMakerParams,
    tag: u8,
    counter: u64,
}

impl MarketMaker {
    /// Build a market maker with the given label and parameters.
    pub fn new(label: String, tag: u8, params: MarketMakerParams) -> Self {
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

    /// Half-spread and mid skew implied by the current inventory.
    fn quote_geometry(&self, inventory: i128) -> (i64, i64) {
        let target = self.params.target_inventory.max(1) as i128;
        let ratio = (inventory.abs() * 100 / target).min(200); // percent, capped at 2x
        let widen = self.params.inventory_widen_bps * ratio as i64 / 100;
        let half_spread = self.params.base_half_spread_bps + widen;
        // Long inventory pushes the quoted mid down so the maker sells more
        // readily than it buys; short inventory does the reverse.
        let skew_mag = self.params.inventory_skew_bps * ratio as i64 / 100;
        let skew = if inventory > 0 { -skew_mag } else { skew_mag };
        (half_spread, skew)
    }
}

impl Bot for MarketMaker {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::MarketMaker
    }

    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        let mut out = Vec::new();
        // A maker with bad debt has already failed; it stops quoting rather
        // than digging deeper, which keeps the book honest about the fact
        // that liquidity really did disappear.
        if obs.me.bad_debt > 0 {
            for (oid, _, _, _, _) in &obs.me.resting {
                out.push(TxBody::CancelPerpOrder { order_id: *oid });
            }
            return out;
        }

        let markets: Vec<DerivId> = obs.tradeable().map(|m| m.id).collect();
        for market in markets {
            let m = match obs.market(market) {
                Some(m) => m,
                None => continue,
            };
            // Quote around the chain's mark, which is oracle-derived, rather
            // than around the book's own midpoint. Quoting around the mid
            // makes the maker chase itself: every fill moves the mid, the
            // next quote follows it, and a directional taker walks the book
            // away from fair value for free. A real maker anchors on an
            // external price.
            let reference = if m.mark_q8 > 0 {
                m.mark_q8
            } else {
                m.reference_q8()
            };
            let inventory = obs.me.signed_qty(market);
            let (half_spread, skew) = self.quote_geometry(inventory);
            let center = bump_bps(reference, skew);
            let bid = on_tick(bump_bps(center, -half_spread), m.tick);
            let ask = on_tick(bump_bps(center, half_spread), m.tick);

            // Cancel quotes the market has walked away from.
            let mut have_bid = false;
            let mut have_ask = false;
            for (oid, om, side, px, _rem) in &obs.me.resting {
                if *om != market {
                    continue;
                }
                let drift_bps = if *px > reference {
                    (px - reference).saturating_mul(10_000) / reference.max(1)
                } else {
                    (reference - px).saturating_mul(10_000) / reference.max(1)
                };
                if drift_bps > self.params.requote_bps {
                    out.push(TxBody::CancelPerpOrder { order_id: *oid });
                    continue;
                }
                match side {
                    Side::Buy => have_bid = true,
                    Side::Sell => have_ask = true,
                }
            }

            // Size down the side that would grow an already-large inventory.
            let long_heavy = inventory > self.params.target_inventory as i128;
            let short_heavy = inventory < -(self.params.target_inventory as i128);
            let jitter = rng.gen_range(0..=(self.params.quote_qty / 4).max(1));
            let qty = self.params.quote_qty + jitter;

            if !have_bid && !long_heavy && bid > 0 {
                let cid = self.next_id();
                out.push(place_perp(
                    market,
                    Side::Buy,
                    bid,
                    qty,
                    Tif::Gtc,
                    true,
                    false,
                    cid,
                ));
            }
            if !have_ask && !short_heavy && ask > bid {
                let cid = self.next_id();
                out.push(place_perp(
                    market,
                    Side::Sell,
                    ask,
                    qty,
                    Tif::Gtc,
                    true,
                    false,
                    cid,
                ));
            }
        }

        // Quote spot too, so the spot lock-accounting path is exercised by
        // something other than the noise bot's rejects.
        let spot = &obs.spot;
        if spot.enabled && spot.reference > 0 && rng.gen_bool(0.35) {
            let half = bump_bps(spot.reference, self.params.base_half_spread_bps);
            let bid = on_tick(
                bump_bps(spot.reference, -self.params.base_half_spread_bps),
                spot.tick,
            );
            let ask = on_tick(half, spot.tick);
            let qty = spot.min_qty.max(1) * rng.gen_range(1_000..=5_000);
            let side = if rng.gen_bool(0.5) {
                Side::Buy
            } else {
                Side::Sell
            };
            let (price, have_balance) = match side {
                Side::Buy => (
                    bid,
                    obs.me.spot_quote_free > bid.saturating_mul(qty) / 100_000_000,
                ),
                Side::Sell => (ask, obs.me.spot_base_free > qty),
            };
            if have_balance && price > 0 {
                let cid = self.next_id();
                out.push(TxBody::PlaceOrder {
                    market: spot.market,
                    side,
                    price,
                    qty,
                    tif: Tif::Gtc,
                    post_only: true,
                    client_order_id: cid,
                });
            }
        }
        // Keep the spot book from silting up with stale quotes.
        if spot.enabled && obs.me.spot_resting.len() > 6 {
            out.push(TxBody::BatchCancel {
                market: spot.market,
            });
        }

        out
    }
}
