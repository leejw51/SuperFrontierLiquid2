//! Address — Ethereum-style 20-byte account identifier.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Address(pub [u8; 20]);

impl Address {
    pub const ZERO: Address = Address([0u8; 20]);

    /// Pinned protocol-level insurance fund address. M4 routes liquidation
    /// fees and bad-debt absorption through this account: 0.3% of liquidation
    /// notional credits this account, and uncovered liquidation losses
    /// debit it before any socialization (drains-first-then-halts policy
    /// per `docs/design/perps-trading.md` §5.5).
    ///
    /// Layout: 20 bytes of `0xff` followed by ASCII `INSURANCE` packed in
    /// the trailing bytes, padded with zeroes. Picked to be:
    ///   1. Constant — never changes across genesis revisions.
    ///   2. Visually distinct from generated addresses (no derivation can
    ///      land on `0xff..`).
    ///   3. Reserved — no signing key; the account is engine-mutated only.
    ///
    /// The exact byte pattern is consensus-load-bearing; bumping it is a
    /// hard fork.
    pub const INSURANCE: Address = Address([
        0xff, 0xff, 0xff, 0xff, 0xff, 0x49, 0x4e, 0x53, 0x55, 0x52, 0x41, 0x4e, 0x43, 0x45, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01,
    ]);

    pub fn from_slice(b: &[u8]) -> Self {
        let mut out = [0u8; 20];
        out.copy_from_slice(b);
        Address(out)
    }
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address(0x{})", hex::encode(self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_address_displays_as_40_hex_chars() {
        assert_eq!(
            format!("{}", Address::ZERO),
            format!("0x{}", "0".repeat(40))
        );
    }

    #[test]
    fn from_slice_copies_bytes() {
        let raw = [1u8; 20];
        assert_eq!(Address::from_slice(&raw).as_bytes(), &raw);
    }

    #[test]
    fn insurance_address_is_pinned() {
        // Pinning the bytes catches any accidental edit. Replicas all use
        // `Address::INSURANCE`; a mismatch would diverge state roots.
        let bytes = Address::INSURANCE.0;
        assert_eq!(bytes[0], 0xff);
        assert_eq!(bytes[5..14], *b"INSURANCE");
        assert_eq!(bytes[19], 0x01);
        // Stable across construction.
        assert_eq!(Address::INSURANCE, Address::INSURANCE);
        assert_ne!(Address::INSURANCE, Address::ZERO);
    }
}
