//! Chain metadata: head height, head hash, state root, chain id, company address,
//! and the address of the current proposer (the validator entitled to sign blocks
//! at the current view). The proposer is updated by genesis (initial leader =
//! validators[0]) and later, when view-change lands, by the consensus layer.

use crate::db::{Database, DbBatch, DbError, CF_META};
use cb_types::{Address, H256};

const HEAD_HEIGHT: &[u8] = b"head_height";
const HEAD_HASH: &[u8] = b"head_hash";
const STATE_ROOT: &[u8] = b"state_root";
const CHAIN_ID: &[u8] = b"chain_id";
const DENOM: &[u8] = b"denom";
const COMPANY_ADDR: &[u8] = b"company_addr";
const COMMUNITY_WALLET_ADDR: &[u8] = b"community_wallet_addr";
const PROPOSER_ADDR: &[u8] = b"proposer_addr";
const VIEW: &[u8] = b"view";
const LAST_QC_HEIGHT: &[u8] = b"last_qc_height";
const HEAD_TIMESTAMP: &[u8] = b"head_timestamp";
const STATE_HASH_SCHEME: &[u8] = b"state_hash_scheme";
const CONSENSUS_SCHEME: &[u8] = b"consensus_scheme";

/// `Meta::consensus_scheme` value: one sequencer signs every block and
/// replicas follow it (the original design).
pub const CONSENSUS_SEQUENCER: u32 = 0;
/// `Meta::consensus_scheme` value: blocks are ordered by the BFT protocol in
/// `cb_consensus::bft`, executed after commit, and each carries its finality
/// proof in the header's `parent_qc` slot.
pub const CONSENSUS_BFT: u32 = 1;

pub struct Meta<'a> {
    db: &'a Database,
}

