//! The trie's root is a function of its contents and nothing else.
//!
//! A state root is only worth something if two nodes holding the same state
//! compute the same hash for it, whatever path each took to get there: a
//! replica that replayed every block, one that synced from a snapshot, one
//! that saw a position opened and closed where another never saw it at all.
//! These tests drive the trie through long random histories and compare the
//! result against a trie built fresh from the final contents alone.
//!
//! What the group proves:
//! - any interleaving of inserts, overwrites and removes lands on the root of
//!   the fresh build of what is left (at sizes from a handful of keys up to
//!   thousands, so deep shared prefixes and long collapse chains occur);
//! - removing everything returns the empty root;
//! - a removal of a key that is not there changes nothing;
//! - arbitrary bytes handed to the node decoder never panic, and whatever
//!   decodes re-encodes to the same bytes (one encoding per node).

use cb_trie::node::{Node, EMPTY};
use cb_trie::{MemKv, MerkleTrie};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn fresh_root(contents: &BTreeMap<Vec<u8>, Vec<u8>>) -> cb_types::H256 {
    let mut kv = MemKv::default();
    let mut t = MerkleTrie::new(&mut kv);
    for (k, v) in contents {
        t.insert(k, v.clone()).unwrap();
    }
    t.root()
}

/// A tiny deterministic generator so the large tests need no dependency and
/// replay identically.
struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[derive(Debug, Clone)]
enum Op {
    Put(u16, u8),
    Del(u16),
}

fn ops(key_space: u16, max_len: usize) -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
        prop_oneof![
            3 => (0..key_space, any::<u8>()).prop_map(|(k, v)| Op::Put(k, v)),
            2 => (0..key_space).prop_map(Op::Del),
        ],
        0..max_len,
    )
}

fn run(ops: &[Op]) -> (cb_types::H256, BTreeMap<Vec<u8>, Vec<u8>>) {
    let mut kv = MemKv::default();
    let mut t = MerkleTrie::new(&mut kv);
    let mut oracle = BTreeMap::new();
    for op in ops {
        match *op {
            Op::Put(k, v) => {
                let key = k.to_be_bytes().to_vec();
                t.insert(&key, vec![v]).unwrap();
                oracle.insert(key, vec![v]);
            }
            Op::Del(k) => {
                let key = k.to_be_bytes().to_vec();
                t.remove(&key).unwrap();
                oracle.remove(&key);
            }
        }
    }
    (t.root(), oracle)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Small key space, long histories: most removes hit, most keys are
    /// written several times.
    #[test]
    fn history_does_not_reach_the_root_in_a_small_key_space(ops in ops(16, 200)) {
        let (root, oracle) = run(&ops);
        prop_assert_eq!(root, fresh_root(&oracle));
    }

    /// Larger key space: deeper tries, collapses several levels up.
    #[test]
    fn history_does_not_reach_the_root_in_a_large_key_space(ops in ops(2048, 600)) {
        let (root, oracle) = run(&ops);
        prop_assert_eq!(root, fresh_root(&oracle));
    }

    /// Two different histories that end with the same contents agree: the
    /// second history is the first one's final contents inserted in a random
    /// order, preceded by junk that is written and then removed.
    #[test]
    fn two_histories_with_the_same_ending_agree(
        ops in ops(512, 300), junk in proptest::collection::vec(512u16..1024, 0..100), seed in any::<u64>(),
    ) {
        let (root_a, oracle) = run(&ops);
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        for j in &junk {
            t.insert(&j.to_be_bytes(), b"junk".to_vec()).unwrap();
        }
        let mut entries: Vec<_> = oracle.iter().collect();
        let mut rng = SplitMix(seed);
        for i in (1..entries.len()).rev() {
            entries.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        for (k, v) in entries {
            t.insert(k, v.clone()).unwrap();
        }
        for j in &junk {
            t.remove(&j.to_be_bytes()).unwrap();
        }
        prop_assert_eq!(t.root(), root_a);
    }

    #[test]
    fn node_decode_survives_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..160)) {
        if let Ok(n) = Node::decode(&bytes) {
            prop_assert_eq!(n.encode(), bytes);
        }
    }

    #[test]
    fn node_decode_inverts_encode(
        suffix in proptest::collection::vec(any::<u8>(), 0..=255),
        value in proptest::collection::vec(any::<u8>(), 0..300),
        l in any::<[u8; 32]>(), r in any::<[u8; 32]>(),
    ) {
        let leaf = Node::Leaf { key_suffix: suffix, value };
        prop_assert_eq!(Node::decode(&leaf.encode()).unwrap(), leaf);
        let branch = Node::Branch { left: cb_types::H256(l), right: cb_types::H256(r) };
        prop_assert_eq!(Node::decode(&branch.encode()).unwrap(), branch);
    }
}

#[test]
fn thousands_of_keys_removed_in_random_order_leave_the_fresh_root() {
    // 5,000 keys in, a random 60% out in a random order, then the rest out.
    let mut rng = SplitMix(0xC0FFEE);
    let mut kv = MemKv::default();
    let mut t = MerkleTrie::new(&mut kv);
    let mut oracle = BTreeMap::new();
    for i in 0u32..5_000 {
        let key = i.to_be_bytes().to_vec();
        let value = rng.next().to_be_bytes().to_vec();
        t.insert(&key, value.clone()).unwrap();
        oracle.insert(key, value);
    }
    let mut keys: Vec<Vec<u8>> = oracle.keys().cloned().collect();
    for i in (1..keys.len()).rev() {
        keys.swap(i, (rng.next() % (i as u64 + 1)) as usize);
    }
    let (gone, kept) = keys.split_at(3_000);
    for k in gone {
        t.remove(k).unwrap();
        oracle.remove(k);
    }
    assert_eq!(
        t.root(),
        fresh_root(&oracle),
        "after removing 3,000 of 5,000"
    );
    for k in kept {
        assert_eq!(t.get(k).unwrap(), oracle.get(k).cloned());
    }
    for k in kept {
        t.remove(k).unwrap();
    }
    assert_eq!(t.root(), EMPTY, "removing every key empties the trie");
}

#[test]
fn removing_absent_keys_from_a_large_trie_changes_nothing() {
    let mut kv = MemKv::default();
    let mut t = MerkleTrie::new(&mut kv);
    for i in 0u32..1_000 {
        t.insert(&i.to_be_bytes(), vec![1]).unwrap();
    }
    let before = t.root();
    for i in 1_000u32..2_000 {
        t.remove(&i.to_be_bytes()).unwrap();
    }
    assert_eq!(t.root(), before);
}
