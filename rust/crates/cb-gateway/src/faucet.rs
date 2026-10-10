//! The faucet: the only place new money enters the simulation.
//!
//! ## It is a real transaction, not a database row
//!
//! A grant is a `TxBody::Deposit` signed by the chain's treasury key and
//! executed by the sequencer, so the credited balance is consensus state like
//! any other. Recording a grant in SQLite and calling it done would give the
//! user a number the chain has never heard of, and every invariant in
//! `docs/PLAN.md` §6 — conservation above all — would be checking the wrong
//! ledger. `FaucetRepository` records what was granted; it does not *do* the
//! granting.
//!
//! `Deposit` is the only minting transaction, and the executor refuses it from
//! anyone but the configured treasury signer
//! (`cb_block::executor` → `ExecError::NotAuthorized`). A gateway configured
//! with the wrong key finds out on the first claim, not silently.
//!
//! ## The double-claim race
//!
//! Check-cooldown-then-grant is a read followed by a write, and two requests
//! that interleave between them both see "available" and both grant. The
//! cooldown is therefore enforced under a **per-user lock held across the
//! whole claim** — read, submit, record — so the second request finds the
//! first one's grant already written and is refused with `429`.
//!
//! The lock is per user, not global: one player's claim must not queue behind
//! another's. The treasury *nonce* is a separate problem, and it belongs to
//! [`crate::signer::TxSubmitter`], which serialises per signing address —
//! which is exactly what the one shared treasury key needs.

use crate::decimal::format_scaled;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::signer::TxSubmitter;
use crate::symbols::asset_display_decimals;
use cb_crypto::SecretKey;
use cb_ports::{AppDatabase, Clock, FaucetClaim};
use cb_tx::TxBody;
use cb_types::{Address, AssetId};
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde_json::json;
use std::sync::Arc;

/// One asset the faucet hands out, and how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// Asset granted.
    pub asset: AssetId,
    /// Amount in that asset's base units.
    pub amount: u128,
}

impl Grant {
    /// The amount as the contract's decimal string.
    pub fn amount_string(&self) -> String {
        format_scaled(
            self.amount,
            self.asset.decimals() as u32,
            asset_display_decimals(self.asset),
        )
    }
}

/// How the faucet is configured.
#[derive(Debug, Clone)]
pub struct FaucetConfig {
    /// Whether the faucet is open at all.
    pub enabled: bool,
    /// Seconds a user must wait between claims.
    pub cooldown_seconds: u64,
    /// What one claim grants.
    pub grants: Vec<Grant>,
    /// How many claims one account may ever make. `None` is unlimited.
    pub max_claims_per_user: Option<u64>,
}

impl Default for FaucetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_claims_per_user: None,
            cooldown_seconds: 3_600,
            grants: vec![
                Grant {
                    asset: AssetId::Usd,
                    amount: 100_000_000_000, // 100,000.00 USD
                },
                Grant {
                    asset: AssetId::Btc,
                    amount: 100_000_000, // 1.00000000 BTC
                },
            ],
        }
    }
}

/// What `GET /faucet` reports.
#[derive(Debug, Clone)]
pub struct FaucetStatus {
    /// Whether a claim would succeed now.
    pub available: bool,
    /// When the next claim becomes possible, if not now.
    pub next_available_at: Option<DateTime<Utc>>,
    /// The configured cooldown.
    pub cooldown_seconds: u64,
    /// What a claim grants.
    pub grant: Vec<Grant>,
    /// How many claims this user has made.
    pub claims_made: u64,
}

/// What a successful claim produced.
#[derive(Debug, Clone)]
pub struct ClaimReceipt {
    /// Stable claim id.
    pub claim_id: String,
    /// Hash of the transaction that credited the account.
    pub tx_hash: String,
    /// What was granted.
    pub granted: Vec<Grant>,
    /// When the next claim becomes possible.
    pub next_available_at: DateTime<Utc>,
}

/// The faucet.
pub struct FaucetService {
    db: Arc<dyn AppDatabase>,
    submitter: Arc<TxSubmitter>,
    clock: Arc<dyn Clock>,
    /// Mutable at runtime: `PATCH /admin/faucet` changes it without a restart.
    config: parking_lot::RwLock<FaucetConfig>,
    treasury_key: SecretKey,
    treasury_address: Address,
    /// One lock per user, held across the whole claim. See the module docs.
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl std::fmt::Debug for FaucetService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaucetService")
            .field("cooldown_seconds", &self.config.read().cooldown_seconds)
            .field("treasury", &hex::encode(self.treasury_address.0))
            .finish_non_exhaustive()
    }
}

