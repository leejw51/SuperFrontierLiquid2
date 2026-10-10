//! Intra-block ordering: cancellations > post-only > everything else across
//! accounts, and strict nonce order within one account.
//!
//! The two rules pull against each other, and the earlier version of this
//! function let the first win: it sorted every transaction by
//! `(rank, sender, nonce)`, so an account that placed an order at nonce `n` and
//! cancelled it at nonce `n + 1` inside the same block interval had its cancel
//! sorted *ahead* of its placement — where it failed `BadNonce`, was dropped,
//! and the placement then executed. A cancel that silently does nothing is the
//! worst outcome an order-management client can have, and the gateway
//! pipelines nonces within a block interval, so this was ordinary traffic.
//!
//! Now a sender's transactions are kept together, in nonce order, and the
//! whole group takes the rank of its *first* transaction. Cancels still beat
//! placements between accounts; within an account the chain's nonce rule is
//! never violated by the sorter that feeds it.
//!
//! This is leader-local. Replicas apply the order the block commits, so a
//! change here is not a consensus change.

use cb_crypto::keccak256;
use cb_tx::{TxBody, VerifiedTx};
use cb_types::{Address, H256};
use rustc_hash::FxHashMap as HashMap;

/// `seed` decides the order of *accounts* within a rank. Pass the parent
/// block's hash: accounts are then ordered by `keccak(seed || sender)`, a
/// permutation nobody can choose and that changes every block. Ordering by
/// bare address, as this used to, made a low address a permanent priority
/// advantage in every block (core review L6). Leader-local, like the rest of
/// this function.
pub fn order_for_block(txs: Vec<VerifiedTx>, seed: H256) -> Vec<VerifiedTx> {
    fn rank(b: &TxBody) -> u8 {
        match b {
            TxBody::CancelOrder { .. } | TxBody::BatchCancel { .. } => 0,
            TxBody::PlaceOrder {
                post_only: true, ..
            } => 1,
            _ => 2,
        }
    }
    // Group by sender, keyed by the seeded permutation so the tie-break
    // between accounts is `keccak(seed || sender)` rather than the address.
    // The key is computed once per sender, not once per transaction: a block
    // is thousands of transactions from far fewer senders, and the per-
    // transaction keccak was most of this function's cost.
    let mut by_addr: HashMap<Address, Vec<VerifiedTx>> = HashMap::default();
    for v in txs {
        by_addr.entry(v.sender).or_default().push(v);
    }
    let mut by_sender: Vec<([u8; 32], Vec<VerifiedTx>)> = by_addr
        .into_iter()
        .map(|(sender, group)| {
            let mut pre = [0u8; 52];
            pre[..32].copy_from_slice(&seed.0);
            pre[32..].copy_from_slice(&sender.0);
            (keccak256(&pre).0, group)
        })
        .collect();
    // Keys are distinct per sender, so this is the same order the map keyed
    // by them gave.
    by_sender.sort_unstable_by_key(|a| a.0);
    let mut groups: Vec<(u8, Vec<VerifiedTx>)> = by_sender
        .into_iter()
        .map(|(_, mut group)| {
            group.sort_by_key(|v| v.signed.tx.nonce);
            let r = rank(&group[0].signed.tx.body);
            (r, group)
        })
        .collect();
    // Stable, so equal ranks keep their seeded order.
    groups.sort_by_key(|(r, _)| *r);
    groups.into_iter().flat_map(|(_, g)| g).collect()
}
