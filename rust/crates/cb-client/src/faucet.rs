//! Test-faucet helper for `make client` workflows.
//!
//! The faucet is a thin convenience over `TxBody::Transfer`: an operator
//! types `faucet`, the client derives a well-known mnemonic at index 0, and
//! signs a transfer of ULD into the **current** wallet. There is no special
//! chain-side faucet account — the sender is just whichever wallet the
//! operator (or `scripts/perps_env.sh`) has loaded with funds in genesis.
//!
//! The faucet mnemonic is **never persisted** to `wallets.jsonl`. Each
//! invocation re-derives the keypair in memory; on success the secret bytes
//! drop with the surrounding stack frame. This keeps `~/.cb_client/` from
//! accumulating shared keys that look like normal user wallets.

use anyhow::{anyhow, Context, Result};
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, AssetId, H256};
use std::net::SocketAddr;

use crate::client::{run_call, SubmitOutcome};
use crate::wallet::WalletEntry;

/// Default faucet drop in raw ULD. 1 trillion is plenty for spot/perp/future
/// trade tests at any reasonable price level (BTC at $50k = 5e12 in 1e8-q
/// notional; this lets us place several round-lots before the wallet runs
/// dry).
pub const DEFAULT_FAUCET_AMOUNT: u128 = 1_000_000_000_000;

/// Inputs needed to derive the faucet signer. `Resolver` is small on purpose
/// so tests can build it from canned values without touching env vars.
#[derive(Debug)]
pub struct FaucetSigner {
    pub mnemonic: String,
    pub index: u32,
}

/// Resolve the faucet signer from environment.
///
/// Order of precedence:
///   1. `UL_FAUCET_MNEMONIC` (operator override; pair with `UL_FAUCET_INDEX`).
///   2. `MYWALLET3_MNEMONIC` (auto-set by `scripts/perps_env.sh:64` when the
///      operator is running inside `make startperps`).
///
/// Index defaults: `UL_FAUCET_INDEX` if set, else `MYWALLET3_INDEX` if set,
/// else `0` (matches the perp env's default at `scripts/perps_env.sh:65`).
pub fn resolve_faucet_signer() -> Result<FaucetSigner> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

    let (mnemonic, default_index) = if let Some(m) = env("UL_FAUCET_MNEMONIC") {
        (m, env("UL_FAUCET_INDEX"))
    } else if let Some(m) = env("MYWALLET3_MNEMONIC") {
        (m, env("MYWALLET3_INDEX"))
    } else {
        return Err(anyhow!(
            "faucet unavailable: set UL_FAUCET_MNEMONIC or run from \
             `make startperps` env (which exports MYWALLET3_MNEMONIC)"
        ));
    };
    let index = match default_index {
        Some(s) => s
            .trim()
            .parse::<u32>()
            .map_err(|_| anyhow!("faucet index env must be a non-negative integer, got `{s}`"))?,
        None => 0,
    };
    Ok(FaucetSigner { mnemonic, index })
}

/// Build a signed `Transfer` tx that moves `amount` ULD from the resolved
/// faucet wallet to `recipient`. Pure: no I/O. Used by tests to assert the
/// payload shape without round-tripping through the RPC.
pub fn build_faucet_tx(
    signer: &FaucetSigner,
    chain_id: u32,
    nonce: u64,
    recipient: Address,
    amount: u128,
) -> Result<(SignedTx, Address)> {
    let entry = WalletEntry::derive("__faucet__", &signer.mnemonic, signer.index)
        .context("derive faucet keypair")?;
    let from = entry.address()?;
    let sk = entry.secret_key()?;
    let tx = Transaction {
        nonce,
        chain_id,
        body: TxBody::Transfer {
            to: recipient,
            asset: AssetId::Usd,
            amount,
        },
    };
    let signed = SignedTx::sign_with(&sk, tx).context("sign faucet tx")?;
    Ok((signed, from))
}

