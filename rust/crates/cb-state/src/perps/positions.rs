//! `CF_POSITIONS`: `(owner, deriv_id) -> Position`.
//!
//! Ships the CF read/write surface plus a per-owner prefix iterator. Trie
//! commitment of the positions root lives in `lib.rs::compute_state_root`; the
//! `commit_batch` method below is what `BlockState::commit` (M8+) calls to
//! flush the per-block touched-keys set into the trie atomically alongside
//! accounts and markets.

use crate::db::{BufferedKv, Database, DbBatch, DbError, CF_META, CF_POSITIONS, CF_TRIE_NODES};
use crate::perps::keys::position_key;
use cb_trie::MerkleTrie;
use cb_types::{Address, DerivId, Position, H256};
use std::collections::HashSet;

/// Meta-CF key holding the latest positions trie root. Read on block-apply,
/// rewritten by `commit_batch`. Keep distinct from the accounts root meta key
/// — the two tries are independent.
const META_POSITIONS_ROOT: &[u8] = b"positions_root";

pub struct Positions<'a> {
    db: &'a Database,
}

impl<'a> Positions<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, p: &Position) {
        let k = position_key(&p.owner, &p.market);
        self.db
            .put(
                CF_POSITIONS,
                &k,
                &bincode::serialize(p).expect("Position encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, owner: &Address, market: &DerivId) -> Option<Position> {
        let k = position_key(owner, market);
        self.db
            .get(CF_POSITIONS, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("Position decode"))
    }

    pub fn delete(&self, owner: &Address, market: &DerivId) {
        let k = position_key(owner, market);
        self.db.delete(CF_POSITIONS, &k).expect("rocks");
    }

    /// Read the current positions trie root. Genesis returns `H256::ZERO`.
    pub fn root(&self) -> H256 {
        match self.db.get(CF_META, META_POSITIONS_ROOT).expect("rocks") {
            Some(b) if b.len() == 32 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            }
            _ => H256::ZERO,
        }
    }

    /// Apply a batch of position-key deltas to the trie atomically. Each entry
    /// in `touched` is a `(owner, market)` pair whose CF row was either written
    /// or deleted during the block — `commit_batch` re-reads the current CF
    /// value for each key and applies the trie insert (Some) or delete (None).
    ///
    /// Determinism: keys are sorted before iteration. The MPT root is a
    /// function of the key/value set (not insert order), but sorting still
    /// matters for cache-line-stable trie-node writes across replicas.
    ///
    /// The caller has already flushed CF row writes during the tx loop, so the
    /// CF is the source of truth here. Trie nodes + meta-root land in `batch`
    /// and commit alongside accounts in a single `WriteBatch`.
    pub fn commit_batch(
        &self,
        touched: &HashSet<(Address, DerivId)>,
        batch: &mut DbBatch<'_>,
    ) -> Result<H256, DbError> {
        let prior_root = self.root();
        if touched.is_empty() {
            return Ok(prior_root);
        }
        let mut keys: Vec<(Address, DerivId)> = touched.iter().copied().collect();
        keys.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then_with(|| a.1 .0.cmp(&b.1 .0)));

        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, prior_root);
            // M12: prefetch every touched path's interior nodes in one BFS
            // walk before the actual insert/remove pass. Round-trip
            // savings scale with batch size; for a typical 100-position
            // block this collapses ~100*20 = 2000 sequential get_cf calls
            // down to ~20 batched multi_get_cf calls (one per BFS level).
            let key_buf: Vec<[u8; 24]> = keys.iter().map(|(o, m)| position_key(o, m)).collect();
            let key_refs: Vec<&[u8]> = key_buf.iter().map(|k| k.as_slice()).collect();
            trie.prefetch_paths(&key_refs).expect("trie prefetch");

            for (owner, market) in &keys {
                let k = position_key(owner, market);
                match self.db.get(CF_POSITIONS, &k)? {
                    Some(bytes) => trie.insert(&k, bytes).expect("trie insert"),
                    None => trie.remove(&k).expect("trie remove"),
                }
            }
            trie.root()
        };
        buffered.flush_into(batch)?;
        batch.put(CF_META, META_POSITIONS_ROOT, &new_root.0)?;
        Ok(new_root)
    }

    /// One-shot convenience used by genesis: rebuild the positions trie from
    /// scratch over the current CF contents. Walks `CF_POSITIONS`, inserts
    /// every row, writes the resulting root + nodes in a single `WriteBatch`.
    /// Tests hit this too.
    pub fn rebuild_root(&self) -> Result<H256, DbError> {
        let mut batch = self.db.batch();
        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, H256::ZERO);
            // RocksDB iterates keys in lex order — the same canonical order
            // sort_by used in commit_batch, so the resulting trie root is
            // identical regardless of insert path.
            let it = self.db.iter_prefix(CF_POSITIONS, &[])?;
            for kv in it {
                let (k, v) = match kv {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if k.len() != 24 {
                    continue;
                }
                trie.insert(&k, v.to_vec()).expect("trie insert");
            }
            trie.root()
        };
        buffered.flush_into(&mut batch)?;
        batch.put(CF_META, META_POSITIONS_ROOT, &new_root.0)?;
        self.db.write_batch(batch)?;
        Ok(new_root)
    }

    /// Enumerate every position belonging to `owner`. Used by margin health
    /// checks (one prefix scan, no extra index).
    pub fn list_for_owner(&self, owner: &Address) -> Vec<Position> {
        let prefix = owner.0;
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_POSITIONS, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if let Ok(p) = bincode::deserialize::<Position>(&v) {
                out.push(p);
            }
        }
        out
    }

    /// Enumerate every open position on `market`. The CF key prefix is
    /// `[owner: 20]`, so this walks the whole CF and filters by the market
    /// suffix in each key. Returned positions are sorted by `(owner)` byte
    /// order — same order the caller gets from a single full scan, which
    /// keeps trie writes deterministic.
    ///
    /// O(N) in total positions, not O(positions on this market). At v1
    /// scales (a few markets, tens of thousands of total positions) this is
    /// fine; M2's per-market index would be a future optimization.
    pub fn list_for_market(&self, market: &DerivId) -> Vec<Position> {
        let prefix: [u8; 0] = [];
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_POSITIONS, &prefix) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if k.len() != 24 {
                continue;
            }
            // Position key layout: [owner: 20][deriv_id: 4]. Filter on suffix.
            if &k[20..24] != market.as_bytes().as_slice() {
                continue;
            }
            if let Ok(p) = bincode::deserialize::<Position>(&v) {
                out.push(p);
            }
        }
        // Owners come back ascending from RocksDB; explicit sort is defensive.
        out.sort_by_key(|a| a.owner.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::{DerivId, Side, Signed};
    use tempfile::tempdir;

    fn sample(owner: Address, market: DerivId) -> Position {
        Position {
            owner,
            market,
            side: Side::Buy,
            qty: 1_000,
            entry_funding_index: Signed::ZERO,
            entry_px_q8: 50_000 * 100_000_000,
            margin_mode: 0,
            isolated_margin: 0,
            last_funding_height: 0,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        let p = sample(Address([1; 20]), DerivId::perp(0));
        ps.put(&p);
        let back = ps.get(&p.owner, &p.market).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn delete_removes_entry() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        let p = sample(Address([1; 20]), DerivId::perp(0));
        ps.put(&p);
        ps.delete(&p.owner, &p.market);
        assert!(ps.get(&p.owner, &p.market).is_none());
    }

    #[test]
    fn list_for_owner_returns_all_markets() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        let alice = Address([1; 20]);
        let bob = Address([2; 20]);
        ps.put(&sample(alice, DerivId::perp(0)));
        ps.put(&sample(alice, DerivId::perp(1)));
        ps.put(&sample(bob, DerivId::perp(0)));
        let alice_positions = ps.list_for_owner(&alice);
        assert_eq!(alice_positions.len(), 2);
        assert!(alice_positions.iter().all(|p| p.owner == alice));
    }

    #[test]
    fn list_for_market_returns_all_owners_with_position_in_market() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        let alice = Address([1; 20]);
        let bob = Address([2; 20]);
        let m0 = DerivId::future(0);
        let m1 = DerivId::future(1);
        ps.put(&sample(alice, m0));
        ps.put(&sample(bob, m0));
        ps.put(&sample(alice, m1));
        let on_m0 = ps.list_for_market(&m0);
        assert_eq!(on_m0.len(), 2);
        // Sorted ascending by owner.
        assert_eq!(on_m0[0].owner, alice);
        assert_eq!(on_m0[1].owner, bob);
        let on_m1 = ps.list_for_market(&m1);
        assert_eq!(on_m1.len(), 1);
        assert_eq!(on_m1[0].owner, alice);
    }

    // ----------------------------------------------------------------------
    // M8 trie tests
    // ----------------------------------------------------------------------

    #[test]
    fn root_zero_when_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        assert_eq!(ps.root(), H256::ZERO);
    }

    #[test]
    fn commit_batch_changes_root_then_revert_removes_change() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);

        // Insert N entries and commit.
        let mut touched: HashSet<(Address, DerivId)> = HashSet::new();
        for i in 0..8u8 {
            let owner = Address([i; 20]);
            let market = DerivId::perp(i as u16);
            ps.put(&sample(owner, market));
            touched.insert((owner, market));
        }
        let mut batch = db.batch();
        let root_full = ps.commit_batch(&touched, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_full, H256::ZERO);

        // Mutate one entry: root must change.
        let target = (Address([0; 20]), DerivId::perp(0));
        let mut p = ps.get(&target.0, &target.1).unwrap();
        p.qty += 500;
        ps.put(&p);
        let mut batch = db.batch();
        let mut t1 = HashSet::new();
        t1.insert(target);
        let root_mut = ps.commit_batch(&t1, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_mut, root_full);

        // Revert: root reverts to the original.
        let mut p = ps.get(&target.0, &target.1).unwrap();
        p.qty -= 500;
        ps.put(&p);
        let mut batch = db.batch();
        let root_rev = ps.commit_batch(&t1, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_eq!(root_rev, root_full);
    }

    #[test]
    fn commit_batch_delete_round_trips() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let ps = Positions::new(&db);
        let owner = Address([7; 20]);
        let market = DerivId::perp(3);
        let p = sample(owner, market);

        // Empty root first.
        let empty_root = ps.root();
        // Insert.
        ps.put(&p);
        let mut t = HashSet::new();
        t.insert((owner, market));
        let mut batch = db.batch();
        let root_after_insert = ps.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_after_insert, empty_root);

        // Delete and commit again — root returns to empty.
        ps.delete(&owner, &market);
        let mut batch = db.batch();
        let root_after_delete = ps.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_eq!(root_after_delete, empty_root);
    }

    #[test]
    fn rebuild_root_matches_incremental() {
        let dir_a = tempdir().unwrap();
        let db_a = Database::open(dir_a.path()).unwrap();
        let ps_a = Positions::new(&db_a);
        let mut touched = HashSet::new();
        for i in 0..16u8 {
            let owner = Address([i; 20]);
            let market = DerivId::perp(i as u16 % 3);
            ps_a.put(&sample(owner, market));
            touched.insert((owner, market));
        }
        let mut batch = db_a.batch();
        let incremental = ps_a.commit_batch(&touched, &mut batch).unwrap();
        db_a.write_batch(batch).unwrap();

        // Fresh DB, same rows put directly, then rebuild_root.
        let dir_b = tempdir().unwrap();
        let db_b = Database::open(dir_b.path()).unwrap();
        let ps_b = Positions::new(&db_b);
        for i in 0..16u8 {
            let owner = Address([i; 20]);
            let market = DerivId::perp(i as u16 % 3);
            ps_b.put(&sample(owner, market));
        }
        let rebuilt = ps_b.rebuild_root().unwrap();
        assert_eq!(incremental, rebuilt);
    }
}
