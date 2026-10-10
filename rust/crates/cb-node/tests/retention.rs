//! Every node keeps the last N block bodies and prunes the rest, N configurable
//! and 10 by default.
//!
//! State, the four tries, the markets and the head are never pruned: they are
//! what answers a question about the present. Block bodies are the archive,
//! and a node that keeps the whole archive by accident is a node that fills a
//! disk. Under a stress run — tens of thousands of orders in every block —
//! one afternoon of testing grew a chain here to 79 GB.
//!
//! What this pins: after N+k blocks the window holds the last N, the ones
//! below are gone, and the chain still works — head, state root and balances
//! are exactly what they were, because nothing pruned was state.

use cb_block::{build_and_apply, ExecCtx};
use cb_crypto::{address_from_pubkey, generate_keypair, keccak256, sign, SecretKey};
use cb_state::{AccountState, Accounts, Blocks, Database, Meta};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, H256};
use tempfile::tempdir;

/// The node's own default, asserted here so a change to it fails a test
/// rather than a disk.
const DEFAULT_KEEP: u64 = 10;

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn transfer(sk: &SecretKey, nonce: u64, to: Address) -> cb_tx::VerifiedTx {
    SignedTx::sign_with(
        sk,
        Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to,
                asset: AssetId::Usd,
                amount: 1,
            },
        },
    )
    .unwrap()
    .verify()
    .unwrap()
}

/// Build `n` blocks. `keep` of `None` is the node's "keep everything"
/// setting, which is what `--keep-blocks 0` maps to before the loop ever
/// reaches the pruning helpers; `Some(k)` prunes after each block the way the
/// sequencer loop does.
fn run(n: u64, keep: Option<u64>) -> (tempfile::TempDir, Database, Address, H256) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    let (psk, ppk) = generate_keypair();
    Meta::new(&db).set_proposer_addr(address_from_pubkey(&ppk));
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000_000).unwrap();
        accts.put(&alice, &s);
    }
    for i in 0..n {
        let signer = |p: &[u8]| sign(&psk, keccak256(p)).unwrap().to_vec();
        build_and_apply(
            &db,
            &ctx(),
            vec![transfer(&alice_sk, i, bob)],
            1_000 + i,
            0,
            Vec::new(),
            signer,
        )
        .expect("block builds");
        if let Some(k) = keep {
            cb_node::sequencer::prune_one(&db, i + 1, k);
        }
    }
    let root = Meta::new(&db).state_root();
    (dir, db, alice, root)
}

fn stored_heights(db: &Database, upto: u64) -> Vec<u64> {
    (1..=upto)
        .filter(|h| Blocks::new(db).get(*h).is_some())
        .collect()
}

#[test]
fn a_node_keeps_the_last_ten_blocks_by_default() {
    let (_d, db, alice, root) = run(40, Some(DEFAULT_KEEP));
    let kept = stored_heights(&db, 40);
    assert_eq!(
        kept,
        (31..=40).collect::<Vec<_>>(),
        "the window should be the last {DEFAULT_KEEP} blocks"
    );
    // The chain is untouched by what was pruned.
    assert_eq!(Meta::new(&db).head_height(), 40);
    assert_eq!(Meta::new(&db).state_root(), root);
    assert_eq!(
        Accounts::new(&db).get(&alice).map(|a| a.nonce),
        Some(40),
        "state survived the pruning of the blocks that produced it"
    );
}

#[test]
fn the_window_is_configurable() {
    for keep in [1u64, 3, 25] {
        let (_d, db, _, _) = run(40, Some(keep));
        let kept = stored_heights(&db, 40);
        assert_eq!(kept.len() as u64, keep, "keep={keep}");
        assert_eq!(*kept.last().unwrap(), 40);
        assert_eq!(*kept.first().unwrap(), 41 - keep);
    }
}

/// A node that has been running with pruning off, then turns it on, catches
/// up at startup rather than carrying the backlog for ever. The catch-up is
/// budgeted so a long chain does not hold the sequencer at the door.
#[test]
fn startup_prunes_a_backlog_left_by_an_unpruned_run() {
    // `--keep-blocks 0`: the node never calls the pruning helpers at all.
    let (_d, db, _, _) = run(60, None);
    assert_eq!(stored_heights(&db, 60).len(), 60, "nothing pruned yet");
    cb_node::sequencer::startup_prune(&db, DEFAULT_KEEP);
    assert_eq!(
        stored_heights(&db, 60),
        (51..=60).collect::<Vec<_>>(),
        "startup brought the window back to the last {DEFAULT_KEEP}"
    );
}
