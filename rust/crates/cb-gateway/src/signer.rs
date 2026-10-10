//! Signing and submitting transactions on a user's behalf.
//!
//! ## The nonce is the hard part
//!
//! Every account's transactions carry a strictly sequential nonce, and the
//! sequencer rejects anything else with `badNonce`. Reading the nonce from the
//! chain before each submission is not enough: two requests from the same
//! account — or, much more commonly, two faucet claims from *different* users,
//! both signed by the one treasury key — read the same value, sign the same
//! nonce, and one of them is thrown away. The symptom looks like flakiness and
//! is really a race.
//!
//! So nonces are serialised per address. Each address has its own async mutex
//! and a cached next-nonce; a submission holds the mutex across read, sign and
//! submit, and only then releases it. Two callers for one address queue;
//! callers for different addresses do not contend at all.
//!
//! When the sequencer does report `badNonce` — which happens whenever
//! something outside this process has moved the account, or the node restarted
//! and lost a mempool — the cache is dropped and the submission is retried
//! once against a freshly read nonce. Once, not in a loop: a persistent
//! `badNonce` means something is wrong that retrying will not fix.
//!
//! ## The nonce is also the receipt
//!
//! Under BFT ordering (ADR 0008) a block keeps every committed transaction
//! whether or not the executor applied it, and nothing on the wire says
//! which. What the chain does say is the account's nonce: an applied
//! transaction advances it by one and a refused one leaves it alone, so for
//! a transaction *this process signed* — whose nonce it therefore knows —
//! the nonce read after the block carrying it is an exact verdict. The
//! submitter keeps every accepted submission in an [`InFlight`] registry for
//! that reason, and the watcher reads verdicts from it.
//!
//! One case would fool a nonce comparison: a refused transaction whose nonce
//! this process later signs *again*, because the retry path re-read the
//! chain. If the retry lands before the watcher has looked at the block that
//! refused the original, the original's nonce reads as consumed and it would
//! be taken for applied. The retry is therefore the point at which the
//! original is declared dead: a re-signed nonce can only be admitted when
//! nothing of that address's is left in the mempool (`cb_rpc::admission`
//! expects `max(pending, chain)` and forgets an address only when the block
//! builder refused its transaction), so every earlier submission at or above
//! the re-read nonce is gone for good. That rests on this process being the
//! only signer for the address, which is true by construction for a trading
//! key — it is derived from the server secret and never leaves the gateway —
//! and is a deployment fact for the treasury key.

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::node::{NodeClient, SubmitOutcome, SubmitRejection};
use cb_crypto::SecretKey;
use cb_tx::{SignedTx, Transaction, TxBody};
use cb_types::{Address, H256};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// A transaction this process signed and the mempool accepted, until the
/// chain has said what became of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    /// The signing address.
    pub address: Address,
    /// The nonce it was signed with. The chain's nonce for `address` moving
    /// past this is the only proof the gateway has that it applied.
    pub nonce: u64,
    /// When it was accepted, so a submission the chain never answers for can
    /// be expired rather than held for ever.
    pub accepted_at: DateTime<Utc>,
}

/// How long an accepted submission is remembered while nothing answers for
/// it.
///
/// A transaction that reaches neither a block nor the dead list — a node that
/// restarted and lost its mempool, with no later submission from the same
/// address to expose the loss — would otherwise be remembered for the life of
/// the process. An hour is longer than any mempool keeps a transaction.
pub const IN_FLIGHT_TTL: chrono::Duration = chrono::Duration::hours(1);

/// Signs transactions and gets them into the mempool.
pub struct TxSubmitter {
    node: Arc<dyn NodeClient>,
    clock: Arc<dyn cb_ports::Clock>,
    /// One lock per signing address. The value is the next nonce to use, or
    /// `None` when it must be read from the chain.
    nonces: Mutex<HashMap<Address, Arc<tokio::sync::Mutex<Option<u64>>>>>,
    chain_id: Mutex<Option<u32>>,
    /// Every accepted submission the chain has not yet answered for, by hash.
    in_flight: DashMap<H256, InFlight>,
    /// Submissions the retry path has proven can never apply, waiting for the
    /// watcher to collect them. See the module docs.
    dead: Mutex<Vec<H256>>,
}

impl std::fmt::Debug for TxSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxSubmitter")
            .field("chain_id", &*self.chain_id.lock())
            .finish_non_exhaustive()
    }
}

