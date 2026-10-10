//! Determinism through the real block builder, and independence from the
//! order transactions *arrived* in.
//!
//! `order_for_block` groups a batch by sender, sorts each group by nonce, and
//! orders the groups by `(rank of the group's first transaction,
//! keccak(parent hash ‖ sender))`. Nothing in that key is the position a
//! transaction had in the batch, so the design's claim is that arrival order
//! is erased: any permutation of the same batch builds the same block. (It is
//! leader-local — replicas execute the block's order as given — which is why
//! it is the leader's builder that is tested here.)
//!
//! `determinism.rs` replays fixed scenarios twice and `coincident_boundary.rs`
//! permutes one hand-built block. This is the random version, across spot,
//! perpetual, oracle, cancel and transfer traffic from many accounts over many
//! blocks, on three independent databases:
//!
//! * chain A builds each block from the batch as generated;
//! * chain B builds it from a fresh random shuffle of the same batch;
//! * chain C is a replica: it never builds anything, it re-executes A's
//!   blocks through `verify_and_commit`, which refuses a block whose root it
//!   cannot reproduce.
//!
//! Per block, A and B must produce the same state root *and* the same
//! transaction list in the same order, and C must accept A's block.

mod support;

use cb_block::{build_and_apply, verify_and_commit, BuildResult, ExecCtx};
use cb_state::{Database, Meta, Orders, PerpOrders};
use cb_tx::{TxBody, VerifiedTx};
use cb_types::{Address, AssetId, Tif};
use support::*;

fn genesis(chain: &Chain, traders: &[Actor]) {
    chain.list_spot(spot_market(1, 1, 10));
    let mut m = perp_market(PERP, chain.oracle.addr);
    // No funding boundary inside the run, so no block needs a system tick.
    m.funding_interval_blocks = 1_000_000;
    chain.list_perp(m);
    for t in traders {
        chain.fund_spot(t.addr, AssetId::Usd, 10u128.pow(18));
        chain.fund_spot(t.addr, AssetId::Btc, 1_000 * ONE_BTC);
        chain.fund_margin(t.addr, 1_000_000 * MICRO);
    }
}

fn ctx() -> ExecCtx {
    ExecCtx {
        treasury_signer: Address::ZERO,
        ..Default::default()
    }
}

fn build(db: &Database, txs: Vec<VerifiedTx>) -> BuildResult {
    build_and_apply(db, &ctx(), txs, 1_700_000_000, 0, Vec::new(), |_| {
        vec![0u8; 65]
    })
    .expect("block")
}

/// One block's batch, pre-signed with consecutive nonces per sender read
/// from `db` (all three chains agree on nonces, which is asserted).
fn batch(
    rng: &mut Rng,
    db: &Database,
    traders: &[Actor],
    oracle_actor: &Actor,
    height: u64,
) -> Vec<VerifiedTx> {
    let mut nonces: Vec<u64> = traders
        .iter()
        .map(|t| {
            cb_state::Accounts::new(db)
                .get(&t.addr)
                .map(|s| s.nonce)
                .unwrap_or(0)
        })
        .collect();
    let mut out = Vec::new();
    let oracle_nonce = cb_state::Accounts::new(db)
        .get(&oracle_actor.addr)
        .map(|s| s.nonce)
        .unwrap_or(0);
    let px = (50_000 + rng.range(0, 200) - 100) * Q8;
    out.push(sign(
        &oracle_actor.sk,
        oracle_nonce,
        oracle(PERP, px, height),
    ));
    let spot_orders = Orders::new(db).list_for_market(SPOT);
    let perp_orders = PerpOrders::new(db).list_for_market(&PERP);
    for _ in 0..(20 + rng.below(30)) {
        let i = rng.below(traders.len() as u64) as usize;
        let t = &traders[i];
        let body = match rng.below(10) {
            0 => match spot_orders.iter().find(|o| o.owner == t.addr) {
                Some(o) => TxBody::CancelOrder {
                    order_id: o.order_id,
                },
                None => TxBody::BatchCancel { market: SPOT },
            },
            1 => match perp_orders.iter().find(|o| o.owner == t.addr) {
                Some(o) => TxBody::CancelPerpOrder {
                    order_id: o.order_id,
                },
                None => continue,
            },
            2 => TxBody::Transfer {
                to: traders[(i + 1) % traders.len()].addr,
                asset: AssetId::Usd,
                amount: rng.range(1, 1_000),
            },
            3..=5 => spot_order(
                rng.side(),
                (50_000 + rng.range(0, 40) - 20) * Q8,
                rng.range(1, ONE_BTC / 10),
                if rng.chance(20) { Tif::Ioc } else { Tif::Gtc },
                rng.chance(20),
            ),
            _ => perp_order(
                PERP,
                rng.side(),
                (50_000 + rng.range(0, 40) - 20) * Q8,
                rng.range(1, ONE_BTC / 10),
                if rng.chance(20) { Tif::Ioc } else { Tif::Gtc },
                rng.chance(10),
                rng.chance(10),
            ),
        };
        out.push(sign(&t.sk, nonces[i], body));
        nonces[i] += 1;
    }
    out
}