impl<'a> Meta<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn head_height(&self) -> u64 {
        self.db
            .get(CF_META, HEAD_HEIGHT)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn set_head(&self, height: u64, hash: H256, state_root: H256) {
        self.db
            .put(CF_META, HEAD_HEIGHT, &height.to_be_bytes())
            .expect("rocks");
        self.db.put(CF_META, HEAD_HASH, &hash.0).expect("rocks");
        self.db
            .put(CF_META, STATE_ROOT, &state_root.0)
            .expect("rocks");
    }

    /// The same three writes, added to a caller-owned batch. Block commit
    /// uses this so the head pointer lands in the same atomic write as the
    /// state it describes.
    pub fn set_head_into_batch(
        &self,
        batch: &mut DbBatch<'_>,
        height: u64,
        hash: H256,
        state_root: H256,
        timestamp: u64,
    ) -> Result<(), DbError> {
        batch.put(CF_META, HEAD_HEIGHT, &height.to_be_bytes())?;
        batch.put(CF_META, HEAD_HASH, &hash.0)?;
        batch.put(CF_META, STATE_ROOT, &state_root.0)?;
        batch.put(CF_META, HEAD_TIMESTAMP, &timestamp.to_be_bytes())
    }

    /// The head block's header timestamp, milliseconds. `0` before any block.
    ///
    /// Kept here rather than read from the parent block so a replica whose
    /// old blocks were pruned can still refuse a header whose clock runs
    /// backwards (core review L5). Outside the state root, like the rest of
    /// `Meta`.
    /// The head's height and timestamp as `(key, value)` rows of `CF_META`,
    /// exactly as [`Meta::set_head_into_batch`] writes them. A pipelined
    /// leader makes these visible as soon as a block has executed, because
    /// the next block builds on them; the head hash and state root follow
    /// once the block is sealed ([`Meta::sealed_head_rows`]).
    pub fn head_progress_rows(height: u64, timestamp: u64) -> [(Vec<u8>, Vec<u8>); 2] {
        [
            (HEAD_HEIGHT.to_vec(), height.to_be_bytes().to_vec()),
            (HEAD_TIMESTAMP.to_vec(), timestamp.to_be_bytes().to_vec()),
        ]
    }

    /// The head hash and state root as `(key, value)` rows of `CF_META`.
    pub fn sealed_head_rows(hash: H256, state_root: H256) -> [(Vec<u8>, Vec<u8>); 2] {
        [
            (HEAD_HASH.to_vec(), hash.0.to_vec()),
            (STATE_ROOT.to_vec(), state_root.0.to_vec()),
        ]
    }

    pub fn head_timestamp(&self) -> u64 {
        self.db
            .get(CF_META, HEAD_TIMESTAMP)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn head_hash(&self) -> H256 {
        self.db
            .get(CF_META, HEAD_HASH)
            .expect("rocks")
            .map(|b| {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            })
            .unwrap_or(H256::ZERO)
    }

    pub fn state_root(&self) -> H256 {
        self.db
            .get(CF_META, STATE_ROOT)
            .expect("rocks")
            .map(|b| {
                let mut h = [0u8; 32];
                h.copy_from_slice(&b);
                H256(h)
            })
            .unwrap_or(H256::ZERO)
    }

    pub fn chain_id(&self) -> u32 {
        self.db
            .get(CF_META, CHAIN_ID)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 4];
                a.copy_from_slice(&b);
                u32::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn set_chain_id(&self, id: u32) {
        self.db
            .put(CF_META, CHAIN_ID, &id.to_be_bytes())
            .expect("rocks");
    }

    /// Which state-commitment scheme this data directory's roots were built
    /// with (`state_hash::STATE_HASH_SCHEME`). `0` means the key was never
    /// written: a directory from before the scheme was recorded, whose roots
    /// are merkle-trie roots.
    pub fn state_hash_scheme(&self) -> u32 {
        self.db
            .get(CF_META, STATE_HASH_SCHEME)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 4];
                a.copy_from_slice(&b);
                u32::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn set_state_hash_scheme(&self, scheme: u32) {
        self.db
            .put(CF_META, STATE_HASH_SCHEME, &scheme.to_be_bytes())
            .expect("rocks");
    }

    /// How this chain's blocks are produced and checked:
    /// [`CONSENSUS_SEQUENCER`] (the default, and what an unwritten key means)
    /// or [`CONSENSUS_BFT`]. Fixed at genesis; a block from one scheme does
    /// not verify under the other.
    pub fn consensus_scheme(&self) -> u32 {
        self.db
            .get(CF_META, CONSENSUS_SCHEME)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 4];
                a.copy_from_slice(&b);
                u32::from_be_bytes(a)
            })
            .unwrap_or(CONSENSUS_SEQUENCER)
    }

    pub fn set_consensus_scheme(&self, scheme: u32) {
        self.db
            .put(CF_META, CONSENSUS_SCHEME, &scheme.to_be_bytes())
            .expect("rocks");
    }

    pub fn is_bft(&self) -> bool {
        self.consensus_scheme() == CONSENSUS_BFT
    }

    /// User-facing denomination string (e.g. "USD", "ULD"). Set from the
    /// `chain.denom` field in genesis. Display-layer metadata only — the
    /// engine still keys balances by `AssetId::Usd` regardless of this name.
    /// Returns "USD" when the key has never been written.
    pub fn denom(&self) -> String {
        self.db
            .get(CF_META, DENOM)
            .expect("rocks")
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_else(|| "USD".to_string())
    }

    pub fn set_denom(&self, s: &str) {
        self.db.put(CF_META, DENOM, s.as_bytes()).expect("rocks");
    }

    pub fn company_addr(&self) -> Address {
        self.db
            .get(CF_META, COMPANY_ADDR)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 20];
                a.copy_from_slice(&b);
                Address(a)
            })
            .unwrap_or(Address::ZERO)
    }

    pub fn set_company_addr(&self, a: Address) {
        self.db.put(CF_META, COMPANY_ADDR, &a.0).expect("rocks");
    }

    /// Address of the community wallet — a special account credited at genesis
    /// that holds protocol-level funds (e.g. unallocated treasury). Independent
    /// from the company address; both are persisted separately.
    pub fn community_wallet_addr(&self) -> Address {
        self.db
            .get(CF_META, COMMUNITY_WALLET_ADDR)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 20];
                a.copy_from_slice(&b);
                Address(a)
            })
            .unwrap_or(Address::ZERO)
    }

    pub fn set_community_wallet_addr(&self, a: Address) {
        self.db
            .put(CF_META, COMMUNITY_WALLET_ADDR, &a.0)
            .expect("rocks");
    }

    pub fn proposer_addr(&self) -> Address {
        self.db
            .get(CF_META, PROPOSER_ADDR)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 20];
                a.copy_from_slice(&b);
                Address(a)
            })
            .unwrap_or(Address::ZERO)
    }

    pub fn set_proposer_addr(&self, a: Address) {
        self.db.put(CF_META, PROPOSER_ADDR, &a.0).expect("rocks");
    }

    pub fn view(&self) -> u64 {
        self.db
            .get(CF_META, VIEW)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn set_view(&self, v: u64) {
        self.db.put(CF_META, VIEW, &v.to_be_bytes()).expect("rocks");
    }

    /// Highest block height for which a QuorumCert is persisted in `CF_QC`.
    /// Cheap O(1) read for dashboards / verification scripts; updated by the
    /// leader's vote pool whenever quorum forms for a new height.
    pub fn last_qc_height(&self) -> u64 {
        self.db
            .get(CF_META, LAST_QC_HEIGHT)
            .expect("rocks")
            .map(|b| {
                let mut a = [0u8; 8];
                a.copy_from_slice(&b);
                u64::from_be_bytes(a)
            })
            .unwrap_or(0)
    }

    pub fn set_last_qc_height(&self, h: u64) {
        self.db
            .put(CF_META, LAST_QC_HEIGHT, &h.to_be_bytes())
            .expect("rocks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use tempfile::tempdir;

    #[test]
    fn meta_set_get_roundtrip() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let meta = Meta::new(&db);
        meta.set_chain_id(1);
        meta.set_head(5, H256([1; 32]), H256([2; 32]));
        meta.set_company_addr(Address([0xc0; 20]));
        meta.set_community_wallet_addr(Address([0xcc; 20]));
        meta.set_proposer_addr(Address([0xab; 20]));
        meta.set_view(7);
        assert_eq!(meta.chain_id(), 1);
        assert_eq!(meta.head_height(), 5);
        assert_eq!(meta.head_hash(), H256([1; 32]));
        assert_eq!(meta.state_root(), H256([2; 32]));
        assert_eq!(meta.company_addr(), Address([0xc0; 20]));
        assert_eq!(meta.community_wallet_addr(), Address([0xcc; 20]));
        assert_eq!(meta.proposer_addr(), Address([0xab; 20]));
        assert_eq!(meta.view(), 7);
    }

    #[test]
    fn community_wallet_addr_defaults_to_zero() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let meta = Meta::new(&db);
        assert_eq!(meta.community_wallet_addr(), Address::ZERO);
    }

    #[test]
    fn denom_defaults_to_usd_when_unset() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let meta = Meta::new(&db);
        assert_eq!(meta.denom(), "USD");
    }

    #[test]
    fn denom_roundtrips_through_db() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let meta = Meta::new(&db);
        meta.set_denom("ULD");
        assert_eq!(meta.denom(), "ULD");
        // Reopen to confirm persistence. `Meta` borrows `db`, so letting the
        // borrow end is all that is needed before the handle itself is closed;
        // `drop` on a type with no `Drop` impl only extends its lifetime.
        let _ = meta;
        drop(db);
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(Meta::new(&db).denom(), "ULD");
    }

    #[test]
    fn state_hash_scheme_defaults_to_zero_and_round_trips() {
        let dir = tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        let meta = Meta::new(&db);
        assert_eq!(meta.state_hash_scheme(), 0);
        meta.set_state_hash_scheme(2);
        assert_eq!(meta.state_hash_scheme(), 2);
    }
}
