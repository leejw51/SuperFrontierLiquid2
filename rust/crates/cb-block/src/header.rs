//! Block header.

use cb_consensus::{QuorumCert, View};
use cb_crypto::keccak256;
use cb_types::H256;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHeader {
    pub height: u64,
    pub prev_hash: H256,
    pub tx_root: H256,
    pub state_root: H256,
    pub timestamp: u64,
    /// View this block was proposed in. The leader is `validators[view % N]`.
    /// Block 1 is always view 0; views advance on commit and on view-change.
    pub view: View,
    /// QC certifying that block `height - 1` was finalized by 2/3 of validators.
    /// Empty when `height == 1` (genesis predecessor was never voted on) or
    /// before the consensus voting layer is active. Stored as raw bincode bytes
    /// so the header type stays free of cycles with the consensus crate while
    /// still being part of the signed preimage.
    pub parent_qc: Vec<u8>,
    pub sequencer_sig: Vec<u8>,
}

/// Domain tag prefixed to the header preimage the proposer signs. Same
/// rationale as `cb_tx::TX_SIGNING_DOMAIN`: a header signature and a
/// transaction signature must not share a hash space. (Core review L2.)
pub const HEADER_SIGNING_DOMAIN: &[u8] = b"CB-BLOCK\0";

impl BlockHeader {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut h = self.clone();
        h.sequencer_sig.clear();
        let encoded = bincode::serialize(&h).expect("BlockHeader signing_bytes");
        let mut out = Vec::with_capacity(HEADER_SIGNING_DOMAIN.len() + encoded.len());
        out.extend_from_slice(HEADER_SIGNING_DOMAIN);
        out.extend_from_slice(&encoded);
        out
    }
    pub fn hash(&self) -> H256 {
        // Proofs and signatures are witnesses, not block identity. A valid QC
        // has multiple encodings (vote order and quorum subset); hashing those
        // bytes lets an untrusted relay change the parent hash without a key.
        // Consensus upgrade: old chains must be restarted/migrated explicitly.
        let content = (
            self.height,
            self.prev_hash,
            self.tx_root,
            self.state_root,
            self.timestamp,
            self.view,
        );
        let mut bytes = b"CB-BLOCK-ID-V2\0".to_vec();
        bytes.extend(bincode::serialize(&content).expect("BlockHeader identity"));
        keccak256(&bytes)
    }

    /// Decode `parent_qc` if present. Returns `Ok(None)` for genesis-predecessor
    /// blocks (empty `parent_qc`), `Ok(Some(_))` once the voting layer is wired
    /// and parent QCs are filled, `Err` if the bytes are malformed.
    pub fn parent_qc_decoded(&self) -> anyhow::Result<Option<QuorumCert>> {
        if self.parent_qc.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            QuorumCert::decode(&self.parent_qc).map_err(|e| anyhow::anyhow!("{e}"))?,
        ))
    }
}
