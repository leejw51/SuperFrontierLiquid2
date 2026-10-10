//! Static validation: shape checks that don't need state.

use crate::tx::{Transaction, TxBody};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TxStaticError {
    #[error("zero amount")]
    ZeroAmount,
    #[error("zero quantity")]
    ZeroQty,
    #[error("zero price")]
    ZeroPrice,
    #[error("slippage bound out of range (got {0}, max 10000)")]
    BadSlippage(u16),
    #[error("oracle price out of range")]
    OraclePxOutOfRange,
    #[error("invalid deriv-id kind")]
    InvalidDerivId,
}

/// Largest oracle price the protocol will accept on the wire. Sized to leave
/// plenty of headroom for `qty * px_q8` notional math without overflowing
/// `u128`. 1e36 in the design doc; we use 10^36 here.
const ORACLE_PX_MAX: u128 = 10u128.pow(36);

pub fn validate_static(t: &Transaction) -> Result<(), TxStaticError> {
    match &t.body {
        // ---------------- spot / accounts ----------------
        TxBody::Transfer { amount, .. }
        | TxBody::Deposit { amount, .. }
        | TxBody::Withdraw { amount, .. } => {
            if *amount == 0 {
                return Err(TxStaticError::ZeroAmount);
            }
        }
        TxBody::PlaceOrder { qty, price, .. } => {
            if *qty == 0 {
                return Err(TxStaticError::ZeroQty);
            }
            if *price == 0 {
                return Err(TxStaticError::ZeroPrice);
            }
        }
        TxBody::CancelOrder { .. }
        | TxBody::BatchCancel { .. }
        | TxBody::WithdrawSettled { .. } => {}

        // ---------------- derivatives ----------------
        TxBody::PlacePerpOrder {
            market,
            qty,
            price_q8,
            ..
        } => {
            check_deriv(market)?;
            if *qty == 0 {
                return Err(TxStaticError::ZeroQty);
            }
            if *price_q8 == 0 {
                return Err(TxStaticError::ZeroPrice);
            }
        }
        TxBody::CancelPerpOrder { .. } => {}
        TxBody::OpenPosition {
            market,
            qty,
            max_slippage_bps,
            ..
        } => {
            check_deriv(market)?;
            if *qty == 0 {
                return Err(TxStaticError::ZeroQty);
            }
            check_slippage(*max_slippage_bps)?;
        }
        TxBody::ClosePosition {
            market,
            max_slippage_bps,
            ..
        } => {
            // qty == 0 is the "close all" sentinel, so we don't reject it here.
            check_deriv(market)?;
            check_slippage(*max_slippage_bps)?;
        }
        TxBody::AdjustMargin { market, amount, .. } => {
            check_deriv(market)?;
            if *amount == 0 {
                return Err(TxStaticError::ZeroAmount);
            }
        }
        TxBody::LiquidationTrigger { market, .. } => {
            check_deriv(market)?;
        }
        TxBody::OracleUpdate { market, px_q8, .. } => {
            check_deriv(market)?;
            if *px_q8 == 0 {
                return Err(TxStaticError::ZeroPrice);
            }
            if *px_q8 > ORACLE_PX_MAX {
                return Err(TxStaticError::OraclePxOutOfRange);
            }
        }
        TxBody::FundingTick { market } | TxBody::FuturesSettle { market } => {
            check_deriv(market)?;
        }
    }
    Ok(())
}

fn check_slippage(bps: u16) -> Result<(), TxStaticError> {
    if bps > 10_000 {
        return Err(TxStaticError::BadSlippage(bps));
    }
    Ok(())
}

