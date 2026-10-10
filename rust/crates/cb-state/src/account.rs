//! AccountState: nonce + per-asset free and locked balances.

use cb_types::AssetId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// An arithmetic edge on an account's balances.
///
/// These used to be `expect`s. In an executor a panic is a halted chain — and
/// they were reachable: an order left resting by a rolled-back block is backed
/// by a lock that was never taken, so the fill that matches it calls `unlock`
/// on zero and killed the sequencer process. A rejected transaction is
/// strictly better than a dead network, so the arithmetic reports and the
/// caller decides. Every caller in the executor turns one of these into a
/// rejected transaction, which is rolled back whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BalanceError {
    #[error("balance overflow")]
    BalanceOverflow,
    #[error("balance underflow")]
    BalanceUnderflow,
    #[error("lock overflow")]
    LockOverflow,
    #[error("unlock underflow")]
    UnlockUnderflow,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub nonce: u64,
    /// asset key = `AssetId::as_u8()` -> amount
    pub balances: BTreeMap<u8, u128>,
    pub locked: BTreeMap<u8, u128>,
}

impl AccountState {
    pub fn balance(&self, a: AssetId) -> u128 {
        *self.balances.get(&a.as_u8()).unwrap_or(&0)
    }
    pub fn locked_of(&self, a: AssetId) -> u128 {
        *self.locked.get(&a.as_u8()).unwrap_or(&0)
    }
    pub fn available(&self, a: AssetId) -> u128 {
        self.balance(a).saturating_sub(self.locked_of(a))
    }

    // All four compute first and write second, so a failure leaves the maps
    // exactly as they were. That matters because these structures are trie
    // leaves: an `asset -> 0` entry inserted on a failed path and then
    // committed would encode differently from the same account without it,
    // and shift the state root.
    pub fn add(&mut self, a: AssetId, n: u128) -> Result<(), BalanceError> {
        let next = self
            .balance(a)
            .checked_add(n)
            .ok_or(BalanceError::BalanceOverflow)?;
        self.balances.insert(a.as_u8(), next);
        Ok(())
    }
    pub fn sub(&mut self, a: AssetId, n: u128) -> Result<(), BalanceError> {
        let next = self
            .balance(a)
            .checked_sub(n)
            .ok_or(BalanceError::BalanceUnderflow)?;
        self.balances.insert(a.as_u8(), next);
        Ok(())
    }
    pub fn lock(&mut self, a: AssetId, n: u128) -> Result<(), BalanceError> {
        let next = self
            .locked_of(a)
            .checked_add(n)
            .ok_or(BalanceError::LockOverflow)?;
        self.locked.insert(a.as_u8(), next);
        Ok(())
    }
    /// Release `n` of `a`'s locked balance.
    ///
    /// Returns [`BalanceError::UnlockUnderflow`] rather than panicking when
    /// the account has less locked than the caller is releasing. That is a
    /// corrupted-state signal — the lock was never taken — and the right
    /// response is to reject the transaction that noticed, not to stop the
    /// chain.
    pub fn unlock(&mut self, a: AssetId, n: u128) -> Result<(), BalanceError> {
        let next = self
            .locked_of(a)
            .checked_sub(n)
            .ok_or(BalanceError::UnlockUnderflow)?;
        self.locked.insert(a.as_u8(), next);
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("AccountState encode")
    }
    pub fn decode(b: &[u8]) -> bincode::Result<Self> {
        bincode::deserialize(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_subtracts_locked() {
        let mut a = AccountState::default();
        a.add(AssetId::Usd, 100).unwrap();
        a.lock(AssetId::Usd, 30).unwrap();
        assert_eq!(a.available(AssetId::Usd), 70);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut a = AccountState {
            nonce: 7,
            ..Default::default()
        };
        a.add(AssetId::Btc, 1_000).unwrap();
        let b = AccountState::decode(&a.encode()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unlock_more_than_locked_is_an_error_not_a_panic() {
        let mut a = AccountState::default();
        a.add(AssetId::Btc, 100).unwrap();
        // No lock was ever taken — exactly the state a rolled-back block used
        // to leave behind.
        assert_eq!(
            a.unlock(AssetId::Btc, 1),
            Err(BalanceError::UnlockUnderflow)
        );
        // And the account is untouched.
        assert_eq!(a.locked_of(AssetId::Btc), 0);
        assert_eq!(a.balance(AssetId::Btc), 100);
    }

    #[test]
    fn sub_below_zero_is_an_error_not_a_panic() {
        let mut a = AccountState::default();
        assert_eq!(a.sub(AssetId::Usd, 1), Err(BalanceError::BalanceUnderflow));
        assert_eq!(a.balance(AssetId::Usd), 0);
        assert!(a.balances.is_empty(), "a failed sub must not create a row");
    }

    #[test]
    fn add_past_u128_max_is_an_error_not_a_panic() {
        let mut a = AccountState::default();
        a.add(AssetId::Usd, u128::MAX).unwrap();
        assert_eq!(a.add(AssetId::Usd, 1), Err(BalanceError::BalanceOverflow));
        assert_eq!(a.balance(AssetId::Usd), u128::MAX);
    }
}