impl FaucetService {
    /// Build the faucet.
    pub fn new(
        db: Arc<dyn AppDatabase>,
        submitter: Arc<TxSubmitter>,
        clock: Arc<dyn Clock>,
        config: FaucetConfig,
        treasury_key: SecretKey,
    ) -> Self {
        let public =
            cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &treasury_key);
        let treasury_address = cb_crypto::address_from_pubkey(&public);
        Self {
            db,
            submitter,
            clock,
            config: parking_lot::RwLock::new(config),
            treasury_key,
            treasury_address,
            locks: DashMap::new(),
        }
    }

    /// The address the faucet mints from. Useful for `/health` and operators.
    pub fn treasury_address(&self) -> Address {
        self.treasury_address
    }

    /// The current configuration.
    pub fn config(&self) -> FaucetConfig {
        self.config.read().clone()
    }

    /// Replace the configuration, as `PATCH /admin/faucet` does.
    pub fn reconfigure(&self, config: FaucetConfig) {
        *self.config.write() = config;
    }

    /// Whether this user may claim, and what they would get.
    pub async fn status(&self, user_id: &str) -> ApiResult<FaucetStatus> {
        let config = self.config();
        let now = self.clock.now();
        let last = self.db.faucet().last_claim_at(user_id).await?;
        let claims_made = self.db.faucet().claim_count(user_id).await?;
        let next = last.map(|at| at + self.cooldown());
        let within_quota = config
            .max_claims_per_user
            .is_none_or(|max| claims_made < max);
        let available = config.enabled && within_quota && next.is_none_or(|next| next <= now);
        Ok(FaucetStatus {
            available,
            next_available_at: next.filter(|_| !available),
            cooldown_seconds: config.cooldown_seconds,
            grant: config.grants.clone(),
            claims_made,
        })
    }

    /// Claim, if the cooldown allows.
    pub async fn claim(&self, user_id: &str, address: Address) -> ApiResult<ClaimReceipt> {
        // Held for the whole claim. Two requests from one user serialise here,
        // and the second one then reads the first one's recorded grant.
        let lock = Arc::clone(self.locks.entry(user_id.to_string()).or_default().value());
        let _guard = lock.lock().await;

        let config = self.config();
        if !config.enabled {
            return Err(ApiError::new(ErrorCode::Forbidden, "the faucet is closed"));
        }
        if let Some(max) = config.max_claims_per_user {
            if self.db.faucet().claim_count(user_id).await? >= max {
                return Err(ApiError::new(
                    ErrorCode::FaucetCooldown,
                    format!("this account has used all {max} of its faucet claims"),
                ));
            }
        }

        let now = self.clock.now();
        if let Some(last) = self.db.faucet().last_claim_at(user_id).await? {
            let next = last + self.cooldown();
            if next > now {
                let wait = (next - now).num_seconds().max(0);
                return Err(ApiError::new(
                    ErrorCode::FaucetCooldown,
                    format!("the faucet is on cooldown for another {wait} seconds"),
                )
                .with_details(json!({
                    "next_available_at": next.to_rfc3339(),
                    "cooldown_seconds": config.cooldown_seconds,
                    "retry_after_seconds": wait,
                })));
            }
        }

        self.mint(user_id, address, &config.grants, now, "faucet claim")
            .await
    }

    /// Grant assets outside the cooldown, as `POST /admin/faucet/grant` does.
    ///
    /// The cooldown is a fairness rule for players, not a safety property, so
    /// an operator may bypass it. The grant is recorded in the same ledger and
    /// the same faucet history as any other, which is what keeps a
    /// conservation audit honest.
    pub async fn grant(
        &self,
        user_id: &str,
        address: Address,
        grants: &[Grant],
        reason: &str,
    ) -> ApiResult<ClaimReceipt> {
        if grants.is_empty() {
            return Err(ApiError::validation("a grant must name at least one asset"));
        }
        let lock = Arc::clone(self.locks.entry(user_id.to_string()).or_default().value());
        let _guard = lock.lock().await;
        let now = self.clock.now();
        self.mint(user_id, address, grants, now, reason).await
    }

    /// Record the grant, then mint it on chain.
    ///
    /// **The record comes first, and that ordering is the point.** It used to
    /// be the other way round, with a comment explaining that recording after
    /// the mints meant "a rejected deposit never starts a cooldown the user got
    /// nothing for" — sound for the failure it named and wide open to the
    /// opposite one (§11 of the flow review). A grant is two assets, so a
    /// failure between the first `Deposit` and the last, or a gateway that dies
    /// between the mints and the writes, left the user holding minted money
    /// with **no claim row and therefore no cooldown**. They claim again. And
    /// again. `Deposit` is not idempotent on chain — the executor discards
    /// `external_tx_ref` — so nothing downstream stops it.
    ///
    /// Record-first can cost at most one cooldown window, for a user whose mint
    /// then failed. Mint-first can cost unbounded money, in the one ledger the
    /// conservation checks reconcile against. The trade is not close.
    ///
    /// The claim row therefore carries no transaction hash: there is no hash
    /// yet when it is written, and `FaucetRepository` has no update. The hash
    /// reaches the user's ledger from the block that carried the deposit, where
    /// `crate::watcher::record_chain_ledger` writes the `faucet` entry with the
    /// claim id in its memo — which is also why this function writes no ledger
    /// entry itself. `submit` returns on *mempool acceptance*, and a ledger
    /// that records mempool acceptance as money received is a ledger that lies
    /// whenever a transaction dies at block build.
    async fn mint(
        &self,
        user_id: &str,
        address: Address,
        grants: &[Grant],
        now: DateTime<Utc>,
        memo: &str,
    ) -> ApiResult<ClaimReceipt> {
        let claim_id = format!("clm_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);

        // Every amount converted before anything is written. A grant whose
        // amount the ledger cannot represent used to be recorded as zero and
        // then minted in full, which breaks the one reconciliation
        // `total_granted` exists for: refuse it before a row or a mint exists.
        let mut amounts = Vec::with_capacity(grants.len());
        for grant in grants {
            let amount =
                crate::decimal::to_port_decimal(grant.amount, grant.asset.decimals() as u32)
                    .ok_or_else(|| {
                        ApiError::validation(format!(
                            "a grant of {} base units of {} is too large to record",
                            grant.amount,
                            grant.asset.symbol()
                        ))
                    })?;
            amounts.push(amount);
        }

        // 1. The record, and with it the cooldown.
        for (grant, amount) in grants.iter().zip(amounts) {
            self.db
                .faucet()
                .record_claim(&FaucetClaim {
                    claim_id: claim_id.clone(),
                    user_id: user_id.to_string(),
                    asset: grant.asset.symbol().to_string(),
                    amount,
                    // Filled in by nobody: see the function docs.
                    tx_hash: String::new(),
                    claimed_at: now,
                })
                .await?;
        }

        // 2. The mints.
        let mut first_hash = None;
        let mut granted = Vec::new();
        for grant in grants {
            let body = TxBody::Deposit {
                user: address,
                asset: grant.asset,
                amount: grant.amount,
                // The claim id ties the on-chain mint back to the off-chain
                // grant record, which is what a conservation audit needs.
                external_tx_ref: claim_id.clone(),
            };
            let hash = match self
                .submitter
                .submit(&self.treasury_key, self.treasury_address, body)
                .await
            {
                Ok(hash) => hash,
                Err(e) => {
                    // The claim is recorded and the cooldown has started. Say
                    // so, loudly and with the claim id, because the operator is
                    // the only one who can hand out the difference.
                    tracing::error!(
                        target: "cb_gateway::faucet",
                        claim = %claim_id,
                        user = user_id,
                        asset = %grant.asset.symbol(),
                        granted = granted.len(),
                        error = %e,
                        "a recorded faucet claim failed to mint; the cooldown has started \
                         and the user is short of what the claim says they were given"
                    );
                    return Err(
                        annotate_treasury_error(e, self.treasury_address).with_details(json!({
                            "claim_id": claim_id,
                            "assets_minted": granted.len(),
                            "cooldown_started": true,
                        })),
                    );
                }
            };
            let hash_hex = format!("0x{}", hex::encode(hash.0));
            if first_hash.is_none() {
                first_hash = Some(hash_hex);
            }
            granted.push(*grant);
        }

        // The reason a grant was made is the operator's, and it is kept in the
        // audit trail by the handler that supplied it. There is nowhere on the
        // chain to put it and no claim column for it, so it is logged here
        // beside the claim id rather than silently dropped.
        tracing::info!(
            target: "cb_gateway::faucet",
            claim = %claim_id,
            user = user_id,
            reason = memo,
            assets = granted.len(),
            "faucet grant minted"
        );

        Ok(ClaimReceipt {
            claim_id,
            tx_hash: first_hash.unwrap_or_default(),
            granted,
            next_available_at: now + self.cooldown(),
        })
    }

    /// What the faucet account has left to give.
    ///
    /// The *available* balance, not the total: anything locked against a
    /// resting order cannot be granted, and an operator reading "the faucet has
    /// X left" is asking what it can still hand out.
    pub async fn authority_balance(
        &self,
        node: &dyn crate::node::NodeClient,
    ) -> ApiResult<Vec<(cb_types::AssetId, u128)>> {
        let account = node.account(self.treasury_address).await?;
        Ok(account
            .balances
            .keys()
            .map(|asset| (*asset, account.available(*asset)))
            .collect())
    }

    fn cooldown(&self) -> Duration {
        Duration::seconds(self.config.read().cooldown_seconds as i64)
    }
}

