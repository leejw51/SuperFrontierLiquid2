//! `CF_PERP_MARKETS`: `deriv_id -> PerpMarket`.

use crate::db::{Database, CF_PERP_MARKETS};
use crate::perps::keys::deriv_key;
use cb_types::{DerivId, PerpMarket};

pub struct PerpMarkets<'a> {
    db: &'a Database,
}

impl<'a> PerpMarkets<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, m: &PerpMarket) {
        let k = deriv_key(&m.id);
        self.db
            .put(
                CF_PERP_MARKETS,
                &k,
                &bincode::serialize(m).expect("PerpMarket encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, id: &DerivId) -> Option<PerpMarket> {
        let k = deriv_key(id);
        self.db
            .get(CF_PERP_MARKETS, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("PerpMarket decode"))
    }

    /// Enumerate every perp market. Order is RocksDB lex order, which equals
    /// `DerivId` byte order — i.e. ascending market index.
    pub fn list(&self) -> Vec<PerpMarket> {
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_PERP_MARKETS, &[]) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(m) = bincode::deserialize::<PerpMarket>(&v) {
                out.push(m);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use cb_types::{Address, AssetId};
    use tempfile::tempdir;

    fn sample(id: DerivId) -> PerpMarket {
        PerpMarket {
            id,
            underlying: AssetId::Btc,
            tick_size: 100,
            lot_size: 1000,
            max_leverage_x100: 5000,
            initial_margin_bps: 200,
            maintenance_margin_bps: 100,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            funding_interval_blocks: 3600,
            funding_rate_cap_q9: 1_000_000,
            oracle_authority: Address([3; 20]),
            max_position_notional: 100_000_000_000_000,
            paused: false,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let pms = PerpMarkets::new(&db);
        let m = sample(DerivId::perp(0));
        pms.put(&m);
        assert_eq!(pms.get(&m.id).unwrap(), m);
    }

    #[test]
    fn list_returns_all_markets_in_index_order() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let pms = PerpMarkets::new(&db);
        pms.put(&sample(DerivId::perp(2)));
        pms.put(&sample(DerivId::perp(0)));
        pms.put(&sample(DerivId::perp(1)));
        let list = pms.list();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, DerivId::perp(0));
        assert_eq!(list[1].id, DerivId::perp(1));
        assert_eq!(list[2].id, DerivId::perp(2));
    }
}
