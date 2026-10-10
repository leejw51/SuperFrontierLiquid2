//! The noise trader: random small orders, deliberately including bad ones.
//!
//! Rejection paths are the least-tested code in any exchange, because unit
//! tests reach them once each and then nothing exercises them again. This bot
//! makes rejection continuous: a configurable fraction of its orders are
//! invalid by construction — zero quantity, zero price, a price off the tick
//! grid, a notional that rounds to nothing, a cancel of an order that never
//! existed, a reduce-only order with no position behind it.
//!
//! Every one of those must be rejected *cleanly*: no state mutated, no nonce
//! advanced, no lock left behind. The invariant checker is what confirms it,
//! block after block — in particular lock consistency, which is where a
//! half-applied rejection would show up.

use cb_tx::TxBody;
use cb_types::{Side, Tif};
use rand::Rng;
use rand_chacha::ChaCha8Rng;

use super::{client_id, on_tick, place_perp, Bot, BotKind, Observation};
use crate::universe::bump_bps;

/// Parameters for one noise trader.
#[derive(Clone, Copy, Debug)]
pub struct NoiseParams {
    /// Probability of acting at all in a given block, in percent.
    pub activity_pct: u32,
    /// Probability that an order is deliberately malformed, in percent.
    pub bad_order_pct: u32,
    /// Maximum order size in base units.
    pub max_qty: u128,
    /// Emit the invalid orders that are known to abort a debug build of the
    /// executor (see `docs/qa-report.md`, the zero-quantity perpetual order).
    /// Off by default so a long run is not cut short by a bug that already
    /// has a dedicated regression test; turn it on to confirm the panic.
    pub include_consensus_crashers: bool,
}

impl Default for NoiseParams {
    fn default() -> Self {
        Self {
            activity_pct: 70,
            bad_order_pct: 30,
            max_qty: 3_000_000,
            include_consensus_crashers: false,
        }
    }
}

/// The kinds of deliberately invalid order this bot emits. Named so a report
/// can say which rejection paths a run actually covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BadOrder {
    ZeroQty,
    ZeroPrice,
    OffTickSpot,
    DustSpotNotional,
    SpotQtyNotMultipleOfMin,
    CancelUnknown,
    ReduceOnlyWithoutPosition,
    AbsurdlyLarge,
}

/// Invalid orders that every run emits. All of these must be rejected
/// cleanly: no state mutated, no nonce advanced, no lock left behind.
impl BadOrder {
    /// Whether this case needs a listed spot market.
    fn is_spot(self) -> bool {
        matches!(
            self,
            BadOrder::OffTickSpot | BadOrder::DustSpotNotional | BadOrder::SpotQtyNotMultipleOfMin
        )
    }
}

const SAFE_BAD_ORDERS: [BadOrder; 7] = [
    BadOrder::ZeroPrice,
    BadOrder::OffTickSpot,
    BadOrder::DustSpotNotional,
    BadOrder::SpotQtyNotMultipleOfMin,
    BadOrder::CancelUnknown,
    BadOrder::ReduceOnlyWithoutPosition,
    BadOrder::AbsurdlyLarge,
];

/// A random small-order trader.
pub struct Noise {
    label: String,
    params: NoiseParams,
    tag: u8,
    counter: u64,
}