impl TxSubmitter {
    /// Build a submitter over a node client.
    pub fn new(node: Arc<dyn NodeClient>) -> Self {
        Self::with_clock(node, Arc::new(cb_ports::SystemClock))
    }

    /// Build a submitter that stamps its in-flight registry from `clock`.
    pub fn with_clock(node: Arc<dyn NodeClient>, clock: Arc<dyn cb_ports::Clock>) -> Self {
        Self {
            node,
            clock,
            nonces: Mutex::new(HashMap::new()),
            chain_id: Mutex::new(None),
            in_flight: DashMap::new(),
            dead: Mutex::new(Vec::new()),
        }
    }

    /// The chain id, read once and cached. Signing without it would produce a
    /// transaction valid on another chain.
    pub async fn chain_id(&self) -> ApiResult<u32> {
        if let Some(id) = *self.chain_id.lock() {
            return Ok(id);
        }
        let info = self.node.chain_info().await?;
        *self.chain_id.lock() = Some(info.chain_id);
        Ok(info.chain_id)
    }

    /// Sign `body` as `address` and submit it, returning the transaction hash.
    pub async fn submit(&self, key: &SecretKey, address: Address, body: TxBody) -> ApiResult<H256> {
        self.submit_with_nonce(key, address, body)
            .await
            .map(|(hash, _nonce)| hash)
    }

    /// Sign `body` as `address` and submit it, returning the transaction hash
    /// and the nonce it was signed with.
    ///
    /// The nonce is what a caller keeps when it needs to learn later whether
    /// the chain applied the transaction: see the module docs.
    pub async fn submit_with_nonce(
        &self,
        key: &SecretKey,
        address: Address,
        body: TxBody,
    ) -> ApiResult<(H256, u64)> {
        let chain_id = self.chain_id().await?;
        let cell = self.cell_for(address);
        let mut guard = cell.lock().await;

        let mut nonce = match *guard {
            Some(nonce) => nonce,
            None => self.node.account(address).await?.nonce,
        };
        let mut re_read = false;

        for attempt in 0..2 {
            let signed = sign(key, chain_id, nonce, body.clone())?;
            let hash = signed.hash();
            match self.node.submit_tx(signed).await? {
                SubmitOutcome::Accepted => {
                    *guard = Some(nonce + 1);
                    if re_read {
                        // A nonce the chain handed back after `badNonce` was
                        // admitted, so nothing of this address's is left in
                        // the mempool: every earlier submission at or above it
                        // is dead. Declared before the new one is recorded, so
                        // the sweep cannot catch it.
                        self.declare_dead_from(address, nonce, hash);
                    }
                    self.in_flight.insert(
                        hash,
                        InFlight {
                            address,
                            nonce,
                            accepted_at: self.clock.now(),
                        },
                    );
                    tracing::debug!(
                        target: "cb_gateway::signer",
                        address = %hex::encode(address.0),
                        nonce,
                        tx = %hex::encode(hash.0),
                        "transaction accepted"
                    );
                    return Ok((hash, nonce));
                }
                SubmitOutcome::Rejected { code, message } => {
                    if code == SubmitRejection::BadNonce && attempt == 0 {
                        // Something moved the account underneath us. Re-read
                        // once; a second failure is not a race.
                        *guard = None;
                        nonce = self.node.account(address).await?.nonce;
                        re_read = true;
                        continue;
                    }
                    *guard = None;
                    return Err(rejection_to_api_error(code, &message));
                }
            }
        }
        *guard = None;
        Err(ApiError::new(
            ErrorCode::Internal,
            "transaction nonce could not be resolved after a retry",
        ))
    }

    /// Every accepted submission from `address` signed at or above `nonce`,
    /// other than `survivor`, can no longer apply. Move them to the dead list.
    ///
    /// A retry whose bytes are identical to the original — same body, same
    /// nonce, and a deterministic signature — has the original's hash, is the
    /// same transaction, and is the survivor.
    fn declare_dead_from(&self, address: Address, nonce: u64, survivor: H256) {
        let dead: Vec<H256> = self
            .in_flight
            .iter()
            .filter(|entry| {
                entry.address == address && entry.nonce >= nonce && *entry.key() != survivor
            })
            .map(|entry| *entry.key())
            .collect();
        if dead.is_empty() {
            return;
        }
        for hash in &dead {
            self.in_flight.remove(hash);
        }
        tracing::info!(
            target: "cb_gateway::signer",
            address = %hex::encode(address.0),
            from_nonce = nonce,
            count = dead.len(),
            "earlier submissions can no longer apply; the chain handed their nonce back"
        );
        self.dead.lock().extend(dead);
    }

