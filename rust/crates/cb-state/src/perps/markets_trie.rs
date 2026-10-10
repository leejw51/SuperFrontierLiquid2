//! Unified markets trie: covers both `CF_PERP_MARKETS` and `CF_FUTURES_MARKETS`.
//!
//! M8 commits a single merkle root over **all** derivative markets, perp or
//! dated future. The two CFs stay separate at the storage layer (different
//! bincode value layouts) but share one trie because:
//!
//! 1. The state-root preimage has only one slot for "markets" — perps and
//!    futures together.
//! 2. `DerivId` already encodes the kind in byte 0 (`1 = perp`, `2 = future`),
//!    so the 4-byte trie keys are naturally disjoint between the two CFs. A
//!    rogue perp record under a future's id (or vice versa) would land at a
//!    different trie key and be obvious.
//! 3. The trie value is `[kind: 1][bincode(market): N]` — the kind byte is a
//!    redundancy check the verifier can use to refuse a value whose payload
//!    decodes as the wrong struct. Cheap insurance against a CF-mixup bug.
//!
//! Updates are rare (governance / genesis only). The hot path is read; the
//! commit path runs once per block and processes a tiny set.

use crate::db::{
    BufferedKv, Database, DbBatch, DbError, CF_FUTURES_MARKETS, CF_META, CF_PERP_MARKETS,
    CF_TRIE_NODES,
};
use crate::perps::keys::deriv_key;
use cb_trie::MerkleTrie;
use cb_types::{DerivId, MarketKind, H256};
use std::collections::HashSet;

/// Meta-CF key holding the latest markets trie root (perps + futures combined).
const META_MARKETS_ROOT: &[u8] = b"markets_root";

/// Discriminator bytes prefixed onto the trie value. Mirrors `MarketKind` but
/// pinned to byte values so a future enum reorder can't silently shift the
/// trie value layout.
const VAL_KIND_PERP: u8 = 1;
const VAL_KIND_FUTURE: u8 = 2;

pub struct MarketsTrie<'a> {
    db: &'a Database,
}