/// Make the "wrong treasury key" misconfiguration obvious.
fn annotate_treasury_error(error: ApiError, treasury: Address) -> ApiError {
    if error.code == ErrorCode::Forbidden {
        return ApiError::internal(format!(
            "the faucet's configured treasury address 0x{} is not the chain's \
             authorised deposit signer; check --faucet-treasury-key against genesis",
            hex::encode(treasury.0)
        ));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryDatabase;
    use crate::node::{MockNode, NodeClient, SubmitOutcome, SubmitRejection};
    use cb_ports::FixedClock;

    struct Harness {
        node: Arc<MockNode>,
        faucet: Arc<FaucetService>,
        clock: FixedClock,
        db: Arc<InMemoryDatabase>,
    }

    fn harness() -> Harness {
        let node = Arc::new(MockNode::new());
        let db = InMemoryDatabase::shared();
        let clock = FixedClock::new(
            DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let submitter = Arc::new(TxSubmitter::new(node.clone()));
        let (treasury_key, _public) = cb_crypto::generate_keypair();
        let faucet = Arc::new(FaucetService::new(
            db.clone(),
            submitter,
            Arc::new(clock.clone()),
            FaucetConfig::default(),
            treasury_key,
        ));
        Harness {
            node,
            faucet,
            clock,
            db,
        }
    }

    fn deposits(node: &MockNode) -> Vec<(AssetId, u128, Address)> {
        node.submitted_bodies()
            .into_iter()
            .filter_map(|body| match body {
                TxBody::Deposit {
                    user,
                    asset,
                    amount,
                    ..
                } => Some((asset, amount, user)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_claim_is_a_real_on_chain_deposit_from_the_treasury() {
        let h = harness();
        let user_address = Address([7u8; 20]);
        let receipt = h
            .faucet
            .claim("usr_a", user_address)
            .await
            .expect("granted");

        let sent = deposits(&h.node);
        assert_eq!(sent.len(), 2, "one Deposit per configured asset");
        assert!(sent.iter().all(|(_, _, user)| *user == user_address));
        assert!(sent
            .iter()
            .any(|(asset, amount, _)| *asset == AssetId::Usd && *amount == 100_000_000_000));
        assert!(receipt.tx_hash.starts_with("0x"));

        // Every deposit must be signed by the treasury, not the user.
        let treasury = h.faucet.treasury_address();
        for tx in h.node.submitted() {
            assert_eq!(tx.sender().expect("recoverable"), treasury);
        }
    }

    #[tokio::test]
    async fn a_grant_is_recorded_in_the_faucet_history_and_not_yet_in_the_ledger() {
        // `submit` returns on mempool acceptance, which is not the chain having
        // credited anybody. The user's ledger — what `GET /account/transfers`
        // and the activity feed read — is written by the watcher when the block
        // carrying the deposit arrives, with the height and the balance the
        // chain reports rather than ones the gateway assumed (§11 and §20 of
        // the flow review).
        let h = harness();
        h.faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect("granted");
        assert_eq!(h.db.faucet().claim_count("usr_a").await.unwrap(), 1);
        assert_eq!(h.db.faucet().history("usr_a", 10).await.unwrap().len(), 2);
        assert!(
            h.db.ledger()
                .for_user("usr_a", None, 10)
                .await
                .unwrap()
                .is_empty(),
            "nothing is in the ledger until a block says the deposit applied"
        );
        let totals = h.db.faucet().total_granted().await.unwrap();
        assert_eq!(
            totals.len(),
            2,
            "conservation checks reconcile against this"
        );
    }

    #[tokio::test]
    async fn a_multi_asset_grant_records_one_row_per_asset_without_colliding() {
        // Regression. `claim_id` is the primary key of `faucet_claims`, and a
        // grant of several assets writes one row each. Reusing the group id for
        // every row violated that key, so the claim failed AFTER the deposits
        // had already been minted: the caller saw an error and got the money
        // anyway. Found by running the real stack, not by a unit test.
        let h = harness();
        let receipt = h
            .faucet
            .claim("usr_multi", Address([9u8; 20]))
            .await
            .expect("a multi-asset grant must not fail on its own bookkeeping");

        assert_eq!(receipt.granted.len(), 2, "the fixture grants two assets");

        let rows =
            h.db.faucet()
                .history("usr_multi", 100)
                .await
                .expect("history readable");
        assert_eq!(rows.len(), 2, "one persisted row per granted asset");

        let assets: std::collections::HashSet<_> = rows.iter().map(|r| r.asset.as_str()).collect();
        assert_eq!(assets.len(), 2, "one row per distinct asset");
        assert!(
            rows.iter().all(|r| r.claim_id == receipt.claim_id),
            "every row carries the claim id the caller was given"
        );
        assert_eq!(
            h.db.faucet().claim_count("usr_multi").await.unwrap(),
            1,
            "a multi-asset grant is ONE claim, however many rows it writes"
        );
    }

    #[tokio::test]
    async fn a_second_claim_inside_the_cooldown_is_refused() {
        let h = harness();
        h.faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect("first");
        let err = h
            .faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect_err("cooldown");
        assert_eq!(err.code, ErrorCode::FaucetCooldown);
        assert_eq!(err.code.status().as_u16(), 429);
        assert!(err
            .details
            .expect("details")
            .get("next_available_at")
            .is_some());
        assert_eq!(deposits(&h.node).len(), 2, "no extra mint");
    }

    #[tokio::test]
    async fn the_cooldown_is_measured_against_the_injected_clock() {
        let h = harness();
        h.faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect("first");
        h.clock.advance(Duration::seconds(3_599));
        assert!(h.faucet.claim("usr_a", Address([7u8; 20])).await.is_err());
        h.clock.advance(Duration::seconds(2));
        h.faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect("cooldown elapsed");
        assert_eq!(deposits(&h.node).len(), 4);
    }

    #[tokio::test]
    async fn two_simultaneous_claims_from_one_user_produce_exactly_one_grant() {
        // The race the module docs describe: without the per-user lock, both
        // requests read "no previous claim" and both mint.
        let h = harness();
        let faucet = Arc::clone(&h.faucet);
        let address = Address([7u8; 20]);

        let first = {
            let faucet = Arc::clone(&faucet);
            tokio::spawn(async move { faucet.claim("usr_a", address).await })
        };
        let second = {
            let faucet = Arc::clone(&faucet);
            tokio::spawn(async move { faucet.claim("usr_a", address).await })
        };
        let (a, b) = (first.await.expect("join"), second.await.expect("join"));

        let granted = [&a, &b].iter().filter(|r| r.is_ok()).count();
        let refused = [&a, &b]
            .iter()
            .filter(|r| {
                r.as_ref()
                    .err()
                    .is_some_and(|e| e.code == ErrorCode::FaucetCooldown)
            })
            .count();
        assert_eq!(granted, 1, "exactly one claim may succeed");
        assert_eq!(
            refused, 1,
            "the other must be refused with the cooldown code"
        );
        assert_eq!(
            deposits(&h.node).len(),
            2,
            "one grant's worth of mints, not two"
        );
        assert_eq!(h.db.faucet().claim_count("usr_a").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn two_different_users_do_not_block_each_other() {
        let h = harness();
        let faucet = Arc::clone(&h.faucet);
        let (a, b) = tokio::join!(
            faucet.claim("usr_a", Address([7u8; 20])),
            faucet.claim("usr_b", Address([8u8; 20]))
        );
        a.expect("a granted");
        b.expect("b granted");
        assert_eq!(deposits(&h.node).len(), 4);
        // And the shared treasury key still produced strictly sequential nonces.
        let mut nonces: Vec<u64> = h.node.submitted().iter().map(|t| t.tx.nonce).collect();
        nonces.sort_unstable();
        assert_eq!(nonces, vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn status_reports_the_cooldown_and_the_grant() {
        let h = harness();
        let before = h.faucet.status("usr_a").await.expect("status");
        assert!(before.available);
        assert!(before.next_available_at.is_none());
        assert_eq!(before.claims_made, 0);
        assert_eq!(before.grant.len(), 2);
        assert_eq!(before.grant[0].amount_string(), "100000.00");
        assert_eq!(before.grant[1].amount_string(), "1.00000000");

        h.faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect("granted");
        let after = h.faucet.status("usr_a").await.expect("status");
        assert!(!after.available);
        assert_eq!(
            after.next_available_at.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-09-18T13:00:00+00:00")
        );
        assert_eq!(after.claims_made, 1);
    }

    #[tokio::test]
    async fn a_rejected_deposit_still_costs_the_cooldown_it_was_recorded_under() {
        // The deliberate trade-off of recording before minting (§11 of the
        // flow review), asserted so nobody quietly reverses it: a user whose
        // mint was refused has burnt a cooldown window and received nothing.
        //
        // The alternative — mint first, record after — loses the *money*
        // instead. A grant is several assets; a failure between the first
        // deposit and the last, or a process that dies before the write, left
        // minted money with no claim row and therefore no cooldown, and the
        // user could claim it again immediately. One costs a player an hour;
        // the other costs the simulation its only conservation ledger.
        let h = harness();
        h.node.edit(|state| {
            state.submit_reply = Some(SubmitOutcome::Rejected {
                code: SubmitRejection::InsufficientBalance,
                message: "treasury is dry".into(),
            })
        });
        let error = h
            .faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect_err("a refused mint is an error");
        let details = error.details.expect("the failure carries its claim id");
        assert_eq!(details["cooldown_started"], true);
        assert!(details["claim_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("clm_")));

        assert_eq!(
            h.db.faucet().claim_count("usr_a").await.unwrap(),
            1,
            "the claim was recorded before the mint was attempted"
        );
        assert!(
            !h.faucet.status("usr_a").await.expect("status").available,
            "and the cooldown it started is in force"
        );
    }

    #[tokio::test]
    async fn a_wrong_treasury_key_is_reported_as_a_misconfiguration() {
        let h = harness();
        h.node.edit(|state| {
            state.submit_reply = Some(SubmitOutcome::Rejected {
                code: SubmitRejection::NotAuthorized,
                message: "not the treasury".into(),
            })
        });
        let err = h
            .faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect_err("refused");
        assert!(
            err.message.contains("authorised deposit signer"),
            "the operator needs to be told what to fix: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn a_grant_too_large_to_record_is_refused_before_anything_is_minted() {
        let h = harness();
        let error = h
            .faucet
            .grant(
                "usr_a",
                Address([7u8; 20]),
                &[Grant {
                    asset: AssetId::Usd,
                    amount: u128::MAX,
                }],
                "operator typo",
            )
            .await
            .expect_err("unrepresentable");
        assert_eq!(error.code, ErrorCode::ValidationFailed);
        assert!(deposits(&h.node).is_empty(), "nothing minted");
        assert!(
            h.db.faucet().history("usr_a", 10).await.unwrap().is_empty(),
            "and no zero-amount row recorded"
        );
    }

    #[tokio::test]
    async fn an_unreachable_node_is_a_503() {
        let h = harness();
        h.node.take_offline("restarting");
        let err = h
            .faucet
            .claim("usr_a", Address([7u8; 20]))
            .await
            .expect_err("down");
        assert_eq!(err.code, ErrorCode::NodeUnavailable);
        let _ = h.node.health();
    }
}
