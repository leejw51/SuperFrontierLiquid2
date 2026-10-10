//! Four validators order real transactions through the BFT engine and
//! execute what commits; an observer replays the executed blocks through
//! the finality-proof path and lands on the same roots.
//!
//! In process, without the network: the engines hand each other messages
//! directly, so what is tested is the seam between ordering and execution —
//! that every validator executes the same committed payload to the same
//! block bytes, that a payload with a failing transaction is kept whole and
//! replayed identically, and that a block whose proof was tampered with is
//! refused by anyone checking it.

use cb_block::{order_for_block, Block, ExecCtx};
use cb_consensus::bft::{Action, Committed, Config, Engine, Finality, Message, Root, ValidatorSet};
use cb_crypto::{address_from_pubkey, generate_keypair, keypair_from_seed, SecretKey};
use cb_net::apply_remote_block;
use cb_node::bft_writer::{BlockWriter, Roots};
use cb_node::validator::{genesis_id, payload_root};
use cb_state::{AccountState, Accounts, Blocks, Database, Meta, ValidatorInfo, Validators};
use cb_tx::{SignedTx, Transaction, TxBody, VerifiedTx};
use cb_types::{Address, AssetId, H256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

struct Node {
    _dir: tempfile::TempDir,
    db: Database,
    engine: Engine,
    writer: BlockWriter,
    roots: Roots,
    executed: Vec<Committed>,
}

fn new_writer(db: &Database) -> (BlockWriter, Roots) {
    let roots: Roots = Arc::new(Mutex::new(BTreeMap::new()));
    let w = BlockWriter::new(db.clone(), ctx(), roots.clone(), Arc::new(|_, _| {}));
    (w, roots)
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn open_db(set: &[ValidatorInfo], alice: Address) -> (tempfile::TempDir, Database) {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    Meta::new(&db).set_chain_id(1);
    Meta::new(&db).set_proposer_addr(set[0].addr);
    Meta::new(&db).set_consensus_scheme(cb_state::CONSENSUS_BFT);
    Validators::new(&db).put_set(set);
    let mut s = AccountState::default();
    s.add(AssetId::Usd, 1_000_000).unwrap();
    Accounts::new(&db).put(&alice, &s);
    (dir, db)
}

fn cluster(n: usize, alice: Address) -> (Vec<Node>, Vec<SecretKey>) {
    let keys: Vec<(SecretKey, Address)> = (0..n)
        .map(|i| {
            let (sk, pk) = keypair_from_seed(format!("bft-exec-{i}").as_bytes());
            (sk, address_from_pubkey(&pk))
        })
        .collect();
    let set: Vec<ValidatorInfo> = keys
        .iter()
        .map(|(_, a)| ValidatorInfo {
            addr: *a,
            pubkey: vec![0; 33],
            weight: 1,
        })
        .collect();
    let vset = ValidatorSet::new(keys.iter().map(|(_, a)| *a).collect()).unwrap();
    let nodes = keys
        .iter()
        .map(|(sk, a)| {
            let (dir, db) = open_db(&set, alice);
            let g = genesis_id(&db);
            let engine = Engine::new(
                Config {
                    base_timeout_ms: 1_000,
                    max_timeout_ms: 8_000,
                    retain_rounds: 64,
                },
                vset.clone(),
                *a,
                *sk,
                g,
                Root::genesis(g),
                payload_root,
                0,
            );
            let (writer, roots) = new_writer(&db);
            Node {
                _dir: dir,
                db,
                engine,
                writer,
                roots,
                executed: Vec::new(),
            }
        })
        .collect();
    (nodes, keys.into_iter().map(|(sk, _)| sk).collect())
}

fn transfer(sk: &SecretKey, nonce: u64, to: Address) -> VerifiedTx {
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

fn decode_payload(payload: &[u8]) -> Vec<VerifiedTx> {
    let signed: Vec<SignedTx> = bincode::deserialize(payload).unwrap();
    let senders = cb_tx::recover_senders_par(&signed).unwrap();
    signed
        .into_iter()
        .zip(senders)
        .map(|(s, a)| VerifiedTx::from_parts_unchecked(s, a))
        .collect()
}

/// What the validator's driver does with a `Commit` entry: seal the block
/// executed at commit if this is its certificate, otherwise execute and
/// seal at once.
fn commit(node: &mut Node, c: &Committed) {
    if node.writer.unsealed() == Some(c.header.id()) {
        assert!(node.writer.seal(c).unwrap());
        return;
    }
    if c.header.is_empty() {
        return;
    }
    node.writer
        .execute_sealed(c, decode_payload(&c.payload))
        .expect("committed proposal executes");
}

/// Deliver every broadcast to every other node, immediately, until no node
/// has anything left to say. `next_payload` is asked whenever a leader may
/// propose.
/// `next_payload` returns `None` when the leader should stop proposing.
#[allow(clippy::needless_range_loop)]
fn settle(
    nodes: &mut [Node],
    now: &mut u64,
    mut next_payload: impl FnMut(u64, &Engine) -> Option<Vec<VerifiedTx>>,
) {
    let mut queue: Vec<(usize, Action)> = Vec::new();
    for (i, n) in nodes.iter_mut().enumerate() {
        for a in n.engine.take_initial_actions() {
            queue.push((i, a));
        }
    }
    let mut guard = 0;
    let mut stalls = 0;
    loop {
        if queue.is_empty() {
            // Nothing in flight: let a leader propose, or stop.
            let mut any = false;
            let mut done = false;
            for i in 0..nodes.len() {
                if nodes[i].engine.is_leader() {
                    let round = nodes[i].engine.round();
                    let Some(txs) = next_payload(round, &nodes[i].engine) else {
                        done = true;
                        continue;
                    };
                    let seed = nodes[i].engine.high_qc().id;
                    let txs = order_for_block(txs, seed);
                    let signed: Vec<SignedTx> = txs.iter().map(|v| v.signed.clone()).collect();
                    let payload = if signed.is_empty() {
                        Vec::new()
                    } else {
                        bincode::serialize(&signed).unwrap()
                    };
                    *now += 1;
                    let durable = nodes[i].db.durable_view();
                    let exec_height = Meta::new(&durable).head_height();
                    let exec_root = Meta::new(&durable).state_root();
                    let acts = nodes[i].engine.propose(
                        payload,
                        signed.len() as u32,
                        *now,
                        exec_height,
                        exec_root,
                        *now,
                    );
                    if !acts.is_empty() {
                        any = true;
                    }
                    for a in acts {
                        queue.push((i, a));
                    }
                }
            }
            if !any && (done || stalls >= 16) {
                return;
            }
            if !any {
                // The round's leader is not among these nodes: let the
                // round time out.
                stalls += 1;
                *now = nodes
                    .iter()
                    .map(|n| n.engine.next_deadline_ms())
                    .max()
                    .unwrap_or(*now)
                    .max(*now + 1);
                for i in 0..nodes.len() {
                    for a in nodes[i].engine.on_tick(*now) {
                        queue.push((i, a));
                    }
                }
                continue;
            }
            stalls = 0;
        }
        guard += 1;
        assert!(guard < 100_000, "the cluster did not settle");
        let (from, action) = queue.remove(0);
        match action {
            Action::Broadcast(msg) => {
                for to in 0..nodes.len() {
                    if to == from {
                        continue;
                    }
                    let acts = nodes[to].engine.on_message(msg.clone(), None, *now);
                    for a in acts {
                        queue.push((to, a));
                    }
                }
            }
            Action::Commit(list) => {
                for c in list {
                    commit(&mut nodes[from], &c);
                    nodes[from].executed.push(c);
                }
            }
            Action::Ordered(o) => {
                // Executed at commit; sealed when its certificate comes.
                if !o.header.is_empty() {
                    nodes[from]
                        .writer
                        .execute(&o.header, decode_payload(&o.payload))
                        .expect("committed proposal executes");
                }
            }
            Action::ReadyToPropose { .. } => {}
            Action::RequestProposal { id, .. } => {
                let p = nodes.iter().find_map(|n| n.engine.proposal(&id)).unwrap();
                let acts = nodes[from]
                    .engine
                    .on_message(Message::Proposal(p), None, *now);
                for a in acts {
                    queue.push((from, a));
                }
            }
        }
    }
}

/// Every validator sealed the same block, byte for byte, at every height —
/// the finality proof in `parent_qc` included, which is part of the hashed
/// header. Nothing in the node compares block hashes across validators (the
/// proposer audit compares state roots, which do not cover the proof), so
/// this is where a divergence in what gets sealed shows.
fn assert_same_blocks(nodes: &[Node]) {
    // The sealed heads: a block executed and waiting for its certificate
    // has no body yet.
    let sealed = |n: &Node| Meta::new(&n.db.durable_view()).head_height();
    let head = sealed(&nodes[0]);
    for n in &nodes[1..] {
        assert_eq!(sealed(n), head, "sealed heads differ");
    }
    for h in 1..=head {
        let ours = Blocks::new(&nodes[0].db).get(h).unwrap();
        let hash = Block::decode(&ours).unwrap().header.hash();
        for (i, n) in nodes.iter().enumerate().skip(1) {
            let theirs = Blocks::new(&n.db).get(h).unwrap();
            assert_eq!(
                Block::decode(&theirs).unwrap().header.hash(),
                hash,
                "validator {i} sealed a different block at height {h}"
            );
            assert_eq!(
                theirs, ours,
                "validator {i}: block bytes differ at height {h}"
            );
        }
    }
}

#[test]
fn four_validators_execute_the_same_blocks_and_an_observer_replays_them() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let mut now = 0u64;
    let mut nonce = 0u64;

    // Twelve rounds: the first eight carry three transfers each, then four
    // empty rounds so the last non-empty proposal commits.
    let mut round_budget = 12u64;
    settle(&mut nodes, &mut now, |_, _| {
        if round_budget == 0 {
            return None;
        }
        round_budget -= 1;
        if round_budget < 4 {
            return Some(Vec::new());
        }
        let txs = (0..3).map(|_| {
            let t = transfer(&alice_sk, nonce, bob);
            nonce += 1;
            t
        });
        Some(txs.collect())
    });

    let heads: Vec<(u64, H256, H256)> = nodes
        .iter()
        .map(|n| {
            let m = Meta::new(&n.db);
            (m.head_height(), m.head_hash(), m.state_root())
        })
        .collect();
    assert!(heads[0].0 >= 6, "too few blocks executed: {heads:?}");
    for h in &heads[1..] {
        assert_eq!(h, &heads[0], "validators disagree on the executed head");
    }
    assert_same_blocks(&nodes);
    // Every executed block carries a proof that verifies against the set.
    let set = ValidatorSet::new(
        Validators::new(&nodes[0].db)
            .list()
            .into_iter()
            .map(|v| v.addr)
            .collect(),
    )
    .unwrap();
    let g = genesis_id(&nodes[0].db);
    for h in 1..=heads[0].0 {
        let block = Block::decode(&Blocks::new(&nodes[0].db).get(h).unwrap()).unwrap();
        let fin = Finality::decode(&block.header.parent_qc).unwrap();
        fin.verify(&set, g).unwrap();
        assert_eq!(fin.header.payload_hash, block.header.tx_root);
    }

    // An observer with no part in consensus replays the blocks through the
    // proof path and reaches the same state.
    let set_infos = Validators::new(&nodes[0].db).list();
    let (_odir, observer) = open_db(&set_infos, alice);
    for h in 1..=heads[0].0 {
        let block = Block::decode(&Blocks::new(&nodes[0].db).get(h).unwrap()).unwrap();
        apply_remote_block(&observer, &ctx(), &block).expect("observer accepts a proven block");
    }
    let m = Meta::new(&observer);
    assert_eq!((m.head_height(), m.head_hash(), m.state_root()), heads[0]);
    assert_eq!(
        Accounts::new(&observer)
            .get(&bob)
            .unwrap()
            .available(AssetId::Usd),
        Accounts::new(&nodes[0].db)
            .get(&bob)
            .unwrap()
            .available(AssetId::Usd)
    );
}

#[test]
fn a_failing_transaction_stays_in_the_block_and_replays_identically() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let mut now = 0u64;

    // Nonce 0, then nonce 5 (a gap: refused), then nonce 1 in a later round.
    let mut round_budget = 8u64;
    settle(&mut nodes, &mut now, |_, _| {
        if round_budget == 0 {
            return None;
        }
        round_budget -= 1;
        Some(match round_budget {
            7 => vec![transfer(&alice_sk, 0, bob), transfer(&alice_sk, 5, bob)],
            6 => vec![transfer(&alice_sk, 1, bob)],
            _ => Vec::new(),
        })
    });
    let heads: Vec<(u64, H256)> = nodes
        .iter()
        .map(|n| {
            (
                Meta::new(&n.db).head_height(),
                Meta::new(&n.db).state_root(),
            )
        })
        .collect();
    assert_eq!(heads[0].0, 2);
    for h in &heads[1..] {
        assert_eq!(h, &heads[0]);
    }
    assert_same_blocks(&nodes);
    let block1 = Block::decode(&Blocks::new(&nodes[0].db).get(1).unwrap()).unwrap();
    assert_eq!(
        block1.txs.len(),
        2,
        "the refused transaction stays in the block"
    );
    assert_eq!(Accounts::new(&nodes[0].db).get(&alice).unwrap().nonce, 2);
    assert_eq!(
        block1.applied,
        vec![true, false],
        "the block says which of its transactions applied"
    );

    let set_infos = Validators::new(&nodes[0].db).list();
    let (_odir, observer) = open_db(&set_infos, alice);
    for h in 1..=2 {
        let block = Block::decode(&Blocks::new(&nodes[0].db).get(h).unwrap()).unwrap();
        apply_remote_block(&observer, &ctx(), &block).expect("observer skips the same failure");
    }
    assert_eq!(Meta::new(&observer).state_root(), heads[0].1);
}

#[test]
fn a_block_with_a_tampered_proof_is_refused() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let mut now = 0u64;
    let mut round_budget = 6u64;
    settle(&mut nodes, &mut now, |_, _| {
        if round_budget == 0 {
            return None;
        }
        round_budget -= 1;
        Some(if round_budget == 5 {
            vec![transfer(&alice_sk, 0, bob)]
        } else {
            Vec::new()
        })
    });
    let set_infos = Validators::new(&nodes[0].db).list();
    let block = Block::decode(&Blocks::new(&nodes[0].db).get(1).unwrap()).unwrap();

    // A vote dropped from the certificate: below quorum.
    let mut short = block.clone();
    let mut fin = Finality::decode(&short.header.parent_qc).unwrap();
    fin.cert.votes.pop();
    short.header.parent_qc = fin.encode();
    let (_d1, o1) = open_db(&set_infos, alice);
    let err = apply_remote_block(&o1, &ctx(), &short).unwrap_err();
    assert!(
        matches!(err, cb_net::SyncError::BadFinality { .. }),
        "{err}"
    );

    // A proof for a different payload.
    let mut other = block.clone();
    let mut fin = Finality::decode(&other.header.parent_qc).unwrap();
    fin.header.payload_hash = H256([7; 32]);
    other.header.parent_qc = fin.encode();
    let (_d2, o2) = open_db(&set_infos, alice);
    let err = apply_remote_block(&o2, &ctx(), &other).unwrap_err();
    assert!(
        matches!(err, cb_net::SyncError::BadFinality { .. }),
        "{err}"
    );

    // No proof at all.
    let mut bare = block.clone();
    bare.header.parent_qc.clear();
    let (_d3, o3) = open_db(&set_infos, alice);
    let err = apply_remote_block(&o3, &ctx(), &bare).unwrap_err();
    assert!(
        matches!(err, cb_net::SyncError::BadFinality { .. }),
        "{err}"
    );

    // And the untouched block is fine.
    let (_d4, o4) = open_db(&set_infos, alice);
    apply_remote_block(&o4, &ctx(), &block).unwrap();
}

