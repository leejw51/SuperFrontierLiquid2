//! Async RPC wrappers around `cb_rpc::client::connect`.
//!
//! Every helper here is `LocalSet`-aware: the Cap'n Proto client futures are
//! `!Send` (they hold raw pointers into the rpc system), so callers must
//! drive them on a `tokio::task::LocalSet`. The convenience `run_call`
//! function spins up a single-thread runtime + LocalSet for one RPC and
//! returns the result. We don't keep a long-lived connection across REPL
//! turns in v1 — connection setup against a local sequencer is fast enough
//! and the simpler lifecycle avoids worrying about a stale TCP stream
//! interleaving with `set-rpc`.

use anyhow::{anyhow, Context, Result};
use cb_rpc::conv::{address_to_capnp, signed_tx_to_capnp};
use cb_schema::rpc_capnp::{submit_result, SubmitErrorCode};
use cb_state::AccountState;
use cb_tx::SignedTx;
use cb_types::Address;
use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use tokio::task::LocalSet;

/// Outcome of a `submit_tx` RPC.
#[derive(Clone, Debug)]
pub enum SubmitOutcome {
    Ok,
    Rejected { code: String, message: String },
}

/// Run a single async RPC call inside a fresh single-thread runtime + LocalSet.
/// Caller closure receives the connected `Client`.
pub fn run_call<F, Fut, T>(addr: SocketAddr, f: F) -> Result<T>
where
    F: FnOnce(cb_rpc::Client) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build single-thread runtime")?;
    let local = LocalSet::new();
    rt.block_on(local.run_until(async move {
        let client = cb_rpc::connect(addr)
            .await
            .with_context(|| format!("connect to sequencer at {addr}"))?;
        f(client).await
    }))
}

/// Fetch `AccountState` for `addr`. Decodes the capnp response into the same
/// `cb_state::AccountState` shape that the engine uses internally so the
/// formatting layer doesn't need to know about capnp.
pub async fn get_account(client: &cb_rpc::Client, addr: &Address) -> Result<AccountState> {
    let mut req = client.cap.get_account_request();
    address_to_capnp(req.get().init_addr(), addr);
    let resp = req.send().promise.await.context("get_account RPC")?;
    let state = resp.get()?.get_state()?;
    let nonce = state.get_nonce();
    let mut balances: BTreeMap<u8, u128> = BTreeMap::new();
    let bals = state.get_balances()?;
    for i in 0..bals.len() {
        let e = bals.get(i);
        let asset = e.get_asset()? as u8;
        let amt = e.get_amount()?;
        balances.insert(asset, cb_rpc::conv::u128_from_capnp(amt));
    }
    let mut locked: BTreeMap<u8, u128> = BTreeMap::new();
    let locks = state.get_locked()?;
    for i in 0..locks.len() {
        let e = locks.get(i);
        let asset = e.get_asset()? as u8;
        let amt = e.get_amount()?;
        locked.insert(asset, cb_rpc::conv::u128_from_capnp(amt));
    }
    Ok(AccountState {
        nonce,
        balances,
        locked,
    })
}

/// Fetch chain-level metadata: id and the user-facing native denom string
/// (e.g. "ULD"). Best-effort; callers may treat a failure as "denom unknown"
/// since the value is purely cosmetic.
pub async fn chain_info(client: &cb_rpc::Client) -> Result<(u32, String)> {
    let req = client.cap.chain_info_request();
    let resp = req.send().promise.await.context("chain_info RPC")?;
    let info = resp.get()?.get_info()?;
    let chain_id = info.get_chain_id();
    let denom = info.get_denom()?.to_str()?.to_string();
    Ok((chain_id, denom))
}

/// Submit a SignedTx; map a `Rejected` response to a structured outcome.
pub async fn submit_tx(client: &cb_rpc::Client, signed: &SignedTx) -> Result<SubmitOutcome> {
    let mut req = client.cap.submit_tx_request();
    signed_tx_to_capnp(req.get().init_tx(), signed);
    let resp = req.send().promise.await.context("submit_tx RPC")?;
    let result = resp.get()?.get_result()?;
    match result.which()? {
        submit_result::Ok(()) => Ok(SubmitOutcome::Ok),
        submit_result::Rejected(rej) => {
            let r = rej?;
            let code = r
                .get_code()
                .map(submit_error_code_to_str)
                .unwrap_or("unknownCode");
            let message = r.get_message()?.to_string()?;
            Ok(SubmitOutcome::Rejected {
                code: code.to_string(),
                message,
            })
        }
    }
}

fn submit_error_code_to_str(c: SubmitErrorCode) -> &'static str {
    use SubmitErrorCode as E;
    match c {
        E::BadSignature => "badSignature",
        E::BadNonce => "badNonce",
        E::InsufficientBalance => "insufficientBalance",
        E::PostOnlyWouldCross => "postOnlyWouldCross",
        E::UnknownMarket => "unknownMarket",
        E::InvalidAmount => "invalidAmount",
        E::NotAuthorized => "notAuthorized",
        E::MempoolFull => "mempoolFull",
        E::Internal => "internal",
    }
}

/// Helper used by `Send`: parse a `0x...` (or bare hex) address.
pub fn parse_address(s: &str) -> Result<Address> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).context("decode address hex")?;
    if bytes.len() != 20 {
        return Err(anyhow!("address must be 20 bytes (40 hex chars)"));
    }
    let mut a = [0u8; 20];
    a.copy_from_slice(&bytes);
    Ok(Address(a))
}

/// Helper used by `Send`: parse the asset name.
pub fn parse_asset(s: &str) -> Result<cb_types::AssetId> {
    // `uld` is the project's canonical native-denom name (Meta::denom default
    // is "USD"; the production chain sets it to "ULD" via ${MYDENOM} in
    // genesis). Both names map to AssetId::Usd on the wire — there is no
    // separate ULD asset enum value. Custom denoms set in MYDENOM that don't
    // match this list still need to be sent as `usd`.
    match s.to_ascii_lowercase().as_str() {
        "btc" => Ok(cb_types::AssetId::Btc),
        "eth" => Ok(cb_types::AssetId::Eth),
        "usd" | "uld" => Ok(cb_types::AssetId::Usd),
        other => Err(anyhow!(
            "unknown asset '{other}' (expected btc|eth|usd|uld)"
        )),
    }
}
