//! `CF_MARGIN_ACCOUNTS`: `owner -> MarginAccount`.
//!
//! Margin rows carry every trader's collateral, every unpaid loss, and the
//! insurance fund. They are therefore **committed into the state root**, via
//! the trie this module owns — `Positions` is the template, and
//! `BlockState::commit` flushes both in the same atomic write batch.
//!
//! They were not always. Until QA finding F3 the root hashed accounts,
//! positions and markets only, so two replicas could disagree about who was
//! solvent, and about how much was left in the insurance fund, while reporting
//! byte-identical roots. Genesis-seeded collateral was unauthenticated for the
//! same reason. The first symptom in production would have been a liquidation
//! one node performed and another refused, discovered after the divergence had
//! been finalised.

use crate::db::{
    BufferedKv, Database, DbBatch, DbError, CF_MARGIN_ACCOUNTS, CF_META, CF_TRIE_NODES,
};
use crate::perps::keys::margin_account_key;
use cb_trie::MerkleTrie;
use cb_types::{Address, MarginAccount, H256};
use std::collections::HashSet;

/// Meta-CF key holding the latest margin-accounts trie root. Distinct from the
/// accounts / positions / markets root keys — the four tries are independent.
const META_MARGIN_ROOT: &[u8] = b"margin_root";

pub struct MarginAccounts<'a> {
    db: &'a Database,
}