#[test]
fn an_old_proof_replayed_as_the_next_block_is_refused() {
    // A peer takes block 1 — a genuine proposal with a genuine quorum
    // certificate — and serves it again as the block after an observer's
    // head, rewriting the unsigned header fields to fit. Every failing
    // transaction is skipped rather than refusing a block, so the replay
    // would execute cleanly; only the proof's height and its place on the
    // chain give it away.
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let mut now = 0u64;
    let mut nonce = 0u64;
    let mut round_budget = 10u64;
    settle(&mut nodes, &mut now, |_, _| {
        if round_budget == 0 {
            return None;
        }
        round_budget -= 1;
        if round_budget < 4 {
            return Some(Vec::new());
        }
        let t = transfer(&alice_sk, nonce, bob);
        nonce += 1;
        Some(vec![t])
    });
    let head = Meta::new(&nodes[0].db).head_height();
    assert!(head >= 4, "too few blocks executed: {head}");
    let block_at = |h: u64| Block::decode(&Blocks::new(&nodes[0].db).get(h).unwrap()).unwrap();

    // Every sealed block's proof names the block's own height.
    for h in 1..=head {
        let fin = Finality::decode(&block_at(h).header.parent_qc).unwrap();
        assert_eq!(fin.header.height, h);
    }

    let set_infos = Validators::new(&nodes[0].db).list();
    let (_odir, observer) = open_db(&set_infos, alice);
    for h in 1..head {
        apply_remote_block(&observer, &ctx(), &block_at(h)).unwrap();
    }
    let m = Meta::new(&observer);
    let (obs_height, obs_hash, obs_root) = (m.head_height(), m.head_hash(), m.state_root());

    for old in 1..head {
        let mut replay = block_at(old);
        replay.header.height = head;
        replay.header.prev_hash = obs_hash;
        replay.header.timestamp = block_at(head - 1).header.timestamp;
        let err = apply_remote_block(&observer, &ctx(), &replay).unwrap_err();
        assert!(
            matches!(err, cb_net::SyncError::BadFinality { .. }),
            "block {old} replayed at {head}: {err}"
        );
    }
    let m = Meta::new(&observer);
    assert_eq!(
        (m.head_height(), m.head_hash(), m.state_root()),
        (obs_height, obs_hash, obs_root),
        "a refused replay left the observer where it was"
    );

    // The real block still lands.
    apply_remote_block(&observer, &ctx(), &block_at(head)).unwrap();
    assert_eq!(
        Meta::new(&observer).head_hash(),
        Meta::new(&nodes[0].db).head_hash()
    );
}

