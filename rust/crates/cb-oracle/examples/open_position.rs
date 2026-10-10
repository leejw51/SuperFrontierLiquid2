//! Manual verification scaffolding: open a real position against a running node.
//!
//! Not part of the crate's behaviour — it exists so `docs/runbook.md`'s claim
//! that a derivative market is tradable once the oracle runs can be checked
//! rather than asserted. Run it against a node the oracle is publishing to:
//!
//! ```text
//! cargo run --release -p cb-oracle --example open_position -- 127.0.0.1:7770
//! ```

use anyhow::{anyhow, Context, Result};
use cb_crypto::{address_from_pubkey, keypair_from_seed};
use cb_rpc::conv;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, DerivId, Side, Tif, H256};
use std::net::SocketAddr;

fn main() -> Result<()> {
    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7770".into())
        .parse()?;
    let chain_id: u32 = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "1000".into())
        .parse()?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        let client = cb_rpc::connect(addr).await?;

        let (sk0, pk0) = keypair_from_seed(b"cb-demo-trader-0");
        let (sk1, pk1) = keypair_from_seed(b"cb-demo-trader-1");
        let a0 = address_from_pubkey(&pk0);
        let a1 = address_from_pubkey(&pk1);
        println!("demo trader 0: 0x{}", hex::encode(a0.0));
        println!("demo trader 1: 0x{}", hex::encode(a1.0));

        let market = DerivId::perp(0);

        let mut req = client.cap.get_mark_price_request();
        conv::deriv_id_to_capnp(req.get().init_market(), &market);
        let resp = req
            .send()
            .promise
            .await
            .context("the market has no mark price — is the oracle running?")?;
        let mark = conv::u128_from_capnp(resp.get()?.get_mark()?.get_px_q8()?);
        println!("mark px_q8: {mark}");

        // Tick size on btc_perp is 100 at the q8 scale.
        let price = (mark / 100) * 100;
        let qty: u128 = 1_000_000; // 0.01 BTC at the 1e8 contract scale.

        for (sk, addr_of, side, label) in [
            (sk0, a0, Side::Sell, "maker sell"),
            (sk1, a1, Side::Buy, "taker buy"),
        ] {
            let mut req = client.cap.get_account_request();
            conv::address_to_capnp(req.get().init_addr(), &addr_of);
            let nonce = req.send().promise.await?.get()?.get_state()?.get_nonce();

            let tx = Transaction {
                nonce,
                chain_id,
                body: TxBody::PlacePerpOrder {
                    market,
                    side,
                    price_q8: price,
                    qty,
                    tif: Tif::Gtc,
                    post_only: false,
                    reduce_only: false,
                    client_order_id: client_order_id(&addr_of, nonce),
                },
            };
            let signed = SignedTx::sign_with(&sk, tx)?;
            let mut req = client.cap.submit_tx_request();
            conv::signed_tx_to_capnp(req.get().init_tx(), &signed);
            let resp = req.send().promise.await?;
            match resp.get()?.get_result()?.which()? {
                cb_schema::rpc_capnp::submit_result::Ok(()) => {
                    println!("{label}: accepted (nonce {nonce}, px {price}, qty {qty})")
                }
                cb_schema::rpc_capnp::submit_result::Rejected(r) => {
                    let r = r?;
                    return Err(anyhow!(
                        "{label} rejected: {:?} {}",
                        r.get_code()?,
                        r.get_message()?.to_str()?
                    ));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        for (who, addr_of) in [("trader 0", a0), ("trader 1", a1)] {
            let mut req = client.cap.get_positions_request();
            conv::address_to_capnp(req.get().init_addr(), &addr_of);
            let resp = req.send().promise.await?;
            let positions = resp.get()?.get_positions()?;
            println!("{who}: {} position(s)", positions.len());
            for i in 0..positions.len() {
                let p = conv::position_from_capnp(positions.get(i))?;
                println!(
                    "  {} {:?} qty={} entry_px_q8={}",
                    p.market, p.side, p.qty, p.entry_px_q8
                );
            }
        }

        Ok::<_, anyhow::Error>(())
    }))
}

fn client_order_id(addr: &Address, nonce: u64) -> H256 {
    let mut material = addr.0.to_vec();
    material.extend_from_slice(&nonce.to_le_bytes());
    cb_crypto::keccak256(&material)
}
