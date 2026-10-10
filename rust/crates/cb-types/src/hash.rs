//! H256 — 32-byte hash newtype.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct H256(pub [u8; 32]);

impl H256 {
    pub const ZERO: H256 = H256([0u8; 32]);
    pub fn from_slice(b: &[u8]) -> Self {
        let mut out = [0u8; 32];
        out.copy_from_slice(b);
        H256(out)
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H256(0x{})", hex::encode(self.0))
    }
}

impl fmt::Display for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_all_zero_bytes() {
        assert_eq!(H256::ZERO.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn display_is_hex_prefixed() {
        let h = H256([0xab; 32]);
        assert_eq!(format!("{h}"), format!("0x{}", "ab".repeat(32)));
    }

    #[test]
    fn from_slice_roundtrips() {
        let raw = [7u8; 32];
        assert_eq!(H256::from_slice(&raw).as_bytes(), &raw);
    }
}