/// Different witnesses for the same committed content keep block identity stable.
#[test]
fn audit_20260929_reordered_certificate_preserves_block_identity() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let (mut nodes, keys) = cluster(4, alice);
    let mut now = 0;
    let mut budget = 8u64;
    let mut nonce = 0;
    settle(&mut nodes, &mut now, |_, _| {
        if budget == 0 {
            return None;
        }
        budget -= 1;
        if budget < 4 {
            return Some(Vec::new());
        }
        let tx = transfer(&alice_sk, nonce, Address([0xbb; 20]));
        nonce += 1;
        Some(vec![tx])
    });
    let first = Block::decode(&Blocks::new(&nodes[0].db).get(1).unwrap()).unwrap();
    let second = Block::decode(&Blocks::new(&nodes[0].db).get(2).unwrap()).unwrap();
    let set_infos = Validators::new(&nodes[0].db).list();
    let original = Finality::decode(&first.header.parent_qc).unwrap();
    let mut reversed = original.clone();
    reversed.cert.votes.reverse();
    let missing = keys
        .iter()
        .find(|sk| {
            let pubkey = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), sk);
            let a = address_from_pubkey(&pubkey);
            !original.cert.votes.iter().any(|v| v.voter == a)
        })
        .unwrap();
    let address = address_from_pubkey(&cb_crypto::PublicKey::from_secret_key(
        &secp256k1::Secp256k1::new(),
        missing,
    ));
    let vote =
        cb_consensus::bft::Vote::sign(missing, address, original.cert.round, original.cert.id);
    let mut extended = original.clone();
    extended.cert.votes.push(vote.clone());
    let mut alternate = original.clone();
    alternate.cert.votes[0] = vote;
    for fin in [reversed, extended, alternate] {
        let (_dir, observer) = open_db(&set_infos, alice);
        let mut changed = first.clone();
        changed.header.parent_qc = fin.encode();
        assert_eq!(changed.header.hash(), first.header.hash());
        assert_eq!(changed.header.state_root, first.header.state_root);
        assert_eq!(changed.header.sequencer_sig, first.header.sequencer_sig);
        apply_remote_block(&observer, &ctx(), &changed).expect("valid alternative QC");
        apply_remote_block(&observer, &ctx(), &second).expect("honest successor still links");
        assert_eq!(Meta::new(&observer).head_hash(), second.header.hash());
    }
    let (_dir, observer) = open_db(&set_infos, alice);
    let mut changed = first;
    changed.header.timestamp += 1;
    assert!(matches!(
        apply_remote_block(&observer, &ctx(), &changed),
        Err(cb_net::SyncError::BadFinality { .. })
    ));
}