impl<'a> MarketsTrie<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    /// Read the current markets trie root. Genesis returns `H256::ZERO` until
    /// `commit_batch` or `rebuild_root` runs.
    pub fn root(&self) -> H256 {
        match self.db.get(CF_META, META_MARKETS_ROOT).expect("rocks") {
            Some(b) if b.len() == 32 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            }
            _ => H256::ZERO,
        }
    }

    /// Resolve the value for one trie key by reading the appropriate CF based
    /// on the `DerivId`'s kind. Returns the trie-encoded value
    /// `[kind: 1][cf_bytes...]` or `None` if the row is missing (deletion) or
    /// the `DerivId` is malformed (defensive — caller bug to commit one).
    fn read_value(&self, market: &DerivId) -> Result<Option<Vec<u8>>, DbError> {
        match market.kind() {
            Ok(MarketKind::Perp) => {
                let raw = self.db.get(CF_PERP_MARKETS, &deriv_key(market))?;
                Ok(raw.map(|bytes| {
                    let mut out = Vec::with_capacity(1 + bytes.len());
                    out.push(VAL_KIND_PERP);
                    out.extend_from_slice(&bytes);
                    out
                }))
            }
            Ok(MarketKind::Future) => {
                let raw = self.db.get(CF_FUTURES_MARKETS, &deriv_key(market))?;
                Ok(raw.map(|bytes| {
                    let mut out = Vec::with_capacity(1 + bytes.len());
                    out.push(VAL_KIND_FUTURE);
                    out.extend_from_slice(&bytes);
                    out
                }))
            }
            Err(_) => Ok(None),
        }
    }

    /// Apply a batch of touched market keys to the markets trie. Mirrors
    /// `Positions::commit_batch` — we re-read the CF for each touched id and
    /// trie-insert (Some) or trie-remove (None). Trie nodes + meta-root land
    /// in `batch` for atomic commit alongside accounts and positions.
    pub fn commit_batch(
        &self,
        touched: &HashSet<DerivId>,
        batch: &mut DbBatch<'_>,
    ) -> Result<H256, DbError> {
        let prior_root = self.root();
        if touched.is_empty() {
            return Ok(prior_root);
        }
        let mut keys: Vec<DerivId> = touched.iter().copied().collect();
        keys.sort_by_key(|a| a.0);

        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, prior_root);
            // M12: prefetch the touched paths' interior nodes via one BFS
            // walk. Markets touch sets are tiny (governance only) so the
            // win is small here, but the consistency with positions makes
            // the commit-path code uniform.
            let key_buf: Vec<[u8; 4]> = keys.iter().map(deriv_key).collect();
            let key_refs: Vec<&[u8]> = key_buf.iter().map(|k| k.as_slice()).collect();
            trie.prefetch_paths(&key_refs).expect("trie prefetch");

            for did in &keys {
                let k = deriv_key(did);
                match self.read_value(did)? {
                    Some(v) => trie.insert(&k, v).expect("trie insert"),
                    None => trie.remove(&k).expect("trie remove"),
                }
            }
            trie.root()
        };
        buffered.flush_into(batch)?;
        batch.put(CF_META, META_MARKETS_ROOT, &new_root.0)?;
        Ok(new_root)
    }

    /// Genesis-time helper: walk both CFs and rebuild the trie from scratch,
    /// committing in a single `WriteBatch`. Skips spot — only derivatives
    /// belong here.
    pub fn rebuild_root(&self) -> Result<H256, DbError> {
        let mut batch = self.db.batch();
        let mut buffered = BufferedKv::new(self.db, CF_TRIE_NODES);
        let new_root = {
            let mut trie = MerkleTrie::open(&mut buffered, H256::ZERO);
            // Perp markets first; CF_PERP_MARKETS keys are 4 bytes with byte 0
            // = 1 (perp), so they sort before futures (kind=2) in lex order.
            for kv in self.db.iter_prefix(CF_PERP_MARKETS, &[])? {
                let (k, v) = match kv {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if k.len() != 4 {
                    continue;
                }
                let mut val = Vec::with_capacity(1 + v.len());
                val.push(VAL_KIND_PERP);
                val.extend_from_slice(&v);
                trie.insert(&k, val).expect("trie insert");
            }
            for kv in self.db.iter_prefix(CF_FUTURES_MARKETS, &[])? {
                let (k, v) = match kv {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if k.len() != 4 {
                    continue;
                }
                let mut val = Vec::with_capacity(1 + v.len());
                val.push(VAL_KIND_FUTURE);
                val.extend_from_slice(&v);
                trie.insert(&k, val).expect("trie insert");
            }
            trie.root()
        };
        buffered.flush_into(&mut batch)?;
        batch.put(CF_META, META_MARKETS_ROOT, &new_root.0)?;
        self.db.write_batch(batch)?;
        Ok(new_root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perps::futures_markets::FuturesMarkets;
    use crate::perps::perp_markets::PerpMarkets;
    use cb_types::{Address, AssetId, FutureMarket, PerpMarket};
    use tempfile::tempdir;

    fn perp_sample(id: DerivId) -> PerpMarket {
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

    fn future_sample(id: DerivId) -> FutureMarket {
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
    fn root_zero_when_empty() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mt = MarketsTrie::new(&db);
        assert_eq!(mt.root(), H256::ZERO);
    }

    #[test]
    fn commit_batch_changes_root_and_revert_undoes_it() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mt = MarketsTrie::new(&db);
        let pms = PerpMarkets::new(&db);
        let fms = FuturesMarkets::new(&db);

        // Insert N perp + M future markets.
        let mut touched = HashSet::new();
        for i in 0..4u16 {
            pms.put(&perp_sample(DerivId::perp(i)));
            touched.insert(DerivId::perp(i));
        }
        for i in 0..3u16 {
            fms.put(&future_sample(DerivId::future(i)));
            touched.insert(DerivId::future(i));
        }
        let mut batch = db.batch();
        let root_full = mt.commit_batch(&touched, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_full, H256::ZERO);

        // Mutate one perp record: root changes.
        let mut p0 = pms.get(&DerivId::perp(0)).unwrap();
        p0.taker_fee_bps = 99;
        pms.put(&p0);
        let mut t = HashSet::new();
        t.insert(DerivId::perp(0));
        let mut batch = db.batch();
        let root_mut = mt.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_ne!(root_mut, root_full);

        // Revert to original taker fee — root recovers.
        let mut p0 = pms.get(&DerivId::perp(0)).unwrap();
        p0.taker_fee_bps = 3;
        pms.put(&p0);
        let mut batch = db.batch();
        let root_rev = mt.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();
        assert_eq!(root_rev, root_full);
    }

    #[test]
    fn perp_and_future_with_same_index_have_distinct_trie_keys() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mt = MarketsTrie::new(&db);
        let pms = PerpMarkets::new(&db);
        let fms = FuturesMarkets::new(&db);
        // Same numeric index on both kinds → the kind byte in DerivId
        // disambiguates them, so the trie sees two distinct entries.
        pms.put(&perp_sample(DerivId::perp(7)));
        fms.put(&future_sample(DerivId::future(7)));
        let mut t = HashSet::new();
        t.insert(DerivId::perp(7));
        t.insert(DerivId::future(7));
        let mut batch = db.batch();
        let root_both = mt.commit_batch(&t, &mut batch).unwrap();
        db.write_batch(batch).unwrap();

        // Compare against just the perp side: root must differ.
        let dir2 = tempdir().unwrap();
        let db2 = Database::open(dir2.path()).unwrap();
        let mt2 = MarketsTrie::new(&db2);
        PerpMarkets::new(&db2).put(&perp_sample(DerivId::perp(7)));
        let mut t2 = HashSet::new();
        t2.insert(DerivId::perp(7));
        let mut batch = db2.batch();
        let root_perp_only = mt2.commit_batch(&t2, &mut batch).unwrap();
        db2.write_batch(batch).unwrap();
        assert_ne!(root_both, root_perp_only);
    }

    #[test]
    fn rebuild_root_matches_incremental() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let mt = MarketsTrie::new(&db);
        let pms = PerpMarkets::new(&db);
        let fms = FuturesMarkets::new(&db);

        let mut touched = HashSet::new();
        for i in 0..5u16 {
            pms.put(&perp_sample(DerivId::perp(i)));
            touched.insert(DerivId::perp(i));
        }
        for i in 0..2u16 {
            fms.put(&future_sample(DerivId::future(i)));
            touched.insert(DerivId::future(i));
        }
        let mut batch = db.batch();
        let incremental = mt.commit_batch(&touched, &mut batch).unwrap();
        db.write_batch(batch).unwrap();

        // Fresh DB, same rows put, then rebuild_root.
        let dir2 = tempdir().unwrap();
        let db2 = Database::open(dir2.path()).unwrap();
        let mt2 = MarketsTrie::new(&db2);
        let pms2 = PerpMarkets::new(&db2);
        let fms2 = FuturesMarkets::new(&db2);
        for i in 0..5u16 {
            pms2.put(&perp_sample(DerivId::perp(i)));
        }
        for i in 0..2u16 {
            fms2.put(&future_sample(DerivId::future(i)));
        }
        let rebuilt = mt2.rebuild_root().unwrap();
        assert_eq!(incremental, rebuilt);
    }
}