fn check_deriv(d: &cb_types::DerivId) -> Result<(), TxStaticError> {
    d.kind()
        .map(|_| ())
        .map_err(|_| TxStaticError::InvalidDerivId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::{Address, AssetId, DerivId, Side, Tif, H256};

    fn tx(body: TxBody) -> Transaction {
        Transaction {
            nonce: 0,
            chain_id: 1,
            body,
        }
    }

    #[test]
    fn rejects_zero_transfer() {
        let t = tx(TxBody::Transfer {
            to: Address::ZERO,
            asset: AssetId::Usd,
            amount: 0,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::ZeroAmount));
    }

    #[test]
    fn accepts_nonzero_transfer() {
        let t = tx(TxBody::Transfer {
            to: Address::ZERO,
            asset: AssetId::Usd,
            amount: 1,
        });
        assert_eq!(validate_static(&t), Ok(()));
    }

    // ---------------- derivative validation ----------------

    #[test]
    fn place_perp_order_rejects_zero_qty_and_price() {
        let t = tx(TxBody::PlacePerpOrder {
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 100,
            qty: 0,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256::ZERO,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::ZeroQty));

        let t = tx(TxBody::PlacePerpOrder {
            market: DerivId::perp(0),
            side: Side::Buy,
            price_q8: 0,
            qty: 100,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256::ZERO,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::ZeroPrice));
    }

    #[test]
    fn place_perp_order_rejects_invalid_deriv() {
        let t = tx(TxBody::PlacePerpOrder {
            market: DerivId([9, 0, 0, 0]), // unknown kind
            side: Side::Buy,
            price_q8: 100,
            qty: 100,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: false,
            client_order_id: H256::ZERO,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::InvalidDerivId));
    }

    #[test]
    fn open_position_validates_slippage() {
        let mk = |s| {
            tx(TxBody::OpenPosition {
                market: DerivId::perp(0),
                side: Side::Buy,
                qty: 1,
                max_slippage_bps: s,
            })
        };
        assert_eq!(validate_static(&mk(0)), Ok(()));
        assert_eq!(validate_static(&mk(10_000)), Ok(()));
        assert_eq!(
            validate_static(&mk(10_001)),
            Err(TxStaticError::BadSlippage(10_001))
        );
    }

    #[test]
    fn close_position_allows_zero_qty_close_all() {
        let t = tx(TxBody::ClosePosition {
            market: DerivId::perp(0),
            qty: 0,
            max_slippage_bps: 100,
        });
        assert_eq!(validate_static(&t), Ok(()));
    }

    #[test]
    fn adjust_margin_rejects_zero_amount() {
        let t = tx(TxBody::AdjustMargin {
            market: DerivId::perp(0),
            is_topup: true,
            amount: 0,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::ZeroAmount));
    }

    #[test]
    fn oracle_update_rejects_zero_and_overflow() {
        let t = tx(TxBody::OracleUpdate {
            market: DerivId::perp(0),
            px_q8: 0,
            submitted_height: 1,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::ZeroPrice));

        let t = tx(TxBody::OracleUpdate {
            market: DerivId::perp(0),
            px_q8: u128::MAX,
            submitted_height: 1,
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::OraclePxOutOfRange));
    }

    #[test]
    fn funding_tick_and_settle_basic_validation() {
        let t = tx(TxBody::FundingTick {
            market: DerivId::perp(0),
        });
        assert_eq!(validate_static(&t), Ok(()));

        let t = tx(TxBody::FuturesSettle {
            market: DerivId::future(0),
        });
        assert_eq!(validate_static(&t), Ok(()));

        let t = tx(TxBody::FundingTick {
            market: DerivId([9, 0, 0, 0]),
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::InvalidDerivId));
    }

    #[test]
    fn liquidation_trigger_validation() {
        let t = tx(TxBody::LiquidationTrigger {
            owner: Address([1; 20]),
            market: DerivId::perp(0),
        });
        assert_eq!(validate_static(&t), Ok(()));

        let t = tx(TxBody::LiquidationTrigger {
            owner: Address([1; 20]),
            market: DerivId([99, 0, 0, 0]),
        });
        assert_eq!(validate_static(&t), Err(TxStaticError::InvalidDerivId));
    }

    #[test]
    fn spot_orders_reject_zero_quantity_before_zero_price() {
        let mk = |qty, price| {
            tx(TxBody::PlaceOrder {
                market: cb_types::MarketId::BtcUsd,
                side: Side::Buy,
                price,
                qty,
                tif: Tif::Gtc,
                post_only: false,
                client_order_id: H256::default(),
            })
        };
        assert_eq!(validate_static(&mk(1, 1)), Ok(()));
        assert_eq!(validate_static(&mk(0, 1)), Err(TxStaticError::ZeroQty));
        assert_eq!(validate_static(&mk(1, 0)), Err(TxStaticError::ZeroPrice));
        assert_eq!(validate_static(&mk(0, 0)), Err(TxStaticError::ZeroQty));
    }

    #[test]
    fn close_position_bounds_slippage_and_market_kind() {
        let mk = |market, bps| {
            tx(TxBody::ClosePosition {
                market,
                qty: 5,
                max_slippage_bps: bps,
            })
        };
        assert_eq!(validate_static(&mk(DerivId::perp(0), 10_000)), Ok(()));
        assert_eq!(validate_static(&mk(DerivId::future(3), 0)), Ok(()));
        assert_eq!(
            validate_static(&mk(DerivId::perp(0), 10_001)),
            Err(TxStaticError::BadSlippage(10_001))
        );
        assert_eq!(
            validate_static(&mk(DerivId::perp(0), u16::MAX)),
            Err(TxStaticError::BadSlippage(u16::MAX))
        );
        assert_eq!(
            validate_static(&mk(DerivId([0, 0, 0, 0]), 1)),
            Err(TxStaticError::InvalidDerivId)
        );
    }

    #[test]
    fn deposits_and_withdrawals_reject_zero_amounts() {
        let dep = |amount| {
            tx(TxBody::Deposit {
                user: Address([1; 20]),
                asset: AssetId::Usd,
                amount,
                external_tx_ref: "ref".into(),
            })
        };
        assert_eq!(validate_static(&dep(0)), Err(TxStaticError::ZeroAmount));
        assert_eq!(validate_static(&dep(1)), Ok(()));
        let wd = |amount| {
            tx(TxBody::Withdraw {
                asset: AssetId::Usd,
                amount,
                external_address: "x".into(),
            })
        };
        assert_eq!(validate_static(&wd(0)), Err(TxStaticError::ZeroAmount));
        assert_eq!(validate_static(&wd(u128::MAX)), Ok(()));
    }

    /// The oracle bound is inclusive at 1e36 and every derivative variant
    /// refuses an unknown market kind, so none of them reaches the executor
    /// with a market it cannot route.
    #[test]
    fn every_derivative_variant_refuses_an_invalid_market_kind() {
        let bad = DerivId([7, 0, 0, 0]);
        let bodies = [
            TxBody::PlacePerpOrder {
                market: bad,
                side: Side::Buy,
                price_q8: 1,
                qty: 1,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: false,
                client_order_id: H256::default(),
            },
            TxBody::OpenPosition {
                market: bad,
                side: Side::Buy,
                qty: 1,
                max_slippage_bps: 1,
            },
            TxBody::ClosePosition {
                market: bad,
                qty: 0,
                max_slippage_bps: 1,
            },
            TxBody::AdjustMargin {
                market: bad,
                is_topup: true,
                amount: 1,
            },
            TxBody::LiquidationTrigger {
                owner: Address([1; 20]),
                market: bad,
            },
            TxBody::OracleUpdate {
                market: bad,
                px_q8: 1,
                submitted_height: 0,
            },
            TxBody::FundingTick { market: bad },
            TxBody::FuturesSettle { market: bad },
        ];
        for b in bodies {
            assert_eq!(
                validate_static(&tx(b.clone())),
                Err(TxStaticError::InvalidDerivId),
                "{b:?}"
            );
        }
        let at = |px| {
            tx(TxBody::OracleUpdate {
                market: DerivId::perp(0),
                px_q8: px,
                submitted_height: 0,
            })
        };
        assert_eq!(validate_static(&at(10u128.pow(36))), Ok(()));
        assert_eq!(
            validate_static(&at(10u128.pow(36) + 1)),
            Err(TxStaticError::OraclePxOutOfRange)
        );
    }
}