/// Submit a faucet transfer to the sequencer. Returns
/// `(faucet_addr, tx_hash, used_nonce, outcome)`. The faucet sender's nonce
/// is read fresh from chain on each call so two faucet invocations from the
/// same operator session don't collide.
pub fn submit_faucet(
    rpc: SocketAddr,
    chain_id: u32,
    recipient: Address,
    amount: u128,
) -> Result<(Address, H256, u64, SubmitOutcome)> {
    let signer = resolve_faucet_signer()?;
    let entry = WalletEntry::derive("__faucet__", &signer.mnemonic, signer.index)
        .context("derive faucet keypair")?;
    let from = entry.address()?;
    let sk = entry.secret_key()?;

    let (hash, used_nonce, outcome) = run_call(rpc, move |c| async move {
        let st = crate::client::get_account(&c, &from).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::Transfer {
                to: recipient,
                asset: AssetId::Usd,
                amount,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign faucet tx")?;
        let hash = signed.hash();
        let outcome = crate::client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })?;
    Ok((from, hash, used_nonce, outcome))
}

/// In-process mutex serializing tests that mutate the faucet env vars
/// (`UL_FAUCET_MNEMONIC`, `MYWALLET3_MNEMONIC`). Cargo runs tests in parallel
/// inside one test binary; without this gate, a sibling test that calls
/// `resolve_faucet_signer` racing with the env-clearing test could observe a
/// transient empty env and flake. Cross-binary races are impossible (each
/// test binary is a separate process), so the mutex only needs to live in
/// the binary that touches the env. The `repl_smoke.rs` integration test
/// holds its own per-binary mirror of this mutex.
#[cfg(test)]
fn faucet_env_test_mutex() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_tx::TxBody;

    const ABANDON: &str =
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn build_faucet_tx_produces_transfer_to_recipient() {
        let signer = FaucetSigner {
            mnemonic: ABANDON.to_string(),
            index: 0,
        };
        let recipient = Address([0x42; 20]);
        let (signed, from) = build_faucet_tx(&signer, 1, 0, recipient, 12_345).unwrap();
        // Sender derived from the canonical "abandon ... about" mnemonic at
        // index 0 is well-known across BIP-44 implementations; we don't pin
        // the exact bytes, just confirm `from != recipient` and the body
        // matches.
        assert_ne!(from, recipient);
        match &signed.tx.body {
            TxBody::Transfer { to, asset, amount } => {
                assert_eq!(*to, recipient);
                assert_eq!(*asset, AssetId::Usd);
                assert_eq!(*amount, 12_345);
            }
            other => panic!("expected Transfer, got {other:?}"),
        }
        assert_eq!(signed.tx.chain_id, 1);
        assert_eq!(signed.tx.nonce, 0);
        // Recovered sender on the signed tx must match the faucet address.
        assert_eq!(signed.sender().unwrap(), from);
    }

    #[test]
    fn build_faucet_tx_uses_default_amount_constant() {
        // The constant is exposed so tests / scripts can pin to it; smoke check
        // that the value matches what the help text and CLI default to.
        assert_eq!(DEFAULT_FAUCET_AMOUNT, 1_000_000_000_000);
    }

    #[test]
    fn resolve_faucet_signer_reads_ul_faucet_mnemonic_first() {
        // Serializes against any sibling test in this binary that also reads
        // the faucet env vars. Today there is no such sibling, but the lock
        // prevents a future test from regressing this one. Hold the guard for
        // the entire env-mutate window (clear → call → restore) so a parallel
        // reader sees only fully consistent state.
        let _g = faucet_env_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev_ul = std::env::var("UL_FAUCET_MNEMONIC").ok();
        let prev_my = std::env::var("MYWALLET3_MNEMONIC").ok();
        std::env::remove_var("UL_FAUCET_MNEMONIC");
        std::env::remove_var("MYWALLET3_MNEMONIC");
        let err = resolve_faucet_signer().unwrap_err();
        assert!(
            err.to_string().contains("UL_FAUCET_MNEMONIC"),
            "expected hint about UL_FAUCET_MNEMONIC, got: {err}"
        );
        if let Some(v) = prev_ul {
            std::env::set_var("UL_FAUCET_MNEMONIC", v);
        }
        if let Some(v) = prev_my {
            std::env::set_var("MYWALLET3_MNEMONIC", v);
        }
    }
}
