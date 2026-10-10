//! Mempool admission: what a node checks before it says "accepted".
//!
//! # Why this exists
//!
//! `submitTx` used to validate almost nothing — zero amounts, zero prices and
//! the shape of a derivative id — then put the transaction in the mempool and
//! answer `ok`. "Accepted" therefore meant "the mempool took the bytes", not
//! "this will execute", and two things followed from that.
//!
//! The first is the client's problem: it was told 201 Created, given an order
//! id, and the order never existed, because the transaction died later at
//! block build with nothing to report it.
//!
//! The second is worse, and it bricked accounts. A gateway caches each
//! address's next nonce and advances it on acceptance. When a transaction was
//! accepted and then never applied, the chain's nonce stayed put and the
//! cache was permanently one ahead: every later submission from that address
//! was signed with a nonce the chain would reject, was accepted into the
//! mempool anyway, and used to take a whole block down with it. The escape
//! hatch — re-read the nonce when the sequencer reports `badNonce` — was
//! already written and already unreachable, because the sequencer had no path
//! that returned `badNonce`.
//!
//! [`PendingNonces`] gives it one. It tracks the nonce each address's next
//! transaction must carry: the chain's nonce, advanced for each transaction
//! already sitting in the mempool. A submission that does not match is
//! rejected with `badNonce` and the expected value, which is exactly what the
//! gateway's retry path was written for. When a transaction is later rejected
//! at block build, the sequencer forgets that address, so the next submission
//! is measured against the chain again and the account recovers by itself.
//!
//! Tracking in memory rather than by scanning the mempool is deliberate: a
//! scan is O(mempool) per submission, which is a cheap way to make a node
//! slow. Losing the map (restart, crash) is harmless — it repopulates from the
//! chain's nonce, which is the conservative answer.

use cb_state::{Accounts, Database};
use cb_types::Address;
use std::collections::HashMap;
use std::sync::Mutex;

/// Per-address nonce expectations for transactions in flight.
#[derive(Debug, Default)]
pub struct PendingNonces {
    inner: Mutex<HashMap<Address, u64>>,
}

impl PendingNonces {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Address, u64>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The nonce the next transaction from `addr` must carry.
    ///
    /// The chain's own nonce is the floor and it always wins: a cached value
    /// that the chain has caught up with, or passed, is stale and is dropped.
    /// That is what makes this self-healing — nothing has to notice that a
    /// transaction was dropped, because the next question asked about the
    /// address is answered from the chain.
    pub fn expected(&self, db: &Database, addr: &Address) -> u64 {
        let chain = Accounts::new(db).get(addr).map(|a| a.nonce).unwrap_or(0);
        let mut g = self.lock();
        match g.get(addr) {
            Some(&cached) if cached > chain => cached,
            Some(_) => {
                g.remove(addr);
                chain
            }
            None => chain,
        }
    }

    /// Record that a transaction from `addr` carrying `nonce` was admitted.
    pub fn admitted(&self, addr: Address, nonce: u64) {
        self.lock().insert(addr, nonce.saturating_add(1));
    }

    /// Forget what we expect from `addr`, so the next submission is measured
    /// against the chain. Called for every sender whose transaction the block
    /// builder refused.
    pub fn forget(&self, addr: &Address) {
        self.lock().remove(addr);
    }

    /// Drop entries the chain has caught up with. Called after each block for
    /// that block's senders, so the map stays proportional to what is actually
    /// in flight rather than to every address that has ever traded.
    pub fn retire_settled(&self, db: &Database, addrs: &[Address]) {
        if addrs.is_empty() {
            return;
        }
        // Only senders the ledger holds need their chain nonce read, and
        // each once: a block's sender list is one entry per transaction,
        // tens of thousands from a few dozen accounts under load, and the
        // ledger holds only senders whose admitted transactions are still
        // in flight. Reading an account row per transaction here was 11% of
        // the sequencer's block loop at 871,000 orders/s, for a ledger that
        // was empty (the generator's transactions never pass admission).
        let held: Vec<Address> = {
            let g = self.lock();
            if g.is_empty() {
                return;
            }
            let mut seen = std::collections::HashSet::with_capacity(g.len());
            addrs
                .iter()
                .filter(|a| g.contains_key(a) && seen.insert(**a))
                .copied()
                .collect()
        };
        if held.is_empty() {
            return;
        }
        let settled: Vec<(Address, u64)> = held
            .iter()
            .map(|a| (*a, Accounts::new(db).get(a).map(|s| s.nonce).unwrap_or(0)))
            .collect();
        let mut g = self.lock();
        for (addr, chain) in settled {
            if matches!(g.get(&addr), Some(&cached) if cached <= chain) {
                g.remove(&addr);
            }
        }
    }

    /// Number of addresses currently tracked. Diagnostics and tests.
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn expected_starts_at_the_chain_nonce() {
        let (_d, db) = db();
        let p = PendingNonces::new();
        assert_eq!(p.expected(&db, &Address([1; 20])), 0);
    }

    #[test]
    fn expected_advances_per_admitted_tx() {
        let (_d, db) = db();
        let p = PendingNonces::new();
        let a = Address([1; 20]);
        assert_eq!(p.expected(&db, &a), 0);
        p.admitted(a, 0);
        assert_eq!(p.expected(&db, &a), 1);
        p.admitted(a, 1);
        assert_eq!(p.expected(&db, &a), 2);
    }

    #[test]
    fn forgetting_returns_the_address_to_the_chain_nonce() {
        let (_d, db) = db();
        let p = PendingNonces::new();
        let a = Address([2; 20]);
        p.admitted(a, 0);
        p.admitted(a, 1);
        assert_eq!(p.expected(&db, &a), 2);
        // The block builder refused one of them: the chain never moved.
        p.forget(&a);
        assert_eq!(p.expected(&db, &a), 0);
    }

    #[test]
    fn a_chain_nonce_that_overtakes_the_cache_wins() {
        let (_d, db) = db();
        let p = PendingNonces::new();
        let a = Address([3; 20]);
        p.admitted(a, 0);
        assert_eq!(p.expected(&db, &a), 1);
        // Something outside this process moved the account well past us.
        let st = cb_state::AccountState {
            nonce: 9,
            ..Default::default()
        };
        Accounts::new(&db).put(&a, &st);
        assert_eq!(p.expected(&db, &a), 9);
        assert_eq!(p.tracked(), 0, "the stale entry is dropped, not kept");
    }
}
