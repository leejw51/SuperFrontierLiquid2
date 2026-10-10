//! `CF_FUTURES_MARKETS`: `deriv_id -> FutureMarket`.

use crate::db::{Database, CF_FUTURES_MARKETS};
use crate::perps::keys::deriv_key;
use cb_types::{DerivId, FutureMarket};

pub struct FuturesMarkets<'a> {
    db: &'a Database,
}

impl<'a> FuturesMarkets<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, m: &FutureMarket) {
        let k = deriv_key(&m.id);
        self.db
            .put(
                CF_FUTURES_MARKETS,
                &k,
                &bincode::serialize(m).expect("FutureMarket encode"),
            )
            .expect("rocks");
    }

    pub fn get(&self, id: &DerivId) -> Option<FutureMarket> {
        let k = deriv_key(id);
        self.db
            .get(CF_FUTURES_MARKETS, &k)
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("FutureMarket decode"))
    }

    pub fn list(&self) -> Vec<FutureMarket> {
        let mut out = Vec::new();
        let it = match self.db.iter_prefix(CF_FUTURES_MARKETS, &[]) {
            Ok(it) => it,
            Err(_) => return out,
        };
        for kv in it {
            let (_k, v) = match kv {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Ok(m) = bincode::deserialize::<FutureMarket>(&v) {
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

    fn sample(id: DerivId) -> FutureMarket {
        FutureMarket {
            id,
            underlying: AssetId::Eth,
            tick_size: 100,
            lot_size: 1000,
            max_leverage_x100: 2500,
            initial_margin_bps: 400,
            maintenance_margin_bps: 200,
            taker_fee_bps: 3,
            maker_fee_bps_signed: -1,
            expiry_height: 2_592_000,
            settlement_method: 0,
            settlement_window_blocks: 600,
            oracle_authority: Address([4; 20]),
            paused: false,
            final_settlement_px_q8: None,
            last_trade_px_q8: 0,
            last_trade_height: 0,
            paused_reduce_only_only: false,
        }
    }

    #[test]
    fn put_get_roundtrip_with_and_without_settlement_px() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let fms = FuturesMarkets::new(&db);
        let m0 = sample(DerivId::future(0));
        fms.put(&m0);
        assert_eq!(fms.get(&m0.id).unwrap(), m0);

        let mut m1 = sample(DerivId::future(1));
        m1.final_settlement_px_q8 = Some(3_200 * 100_000_000);
        fms.put(&m1);
        assert_eq!(
            fms.get(&m1.id).unwrap().final_settlement_px_q8,
            m1.final_settlement_px_q8
        );

        let list = fms.list();
        assert_eq!(list.len(), 2);
    }
}
