//! The faucet endpoints.

use super::dto::*;
use super::Caller;
use super::Json;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::State;
use std::sync::Arc;

/// `GET /faucet`.
pub async fn status(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<FaucetStatusResponse>> {
    let status = state.faucet.status(&caller.user.user_id).await?;
    Ok(Json(FaucetStatusResponse {
        available: status.available,
        next_available_at: status.next_available_at.map(|at| at.to_rfc3339()),
        cooldown_seconds: status.cooldown_seconds,
        grant: status
            .grant
            .iter()
            .map(|g| AssetAmount {
                asset: g.asset.symbol().to_string(),
                amount: g.amount_string(),
            })
            .collect(),
        claims_made: status.claims_made,
    }))
}

/// `POST /faucet/claim`.
pub async fn claim(
    State(state): State<Arc<AppState>>,
    caller: Caller,
) -> ApiResult<Json<FaucetClaimResponse>> {
    let address = caller.address()?;
    // Prime the account mirror BEFORE the deposit lands. Without a baseline
    // the watcher's first sighting of this account is the block carrying the
    // grant, which it seeds silently rather than publishing — so a brand-new
    // user's first claim never reached their browser as a balance update, and
    // the equity in the top bar sat at zero until a reload. The order and
    // transfer handlers already do this; the faucet is the first transaction
    // most accounts ever make.
    state
        .prime_account_mirror(&caller.user.user_id, address)
        .await;
    let receipt = state.faucet.claim(&caller.user.user_id, address).await?;
    Ok(Json(FaucetClaimResponse {
        claim_id: receipt.claim_id,
        tx_hash: receipt.tx_hash,
        granted: receipt
            .granted
            .iter()
            .map(|g| AssetAmount {
                asset: g.asset.symbol().to_string(),
                amount: g.amount_string(),
            })
            .collect(),
        next_available_at: receipt.next_available_at.to_rfc3339(),
    }))
}
