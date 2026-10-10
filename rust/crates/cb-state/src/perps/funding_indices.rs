//! `CF_FUNDING_INDICES`: `deriv_id -> FundingIndex`.

use crate::db::{Database, CF_FUNDING_INDICES};
use crate::perps::keys::deriv_key;
use cb_types::{DerivId, FundingIndex};

pub struct FundingIndices<'a> {
    db: &'a Database,
}

impl<'a> FundingIndices<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, f: &FundingIndex) {
        let k = deriv_key(&f.market);
        self.db
            .put(
                CF_FUNDING_INDICES,
                &k,
                &bincode::serialize(f).expect("FundingIndex encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, market: &DerivId) -> Option<FundingIndex> {
        let k = deriv_key(market);
        self.db
            .get(CF_FUNDING_INDICES, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("FundingIndex decode"))
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
        let fs = FundingIndices::new(&db);
        let m = DerivId::perp(0);
        let f = FundingIndex {
            market: m,
            cumulative: Signed::from_i128(123),
            last_tick_height: 7_200,
            rate_q9: 100,
        };
        fs.put(&f);
        assert_eq!(fs.get(&m).unwrap(), f);
    }
}
