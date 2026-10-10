//! keccak256 — the 32-byte legacy Ethereum hash function (NOT SHA3-256).

use cb_types::H256;
use tiny_keccak::{Hasher, Keccak};

pub fn keccak256(input: &[u8]) -> H256 {
    let mut h = Keccak::v256();
    h.update(input);
    let mut out = [0u8; 32];
    h.finalize(&mut out);
    H256(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_matches_known_vector() {
        // keccak256("") = c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470
        let got = keccak256(&[]);
        assert_eq!(
            hex::encode(got.0),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn abc_matches_known_vector() {
        // keccak256("abc") = 4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45
        let got = keccak256(b"abc");
        assert_eq!(
            hex::encode(got.0),
            "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45"
        );
    }
}
