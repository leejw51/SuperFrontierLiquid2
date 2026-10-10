//! The link to the node, and the trait that lets the run loop be tested.
//!
//! # Why a trait
//!
//! Cap'n Proto's generated client futures are `!Send` — they hold pointers
//! into the RPC system — so every call here must be driven on a
//! [`tokio::task::LocalSet`]. `cb-client` solves this with one runtime per
//! call and `cb-gateway` with a dedicated thread holding a long-lived session;
//! this binary does the simplest version of the same thing, because it has no
//! other threads to satisfy: `main` builds a current-thread runtime, puts a
//! `LocalSet` around the entire program, and everything inside is free to be
//! `!Send`. That is why [`NodeLink`] is `#[async_trait(?Send)]`.
//!
//! The trait also buys the thing a live node cannot: a test for the reconnect
//! policy. `cb-gateway` makes the same argument for its `Connector`. A fake
//! that refuses three connections and then succeeds proves publishing resumes
//! and the process survives, in milliseconds and with no ports involved.
//!
//! # Every call has a deadline
//!
//! A socket to a machine that went away does not always fail its promise
//! promptly. An oracle parked forever on one `await` is indistinguishable from
//! a working one until somebody notices the mark prices stopped moving — which
//! is the exact failure this crate exists to prevent — so every RPC is wrapped
//! in a timeout and a timeout ends the session.

use crate::markets::{Discovery, Excluded, OracleMarket};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use cb_rpc::conv;
use cb_schema::rpc_capnp;
use cb_tx::SignedTx;
use cb_types::{Address, DerivId};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

/// What the node said about a submitted transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// Admitted to the mempool.
    Accepted,
    /// Refused, with the node's own code and message.
    Rejected {
        /// The node's error code, e.g. `badNonce`.
        code: String,
        /// The node's explanation.
        message: String,
    },
}

/// Everything this oracle asks a node for.
#[async_trait(?Send)]
pub trait NodeLink {
    /// The chain id every transaction must be signed with.
    async fn chain_id(&self) -> Result<u32>;

    /// Every derivative market the chain lists, split into the ones this
    /// authority may price and the ones it may not.
    async fn discover(&self, authority: Address) -> Result<Discovery>;

    /// The nonce the authority account's next transaction must carry.
    async fn account_nonce(&self, addr: &Address) -> Result<u64>;

    /// The `source_height` of the market's stored oracle price, or `None` when
    /// no price has ever landed.
    async fn oracle_height(&self, market: &DerivId) -> Result<Option<u64>>;

    /// The block height at which the market's mark price was last recomputed,
    /// or `None` when there is no mark yet.
    async fn mark_height(&self, market: &DerivId) -> Result<Option<u64>>;

    /// Submit one signed transaction.
    async fn submit(&self, tx: &SignedTx) -> Result<Submitted>;
}

/// Establishes a link. Fallible on purpose: the node may be down.
#[async_trait(?Send)]
pub trait Connector {
    /// Open a new link, or explain why not.
    async fn connect(&self) -> Result<Rc<dyn NodeLink>>;

    /// Where this connector points, for log lines.
    fn target(&self) -> String;
}

/// A [`Connector`] that dials a node's Cap'n Proto RPC port.
#[derive(Debug, Clone)]
pub struct CapnpConnector {
    addr: SocketAddr,
    timeout: Duration,
}

impl CapnpConnector {
    /// Dial `addr`, giving every call `timeout` to answer.
    pub fn new(addr: SocketAddr, timeout: Duration) -> Self {
        Self { addr, timeout }
    }
}

#[async_trait(?Send)]
impl Connector for CapnpConnector {
    async fn connect(&self) -> Result<Rc<dyn NodeLink>> {
        let client = tokio::time::timeout(self.timeout, cb_rpc::connect(self.addr))
            .await
            .map_err(|_| anyhow!("connecting to {} timed out", self.addr))?
            .with_context(|| format!("connect to the node at {}", self.addr))?;
        Ok(Rc::new(CapnpLink {
            client,
            timeout: self.timeout,
        }))
    }

    fn target(&self) -> String {
        self.addr.to_string()
    }
}

/// A live Cap'n Proto session.
pub struct CapnpLink {
    client: cb_rpc::Client,
    timeout: Duration,
}

impl CapnpLink {
    async fn with_deadline<T>(
        &self,
        what: &str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| anyhow!("{what} did not answer within {:?}", self.timeout))?
    }
}

