//! Accounts column family: address -> AccountState.
//!
//! This used to maintain a merkle trie over the rows and an `accounts_root` in
//! `CF_META`. The state commitment is now a hash chain over each block's
//! changes (`state_hash`), so a row is just a row.

use crate::account::AccountState;
use crate::db::{Database, DbBatch, DbError, CF_ACCOUNTS};
use cb_types::Address;

pub struct Accounts<'a> {
    db: &'a Database,
}

impl<'a> Accounts<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn get(&self, addr: &Address) -> Option<AccountState> {
        let raw = self.db.get(CF_ACCOUNTS, &addr.0).expect("rocks");
        raw.map(|b| AccountState::decode(&b).expect("decode AccountState"))
    }

    pub fn put(&self, addr: &Address, state: &AccountState) {
        self.db
            .put(CF_ACCOUNTS, &addr.0, &state.encode())
            .expect("rocks");
    }

    /// Write one account row into a caller-owned batch.
    pub fn put_into_batch(
        &self,
        batch: &mut DbBatch<'_>,
        addr: &Address,
        encoded: &[u8],
    ) -> Result<(), DbError> {
        batch.put(CF_ACCOUNTS, &addr.0, encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::AssetId;
    use tempfile::tempdir;

    #[test]
    fn put_then_get_roundtrips() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let accts = Accounts::new(&db);
        let addr = Address([1; 20]);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000).expect("seed balance");
        accts.put(&addr, &s);
        assert_eq!(accts.get(&addr).unwrap(), s);
    }
}