fn transfers(sk: &SecretKey, nonce: &mut u64, to: Address, n: usize) -> Vec<VerifiedTx> {
    (0..n)
        .map(|_| {
            let t = transfer(sk, *nonce, to);
            *nonce += 1;
            t
        })
        .collect()
}

/// `busy` rounds of transfers, then `idle` empty ones, then stop.
fn run_rounds(
    nodes: &mut [Node],
    now: &mut u64,
    sk: &SecretKey,
    nonce: &mut u64,
    to: Address,
    busy: u64,
    idle: u64,
) {
    let mut left = busy + idle;
    settle(nodes, now, |_, _| {
        if left == 0 {
            return None;
        }
        left -= 1;
        Some(if left >= idle {
            transfers(sk, nonce, to, 2)
        } else {
            Vec::new()
        })
    });
}

/// A validator killed with a block executed at commit and not yet sealed
/// loses nothing durable: its rows, body and head hash were never written.
/// It restarts from the last sealed block, fetches the proposal again, and
/// executes and seals it exactly as the others did.
#[test]
fn a_validator_killed_with_an_unsealed_block_rederives_it() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let (mut now, mut nonce) = (0u64, 0u64);
    // Busy to the end: the newest committed proposal is executed and its
    // certificate has not come yet.
    run_rounds(&mut nodes, &mut now, &alice_sk, &mut nonce, bob, 6, 0);

    let n3 = nodes.pop().unwrap();
    assert!(
        n3.writer.unsealed().is_some(),
        "no block waits for its certificate"
    );
    let visible = Meta::new(&n3.db).head_height();
    let durable = Meta::new(&n3.db.durable_view()).head_height();
    assert_eq!(
        visible,
        durable + 1,
        "the unsealed block is visible, not durable"
    );
    assert!(
        Blocks::new(&n3.db).get(visible).is_none(),
        "an unsealed block has no body to serve"
    );

    // Kill it: nothing it has not written survives.
    let voted = n3.engine.last_voted_round();
    let high = n3.engine.high_qc().clone();
    let Node {
        _dir: dir,
        db,
        engine,
        writer,
        ..
    } = n3;
    drop(writer);
    drop(engine);
    drop(db);
    let db = Database::open(dir.path()).unwrap();
    assert_eq!(Meta::new(&db).head_height(), durable);
    assert!(Blocks::new(&db).get(visible).is_none());

    // Restart from the sealed head, as the validator does.
    let set = ValidatorSet::new(
        Validators::new(&db)
            .list()
            .into_iter()
            .map(|v| v.addr)
            .collect(),
    )
    .unwrap();
    let (sk, pk) = keypair_from_seed(b"bft-exec-3");
    let me = address_from_pubkey(&pk);
    let g = genesis_id(&db);
    let root = cb_node::validator::root_from_db(&db);
    assert_eq!(root.height, durable);
    let mut engine = Engine::new(
        Config {
            base_timeout_ms: 1_000,
            max_timeout_ms: 8_000,
            retain_rounds: 64,
        },
        set,
        me,
        sk,
        g,
        root,
        payload_root,
        now,
    );
    engine.restore_safety(voted, high);
    let (writer, roots) = new_writer(&db);
    nodes.push(Node {
        _dir: dir,
        db,
        engine,
        writer,
        roots,
        executed: Vec::new(),
    });

    run_rounds(&mut nodes, &mut now, &alice_sk, &mut nonce, bob, 4, 4);
    let sealed = Meta::new(&nodes[3].db.durable_view()).head_height();
    assert!(
        sealed > visible,
        "the restarted validator did not get past {visible}"
    );
    assert_same_blocks(&nodes);
}

