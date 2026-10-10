//! Validators CF: stores the BFT validator set persistent across restarts.
//!
//! Layout:
//!  - `count` (be_u64) → number of validators
//!  - `i:N`            → bincode-encoded `ValidatorInfo` for index N (0..count)
//!  - `addr:<20 bytes>` → bincode-encoded `ValidatorInfo` for that address
//!
//! The dual layout (by index + by address) supports both ordered iteration (for
//! deterministic leader rotation `validators[view % N]`) and O(1) membership /
//! pubkey lookups when verifying signatures.

use crate::db::{Database, CF_VALIDATORS};
use cb_types::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub addr: Address,
    /// Compressed secp256k1 pubkey (33 bytes). Stored so replicas can recover and
    /// verify proposer / cosigner signatures without recomputing from a seed.
    pub pubkey: Vec<u8>,
    pub weight: u64,
}

pub struct Validators<'a> {
    db: &'a Database,
}

const KEY_COUNT: &[u8] = b"count";

impl<'a> Validators<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Replace the entire validator set atomically. Idempotent — safe to call from
    /// genesis apply on a fresh DB.
    pub fn put_set(&self, list: &[ValidatorInfo]) {
        let mut batch = self.db.batch();
        let count = list.len() as u64;
        batch
            .put(CF_VALIDATORS, KEY_COUNT, &count.to_be_bytes())
            .expect("rocks");
        for (i, v) in list.iter().enumerate() {
            let bytes = bincode::serialize(v).expect("ValidatorInfo encode");
            batch
                .put(CF_VALIDATORS, &index_key(i as u64), &bytes)
                .expect("rocks");
            batch
                .put(CF_VALIDATORS, &addr_key(&v.addr), &bytes)
                .expect("rocks");
        }
        self.db.write_batch(batch).expect("rocks");
    }

    pub fn count(&self) -> u64 {
        self.db
            .get(CF_VALIDATORS, KEY_COUNT)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn get_by_index(&self, i: u64) -> Option<ValidatorInfo> {
        self.db
            .get(CF_VALIDATORS, &index_key(i))
            .expect("rocks")
            .and_then(|b| bincode::deserialize(&b).ok())
    }

    pub fn get_by_addr(&self, a: &Address) -> Option<ValidatorInfo> {
        self.db
            .get(CF_VALIDATORS, &addr_key(a))
            .expect("rocks")
            .and_then(|b| bincode::deserialize(&b).ok())
    }

    pub fn list(&self) -> Vec<ValidatorInfo> {
        let n = self.count();
        (0..n).filter_map(|i| self.get_by_index(i)).collect()
    }

    /// Round-robin leader for `view`: `validators[view % count]`. Returns `None` only
    /// when the set is empty (pre-genesis).
    pub fn leader_for_view(&self, view: u64) -> Option<ValidatorInfo> {
        let n = self.count();
        if n == 0 {
            return None;
        }
        self.get_by_index(view % n)
    }
}

fn index_key(i: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(2 + 8);
    k.extend_from_slice(b"i:");
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn addr_key(a: &Address) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + 20);
    k.extend_from_slice(b"addr:");
    k.extend_from_slice(&a.0);
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use tempfile::tempdir;

    fn vinfo(byte: u8, weight: u64) -> ValidatorInfo {
        ValidatorInfo {
            addr: Address([byte; 20]),
            pubkey: vec![byte; 33],
            weight,
        }
    }

    #[test]
    fn put_set_then_list_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let v = Validators::new(&db);
        let set = vec![vinfo(1, 100), vinfo(2, 50), vinfo(3, 25)];
        v.put_set(&set);
        assert_eq!(v.count(), 3);
        assert_eq!(v.list(), set);
    }

    #[test]
    fn get_by_index_and_addr() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let v = Validators::new(&db);
        let a = vinfo(7, 1);
        v.put_set(std::slice::from_ref(&a));
        assert_eq!(v.get_by_index(0), Some(a.clone()));
        assert_eq!(v.get_by_addr(&Address([7; 20])), Some(a));
        assert_eq!(v.get_by_index(99), None);
    }

    #[test]
    fn leader_for_view_round_robin() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let v = Validators::new(&db);
        v.put_set(&[vinfo(1, 1), vinfo(2, 1), vinfo(3, 1)]);
        assert_eq!(v.leader_for_view(0).unwrap().addr, Address([1; 20]));
        assert_eq!(v.leader_for_view(1).unwrap().addr, Address([2; 20]));
        assert_eq!(v.leader_for_view(2).unwrap().addr, Address([3; 20]));
        assert_eq!(v.leader_for_view(3).unwrap().addr, Address([1; 20]));
    }

    #[test]
    fn put_set_replaces_old_set() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let v = Validators::new(&db);
        v.put_set(&[vinfo(1, 1), vinfo(2, 1), vinfo(3, 1)]);
        v.put_set(&[vinfo(9, 1)]);
        assert_eq!(v.count(), 1);
        assert_eq!(v.get_by_index(0).unwrap().addr, Address([9; 20]));
    }
}
