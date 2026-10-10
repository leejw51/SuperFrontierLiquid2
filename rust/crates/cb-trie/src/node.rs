//! Trie node encoding.
//!
//! - Leaf:   tag=0x00 || key_suffix_len(u8) || key_suffix || value_len(u32 BE) || value
//! - Branch: tag=0x01 || left(H256) || right(H256)
//!
//! Empty subtrees use H256::ZERO sentinel.

use cb_crypto::keccak256;
use cb_types::H256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Leaf { key_suffix: Vec<u8>, value: Vec<u8> },
    Branch { left: H256, right: H256 },
}

pub const EMPTY: H256 = H256([0u8; 32]);

impl Node {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Node::Leaf { key_suffix, value } => {
                let mut out = Vec::with_capacity(1 + 1 + key_suffix.len() + 4 + value.len());
                out.push(0x00);
                out.push(key_suffix.len() as u8);
                out.extend_from_slice(key_suffix);
                out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                out.extend_from_slice(value);
                out
            }
            Node::Branch { left, right } => {
                let mut out = Vec::with_capacity(1 + 32 + 32);
                out.push(0x01);
                out.extend_from_slice(&left.0);
                out.extend_from_slice(&right.0);
                out
            }
        }
    }

    pub fn hash(&self) -> H256 {
        keccak256(&self.encode())
    }

    /// Inverse of [`Node::encode`], refusing anything `encode` could not have
    /// produced.
    ///
    /// Node bytes come from a store, and a store can be truncated by a crash,
    /// corrupted on disk, or handed over by a peer during state sync. Every
    /// length is therefore checked before it is used as an index, and trailing
    /// bytes are an error too: a node has exactly one encoding, and accepting
    /// padding would let two byte strings stand for the same node.
    pub fn decode(bytes: &[u8]) -> Result<Node, String> {
        let (&tag, rest) = bytes.split_first().ok_or("empty node")?;
        match tag {
            0x00 => {
                let (&suffix_len, rest) = rest.split_first().ok_or("leaf without a length")?;
                let suffix_len = suffix_len as usize;
                if rest.len() < suffix_len + 4 {
                    return Err(format!(
                        "leaf truncated: {} bytes after the suffix length, need at least {}",
                        rest.len(),
                        suffix_len + 4
                    ));
                }
                let (key_suffix, rest) = rest.split_at(suffix_len);
                let (len, value) = rest.split_at(4);
                let val_len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
                if value.len() != val_len {
                    return Err(format!(
                        "leaf declares a {val_len}-byte value but carries {}",
                        value.len()
                    ));
                }
                Ok(Node::Leaf {
                    key_suffix: key_suffix.to_vec(),
                    value: value.to_vec(),
                })
            }
            0x01 => {
                if rest.len() != 64 {
                    return Err(format!("branch is {} bytes, must be 64", rest.len()));
                }
                Ok(Node::Branch {
                    left: H256::from_slice(&rest[..32]),
                    right: H256::from_slice(&rest[32..]),
                })
            }
            other => Err(format!("bad tag {other:#04x}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_hash_is_deterministic() {
        let a = Node::Leaf {
            key_suffix: vec![1, 2, 3],
            value: vec![9; 4],
        };
        let b = Node::Leaf {
            key_suffix: vec![1, 2, 3],
            value: vec![9; 4],
        };
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn branch_with_distinct_children_hashes_distinctly() {
        let l = Node::Branch {
            left: H256([1; 32]),
            right: H256([2; 32]),
        };
        let r = Node::Branch {
            left: H256([2; 32]),
            right: H256([1; 32]),
        };
        assert_ne!(l.hash(), r.hash());
    }

    #[test]
    fn decode_inverts_encode_for_both_kinds() {
        for n in [
            Node::Leaf {
                key_suffix: vec![7; 32],
                value: b"balance".to_vec(),
            },
            Node::Leaf {
                key_suffix: vec![1; 32],
                value: vec![],
            },
            Node::Branch {
                left: H256([1; 32]),
                right: H256::ZERO,
            },
        ] {
            assert_eq!(Node::decode(&n.encode()).unwrap(), n);
        }
    }

    #[test]
    fn every_truncation_of_a_node_is_an_error_not_a_panic() {
        // Cut a leaf and a branch at every possible length. Each prefix must
        // come back as `Err`; before `decode` existed the trie indexed these
        // bytes directly and a short node took the process down.
        let leaf = Node::Leaf {
            key_suffix: vec![9; 32],
            value: b"100".to_vec(),
        }
        .encode();
        let branch = Node::Branch {
            left: H256([1; 32]),
            right: H256([2; 32]),
        }
        .encode();
        for bytes in [leaf, branch] {
            for cut in 0..bytes.len() {
                assert!(
                    Node::decode(&bytes[..cut]).is_err(),
                    "a node cut to {cut} of {} bytes decoded",
                    bytes.len()
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_and_unknown_tags_are_refused() {
        let mut leaf = Node::Leaf {
            key_suffix: vec![9; 32],
            value: b"100".to_vec(),
        }
        .encode();
        leaf.push(0);
        assert!(Node::decode(&leaf).is_err(), "padding after the value");
        let mut branch = Node::Branch {
            left: H256([1; 32]),
            right: H256([2; 32]),
        }
        .encode();
        branch.push(0);
        assert!(Node::decode(&branch).is_err(), "padding after the children");
        assert!(Node::decode(&[0x02; 65]).is_err(), "unknown tag");
    }
}
