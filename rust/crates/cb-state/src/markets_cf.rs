//! Markets CF: MarketId -> MarketConfig.

use crate::db::{Database, CF_MARKETS};
use cb_types::{AssetId, MarketId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketConfig {
    pub id: MarketId,
    pub base: AssetId,
    pub quote: AssetId,
    pub tick_size: u128,
    pub min_qty: u128,
    pub fee_bps: u16,
}

pub struct Markets<'a> {
    db: &'a Database,
}

impl<'a> Markets<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn put(&self, m: &MarketConfig) {
        self.db
            .put(CF_MARKETS, &[m.id.as_u8()], &bincode::serialize(m).unwrap())
            .expect("rocks");
    }

    pub fn get(&self, id: MarketId) -> Option<MarketConfig> {
        self.db
            .get(CF_MARKETS, &[id.as_u8()])
            .expect("rocks")
            .map(|b| bincode::deserialize(&b).expect("MarketConfig decode"))
    }
}
