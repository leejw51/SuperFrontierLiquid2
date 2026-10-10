//! End-to-end integration test for `cb-client send`.
//!
//! Boots a real TCP-bound `SequencerServer` on a free localhost port, then
//! drives the *same* code path the CLI's `cmd_send` uses:
//!
//!   1. derive a wallet from `(mnemonic, index)` via `WalletEntry::derive`,
//!   2. `client::run_call` → `get_account` → read nonce,
//!   3. build + sign a Transfer tx with the wallet's secret_key,
//!   4. `client::submit_tx` over Cap'n Proto over TCP,
//!   5. assert `SubmitOutcome::Ok` and that the mempool now holds a
//!      `MempoolRecord` whose cached sender == derived address.
//!
//! This is the "cradle-to-mempool" path the user exercises with
//! `make client` → `send`. Block-application is intentionally NOT exercised
//! here — that's covered by `crates/cb-node/tests/e2e_inprocess.rs`. We're
//! verifying that the *client* signs correctly and submits a tx the server
//! accepts and persists with a recoverable sender.

use cb_client::client::{self, SubmitOutcome};
use cb_client::wallet::WalletEntry;
use cb_rpc::server::SequencerServer;
use cb_state::{AccountState, Accounts, Database, Mempool};
use cb_tx::{MempoolRecord, SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use tokio::sync::Notify;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const LETTER: &str =
    "letter advice cage absurd amount doctor acoustic avoid letter advice cage above";

/// Bind to 127.0.0.1:0, take the assigned port, drop the listener so the
/// real RPC server can re-bind. Tiny race window, mitigated below by polling.
fn pick_free_port() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

/// Spawn `cb_rpc::serve` in a background thread on `addr`. Polls for the
/// listener to come up before returning, so callers can connect without a
/// fixed sleep.
fn boot_server(db: Database) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let addr = pick_free_port();
    let notify = Arc::new(Notify::new());
    let server = SequencerServer::without_voting(db, notify);
    let handle = cb_rpc::client::run_local_serve(addr, server);

    // Wait up to 2s for the listener to accept connections.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            return (addr, handle);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("RPC server did not come up at {addr} within 2s");
}

#[test]
fn cb_client_signs_and_submits_transfer_via_tcp() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    // The transaction below is signed for chain 1, so the chain has to be
    // chain 1. `submitTx` checks this now: a transaction minted for another
    // chain can never execute here, and answering "accepted" to one was how a
    // client ended up holding a transaction hash for something that would
    // never happen.
    cb_state::Meta::new(&db).set_chain_id(1);

    // Same derivation path the CLI uses on `login`.
    let wallet = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    let alice = wallet.address().unwrap();
    let sk = wallet.secret_key().unwrap();

    // Pre-fund alice — mirrors `genesis.jsonl` validator wallet credit.
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 10_000).expect("seed balance");
        accts.put(&alice, &s);
    }
    let bob = Address([0xbb; 20]);
    let amount: u128 = 250;

    let db_for_check = db.clone();
    let (addr, _server_thread) = boot_server(db);

    // Drive the EXACT pattern that `repl::cmd_send` uses.
    let (tx_hash, used_nonce, outcome) = client::run_call(addr, move |c| async move {
        let st = client::get_account(&c, &alice).await?;
        assert_eq!(
            st.balance(AssetId::Usd),
            10_000,
            "pre-fund must be visible via getAccount",
        );
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to: bob,
                asset: AssetId::Usd,
                amount,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).expect("sign with wallet sk");
        // Sanity-check: the signed tx's recovered sender must equal the
        // wallet's stored address before we even put it on the wire.
        assert_eq!(
            signed.sender().expect("recover sender locally"),
            alice,
            "client-side signing must recover to wallet address",
        );
        let hash = signed.hash();
        let outcome = client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })
    .expect("run_call against booted server");

    match outcome {
        SubmitOutcome::Ok => {}
        SubmitOutcome::Rejected { code, message } => {
            panic!("submit_tx unexpectedly rejected: code={code} msg={message}");
        }
    }
    assert_eq!(used_nonce, 0);

    // Mempool persistence: the leader caches the recovered sender for the
    // hot path. We assert here that:
    //   - the mempool now has exactly our hash,
    //   - the persisted MempoolRecord decodes,
    //   - cached sender == alice (== derived address from the mnemonic).
    assert!(
        !Mempool::new(&db_for_check).is_empty(),
        "mempool must be non-empty after a successful submit",
    );
    let blob = Mempool::new(&db_for_check)
        .get(&tx_hash)
        .expect("mempool entry for our tx hash");
    let rec: MempoolRecord = bincode::deserialize(&blob).expect("decode MempoolRecord");
    assert_eq!(rec.signed.hash(), tx_hash);
    assert_eq!(
        rec.sender, alice,
        "server-side recovered sender must equal the wallet's derived address",
    );
    if let TxBody::Transfer {
        to,
        asset,
        amount: a,
    } = &rec.signed.tx.body
    {
        assert_eq!(*to, bob);
        assert_eq!(*asset, AssetId::Usd);
        assert_eq!(*a, amount);
    } else {
        panic!("persisted tx body must be a Transfer");
    }
}

#[test]
fn cb_client_send_zero_amount_is_rejected_by_server() {
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let wallet = WalletEntry::derive("alice", LEGAL, 0).unwrap();
    let alice = wallet.address().unwrap();
    let sk = wallet.secret_key().unwrap();
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Usd, 1_000).expect("seed balance");
        accts.put(&alice, &s);
    }
    let db_for_check = db.clone();
    let (addr, _h) = boot_server(db);

    let outcome = client::run_call(addr, move |c| async move {
        let st = client::get_account(&c, &alice).await?;
        let tx = Transaction {
            nonce: st.nonce,
            chain_id: 1,
            body: TxBody::Transfer {
                to: Address([0xbb; 20]),
                asset: AssetId::Usd,
                amount: 0, // rejected by validate_static
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).unwrap();
        let outcome = client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>(outcome)
    })
    .unwrap();

    match outcome {
        SubmitOutcome::Rejected { code, .. } => {
            assert_eq!(
                code, "invalidAmount",
                "zero-amount should map to invalidAmount"
            );
        }
        SubmitOutcome::Ok => panic!("zero-amount transfer must be rejected"),
    }

    assert!(
        Mempool::new(&db_for_check).is_empty(),
        "rejected tx must not land in mempool",
    );
}

#[test]
fn cb_client_get_account_returns_funded_balance() {
    // Pure read-path test — no signing. Confirms the same `get_account`
    // helper that `cmd_balance` uses surfaces the funded amount over TCP.
    let dir = tempdir().unwrap();
    let db = Database::open(dir.path()).unwrap();
    let wallet = WalletEntry::derive("alice", LETTER, 0).unwrap();
    let alice = wallet.address().unwrap();
    {
        let accts = Accounts::new(&db);
        let mut s = AccountState::default();
        s.add(AssetId::Btc, 42).expect("seed balance");
        s.add(AssetId::Eth, 1_000).expect("seed balance");
        s.add(AssetId::Usd, 999_999).expect("seed balance");
        s.nonce = 3;
        accts.put(&alice, &s);
    }
    let (addr, _h) = boot_server(db);

    let st = client::run_call(addr, move |c| async move {
        client::get_account(&c, &alice).await
    })
    .unwrap();

    assert_eq!(st.nonce, 3);
    assert_eq!(st.balance(AssetId::Btc), 42);
    assert_eq!(st.balance(AssetId::Eth), 1_000);
    assert_eq!(st.balance(AssetId::Usd), 999_999);
}