impl<'a> MarginAccounts<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, m: &MarginAccount) {
        let k = margin_account_key(&m.owner);
        self.db
            .put(
                CF_MARGIN_ACCOUNTS,
                &k,
                &bincode::serialize(m).expect("MarginAccount encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, owner: &Address) -> Option<MarginAccount> {
        let k = margin_account_key(owner);
        self.db
            .get(CF_MARGIN_ACCOUNTS, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("MarginAccount decode"))
    }

    pub fn delete(&self, owner: &Address) {
        let k = margin_account_key(owner);
        self.db.delete(CF_MARGIN_ACCOUNTS, &k).expect("rocks");
    }

    /// Read the current margin-accounts trie root. Genesis returns `H256::ZERO`.
    pub fn root(&self) -> H256 {
        match self.db.get(CF_META, META_MARGIN_ROOT).expect("rocks") {
            Some(b) if b.len() == 32 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            }
            _ => H256::ZERO,
        }
    }

    /// Apply a batch of margin-key deltas to the trie atomically. Each entry in
    /// `touched` is an owner whose CF row was written or deleted during the
    /// block; `commit_batch` re-reads the current CF value for each and applies
    /// the trie insert (Some) or delete (None).
    ///
    /// Determinism: keys are sorted before iteration, mirroring
    /// `Positions::commit_batch`. Trie nodes and the meta-root land in `batch`
    /// and commit alongside the other three roots in a single `WriteBatch`.
    pub fn commit_batch(
        &self,
        touched: &HashSet<Address>,
        batch: &mut DbBatch<'_>,
    ) -> Result<H256, DbError> {
        let prior_root = self.root();
        if touched.is_empty() {
            return Ok(prior_root);
        }
        let mut keys: Vec<Address> = touched.iter().copied().collect();
        keys.sort_by_key(|a| a.0);

        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, prior_root);
            let key_buf: Vec<[u8; 20]> = keys.iter().map(margin_account_key).collect();
            let key_refs: Vec<&[u8]> = key_buf.iter().map(|k| k.as_slice()).collect();
            trie.prefetch_paths(&key_refs).expect("trie prefetch");

            for owner in &keys {
                let k = margin_account_key(owner);
                match self.db.get(CF_MARGIN_ACCOUNTS, &k)? {
                    Some(bytes) => trie.insert(&k, bytes).expect("trie insert"),
                    None => trie.remove(&k).expect("trie remove"),
                }
            }
            trie.root()
        };
        buffered.flush_into(batch)?;
        batch.put(CF_META, META_MARGIN_ROOT, &new_root.0)?;
        Ok(new_root)
    }

    /// One-shot rebuild of the margin trie over the current CF contents, used
    /// by genesis — which seeds collateral directly rather than through a
    /// block — and by tests. Mirrors `Positions::rebuild_root`.
    pub fn rebuild_root(&self) -> Result<H256, DbError> {
        let mut batch = self.db.batch();
        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, H256::ZERO);
            // RocksDB iterates in lex order, the same canonical order
            // `commit_batch` sorts into, so the two agree on the root.
            let it = self.db.iter_prefix(CF_MARGIN_ACCOUNTS, &[])?;
            for kv in it {
                let (k, v) = match kv {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if k.len() != 20 {
                    continue;
                }
                trie.insert(&k, v.to_vec()).expect("trie insert");
            }
            trie.root()
        };
        buffered.flush_into(&mut batch)?;
        batch.put(CF_META, META_MARGIN_ROOT, &new_root.0)?;
        self.db.write_batch(batch)?;
        Ok(new_root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::Signed;
    use tempfile::tempdir;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mas = MarginAccounts::new(&db);
        let owner = Address([1; 20]);
        let m = MarginAccount {
            owner,
            collateral: 1_000_000,
            unrealized_pnl: Signed::from_i128(-50),
            bad_debt: 0,
        };
        mas.put(&m);
        assert_eq!(mas.get(&owner).unwrap(), m);
    }

    #[test]
    fn root_is_zero_when_nothing_was_ever_committed() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(MarginAccounts::new(&db).root(), H256::ZERO);
    }

    #[test]
    fn changing_one_accounts_collateral_changes_the_root() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mas = MarginAccounts::new(&db);
        let mut touched: HashSet<Address> = HashSet::new();
        for i in 0..8u8 {
            let owner = Address([i; 20]);
            mas.put(&MarginAccount {
                owner,
                collateral: 1_000 * (i as u128 + 1),
                unrealized_pnl: Signed::ZERO,
                bad_debt: 0,
            });
            touched.insert(owner);
        }
        let mut batch = db.batch();
        let root_full = mas.commit_batch(&touched, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_full, H256::ZERO);

        // One micro-USD of difference on one account must move the root.
        let target = Address([3; 20]);
        let mut m = mas.get(&target).unwrap();
        m.collateral += 1;
        mas.put(&m);
        let mut one = HashSet::new();
        one.insert(target);
        let mut batch = db.batch();
        let root_mut = mas.commit_batch(&one, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_mut, root_full);

        // And reverting it puts the root back.
        let mut m = mas.get(&target).unwrap();
        m.collateral -= 1;
        mas.put(&m);
        let mut batch = db.batch();
        let root_rev = mas.commit_batch(&one, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_eq!(root_rev, root_full);
    }

    #[test]
    fn bad_debt_alone_changes_the_root() {
        // Collateral is the obvious field; `bad_debt` is the one that records
        // an uncovered loss, and a replica that forgot about it must not be
        // able to report the same root.
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mas = MarginAccounts::new(&db);
        let owner = Address([1; 20]);
        let mut m = MarginAccount {
            owner,
            collateral: 0,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        };
        mas.put(&m);
        let mut t = HashSet::new();
        t.insert(owner);
        let mut batch = db.batch();
        let clean = mas.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();

        m.bad_debt = 3_500_000_000;
        mas.put(&m);
        let mut batch = db.batch();
        let indebted = mas.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(indebted, clean);
    }

    #[test]
    fn rebuild_root_matches_incremental() {
        let dir_a = tempdir().unwrap();
        let db_a = Database::open(dir_a.path()).unwrap();
        let mas_a = MarginAccounts::new(&db_a);
        let mut touched = HashSet::new();
        for i in 0..16u8 {
            let owner = Address([i; 20]);
            mas_a.put(&MarginAccount {
                owner,
                collateral: 7 * (i as u128 + 1),
                unrealized_pnl: Signed::ZERO,
                bad_debt: i as u128,
            });
            touched.insert(owner);
        }
        let mut batch = db_a.batch();
        let incremental = mas_a.commit_batch(&touched, &mut batch).unwrap();
        db_a.write_batch(batch).unwrap();

        let dir_b = tempdir().unwrap();
        let db_b = Database::open(dir_b.path()).unwrap();
        let mas_b = MarginAccounts::new(&db_b);
        for i in 0..16u8 {
            mas_b.put(&MarginAccount {
                owner: Address([i; 20]),
                collateral: 7 * (i as u128 + 1),
                unrealized_pnl: Signed::ZERO,
                bad_debt: i as u128,
            });
        }
        assert_eq!(mas_b.rebuild_root().unwrap(), incremental);
    }

    #[test]
    fn deleting_an_account_round_trips_the_root() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mas = MarginAccounts::new(&db);
        let owner = Address([9; 20]);
        let empty = mas.root();
        mas.put(&MarginAccount {
            owner,
            collateral: 5,
            unrealized_pnl: Signed::ZERO,
            bad_debt: 0,
        });
        let mut t = HashSet::new();
        t.insert(owner);
        let mut batch = db.batch();
        let after_insert = mas.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(after_insert, empty);

        mas.delete(&owner);
        let mut batch = db.batch();
        let after_delete = mas.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_eq!(after_delete, empty);
    }

    #[test]
    fn missing_returns_none() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mas = MarginAccounts::new(&db);
        assert!(mas.get(&Address([9; 20])).is_none());
    }
}