/// Whether a Cap'n Proto error means "this row does not exist" rather than
/// "the link is gone".
///
/// The server answers a missing oracle or mark row with `Error::failed`, and
/// signals a dropped connection with `Disconnected`. Treating the two alike
/// would either tear down a healthy session on a fresh chain — where no mark
/// exists yet, which is every first run — or keep publishing into a socket
/// that closed.
fn is_absent(err: &capnp::Error) -> bool {
    matches!(err.kind, capnp::ErrorKind::Failed)
}

#[async_trait(?Send)]
impl NodeLink for CapnpLink {
    async fn chain_id(&self) -> Result<u32> {
        self.with_deadline("chainInfo", async {
            let resp = self.client.cap.chain_info_request().send().promise.await?;
            Ok(resp.get()?.get_info()?.get_chain_id())
        })
        .await
    }

    async fn discover(&self, authority: Address) -> Result<Discovery> {
        self.with_deadline("listPerpMarkets/listFutureMarkets", async {
            let mut out = Discovery::default();

            let resp = self
                .client
                .cap
                .list_perp_markets_request()
                .send()
                .promise
                .await
                .context("listPerpMarkets")?;
            let markets = resp.get()?.get_markets()?;
            for i in 0..markets.len() {
                let m = conv::perp_market_from_capnp(markets.get(i))?;
                if m.oracle_authority == authority {
                    out.markets
                        .push(OracleMarket::new(m.id, m.underlying, m.oracle_authority));
                } else {
                    out.excluded.push(Excluded::NotOurs {
                        market: m.id,
                        authority: m.oracle_authority,
                    });
                }
            }

            let resp = self
                .client
                .cap
                .list_future_markets_request()
                .send()
                .promise
                .await
                .context("listFutureMarkets")?;
            let markets = resp.get()?.get_markets()?;
            for i in 0..markets.len() {
                let m = conv::future_market_from_capnp(markets.get(i))?;
                if m.final_settlement_px_q8.is_some() {
                    // The executor answers `MarketExpired` to an oracle post
                    // on a settled contract, so posting would be rejections
                    // once a second forever.
                    out.excluded.push(Excluded::Settled(m.id));
                } else if m.oracle_authority == authority {
                    out.markets
                        .push(OracleMarket::new(m.id, m.underlying, m.oracle_authority));
                } else {
                    out.excluded.push(Excluded::NotOurs {
                        market: m.id,
                        authority: m.oracle_authority,
                    });
                }
            }

            Ok(out)
        })
        .await
    }

    async fn account_nonce(&self, addr: &Address) -> Result<u64> {
        self.with_deadline("getAccount", async {
            let mut req = self.client.cap.get_account_request();
            conv::address_to_capnp(req.get().init_addr(), addr);
            let resp = req.send().promise.await.context("getAccount")?;
            Ok(resp.get()?.get_state()?.get_nonce())
        })
        .await
    }

    async fn oracle_height(&self, market: &DerivId) -> Result<Option<u64>> {
        self.with_deadline("getOraclePrice", async {
            let mut req = self.client.cap.get_oracle_price_request();
            conv::deriv_id_to_capnp(req.get().init_market(), market);
            match req.send().promise.await {
                Ok(resp) => Ok(Some(resp.get()?.get_oracle()?.get_source_height())),
                Err(e) if is_absent(&e) => Ok(None),
                Err(e) => Err(anyhow::Error::new(e).context("getOraclePrice")),
            }
        })
        .await
    }

    async fn mark_height(&self, market: &DerivId) -> Result<Option<u64>> {
        self.with_deadline("getMarkPrice", async {
            let mut req = self.client.cap.get_mark_price_request();
            conv::deriv_id_to_capnp(req.get().init_market(), market);
            match req.send().promise.await {
                Ok(resp) => Ok(Some(resp.get()?.get_mark()?.get_computed_at_height())),
                Err(e) if is_absent(&e) => Ok(None),
                Err(e) => Err(anyhow::Error::new(e).context("getMarkPrice")),
            }
        })
        .await
    }

    async fn submit(&self, tx: &SignedTx) -> Result<Submitted> {
        self.with_deadline("submitTx", async {
            let mut req = self.client.cap.submit_tx_request();
            conv::signed_tx_to_capnp(req.get().init_tx(), tx);
            let resp = req.send().promise.await.context("submitTx")?;
            match resp.get()?.get_result()?.which()? {
                rpc_capnp::submit_result::Ok(()) => Ok(Submitted::Accepted),
                rpc_capnp::submit_result::Rejected(rej) => {
                    let r = rej?;
                    Ok(Submitted::Rejected {
                        code: format!("{:?}", r.get_code()?),
                        message: r.get_message()?.to_string()?,
                    })
                }
            }
        })
        .await
    }
}
