//! `CF_ORACLE_STATE`: `(deriv_id, kind) -> OraclePrice | MarkPrice`.
//!
//! `kind` discriminates: `0` = oracle (signed by the per-market authority),
//! `1` = mark (recomputed deterministically by the engine each block from
//! oracle blended with index price), `2` = mark-history sample (M5; one entry
//! per block per dated future, pruned to the last `MARK_HISTORY_BLOCKS` so the
//! TWAP settlement window is bounded — see `cb-engine::dated`).

use crate::db::{Database, CF_ORACLE_STATE};
use crate::perps::keys::{mark_history_key, mark_history_prefix, oracle_state_key};
use cb_types::{DerivId, MarkPrice, OraclePrice};

pub const ORACLE_KIND_ORACLE: u8 = 0;
pub const ORACLE_KIND_MARK: u8 = 1;
pub const ORACLE_KIND_MARK_HISTORY: u8 = 2;

pub struct OracleState<'a> {
    db: &'a Database,
}

impl<'a> OracleState<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put_oracle(&self, o: &OraclePrice) {
        let k = oracle_state_key(&o.market, ORACLE_KIND_ORACLE);
        self.db
            .put(
                CF_ORACLE_STATE,
                &k,
                &bincode::serialize(o).expect("OraclePrice encode"),
            )
            .expect("rocks");
    }

    pub fn get_oracle(&self, market: &DerivId) -> Option<OraclePrice> {
        let k = oracle_state_key(market, ORACLE_KIND_ORACLE);
        self.db
            .get(CF_ORACLE_STATE, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("OraclePrice decode"))
    }

    pub fn put_mark(&self, m: &MarkPrice) {
        let k = oracle_state_key(&m.market, ORACLE_KIND_MARK);
        self.db
            .put(
                CF_ORACLE_STATE,
                &k,
                &bincode::serialize(m).expect("MarkPrice encode"),
            )
            .expect("rocks");
    }

    pub fn get_mark(&self, market: &DerivId) -> Option<MarkPrice> {
        let k = oracle_state_key(market, ORACLE_KIND_MARK);
        self.db
            .get(CF_ORACLE_STATE, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("MarkPrice decode"))
    }

    // ---------------- mark-history ring buffer (M5) ----------------

    /// Append (or overwrite) a mark sample for `market` at `height`. Pruning
    /// is the caller's responsibility — see [`prune_mark_history_before`].
    /// Value is the canonical 16-byte big-endian `mark_q8`.
    pub fn put_mark_history(&self, market: &DerivId, height: u64, mark_q8: u128) {
        let k = mark_history_key(market, height);
        self.db
            .put(CF_ORACLE_STATE, &k, &mark_q8.to_be_bytes())
            .expect("rocks");
    }

    /// Look up a single mark sample. Returns `None` when not present.
    pub fn get_mark_history_at(&self, market: &DerivId, height: u64) -> Option<u128> {
        let k = mark_history_key(market, height);
        self.db
            .get(CF_ORACLE_STATE, &k)
            .expect("rocks")
            .and_then(|b| {
                if b.len() != 16 {
                    return None;
                }
                let mut buf = [0u8; 16];
                buf.copy_from_slice(&b);
                Some(u128::from_be_bytes(buf))
            })
    }

    /// Walk every mark-history sample for `market` in ascending-height order.
    /// Returns `(height, mark_q8)` pairs ready to feed into
    /// `cb_engine::compute_settlement_price`.
    ///
    /// RocksDB iterator order under the `[deriv_id || kind=2]` prefix is
    /// already lex-ascending on `height_be`, which matches ascending height.
    pub fn list_mark_history(&self, market: &DerivId) -> Vec<(u64, u128)> {
        let prefix = mark_history_prefix(market);
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_ORACLE_STATE, &prefix) {
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
            if k.len() != 13 || v.len() != 16 {
                continue;
            }
            let mut h = [0u8; 8];
            h.copy_from_slice(&k[5..13]);
            let mut px = [0u8; 16];
            px.copy_from_slice(&v);
            out.push((u64::from_be_bytes(h), u128::from_be_bytes(px)));
        }
        out
    }

    /// Delete every mark-history sample for `market` whose height is strictly
    /// less than `min_height`. Used by the executor each time a new sample
    /// lands to keep the buffer bounded at `MARK_HISTORY_BLOCKS` entries.
    pub fn prune_mark_history_before(&self, market: &DerivId, min_height: u64) {
        let prefix = mark_history_prefix(market);
        let mut to_delete: Vec<Vec<u8>> = Vec::new();
        let it = match self.db.iter_prefix(CF_ORACLE_STATE, &prefix) {
            Ok(it) => it,
            Err(_) => return,
        };
        for kv in it {
            let (k, _v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            if k.len() != 13 {
                continue;
            }
            let mut h = [0u8; 8];
            h.copy_from_slice(&k[5..13]);
            let height = u64::from_be_bytes(h);
            if height < min_height {
                to_delete.push(k.to_vec());
            } else {
                // Heights are sorted ascending; once we see one that's not
                // below the cutoff, the rest are too.
                break;
            }
        }
        // Sort for canonical mutation order before delete.
        to_delete.sort();
        for k in &to_delete {
            self.db.delete(CF_ORACLE_STATE, k).expect("rocks");
        }
    }

    /// Delete every mark-history sample for `market`. Called from the
    /// `FuturesSettle` handler after settlement to free storage.
    pub fn clear_mark_history(&self, market: &DerivId) {
        let prefix = mark_history_prefix(market);
        let mut to_delete: Vec<Vec<u8>> = Vec::new();
        let it = match self.db.iter_prefix(CF_ORACLE_STATE, &prefix) {
            Ok(it) => it,
            Err(_) => return,
        };
        for kv in it {
            let (k, _v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if !k.starts_with(&prefix) {
                break;
            }
            to_delete.push(k.to_vec());
        }
        to_delete.sort();
        for k in &to_delete {
            self.db.delete(CF_ORACLE_STATE, k).expect("rocks");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::Address;
    use tempfile::tempdir;

    #[test]
    fn oracle_and_mark_coexist_under_same_market() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m = DerivId::perp(0);
        let o = OraclePrice {
            market: m,
            px_q8: 5_000_000_000_000,
            source_height: 100,
            authority: Address([7; 20]),
        };
        let mk = MarkPrice {
            market: m,
            px_q8: 5_000_010_000_000,
            computed_at_height: 100,
            premium_ema_q9: 10,
        };
        os.put_oracle(&o);
        os.put_mark(&mk);
        assert_eq!(os.get_oracle(&m).unwrap(), o);
        assert_eq!(os.get_mark(&m).unwrap(), mk);
    }

    #[test]
    fn missing_returns_none() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        assert!(os.get_oracle(&DerivId::perp(99)).is_none());
        assert!(os.get_mark(&DerivId::perp(99)).is_none());
    }

    // ---------------- mark-history ring buffer (M5) ----------------

    #[test]
    fn mark_history_put_and_list_returns_ascending_heights() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m = DerivId::future(0);
        // Insert out of order; expect ascending order back.
        os.put_mark_history(&m, 10, 100);
        os.put_mark_history(&m, 5, 50);
        os.put_mark_history(&m, 15, 150);
        let history = os.list_mark_history(&m);
        assert_eq!(history, vec![(5, 50), (10, 100), (15, 150)]);
    }

    #[test]
    fn mark_history_prune_below_cutoff() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m = DerivId::future(0);
        for h in [1u64, 2, 3, 4, 5] {
            os.put_mark_history(&m, h, h as u128 * 10);
        }
        os.prune_mark_history_before(&m, 3);
        let history = os.list_mark_history(&m);
        assert_eq!(history, vec![(3, 30), (4, 40), (5, 50)]);
    }

    #[test]
    fn mark_history_clear_removes_all() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m = DerivId::future(0);
        for h in [1u64, 2, 3] {
            os.put_mark_history(&m, h, h as u128);
        }
        os.clear_mark_history(&m);
        assert!(os.list_mark_history(&m).is_empty());
    }

    #[test]
    fn mark_history_isolated_per_market() {
        // Two markets sharing the same heights must not collide.
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m0 = DerivId::future(0);
        let m1 = DerivId::future(1);
        os.put_mark_history(&m0, 10, 100);
        os.put_mark_history(&m1, 10, 200);
        assert_eq!(os.get_mark_history_at(&m0, 10), Some(100));
        assert_eq!(os.get_mark_history_at(&m1, 10), Some(200));
        let h0 = os.list_mark_history(&m0);
        let h1 = os.list_mark_history(&m1);
        assert_eq!(h0, vec![(10, 100)]);
        assert_eq!(h1, vec![(10, 200)]);
    }

    #[test]
    fn mark_history_does_not_collide_with_singleton_oracle_or_mark_rows() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let os = OracleState::new(&db);
        let m = DerivId::future(0);
        let o = OraclePrice {
            market: m,
            px_q8: 1_000,
            source_height: 10,
            authority: Address([7; 20]),
        };
        let mk = MarkPrice {
            market: m,
            px_q8: 1_010,
            computed_at_height: 10,
            premium_ema_q9: 0,
        };
        os.put_oracle(&o);
        os.put_mark(&mk);
        os.put_mark_history(&m, 9, 999);

        // Singletons survive list_mark_history (they're under different kind bytes).
        assert_eq!(os.get_oracle(&m).unwrap(), o);
        assert_eq!(os.get_mark(&m).unwrap(), mk);
        assert_eq!(os.list_mark_history(&m), vec![(9, 999)]);

        // Clearing history must not touch oracle or mark rows.
        os.clear_mark_history(&m);
        assert!(os.list_mark_history(&m).is_empty());
        assert_eq!(os.get_oracle(&m).unwrap(), o);
        assert_eq!(os.get_mark(&m).unwrap(), mk);
    }
}
