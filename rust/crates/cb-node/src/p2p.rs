//! Wiring the peer-to-peer layer into the node binary.
//!
//! The gossip engine itself lives in `cb-net`. This module is the adapter: it
//! turns command-line flags into a [`P2pConfig`], loads the network key, starts
//! the transport, and keeps the engine's idea of the chain head in step with
//! the block producer.
//!
//! # Why the network key is not the validator key
//!
//! A node's network identity is derived from a *separate* secp256k1 key. Two
//! reasons. A non-validating observer still needs a stable identity so the
//! peer table can suppress duplicate and self connections, and it has no
//! validator key to derive one from. And a validator key signs blocks and
//! votes — consensus-weight material that should not also be answering
//! handshakes from strangers. The dev fallback derives the network key from
//! `<dev-key-seed>-p2p`, so a dev cluster gets distinct, deterministic
//! identities without an out-of-band keystore.
//!
//! # How the producer reaches gossip
//!
//! A watcher task polls `Meta::head_height` and calls
//! [`P2pHandle::announce_block`] when it moves. It does not modify the block
//! producer: the sequencer loop already notifies on every block, so the
//! watcher waits on that notification and reads the head. Announcing only the
//! new head — not every height in between — is enough, because a peer that is
//! more than one block behind asks for a range rather than a body.

use cb_block::ExecCtx;
use cb_net::p2p::chain::DbChain;
use cb_net::p2p::config::P2pConfig;
use cb_net::p2p::engine::LocalNode;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::transport::{spawn, P2pHandle};
use cb_state::{Database, Meta};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Command-line surface for the peer-to-peer layer, shared by the sequencer
/// and replica subcommands.
#[derive(Clone, Debug, clap::Args)]
pub struct P2pArgs {
    /// Address to accept inbound peer connections on. Supplying this (or a
    /// seed) is what turns the peer-to-peer layer on.
    #[arg(long)]
    pub p2p_listen: Option<SocketAddr>,
    /// A peer to dial at startup. Repeatable.
    #[arg(long = "p2p-seed")]
    pub p2p_seeds: Vec<SocketAddr>,
    /// TOML file holding the full peer-to-peer configuration, including the
    /// scoring and banning thresholds. Command-line flags win over it.
    #[arg(long)]
    pub p2p_config: Option<PathBuf>,
    /// Hex-encoded 32-byte secp256k1 secret used as this node's *network*
    /// identity. Distinct from the validator key; see the module docs.
    #[arg(long)]
    pub p2p_key_file: Option<PathBuf>,
}

impl P2pArgs {
    /// Whether the operator asked for peer-to-peer networking at all.
    pub fn enabled(&self) -> bool {
        self.p2p_listen.is_some() || !self.p2p_seeds.is_empty() || self.p2p_config.is_some()
    }

    /// Resolve the flags and any config file into a validated config.
    pub fn to_config(&self) -> anyhow::Result<P2pConfig> {
        let mut cfg = match &self.p2p_config {
            Some(path) => P2pConfig::from_toml_file(path)?,
            None => P2pConfig::default(),
        };
        if let Some(listen) = self.p2p_listen {
            cfg.listen = Some(listen);
        }
        if !self.p2p_seeds.is_empty() {
            cfg.seeds = self.p2p_seeds.clone();
        }
        cfg.normalize();
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Start the peer-to-peer layer against `db`.
///
/// `notify` is the sequencer's block-builder wakeup. Passing it means a
/// transaction that arrives by gossip wakes the producer exactly as one that
/// arrived by RPC does; pass `None` on a node that does not produce blocks.
pub async fn start(
    db: &Database,
    ctx: ExecCtx,
    args: &P2pArgs,
    dev_key_seed: &str,
    notify: Option<Arc<Notify>>,
) -> anyhow::Result<P2pHandle> {
    Ok(start_with(db, ctx, args, dev_key_seed, notify, true)
        .await?
        .0)
}

/// [`start`] for a validator: blocks are served and transactions accepted,
/// but a block a peer sends is never applied, because this node executes
/// the committed sequence itself.
pub async fn start_serve_only(
    db: &Database,
    ctx: ExecCtx,
    args: &P2pArgs,
    dev_key_seed: &str,
    notify: Option<Arc<Notify>>,
) -> anyhow::Result<(P2pHandle, Arc<std::sync::atomic::AtomicBool>)> {
    start_with(db, ctx, args, dev_key_seed, notify, false).await
}

async fn start_with(
    db: &Database,
    ctx: ExecCtx,
    args: &P2pArgs,
    dev_key_seed: &str,
    notify: Option<Arc<Notify>>,
    apply_remote: bool,
) -> anyhow::Result<(P2pHandle, Arc<std::sync::atomic::AtomicBool>)> {
    let mut cfg = args.to_config()?;
    cfg.fetch_blocks = apply_remote;
    if !apply_remote {
        // Consensus traffic between validators runs to thousands of
        // messages a second a peer under load; that is the protocol, not
        // a flood.
        cfg.max_messages_per_window = 1_000_000;
    }
    let key = match &args.p2p_key_file {
        Some(path) => crate::keystore::load_key_from_file(path)?,
        None => {
            let seed = format!("{dev_key_seed}-p2p");
            let (sk, _) = cb_crypto::keypair_from_seed(seed.as_bytes());
            tracing::warn!(
                seed,
                "no --p2p-key-file provided; using a deterministic dev network identity"
            );
            sk
        }
    };
    let chain_id = Meta::new(db).chain_id();
    let genesis = GenesisId::from_db(db);
    let local = LocalNode::new(
        key,
        chain_id,
        genesis,
        concat!("cb-node/", env!("CARGO_PKG_VERSION")).to_string(),
    );
    tracing::info!(
        node_id = %local.node_id,
        chain_id,
        %genesis,
        seeds = cfg.seeds.len(),
        "starting peer-to-peer networking"
    );
    let chain = if apply_remote {
        DbChain::new(db.clone(), ctx, notify)
    } else {
        DbChain::serve_only(db.clone(), ctx, notify)
    };
    let switch = chain.apply_switch();
    let chain = Arc::new(Mutex::new(chain));
    Ok((spawn(cfg, local, chain).await?, switch))
}

/// How often the head watcher re-reads the head if no notification arrives.
/// The notification path is the fast one; this bounds the latency when a block
/// lands without one (for example on a node that is applying gossiped blocks).
const HEAD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Watch the chain head and announce every new block to the network.
///
/// Runs forever. Spawned alongside the sequencer loop so gossip stays a
/// separate concern from block production: the producer is not modified, and a
/// stalled network cannot back up into it.
pub async fn announce_new_heads(db: Database, handle: P2pHandle, notify: Arc<Notify>) {
    let mut last = Meta::new(&db).head_height();
    loop {
        let waiter = notify.notified();
        tokio::pin!(waiter);
        let _ = tokio::time::timeout(HEAD_POLL_INTERVAL, waiter).await;

        // Height and hash are two reads of a moving target. Sandwiching the
        // hash between two height reads and discarding a mismatch costs one
        // extra poll and rules out announcing `(hash_of_H+1, H)`, which a peer
        // would fetch and then fail to match against the height we claimed.
        let before = Meta::new(&db).head_height();
        let hash = Meta::new(&db).head_hash();
        let after = Meta::new(&db).head_height();
        if before == after && after > last {
            // Only the new head is announced. A peer more than one block
            // behind responds by asking for a range, so announcing every
            // intermediate height would buy nothing and cost a message each.
            handle.announce_block(hash, after);
            last = after;
        }
    }
}
