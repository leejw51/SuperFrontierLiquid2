//! Insert/get/remove/root over a binary radix trie keyed by keccak256(key) bits.

use crate::node::{Node, EMPTY};
use crate::{KvStore, TrieError};
use cb_crypto::keccak256;
use cb_types::H256;

pub struct MerkleTrie<'a, S: KvStore> {
    store: &'a mut S,
    root: H256,
}

fn bit(bytes: &[u8], idx: usize) -> u8 {
    (bytes[idx / 8] >> (7 - (idx % 8))) & 1
}

impl<'a, S: KvStore> MerkleTrie<'a, S> {
    pub fn new(store: &'a mut S) -> Self {
        Self { store, root: EMPTY }
    }
    pub fn open(store: &'a mut S, root: H256) -> Self {
        Self { store, root }
    }
    pub fn root(&self) -> H256 {
        self.root
    }

    pub fn insert(&mut self, key: &[u8], value: Vec<u8>) -> Result<(), TrieError> {
        let path = keccak256(key);
        self.root = self.insert_inner(self.root, path, 0, value)?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, TrieError> {
        let path = keccak256(key);
        self.get_inner(self.root, &path, 0)
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<(), TrieError> {
        let path = keccak256(key);
        self.root = self.remove_inner(self.root, &path, 0)?;
        Ok(())
    }

    /// Pre-populate the underlying store's cache with every node along the
    /// paths to `keys`. Hashes the keys, then walks the trie level by level
    /// in BFS order: at each depth we collect the distinct branch-child
    /// hashes referenced by the active paths and issue one `multi_get` for
    /// the next level. This collapses N keys × ~depth round-trips down to
    /// ~depth batched round-trips total — the speedup grows with batch size
    /// and with how much of the trie sits on disk vs in cache.
    ///
    /// **Determinism / hashing semantics are unchanged.** Prefetch only
    /// warms the read path; it does not store new nodes, mutate the root,
    /// or alter how `insert`/`get` traverse. A subsequent `commit_batch`
    /// produces the identical trie root with or without this call.
    ///
    /// Empty subtrees (`H256::ZERO`) and already-cached nodes are filtered
    /// out before the batch read so we don't waste a multi-get slot on
    /// keys we know we don't need.
    pub fn prefetch_paths(&mut self, keys: &[&[u8]]) -> Result<(), TrieError> {
        if keys.is_empty() || self.root == EMPTY {
            return Ok(());
        }
        // Hash every key once so the BFS walk doesn't re-hash per level.
        let paths: Vec<H256> = keys.iter().map(|k| keccak256(k)).collect();

        // Frontier = (node_hash, set_of_path_indices_routed_through_it).
        // We start with everything pointed at root.
        let mut frontier: Vec<(H256, Vec<usize>)> = vec![(self.root, (0..paths.len()).collect())];

        // Walk down at most 256 levels (= H256 bit width). In practice the
        // trie depth is much smaller; we exit early when the frontier is
        // empty or when every node in it is a leaf / EMPTY.
        for depth in 0..256 {
            // Collect the distinct hashes at this level so we can issue
            // one multi_get. Filter EMPTY (no node to fetch) and dedup
            // via a HashSet so the per-level cost stays linear in
            // frontier size — at deep levels the frontier can fan out
            // to N entries.
            let mut seen: std::collections::HashSet<H256> =
                std::collections::HashSet::with_capacity(frontier.len());
            let mut to_fetch: Vec<H256> = Vec::with_capacity(frontier.len());
            for (h, _) in &frontier {
                if *h != EMPTY && seen.insert(*h) {
                    to_fetch.push(*h);
                }
            }
            if to_fetch.is_empty() {
                break;
            }
            // Pre-populate the store's cache. The trie itself does the
            // actual decode through `load_node` later; we just want the
            // bytes resident.
            self.store.prefetch(&to_fetch)?;
            // Walk one level down: for each frontier entry, decode the
            // node, route each path's bit to the matching child, and emit
            // a child frontier entry.
            let mut next: Vec<(H256, Vec<usize>)> = Vec::new();
            for (h, idxs) in frontier {
                if h == EMPTY {
                    continue;
                }
                let node = match self.load_node(h)? {
                    Some(n) => n,
                    None => continue,
                };
                match node {
                    Node::Leaf { .. } => {
                        // Leaf already decoded into cache; nothing further
                        // to walk.
                    }
                    Node::Branch { left, right } => {
                        let mut left_idxs: Vec<usize> = Vec::new();
                        let mut right_idxs: Vec<usize> = Vec::new();
                        for i in idxs {
                            if bit(&paths[i].0, depth) == 0 {
                                left_idxs.push(i);
                            } else {
                                right_idxs.push(i);
                            }
                        }
                        if !left_idxs.is_empty() && left != EMPTY {
                            next.push((left, left_idxs));
                        }
                        if !right_idxs.is_empty() && right != EMPTY {
                            next.push((right, right_idxs));
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(())
    }

    fn store_node(&mut self, node: &Node) -> Result<H256, TrieError> {
        let h = node.hash();
        self.store.put(h, node.encode())?;
        Ok(h)
    }

    /// Read and decode the node stored under `h`.
    ///
    /// The bytes are checked against the hash they were fetched by before
    /// they are decoded. The root commits to every node below it only through
    /// these hashes, so a node whose bytes do not hash to its key is not part
    /// of the trie the root describes, whatever it says: a corrupted store,
    /// or a peer serving state, would otherwise feed the executor balances no
    /// root ever committed to.
    fn load_node(&self, h: H256) -> Result<Option<Node>, TrieError> {
        if h == EMPTY {
            return Ok(None);
        }
        let bytes = self
            .store
            .get(&h)?
            .ok_or_else(|| TrieError::Storage(format!("missing node {h}")))?;
        if keccak256(&bytes) != h {
            return Err(TrieError::Decode(format!(
                "node stored under {h} does not hash to it"
            )));
        }
        Node::decode(&bytes)
            .map(Some)
            .map_err(|e| TrieError::Decode(format!("node {h}: {e}")))
    }

    /// Use a simpler invariant: leaves store the FULL 32-byte path and full value.
    /// Branches split on the bit at `depth`. This avoids the suffix-management complexity.
    fn insert_inner(
        &mut self,
        here: H256,
        path: H256,
        depth: usize,
        value: Vec<u8>,
    ) -> Result<H256, TrieError> {
        match self.load_node(here)? {
            None => {
                // Empty subtree -> store leaf carrying the full path as key_suffix.
                let leaf = Node::Leaf {
                    key_suffix: path.0.to_vec(),
                    value,
                };
                self.store_node(&leaf)
            }
            Some(Node::Leaf {
                key_suffix,
                value: existing,
            }) => {
                let existing_path: [u8; 32] = key_suffix
                    .as_slice()
                    .try_into()
                    .expect("leaf path always 32 bytes");
                if existing_path == path.0 {
                    // Replace.
                    return self.store_node(&Node::Leaf {
                        key_suffix: path.0.to_vec(),
                        value,
                    });
                }
                // Find first differing bit at depth >= current depth.
                let mut d = depth;
                while d < 256 && bit(&existing_path, d) == bit(&path.0, d) {
                    d += 1;
                }
                let existing_leaf = Node::Leaf {
                    key_suffix: existing_path.to_vec(),
                    value: existing,
                };
                let new_leaf = Node::Leaf {
                    key_suffix: path.0.to_vec(),
                    value,
                };
                let existing_h = self.store_node(&existing_leaf)?;
                let new_h = self.store_node(&new_leaf)?;
                // Build branch at depth d.
                let (l, r) = if bit(&existing_path, d) == 0 {
                    (existing_h, new_h)
                } else {
                    (new_h, existing_h)
                };
                let mut node_h = self.store_node(&Node::Branch { left: l, right: r })?;
                // Walk back to `depth`, wrapping in single-child branches.
                let mut cur = d;
                while cur > depth {
                    cur -= 1;
                    let common_bit = bit(&existing_path, cur);
                    let (l, r) = if common_bit == 0 {
                        (node_h, EMPTY)
                    } else {
                        (EMPTY, node_h)
                    };
                    node_h = self.store_node(&Node::Branch { left: l, right: r })?;
                }
                Ok(node_h)
            }
            Some(Node::Branch { left, right }) => {
                let b = bit(&path.0, depth);
                if b == 0 {
                    let new_left = self.insert_inner(left, path, depth + 1, value)?;
                    self.store_node(&Node::Branch {
                        left: new_left,
                        right,
                    })
                } else {
                    let new_right = self.insert_inner(right, path, depth + 1, value)?;
                    self.store_node(&Node::Branch {
                        left,
                        right: new_right,
                    })
                }
            }
        }
    }

    fn get_inner(
        &self,
        here: H256,
        path: &H256,
        depth: usize,
    ) -> Result<Option<Vec<u8>>, TrieError> {
        match self.load_node(here)? {
            None => Ok(None),
            Some(Node::Leaf { key_suffix, value }) => {
                if key_suffix == path.0 {
                    Ok(Some(value))
                } else {
                    Ok(None)
                }
            }
            Some(Node::Branch { left, right }) => {
                let b = bit(&path.0, depth);
                if b == 0 {
                    self.get_inner(left, path, depth + 1)
                } else {
                    self.get_inner(right, path, depth + 1)
                }
            }
        }
    }

    fn remove_inner(&mut self, here: H256, path: &H256, depth: usize) -> Result<H256, TrieError> {
        match self.load_node(here)? {
            None => Ok(EMPTY),
            Some(Node::Leaf { key_suffix, .. }) => {
                if key_suffix == path.0 {
                    Ok(EMPTY)
                } else {
                    Ok(here)
                }
            }
            Some(Node::Branch { left, right }) => {
                let b = bit(&path.0, depth);
                let (new_left, new_right) = if b == 0 {
                    (self.remove_inner(left, path, depth + 1)?, right)
                } else {
                    (left, self.remove_inner(right, path, depth + 1)?)
                };
                if (new_left, new_right) == (left, right) {
                    // The key was not below this branch; nothing moved.
                    return Ok(here);
                }
                // Keep the shape canonical: the shape `insert` would have
                // built for the keys that remain. `insert` never leaves a
                // leaf beside an empty sibling (a lone key sits as a leaf at
                // the shallowest depth where it is alone), so a branch that
                // is down to one leaf collapses into that leaf, and the
                // collapse carries on up through every parent that is left
                // with only it. Without this the root depended on history:
                // insert(a), insert(b), remove(b) did not reach the root of
                // insert(a), and two nodes holding the same state could
                // disagree about its hash. A branch whose one child is a
                // branch stays: that subtree still holds two or more keys,
                // and `insert` builds the same single-child chain above them.
                match (new_left == EMPTY, new_right == EMPTY) {
                    (true, true) => Ok(EMPTY),
                    (false, true) | (true, false) => {
                        let only = if new_left == EMPTY {
                            new_right
                        } else {
                            new_left
                        };
                        match self.load_node(only)? {
                            Some(Node::Leaf { .. }) => Ok(only),
                            _ => self.store_node(&Node::Branch {
                                left: new_left,
                                right: new_right,
                            }),
                        }
                    }
                    (false, false) => self.store_node(&Node::Branch {
                        left: new_left,
                        right: new_right,
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemKv;
    use proptest::prelude::*;

    #[test]
    fn empty_trie_root_is_zero() {
        let mut kv = MemKv::default();
        let t = MerkleTrie::new(&mut kv);
        assert_eq!(t.root(), EMPTY);
    }

    #[test]
    fn single_insert_and_get() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        assert_eq!(t.get(b"alice").unwrap(), Some(b"100".to_vec()));
        assert_eq!(t.get(b"bob").unwrap(), None);
    }

    #[test]
    fn root_is_independent_of_insertion_order() {
        let mut kv1 = MemKv::default();
        let mut t1 = MerkleTrie::new(&mut kv1);
        t1.insert(b"a", b"1".to_vec()).unwrap();
        t1.insert(b"b", b"2".to_vec()).unwrap();
        let r1 = t1.root();

        let mut kv2 = MemKv::default();
        let mut t2 = MerkleTrie::new(&mut kv2);
        t2.insert(b"b", b"2".to_vec()).unwrap();
        t2.insert(b"a", b"1".to_vec()).unwrap();
        let r2 = t2.root();

        assert_eq!(r1, r2);
    }

    #[test]
    fn insert_then_remove_yields_empty_root() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"x", b"y".to_vec()).unwrap();
        assert_ne!(t.root(), EMPTY);
        t.remove(b"x").unwrap();
        assert_eq!(t.root(), EMPTY);
    }

    #[test]
    fn prefetch_paths_is_a_no_op_on_empty_trie() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        // No nodes exist yet — prefetch must not error.
        t.prefetch_paths(&[b"a", b"b"]).unwrap();
        assert_eq!(t.root(), EMPTY);
    }

    #[test]
    fn prefetch_paths_preserves_root_and_values() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        t.insert(b"bob", b"200".to_vec()).unwrap();
        t.insert(b"carol", b"300".to_vec()).unwrap();
        let root = t.root();
        // Re-open and prefetch — root must not move. `t` borrows `kv`; ending
        // the binding releases the borrow.
        let _ = t;
        let mut t2 = MerkleTrie::open(&mut kv, root);
        let keys: [&[u8]; 3] = [b"alice", b"bob", b"carol"];
        t2.prefetch_paths(&keys).unwrap();
        assert_eq!(t2.root(), root);
        assert_eq!(t2.get(b"alice").unwrap(), Some(b"100".to_vec()));
        assert_eq!(t2.get(b"bob").unwrap(), Some(b"200".to_vec()));
        assert_eq!(t2.get(b"carol").unwrap(), Some(b"300".to_vec()));
    }

    proptest! {
        #[test]
        fn proptest_root_independent_of_insertion_order(
            kvs in proptest::collection::vec((any::<u32>(), any::<u32>()), 0..16)
        ) {
            // Dedup by key (last write wins) so both orderings settle to the same value-per-key.
            let mut canonical: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
            for (k, v) in &kvs { canonical.insert(*k, *v); }
            let mut a = MemKv::default();
            let mut b = MemKv::default();
            let mut ta = MerkleTrie::new(&mut a);
            let mut tb = MerkleTrie::new(&mut b);
            for (k, v) in &canonical { ta.insert(&k.to_be_bytes(), v.to_be_bytes().to_vec()).unwrap(); }
            for (k, v) in canonical.iter().rev() { tb.insert(&k.to_be_bytes(), v.to_be_bytes().to_vec()).unwrap(); }
            prop_assert_eq!(ta.root(), tb.root());
        }

        #[test]
        fn proptest_prefetch_does_not_change_root(
            kvs in proptest::collection::vec((any::<u32>(), any::<u32>()), 1..32)
        ) {
            // Build a trie, capture the root, prefetch every key, then verify
            // both root and per-key reads are unchanged. This is the trie's
            // load-bearing invariant: prefetch is a pure read-side warmup.
            let mut canonical: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
            for (k, v) in &kvs { canonical.insert(*k, *v); }
            let mut kv = MemKv::default();
            let mut t = MerkleTrie::new(&mut kv);
            for (k, v) in &canonical {
                t.insert(&k.to_be_bytes(), v.to_be_bytes().to_vec()).unwrap();
            }
            let root_before = t.root();
            // End `t`'s borrow of `kv` so it can be re-opened.
            let _ = t;
            let mut t2 = MerkleTrie::open(&mut kv, root_before);
            let key_bytes: Vec<[u8; 4]> = canonical.keys().map(|k| k.to_be_bytes()).collect();
            let key_refs: Vec<&[u8]> = key_bytes.iter().map(|k| k.as_slice()).collect();
            t2.prefetch_paths(&key_refs).unwrap();
            prop_assert_eq!(t2.root(), root_before);
            for (k, v) in &canonical {
                prop_assert_eq!(
                    t2.get(&k.to_be_bytes()).unwrap(),
                    Some(v.to_be_bytes().to_vec())
                );
            }
        }
    }

    #[test]
    fn every_stored_node_sits_under_the_hash_of_its_bytes() {
        // Content addressing is the trie's whole integrity model: a node is
        // found by the hash of its encoding, and the root commits to every
        // node below it through those hashes. Pinned over a trie with
        // branches, a replacement and a removal, so a `store_node` that ever
        // wrote under anything but `node.hash()` is caught here.
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        for i in 0u32..40 {
            t.insert(&i.to_be_bytes(), vec![i as u8; 3]).unwrap();
        }
        t.insert(&7u32.to_be_bytes(), b"replaced".to_vec()).unwrap();
        t.remove(&9u32.to_be_bytes()).unwrap();
        let _ = t;
        assert!(!kv.0.is_empty());
        for (h, bytes) in &kv.0 {
            assert_eq!(keccak256(bytes), *h, "node stored under the wrong hash");
        }
    }

    #[test]
    fn replacing_a_value_moves_the_root_and_the_read() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        t.insert(b"bob", b"200".to_vec()).unwrap();
        let before = t.root();
        t.insert(b"alice", b"101".to_vec()).unwrap();
        assert_ne!(t.root(), before, "a changed balance must change the root");
        assert_eq!(t.get(b"alice").unwrap(), Some(b"101".to_vec()));
        assert_eq!(t.get(b"bob").unwrap(), Some(b"200".to_vec()));
        // Writing the old value back restores the old root: the root is a
        // function of the content, not of how many writes produced it.
        t.insert(b"alice", b"100".to_vec()).unwrap();
        assert_eq!(t.root(), before);
    }

    #[test]
    fn removing_a_key_that_was_never_there_changes_nothing() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        t.insert(b"bob", b"200".to_vec()).unwrap();
        let before = t.root();
        t.remove(b"carol").unwrap();
        assert_eq!(t.root(), before);
        assert_eq!(t.get(b"alice").unwrap(), Some(b"100".to_vec()));
        assert_eq!(t.get(b"bob").unwrap(), Some(b"200".to_vec()));
    }

    proptest! {
        /// A `BTreeMap` is the oracle: after every insert or remove, every key
        /// in the small key space reads back exactly what the oracle holds,
        /// and the root is empty exactly when the map is.
        #[test]
        fn reads_follow_the_last_write_through_interleaved_removes(
            ops in proptest::collection::vec((0u8..8, proptest::option::of(any::<u16>())), 1..64)
        ) {
            let mut kv = MemKv::default();
            let mut t = MerkleTrie::new(&mut kv);
            let mut oracle: std::collections::BTreeMap<u8, u16> = Default::default();
            for (k, op) in ops {
                match op {
                    Some(v) => {
                        t.insert(&[k], v.to_be_bytes().to_vec()).unwrap();
                        oracle.insert(k, v);
                    }
                    None => {
                        t.remove(&[k]).unwrap();
                        oracle.remove(&k);
                    }
                }
                for key in 0u8..8 {
                    prop_assert_eq!(
                        t.get(&[key]).unwrap(),
                        oracle.get(&key).map(|v| v.to_be_bytes().to_vec())
                    );
                }
            }
            prop_assert_eq!(t.root() == EMPTY, oracle.is_empty());
        }
    }

    #[test]
    fn removing_a_key_restores_the_root_of_the_trie_that_never_held_it() {
        // The history-independence the state root depends on. `b` is chosen
        // among many keys so the pair shares a long bit prefix in some cases
        // and diverges at the first bit in others; both shapes must collapse.
        for other in 0u32..64 {
            let mut kv1 = MemKv::default();
            let mut t1 = MerkleTrie::new(&mut kv1);
            t1.insert(b"a", b"1".to_vec()).unwrap();
            t1.insert(&other.to_be_bytes(), b"2".to_vec()).unwrap();
            t1.remove(&other.to_be_bytes()).unwrap();
            let mut kv2 = MemKv::default();
            let mut t2 = MerkleTrie::new(&mut kv2);
            t2.insert(b"a", b"1".to_vec()).unwrap();
            assert_eq!(t1.root(), t2.root(), "removing key {other} left a trace");
        }
    }

    #[test]
    fn a_node_whose_bytes_do_not_match_its_hash_is_refused() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        let root = t.root();
        let _ = t;
        // Rewrite alice's leaf in place, under its old hash, with a bigger
        // balance. The bytes still decode; only the hash gives it away.
        let forged = Node::Leaf {
            key_suffix: keccak256(b"alice").0.to_vec(),
            value: b"999".to_vec(),
        }
        .encode();
        kv.0.insert(root, forged);
        let t = MerkleTrie::open(&mut kv, root);
        assert!(matches!(t.get(b"alice"), Err(TrieError::Decode(_))));
    }

    #[test]
    fn a_truncated_node_is_an_error_not_a_panic() {
        let mut kv = MemKv::default();
        let mut t = MerkleTrie::new(&mut kv);
        t.insert(b"alice", b"100".to_vec()).unwrap();
        t.insert(b"bob", b"200".to_vec()).unwrap();
        let root = t.root();
        let _ = t;
        let hashes: Vec<H256> = kv.0.keys().copied().collect();
        for h in hashes {
            let full = kv.0[&h].clone();
            for cut in [0, 1, 2, full.len() / 2, full.len() - 1] {
                kv.0.insert(h, full[..cut].to_vec());
                let t = MerkleTrie::open(&mut kv, root);
                // Either path may or may not cross the damaged node; whichever
                // does must fail cleanly, and neither may panic.
                let _ = t.get(b"alice");
                let _ = t.get(b"bob");
            }
            kv.0.insert(h, full);
        }
        // And the node at the root itself, cut short, is always reached.
        let full = kv.0[&root].clone();
        kv.0.insert(root, full[..full.len() - 1].to_vec());
        let t = MerkleTrie::open(&mut kv, root);
        assert!(matches!(t.get(b"alice"), Err(TrieError::Decode(_))));
    }

    proptest! {
        /// Any sequence of inserts and removes ends at the root of inserting
        /// the surviving key-value pairs into an empty trie. The keys are
        /// drawn from a small space so removes usually hit, and each pair
        /// shares prefixes often enough to exercise the collapse at depth.
        #[test]
        fn the_root_depends_only_on_the_final_contents(
            ops in proptest::collection::vec((0u8..12, proptest::option::of(any::<u8>())), 0..80)
        ) {
            let mut kv = MemKv::default();
            let mut t = MerkleTrie::new(&mut kv);
            let mut oracle: std::collections::BTreeMap<u8, u8> = Default::default();
            for (k, op) in ops {
                match op {
                    Some(v) => {
                        t.insert(&[k], vec![v]).unwrap();
                        oracle.insert(k, v);
                    }
                    None => {
                        t.remove(&[k]).unwrap();
                        oracle.remove(&k);
                    }
                }
            }
            let mut fresh_kv = MemKv::default();
            let mut fresh = MerkleTrie::new(&mut fresh_kv);
            for (k, v) in &oracle {
                fresh.insert(&[*k], vec![*v]).unwrap();
            }
            prop_assert_eq!(t.root(), fresh.root());
        }
    }
}