    /// What is known about an accepted submission the chain has not yet
    /// answered for. `None` for a hash this process never submitted, or one
    /// already settled.
    pub fn in_flight(&self, hash: &H256) -> Option<InFlight> {
        self.in_flight.get(hash).map(|entry| *entry.value())
    }

    /// Forget a submission the chain has answered for.
    pub fn settle(&self, hash: &H256) {
        self.in_flight.remove(hash);
    }

    /// Take the submissions the retry path has declared dead since the last
    /// call. The watcher marks their orders refused.
    pub fn take_dead(&self) -> Vec<H256> {
        std::mem::take(&mut *self.dead.lock())
    }

    /// Expire submissions nothing has answered for within [`IN_FLIGHT_TTL`],
    /// onto the dead list. Called by the janitor.
    pub fn prune(&self, now: DateTime<Utc>) {
        let cutoff = now - IN_FLIGHT_TTL;
        let expired: Vec<H256> = self
            .in_flight
            .iter()
            .filter(|entry| entry.accepted_at < cutoff)
            .map(|entry| *entry.key())
            .collect();
        for hash in &expired {
            self.in_flight.remove(hash);
        }
        if !expired.is_empty() {
            tracing::warn!(
                target: "cb_gateway::signer",
                count = expired.len(),
                "accepted submissions reached no block within the hour; giving them up"
            );
            self.dead.lock().extend(expired);
        }
    }

    /// How many accepted submissions are still unanswered. For `/health` and
    /// tests.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Forget the cached nonce for an address, forcing the next submission to
    /// re-read it. Used when something outside this process may have signed.
    pub fn invalidate(&self, address: Address) {
        if let Some(cell) = self.nonces.lock().get(&address).cloned() {
            if let Ok(mut guard) = cell.try_lock() {
                *guard = None;
            }
        }
    }

    fn cell_for(&self, address: Address) -> Arc<tokio::sync::Mutex<Option<u64>>> {
        let mut nonces = self.nonces.lock();
        Arc::clone(nonces.entry(address).or_default())
    }
}

/// Sign a transaction body.
pub fn sign(key: &SecretKey, chain_id: u32, nonce: u64, body: TxBody) -> ApiResult<SignedTx> {
    SignedTx::sign_with(
        key,
        Transaction {
            nonce,
            chain_id,
            body,
        },
    )
    .map_err(|e| ApiError::internal(format!("sign transaction: {e}")))
}

