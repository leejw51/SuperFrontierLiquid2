//! A bounded "have I seen this hash?" set.
//!
//! Gossip is redundant by construction: every peer tells every other peer
//! about the same block. Deduplication is what keeps that redundancy from
//! becoming an amplification loop, and it has to be bounded or it becomes a
//! memory leak that an adversary controls the size of.
//!
//! Eviction is strict FIFO rather than least-recently-used. LRU would keep a
//! hot hash resident longer, but a hash's usefulness here is a function of how
//! recently it was *created*, not how often it is mentioned — and FIFO cannot
//! be gamed by an adversary that keeps re-mentioning an old hash to pin it in
//! the cache and push a live one out.
//!
//! Forgetting an entry costs a redundant fetch or a redundant forward. It
//! never costs correctness: a block that arrives twice is verified twice and
//! rejected the second time as non-monotonic.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// A fixed-capacity insertion-ordered set.
#[derive(Debug)]
pub struct SeenSet<T> {
    order: VecDeque<T>,
    set: HashSet<T>,
    capacity: usize,
}

impl<T: Eq + Hash + Clone> SeenSet<T> {
    /// Create a set holding at most `capacity` entries. A capacity of zero is
    /// raised to one so the structure always has room for the entry currently
    /// being inserted.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            order: VecDeque::with_capacity(capacity),
            set: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Whether `value` is currently remembered.
    pub fn contains(&self, value: &T) -> bool {
        self.set.contains(value)
    }

    /// Remember `value`. Returns `true` if it was newly inserted, `false` if
    /// it was already present — which is exactly the "should I act on this?"
    /// question every gossip path asks.
    pub fn insert(&mut self, value: T) -> bool {
        if !self.set.insert(value.clone()) {
            return false;
        }
        self.order.push_back(value);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        true
    }

    /// Number of entries currently remembered.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_reports_novelty() {
        let mut s = SeenSet::new(4);
        assert!(s.insert(1));
        assert!(!s.insert(1));
        assert!(s.contains(&1));
    }

    #[test]
    fn oldest_entries_are_evicted_first() {
        let mut s = SeenSet::new(2);
        s.insert(1);
        s.insert(2);
        // Re-mentioning 1 must not protect it: FIFO, not LRU.
        assert!(!s.insert(1));
        s.insert(3);
        assert!(!s.contains(&1));
        assert!(s.contains(&2));
        assert!(s.contains(&3));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn zero_capacity_still_works() {
        let mut s = SeenSet::new(0);
        assert!(s.insert(1));
        assert!(s.contains(&1));
    }
}