impl Noise {
    /// Build a noise trader.
    pub fn new(label: String, tag: u8, params: NoiseParams) -> Self {
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

    fn bad_order(
        &mut self,
        which: BadOrder,
        obs: &Observation,
        rng: &mut ChaCha8Rng,
    ) -> Vec<TxBody> {
        let cid = self.next_id();
        if !obs.spot.enabled && which.is_spot() {
            return Vec::new();
        }
        let market = obs
            .tradeable()
            .next()
            .map(|m| (m.id, m.reference_q8(), m.tick));
        let spot = &obs.spot;
        match which {
            BadOrder::ZeroQty => match market {
                // `cb_tx::validate_static` rejects this at mempool admission,
                // and the matcher has its own `BookError::ZeroQty` guard — but
                // the pre-trade margin gate runs first and trips a
                // `debug_assert` inside `apply_fill_to_position`. See the
                // zero-quantity finding in `docs/qa-report.md`.
                Some((id, px, tick)) => vec![place_perp(
                    id,
                    Side::Buy,
                    on_tick(px, tick),
                    0,
                    Tif::Gtc,
                    false,
                    false,
                    cid,
                )],
                None => Vec::new(),
            },
            BadOrder::ZeroPrice => match market {
                Some((id, _, _)) => vec![place_perp(
                    id,
                    Side::Sell,
                    0,
                    1_000_000,
                    Tif::Gtc,
                    false,
                    false,
                    cid,
                )],
                None => Vec::new(),
            },
            // Rejected with `BadTick` when the market's tick is > 1.
            BadOrder::OffTickSpot => vec![TxBody::PlaceOrder {
                market: spot.market,
                side: Side::Buy,
                price: spot
                    .reference
                    .saturating_add(spot.tick.saturating_sub(1))
                    .max(1)
                    + 1,
                qty: spot.min_qty.max(1) * 1_000,
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: cid,
            }],
            // Notional underflows to zero: rejected with `BadLot`.
            BadOrder::DustSpotNotional => vec![TxBody::PlaceOrder {
                market: spot.market,
                side: Side::Buy,
                price: 1,
                qty: 1,
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: cid,
            }],
            // Rejected with `BadLot` when `min_qty > 1`.
            BadOrder::SpotQtyNotMultipleOfMin => vec![TxBody::PlaceOrder {
                market: spot.market,
                side: Side::Sell,
                price: on_tick(spot.reference, spot.tick),
                qty: spot.min_qty.saturating_mul(3) + 1,
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: cid,
            }],
            // Idempotent no-op on the perp side; `OrderNotFound` on spot.
            BadOrder::CancelUnknown => {
                if rng.gen_bool(0.5) || !obs.spot.enabled {
                    vec![TxBody::CancelPerpOrder { order_id: cid }]
                } else {
                    vec![TxBody::CancelOrder { order_id: cid }]
                }
            }
            BadOrder::ReduceOnlyWithoutPosition => match market {
                Some((id, px, tick)) if obs.me.signed_qty(id) == 0 => vec![place_perp(
                    id,
                    Side::Sell,
                    on_tick(px, tick),
                    1_000_000,
                    Tif::Gtc,
                    false,
                    true,
                    cid,
                )],
                _ => Vec::new(),
            },
            // Rejected by the initial-margin gate.
            BadOrder::AbsurdlyLarge => match market {
                Some((id, px, tick)) => vec![place_perp(
                    id,
                    Side::Buy,
                    on_tick(px, tick),
                    1_000_000_000_000,
                    Tif::Gtc,
                    false,
                    false,
                    cid,
                )],
                None => Vec::new(),
            },
        }
    }
}

impl Bot for Noise {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::Noise
    }

    fn act(&mut self, obs: &Observation, rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        if !rng.gen_ratio(self.params.activity_pct.min(100), 100) {
            return Vec::new();
        }
        if rng.gen_ratio(self.params.bad_order_pct.min(100), 100) {
            let which = if self.params.include_consensus_crashers && rng.gen_bool(0.1) {
                BadOrder::ZeroQty
            } else {
                SAFE_BAD_ORDERS[rng.gen_range(0..SAFE_BAD_ORDERS.len())]
            };
            return self.bad_order(which, obs, rng);
        }

        // A well-formed small order, on a random venue.
        let markets: Vec<(cb_types::DerivId, u128, u128)> = obs
            .tradeable()
            .map(|m| (m.id, m.reference_q8(), m.tick))
            .collect();
        let side = if rng.gen_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        };
        let qty = rng.gen_range(200_000..=self.params.max_qty.max(200_001));

        if !markets.is_empty() && rng.gen_bool(0.75) {
            let (market, reference, tick) = markets[rng.gen_range(0..markets.len())];
            let offset = rng.gen_range(-60i64..=60);
            let price = on_tick(bump_bps(reference, offset), tick);
            let tif = match rng.gen_range(0..3) {
                0 => Tif::Ioc,
                1 => Tif::Fok,
                _ => Tif::Gtc,
            };
            let cid = self.next_id();
            return vec![place_perp(market, side, price, qty, tif, false, false, cid)];
        }

        let spot = &obs.spot;
        if !spot.enabled || spot.reference == 0 {
            return Vec::new();
        }
        let offset = rng.gen_range(-40i64..=40);
        let price = on_tick(bump_bps(spot.reference, offset), spot.tick);
        let spot_qty = spot.min_qty.max(1) * rng.gen_range(500..=4_000);
        let affordable = match side {
            Side::Buy => obs.me.spot_quote_free > price.saturating_mul(spot_qty) / 100_000_000 * 2,
            Side::Sell => obs.me.spot_base_free > spot_qty,
        };
        if !affordable {
            return Vec::new();
        }
        let cid = self.next_id();
        vec![TxBody::PlaceOrder {
            market: spot.market,
            side,
            price,
            qty: spot_qty,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: cid,
        }]
    }
}
