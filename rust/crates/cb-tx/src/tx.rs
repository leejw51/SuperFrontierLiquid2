//! Plain Rust mirror of the capnp Transaction. Used internally; capnp is the wire format.

use cb_types::{Address, AssetId, DerivId, MarketId, Side, Tif, H256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxBody {
    // ---------- Spot / accounts (unchanged) ----------
    Transfer {
        to: Address,
        asset: AssetId,
        amount: u128,
    },
    Deposit {
        user: Address,
        asset: AssetId,
        amount: u128,
        external_tx_ref: String,
    },
    Withdraw {
        asset: AssetId,
        amount: u128,
        external_address: String,
    },
    WithdrawSettled {
        withdraw_id: H256,
        external_tx_ref: String,
    },
    PlaceOrder {
        market: MarketId,
        side: Side,
        price: u128,
        qty: u128,
        tif: Tif,
        post_only: bool,
        client_order_id: H256,
    },
    CancelOrder {
        order_id: H256,
    },
    BatchCancel {
        market: MarketId,
    },

    // ---------- Derivatives (M1: types only; handlers are M2+) ----------
    /// Place an order against a perp / dated-future book. Same shape as
    /// `PlaceOrder` but typed against a `DerivId` and carrying `reduce_only`.
    PlacePerpOrder {
        market: DerivId,
        side: Side,
        price_q8: u128,
        qty: u128,
        tif: Tif,
        post_only: bool,
        reduce_only: bool,
        client_order_id: H256,
    },
    CancelPerpOrder {
        order_id: H256,
    },
    /// Open or grow a position by market order. Equivalent to PlacePerpOrder
    /// with IOC + extreme price; convenience for HFT clients.
    OpenPosition {
        market: DerivId,
        side: Side,
        qty: u128,
        /// Refuses if average fill exceeds bound. Capped at 10_000 (= 100%).
        max_slippage_bps: u16,
    },
    /// Close all or part of a position at market.
    ClosePosition {
        market: DerivId,
        /// `0` = close all.
        qty: u128,
        max_slippage_bps: u16,
    },
    /// Move USD between free balance and a position's isolated margin. Signed
    /// via `is_topup` since on-wire we forbid `i128`.
    AdjustMargin {
        market: DerivId,
        is_topup: bool,
        amount: u128,
    },
    /// Anyone can submit. Engine validates `owner`'s account is liquidatable
    /// at end-of-block. Successful triggers earn the keeper bounty.
    LiquidationTrigger {
        owner: Address,
        market: DerivId,
    },
    /// Posted by the registered oracle authority for a derivative market.
    /// Sender MUST equal the market's `oracle_authority`. `submitted_height`
    /// lets the engine reject stale or replayed updates.
    OracleUpdate {
        market: DerivId,
        px_q8: u128,
        submitted_height: u64,
    },
    /// System tx (sender == sequencer). Triggers a deterministic funding tick
    /// when `height % funding_interval_blocks == 0`. Putting funding in the
    /// tx stream commits it to the tx_root, keeping replicas honest.
    FundingTick {
        market: DerivId,
    },
    /// System tx (sender == sequencer). Run exactly once per dated future at
    /// `expiry_height`. Computes settlement price, closes every open position
    /// at that price, marks the market settled.
    FuturesSettle {
        market: DerivId,
    },
}

impl TxBody {
    /// `true` if this transaction places an order on a book.
    ///
    /// Used to keep order counters separate from transaction counters: a
    /// `Transfer` is a transaction and not an order, and counting it as one is
    /// how the dashboard came to report a six-figure "orders per second" for a
    /// workload that placed no orders. Cancels are not placements and are not
    /// counted here.
    pub fn is_order_placement(&self) -> bool {
        matches!(
            self,
            TxBody::PlaceOrder { .. }
                | TxBody::PlacePerpOrder { .. }
                | TxBody::OpenPosition { .. }
                | TxBody::ClosePosition { .. }
        )
    }

    /// For an order placement, its side and, for a spot order, its market.
    /// `None` for anything that is not an order placement. This is what the
    /// dashboard's side and per-market tiles are fed from once a block has
    /// applied, so those tiles describe orders that landed rather than
    /// submissions that may not have.
    pub fn order_side_and_market(&self) -> Option<(Side, Option<MarketId>)> {
        match self {
            TxBody::PlaceOrder { side, market, .. } => Some((*side, Some(*market))),
            TxBody::PlacePerpOrder { side, .. } | TxBody::OpenPosition { side, .. } => {
                Some((*side, None))
            }
            // Closing a position is an order on the opposite side of the
            // position, which the body does not carry; count it as a
            // placement without a side.
            TxBody::ClosePosition { .. } => None,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub nonce: u64,
    pub chain_id: u32,
    pub body: TxBody,
}

impl Transaction {
    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("Transaction encode")
    }
    pub fn decode(b: &[u8]) -> bincode::Result<Self> {
        bincode::deserialize(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(body: TxBody) {
        let tx = Transaction {
            nonce: 7,
            chain_id: 1,
            body,
        };
        let bytes = tx.encode();
        let back = Transaction::decode(&bytes).unwrap();
        assert_eq!(tx, back);
    }

    #[test]
    fn only_placements_count_as_orders() {
        assert!(TxBody::PlaceOrder {
            market: MarketId::BtcUsd,
            side: Side::Buy,
            price: 1,
            qty: 1,
            tif: Tif::Gtc,
            post_only: false,
            client_order_id: H256([0; 32]),
        }
        .is_order_placement());
        assert!(TxBody::PlacePerpOrder {
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 1,
            qty: 1,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256([0; 32]),
        }
        .is_order_placement());
        assert!(!TxBody::Transfer {
            to: Address([1; 20]),
            asset: AssetId::Usd,
            amount: 1,
        }
        .is_order_placement());
        assert!(!TxBody::CancelOrder {
            order_id: H256([1; 32]),
        }
        .is_order_placement());
    }

    #[test]
    fn place_perp_order_round_trip() {
        round_trip(TxBody::PlacePerpOrder {
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 50_000 * 100_000_000,
            qty: 1_000_000,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256([1; 32]),
        });
    }

    #[test]
    fn cancel_perp_order_round_trip() {
        round_trip(TxBody::CancelPerpOrder {
            order_id: H256([2; 32]),
        });
    }

    #[test]
    fn open_close_position_round_trip() {
        round_trip(TxBody::OpenPosition {
            market: DerivId::perp(1),
            side: Side::Sell,
            qty: 5_000_000,
            max_slippage_bps: 800,
        });
        round_trip(TxBody::ClosePosition {
            market: DerivId::perp(1),
            qty: 0,
            max_slippage_bps: 800,
        });
    }

    #[test]
    fn adjust_margin_round_trip() {
        round_trip(TxBody::AdjustMargin {
            market: DerivId::perp(0),
            is_topup: true,
            amount: 1_000_000_000,
        });
    }

    #[test]
    fn liquidation_trigger_round_trip() {
        round_trip(TxBody::LiquidationTrigger {
            owner: Address([7; 20]),
            market: DerivId::perp(0),
        });
    }

    #[test]
    fn oracle_update_round_trip() {
        round_trip(TxBody::OracleUpdate {
            market: DerivId::perp(0),
            px_q8: 50_000 * 100_000_000,
            submitted_height: 100,
        });
    }

    #[test]
    fn funding_tick_round_trip() {
        round_trip(TxBody::FundingTick {
            market: DerivId::perp(0),
        });
    }

    #[test]
    fn futures_settle_round_trip() {
        round_trip(TxBody::FuturesSettle {
            market: DerivId::future(0),
        });
    }
}