/// Map a sequencer rejection onto the contract's error codes.
///
/// This is where the chain's vocabulary becomes the frontend's. A post-only
/// order the engine refused must arrive as `POST_ONLY_WOULD_CROSS`, not as a
/// generic failure, because the frontend shows a different message for it.
pub fn rejection_to_api_error(code: SubmitRejection, message: &str) -> ApiError {
    let (code, text) = match code {
        SubmitRejection::InsufficientBalance => (
            ErrorCode::InsufficientBalance,
            "not enough free balance".to_string(),
        ),
        SubmitRejection::PostOnlyWouldCross => (
            ErrorCode::PostOnlyWouldCross,
            "a post-only order may not take liquidity".to_string(),
        ),
        SubmitRejection::UnknownMarket => (ErrorCode::NotFound, "unknown market".to_string()),
        SubmitRejection::InvalidAmount => (
            ErrorCode::ValidationFailed,
            format!("the sequencer rejected the amount: {message}"),
        ),
        SubmitRejection::NotAuthorized => (
            ErrorCode::Forbidden,
            "this account may not send that transaction".to_string(),
        ),
        SubmitRejection::MempoolFull => (
            ErrorCode::RateLimited,
            "the sequencer's mempool is full; try again shortly".to_string(),
        ),
        SubmitRejection::BadNonce => (
            ErrorCode::Internal,
            "transaction nonce was rejected".to_string(),
        ),
        SubmitRejection::BadSignature => (
            ErrorCode::Internal,
            "the gateway produced a signature the sequencer rejected".to_string(),
        ),
        SubmitRejection::Internal => (
            ErrorCode::Internal,
            format!("the sequencer failed: {message}"),
        ),
    };
    ApiError::new(code, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MockNode;
    use cb_types::AssetId;

    fn transfer() -> TxBody {
        TxBody::Transfer {
            to: Address([9u8; 20]),
            asset: AssetId::Usd,
            amount: 1,
        }
    }

    #[tokio::test]
    async fn nonces_advance_without_re_reading_the_chain() {
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);

        for _ in 0..3 {
            submitter
                .submit(&key, address, transfer())
                .await
                .expect("accepted");
        }
        let nonces: Vec<u64> = node.submitted().iter().map(|t| t.tx.nonce).collect();
        assert_eq!(nonces, vec![0, 1, 2], "nonces must be strictly sequential");
    }

    #[tokio::test]
    async fn concurrent_submissions_from_one_address_do_not_reuse_a_nonce() {
        // This is the treasury-key case: many callers, one signer.
        let node = Arc::new(MockNode::new());
        let submitter = Arc::new(TxSubmitter::new(node.clone()));
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let submitter = submitter.clone();
            handles.push(tokio::spawn(async move {
                submitter.submit(&key, address, transfer()).await
            }));
        }
        for handle in handles {
            handle.await.expect("join").expect("accepted");
        }

        let mut nonces: Vec<u64> = node.submitted().iter().map(|t| t.tx.nonce).collect();
        nonces.sort_unstable();
        assert_eq!(
            nonces,
            (0..16).collect::<Vec<u64>>(),
            "every nonce must be used exactly once"
        );
    }

    #[tokio::test]
    async fn a_bad_nonce_is_retried_once_against_a_fresh_read() {
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);

        // The gateway's cache is warm at nonce 0; the chain has moved to 7
        // and rejects the first attempt. The retry must re-read and succeed.
        submitter
            .submit(&key, address, transfer())
            .await
            .expect("warm the cache");
        node.edit(|state| {
            state.accounts.entry(address).or_default().nonce = 7;
            state.submit_replies.push_back(SubmitOutcome::Rejected {
                code: SubmitRejection::BadNonce,
                message: "expected 7".into(),
            });
        });

        submitter
            .submit(&key, address, transfer())
            .await
            .expect("accepted on retry");
        let nonces: Vec<u64> = node.submitted().iter().map(|t| t.tx.nonce).collect();
        assert_eq!(
            nonces,
            vec![0, 1, 7],
            "the stale cached nonce is tried once, then re-read from the chain"
        );
    }

    #[tokio::test]
    async fn an_accepted_submission_is_remembered_until_it_is_settled() {
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);

        let (hash, nonce) = submitter
            .submit_with_nonce(&key, address, transfer())
            .await
            .expect("accepted");
        assert_eq!(nonce, 0);
        let entry = submitter.in_flight(&hash).expect("remembered");
        assert_eq!((entry.address, entry.nonce), (address, 0));
        assert!(submitter.take_dead().is_empty());

        submitter.settle(&hash);
        assert!(submitter.in_flight(&hash).is_none());
        assert_eq!(submitter.in_flight_count(), 0);
    }

    #[tokio::test]
    async fn a_nonce_the_chain_hands_back_declares_the_earlier_submission_dead() {
        // The case a nonce comparison alone would get wrong. Nonce 0 was
        // accepted and then refused at block build, so the chain still expects
        // 0; the next submission at the cached 1 is refused, the retry re-reads
        // 0 and is accepted. If that retry lands before the watcher looks at
        // the block that refused the original, the original's nonce reads as
        // consumed. The retry is therefore what declares it dead.
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);

        let (first, _) = submitter
            .submit_with_nonce(&key, address, transfer())
            .await
            .expect("accepted");
        node.edit(|state| {
            state.submit_replies.push_back(SubmitOutcome::Rejected {
                code: SubmitRejection::BadNonce,
                message: "expected 0".into(),
            });
        });
        // A different body: the same body at the same nonce would be the
        // same bytes and the same hash, which is the same transaction.
        let second_body = TxBody::Transfer {
            to: Address([9u8; 20]),
            asset: AssetId::Usd,
            amount: 2,
        };
        let (second, nonce) = submitter
            .submit_with_nonce(&key, address, second_body)
            .await
            .expect("accepted on retry");
        assert_eq!(nonce, 0, "the chain handed nonce 0 back");

        assert_eq!(submitter.take_dead(), vec![first]);
        assert!(
            submitter.in_flight(&first).is_none(),
            "the dead one is forgotten"
        );
        assert!(
            submitter.in_flight(&second).is_some(),
            "the survivor is not"
        );
        assert!(
            submitter.take_dead().is_empty(),
            "the dead list is drained once"
        );
    }

    #[tokio::test]
    async fn a_chain_that_moved_ahead_declares_nothing_dead() {
        // The other direction: the chain is *past* the cache, so the earlier
        // submission applied. Nothing is below the re-read nonce.
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);
        let (first, _) = submitter
            .submit_with_nonce(&key, address, transfer())
            .await
            .expect("accepted");
        node.edit(|state| {
            state.accounts.entry(address).or_default().nonce = 7;
            state.submit_replies.push_back(SubmitOutcome::Rejected {
                code: SubmitRejection::BadNonce,
                message: "expected 7".into(),
            });
        });
        submitter
            .submit(&key, address, transfer())
            .await
            .expect("accepted on retry");
        assert!(submitter.take_dead().is_empty());
        assert!(submitter.in_flight(&first).is_some());
    }

    #[tokio::test]
    async fn a_retry_that_is_refused_declares_nothing_dead() {
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);
        let (first, _) = submitter
            .submit_with_nonce(&key, address, transfer())
            .await
            .expect("accepted");
        node.edit(|state| {
            state.submit_reply = Some(SubmitOutcome::Rejected {
                code: SubmitRejection::BadNonce,
                message: "no".into(),
            })
        });
        assert!(submitter.submit(&key, address, transfer()).await.is_err());
        assert!(
            submitter.take_dead().is_empty(),
            "a refused retry proves nothing about the earlier submission"
        );
        assert!(submitter.in_flight(&first).is_some());
    }

    #[tokio::test]
    async fn a_submission_nothing_answers_for_is_given_up_after_an_hour() {
        let node = Arc::new(MockNode::new());
        use cb_ports::Clock;
        let clock = cb_ports::FixedClock::new(
            chrono::DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let submitter = TxSubmitter::with_clock(node.clone(), Arc::new(clock.clone()));
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);
        let (hash, _) = submitter
            .submit_with_nonce(&key, address, transfer())
            .await
            .expect("accepted");

        submitter.prune(clock.now() + chrono::Duration::minutes(59));
        assert!(
            submitter.in_flight(&hash).is_some(),
            "still within the hour"
        );
        submitter.prune(clock.now() + chrono::Duration::minutes(61));
        assert!(submitter.in_flight(&hash).is_none());
        assert_eq!(submitter.take_dead(), vec![hash]);
    }

    #[tokio::test]
    async fn a_persistent_bad_nonce_gives_up_rather_than_looping() {
        let node = Arc::new(MockNode::new());
        let submitter = TxSubmitter::new(node.clone());
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);
        node.edit(|state| {
            state.submit_reply = Some(SubmitOutcome::Rejected {
                code: SubmitRejection::BadNonce,
                message: "no".into(),
            })
        });
        assert!(submitter.submit(&key, address, transfer()).await.is_err());
        assert_eq!(node.submitted().len(), 2, "one attempt and one retry");
    }

    #[tokio::test]
    async fn an_offline_node_is_a_503_not_a_hang() {
        let node = Arc::new(MockNode::new());
        node.take_offline("restarting");
        let submitter = TxSubmitter::new(node);
        let (key, public) = cb_crypto::generate_keypair();
        let address = cb_crypto::address_from_pubkey(&public);
        let err = submitter
            .submit(&key, address, transfer())
            .await
            .expect_err("unavailable");
        assert_eq!(err.code, ErrorCode::NodeUnavailable);
    }

    #[test]
    fn sequencer_rejections_map_to_the_contract_codes() {
        let cases = [
            (
                SubmitRejection::InsufficientBalance,
                ErrorCode::InsufficientBalance,
            ),
            (
                SubmitRejection::PostOnlyWouldCross,
                ErrorCode::PostOnlyWouldCross,
            ),
            (SubmitRejection::UnknownMarket, ErrorCode::NotFound),
            (SubmitRejection::NotAuthorized, ErrorCode::Forbidden),
            (SubmitRejection::MempoolFull, ErrorCode::RateLimited),
            (SubmitRejection::InvalidAmount, ErrorCode::ValidationFailed),
        ];
        for (rejection, expected) in cases {
            assert_eq!(rejection_to_api_error(rejection, "why").code, expected);
        }
    }
}
