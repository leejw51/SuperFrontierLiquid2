//! Oracle update helpers — pure protocol policy.
//!
//! The actual on-disk write happens in `cb-block::executor`. This module
//! exposes only the deterministic policy: "is this oracle update acceptable?"
//! Pulling the policy into `cb-engine` keeps the executor focused on I/O and
//! makes the rules unit-testable without spinning up a database.

use cb_types::{Address, OraclePrice};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OracleError {
    /// Sender of the OracleUpdate tx is not the registered authority for the
    /// market. Maps to `ExecError::NotAuthorized` upstream.
    #[error("oracle update authority mismatch")]
    Unauthorized,
    /// `submitted_height` is older than the previously-stored oracle's
    /// `source_height`. Strictly older — equal heights are accepted to allow
    /// re-submitting in the same block (idempotent under leader retries).
    #[error("oracle update is stale: submitted={submitted}, current={current}")]
    Stale { submitted: u64, current: u64 },
}

/// Check whether `submitted_height` is acceptable given the existing oracle
/// row (or `None` if no oracle has ever landed for this market). Stale = strictly
/// older than the existing source height.
pub fn check_freshness(
    submitted_height: u64,
    existing: Option<&OraclePrice>,
) -> Result<(), OracleError> {
    if let Some(o) = existing {
        if submitted_height < o.source_height {
            return Err(OracleError::Stale {
                submitted: submitted_height,
                current: o.source_height,
            });
        }
    }
    Ok(())
}

/// Check that `sender` is the configured authority. Pure; doesn't touch state.
pub fn check_authority(sender: Address, authority: Address) -> Result<(), OracleError> {
    if sender == authority {
        Ok(())
    } else {
        Err(OracleError::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::DerivId;

    fn op(h: u64) -> OraclePrice {
        OraclePrice {
            market: DerivId::perp(0),
            px_q8: 5_000_000_000_000,
            source_height: h,
            authority: Address([1; 20]),
        }
    }

    #[test]
    fn fresh_when_no_prior_oracle() {
        assert_eq!(check_freshness(100, None), Ok(()));
    }

    #[test]
    fn rejects_strictly_older_height() {
        let o = op(100);
        assert_eq!(
            check_freshness(99, Some(&o)),
            Err(OracleError::Stale {
                submitted: 99,
                current: 100,
            })
        );
    }

    #[test]
    fn equal_height_is_accepted() {
        // Idempotent re-submit: leader can re-include the same OracleUpdate
        // and we don't reject it. Same-height with same px is a no-op write,
        // same-height with a different px is the leader's prerogative and
        // signed by the authority either way.
        let o = op(100);
        assert_eq!(check_freshness(100, Some(&o)), Ok(()));
    }

    #[test]
    fn newer_height_is_accepted() {
        let o = op(100);
        assert_eq!(check_freshness(101, Some(&o)), Ok(()));
    }

    #[test]
    fn authority_match_required() {
        let a = Address([1; 20]);
        let b = Address([2; 20]);
        assert_eq!(check_authority(a, a), Ok(()));
        assert_eq!(check_authority(b, a), Err(OracleError::Unauthorized));
    }
}
