//! Movement of value across the exchange boundary.
//!
//! Deposits and withdrawals are the two places where an exchange touches the
//! outside world, and the two places a simulator must fake. The port models
//! the asynchronous reality: a withdrawal is *requested*, then later
//! *settles* or *fails*, and the exchange must hold funds in between.

use crate::error::PortResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// An address the user may send funds to, to credit their exchange account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositIntent {
    /// Correlation id for this intent.
    pub intent_id: String,
    /// Asset the address accepts.
    pub asset: String,
    /// External address or account reference.
    pub address: String,
    /// When the intent stops being valid, if it expires.
    pub expires_at: Option<DateTime<Utc>>,
}

/// The outcome of a withdrawal, once the external system reports back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalReceipt {
    /// Correlation id supplied at request time.
    pub request_id: String,
    /// External transaction reference, when the transfer succeeded.
    pub external_ref: Option<String>,
    /// Terminal state: `settled` or `failed`.
    pub status: String,
    /// Why it failed, when it did.
    pub failure_reason: Option<String>,
    /// When the external system reached this state.
    pub at: DateTime<Utc>,
}

/// Custody of funds outside the exchange ledger.
#[async_trait]
pub trait CustodyProvider: Send + Sync + 'static {
    /// Produce an address the user can deposit `asset` to.
    async fn deposit_intent(&self, user_address: &str, asset: &str) -> PortResult<DepositIntent>;

    /// Request an outbound transfer. Returns a correlation id.
    ///
    /// This does **not** mean the funds have moved. The caller must keep the
    /// amount locked until [`CustodyProvider::withdrawal_status`] reports a
    /// terminal state.
    async fn request_withdrawal(
        &self,
        user_address: &str,
        asset: &str,
        amount: Decimal,
        destination: &str,
    ) -> PortResult<String>;

    /// Poll a withdrawal. `Ok(None)` means still pending.
    async fn withdrawal_status(&self, request_id: &str) -> PortResult<Option<WithdrawalReceipt>>;

    /// Whether the provider is reachable.
    async fn health_check(&self) -> PortResult<()>;
}