/// A catch-up by block sync while a block waits for its certificate: the
/// driver discards it first (what `ExecJob::Resync` does), and the block
/// the others sealed at that height lands in its place.
#[test]
fn a_catch_up_discards_the_unsealed_block_and_lands_the_sealed_one() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    let (mut now, mut nonce) = (0u64, 0u64);
    run_rounds(&mut nodes, &mut now, &alice_sk, &mut nonce, bob, 6, 0);

    let mut n3 = nodes.pop().unwrap();
    assert!(n3.writer.unsealed().is_some());
    let h = Meta::new(&n3.db).head_height();
    assert!(n3.roots.lock().unwrap().contains_key(&h));
    // What peers are told is the sealed head (the announcer reads the
    // durable view), and there is no body at `h` to serve them.
    assert_eq!(Meta::new(&n3.db.durable_view()).head_height(), h - 1);
    assert!(Blocks::new(&n3.db).get(h).is_none());

    n3.writer.reset();
    assert_eq!(n3.writer.unsealed(), None);
    assert_eq!(
        Meta::new(&n3.db).head_height(),
        h - 1,
        "the discarded rows are gone"
    );
    assert!(!n3.roots.lock().unwrap().contains_key(&h));

    // The other three carry on (three of four is a quorum) and seal it.
    run_rounds(&mut nodes, &mut now, &alice_sk, &mut nonce, bob, 4, 4);
    let top = Meta::new(&nodes[0].db.durable_view()).head_height();
    assert!(top > h);
    for k in h..=top {
        let block = Block::decode(&Blocks::new(&nodes[0].db).get(k).unwrap()).unwrap();
        apply_remote_block(&n3.db, &ctx(), &block).expect("a synced block lands");
    }
    nodes.push(n3);
    assert_same_blocks(&nodes);
}

/// The same with a writer thread per node, as the validator runs by
/// default: the thread waits at each block for its certificate and seals
/// in order, and every validator ends with the same sealed blocks.
#[test]
fn with_a_writer_thread_blocks_are_sealed_in_order_and_identical() {
    let (alice_sk, alice_pk) = generate_keypair();
    let alice = address_from_pubkey(&alice_pk);
    let bob = Address([0xbb; 20]);
    let (mut nodes, _keys) = cluster(4, alice);
    for n in &nodes {
        n.db.enable_async_writes().unwrap();
    }
    let (mut now, mut nonce) = (0u64, 0u64);
    run_rounds(&mut nodes, &mut now, &alice_sk, &mut nonce, bob, 12, 0);
    for n in &nodes {
        // Returns with the newest block still waiting for its certificate.
        n.db.flush_pending().unwrap();
        assert!(n.writer.unsealed().is_some());
        let visible = Meta::new(&n.db).head_height();
        assert_eq!(Meta::new(&n.db.durable_view()).head_height() + 1, visible);
    }
    assert!(Meta::new(&nodes[0].db.durable_view()).head_height() >= 8);
    assert_same_blocks(&nodes);
}