#[test]
fn any_arrival_order_builds_the_same_block_and_a_replica_agrees() {
    for seed in 1..=3u64 {
        let traders: Vec<Actor> = (0..12).map(|i| actor(&format!("arrival-{i}"))).collect();
        let (a, b, c) = (Chain::new(), Chain::new(), Chain::new());
        for chain in [&a, &b, &c] {
            genesis(chain, &traders);
        }
        let mut rng = Rng::new(seed);
        let mut shuffle_rng = Rng::new(seed ^ 0x5555);
        let mut built = 0usize;
        for height in 1..=25u64 {
            let txs = batch(&mut rng, &a.db, &traders, &a.oracle, height);
            let mut shuffled = txs.clone();
            shuffle_rng.shuffle(&mut shuffled);

            let ra = build(&a.db, txs);
            let rb = build(&b.db, shuffled);
            assert_eq!(
                ra.state_root, rb.state_root,
                "seed {seed} h{height}: arrival order changed the root"
            );
            let ha: Vec<_> = ra.block.txs.iter().map(|t| t.hash()).collect();
            let hb: Vec<_> = rb.block.txs.iter().map(|t| t.hash()).collect();
            assert_eq!(
                ha, hb,
                "seed {seed} h{height}: arrival order changed the block's order"
            );
            assert_eq!(ra.rejected.len(), rb.rejected.len());
            built += ha.len();

            let replay: Vec<VerifiedTx> = ra
                .block
                .txs
                .iter()
                .cloned()
                .zip(ra.block.senders.iter().copied())
                .map(|(s, a)| VerifiedTx::from_parts_unchecked(s, a))
                .collect();
            let root = verify_and_commit(
                &c.db,
                &ctx(),
                &replay,
                ra.block.header.height,
                ra.block.header.hash(),
                &ra.block.encode(),
                ra.state_root,
                ra.block.header.timestamp,
                &[],
            )
            .unwrap_or_else(|e| panic!("seed {seed} h{height}: replica refused the block: {e}"));
            assert_eq!(root, ra.state_root);
            for t in &traders {
                assert_eq!(a.nonce_of(t.addr), b.nonce_of(t.addr));
                assert_eq!(a.nonce_of(t.addr), c.nonce_of(t.addr));
            }
        }
        assert_eq!(Meta::new(&a.db).head_height(), 25);
        assert_eq!(Meta::new(&a.db).state_root(), Meta::new(&b.db).state_root());
        assert_eq!(Meta::new(&a.db).state_root(), Meta::new(&c.db).state_root());
        assert!(
            built > 300,
            "seed {seed}: only {built} transactions made it into blocks"
        );
        // Both kinds of book actually hold something, so the comparison was
        // over non-trivial state.
        assert!(
            !Orders::new(&a.db).list_for_market(SPOT).is_empty()
                || !PerpOrders::new(&a.db).list_for_market(&PERP).is_empty()
        );
    }
}

/// The ordering key uses the parent hash, so the *same* batch built on two
/// chains whose heads differ may legitimately order accounts differently —
/// the seed is the point (core review L6: no permanent priority for a low
/// address). What must not differ is anything within one account: its
/// transactions stay contiguous and in nonce order whatever the seed.
#[test]
fn within_an_account_nonce_order_survives_every_seed() {
    let traders: Vec<Actor> = (0..6)
        .map(|i| actor(&format!("arrival-seed-{i}")))
        .collect();
    let mut rng = Rng::new(42);
    let db_chain = Chain::new();
    genesis(&db_chain, &traders);
    let txs = batch(&mut rng, &db_chain.db, &traders, &db_chain.oracle, 1);
    for s in 0..16u8 {
        let mut shuffled = txs.clone();
        Rng::new(s as u64 + 1).shuffle(&mut shuffled);
        let ordered = cb_block::order_for_block(shuffled, cb_types::H256([s; 32]));
        assert_eq!(ordered.len(), txs.len());
        for t in &traders {
            let idx: Vec<usize> = ordered
                .iter()
                .enumerate()
                .filter(|(_, v)| v.sender == t.addr)
                .map(|(i, _)| i)
                .collect();
            if idx.is_empty() {
                continue;
            }
            assert_eq!(
                idx.last().unwrap() - idx.first().unwrap() + 1,
                idx.len(),
                "seed {s}: an account's transactions were split"
            );
            let nonces: Vec<u64> = idx.iter().map(|i| ordered[*i].signed.tx.nonce).collect();
            assert!(
                nonces.windows(2).all(|w| w[1] == w[0] + 1),
                "seed {s}: nonces out of order {nonces:?}"
            );
        }
    }
}
