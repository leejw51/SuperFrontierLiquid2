use cb_block::ExecCtx;
use cb_dashboard::Metrics;
use cb_node::keystore::load_or_dev_key;
use cb_state::{Database, Meta, Validators};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::LocalSet;

#[derive(Parser)]
#[command(name = "cb-node", about = "SuperFrontierLiquid L1 node")]
struct Cli {
    /// Persistent data directory (RocksDB lives here).
    #[arg(long)]
    data_dir: PathBuf,

    #[command(subcommand)]
    mode: Mode,
}

/// Everything `cb-node sequencer` is told to be.
///
/// A struct rather than nineteen arguments because a node that takes over
/// mid-flight builds one of these too: failover is not a second
/// implementation of leading, it is the same one, entered from somewhere
/// else.
pub struct SequencerOpts {
    pub listen: SocketAddr,
    pub dashboard_listen: SocketAddr,
    pub block_interval_ms: u64,
    pub max_block_txs: usize,
    pub rpc_threads: usize,
    pub verify_threads: usize,
    pub sync_writes: bool,
    pub key_file: Option<PathBuf>,
    pub dev_key_seed: String,
    pub benchmark: bool,
    pub bench_workload: cb_node::benchmark::BenchWorkload,
    pub bench_traders: u32,
    pub bench_seed_prefix: String,
    pub bench_rng_seed: u64,
    pub bench_workers: usize,
    pub bench_channel_capacity: usize,
    pub keep_blocks: u64,
    pub pruning: bool,
    pub p2p: cb_node::p2p::P2pArgs,
}

/// Everything `cb-node validator` is told to be.
pub struct ValidatorOpts {
    pub listen: SocketAddr,
    pub dashboard_listen: SocketAddr,
    pub max_block_txs: usize,
    pub key_file: Option<PathBuf>,
    pub dev_key_seed: String,
    pub benchmark: bool,
    pub bench_workload: cb_node::benchmark::BenchWorkload,
    pub bench_traders: u32,
    pub bench_seed_prefix: String,
    pub bench_rng_seed: u64,
    pub bench_workers: usize,
    pub bench_channel_capacity: usize,
    pub rpc_threads: usize,
    pub verify_threads: usize,
    pub sync_writes: bool,
    pub keep_blocks: u64,
    pub round_timeout_ms: u64,
    pub idle_interval_ms: u64,
    pub p2p: cb_node::p2p::P2pArgs,
}

/// Run this node as one validator of the BFT cluster until it stops.
///
/// The same stack as the sequencer's — RPC admission with its verifier
/// pool, the dashboard on its own thread, gossip for serving blocks and for
/// carrying consensus messages — around `cb_node::validator::run_validator`
/// instead of the block loop.
async fn run_validator_stack(db: Database, opts: ValidatorOpts) -> anyhow::Result<()> {
    let ValidatorOpts {
        listen,
        dashboard_listen,
        max_block_txs,
        key_file,
        dev_key_seed,
        benchmark,
        bench_workload,
        bench_traders,
        bench_seed_prefix,
        bench_rng_seed,
        bench_workers,
        bench_channel_capacity,
        rpc_threads,
        verify_threads,
        sync_writes,
        keep_blocks,
        round_timeout_ms,
        idle_interval_ms,
        p2p,
    } = opts;
    anyhow::ensure!(
        Meta::new(&db).is_bft(),
        "this chain was not initialised with `--consensus bft`; run `init --consensus bft` on a fresh directory"
    );
    let notify = Arc::new(Notify::new());
    let metrics = Arc::new(Metrics::new());
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        ..Default::default()
    };
    let (sk, me) = load_or_dev_key(key_file.as_deref(), &dev_key_seed)?;
    let validators = Validators::new(&db);
    anyhow::ensure!(
        validators.get_by_addr(&me).is_some(),
        "this key derives {me}, which is not in the genesis validator set"
    );
    anyhow::ensure!(
        validators.count() == 1 || p2p.enabled(),
        "a cluster of {} validators needs the peer-to-peer flags (--p2p-listen, --p2p-seed)",
        validators.count()
    );
    let max_block_txs = max_block_txs.min(cb_block::MAX_BLOCK_TXS);

    // Gossip serves blocks and carries consensus messages; it never applies
    // a block here, since this node executes the committed sequence itself.
    let (p2p_handle, apply_switch, inbound) = {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (handle, switch) = if p2p.enabled() {
            let (h, switch) =
                cb_node::p2p::start_serve_only(&db, ctx, &p2p, &dev_key_seed, Some(notify.clone()))
                    .await?;
            h.set_consensus_sink(tx);
            (Some(h), Some(switch))
        } else {
            (None, None)
        };
        (handle, switch, rx)
    };
    // Admitted transactions are not gossiped one by one; they are forwarded
    // to the other validators in batches over the consensus channel.
    let outbox: cb_node::validator::Outbox = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tx_observer: Option<cb_rpc::TxObserver> = p2p_handle
        .as_ref()
        .map(|_| cb_node::validator::outbox_observer(outbox.clone()));
    let pending_nonces = std::sync::Arc::new(cb_rpc::PendingNonces::new());
    let verifier = cb_rpc::Verifier::spawn(
        cb_rpc::AdmissionCtx {
            db: db.clone(),
            notify: notify.clone(),
            pending_nonces: Some(pending_nonces.clone()),
            tx_observer: tx_observer.clone(),
        },
        verify_threads,
    )?;
    let server = cb_rpc::SequencerServer {
        verifier: Some(verifier.clone()),
        db: db.clone(),
        notify: notify.clone(),
        vote_handler: None,
        // Under BFT a refused transaction stays in the block with
        // `applied` false; there is nothing to publish separately.
        rejected: None,
        tx_observer,
        pending_nonces: Some(pending_nonces.clone()),
    };
    let bypass = if benchmark {
        let chain_id = Meta::new(&db).chain_id();
        let workers = if bench_workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(bench_traders as usize)
                .max(1)
        } else {
            bench_workers
        };
        Some(cb_node::benchmark::spawn_benchmark(
            &db,
            metrics.clone(),
            cb_node::benchmark::BenchConfig {
                workload: bench_workload,
                chain_id,
                num_traders: bench_traders,
                seed_prefix: bench_seed_prefix,
                rng_seed: bench_rng_seed,
                workers,
                channel_capacity: bench_channel_capacity,
            },
        )?)
    } else {
        None
    };
    let rpc_threads = if rpc_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 8))
            .unwrap_or(2)
    } else {
        rpc_threads
    };
    let _rpc_pool = cb_rpc::run_local_serve_multi(listen, server, rpc_threads)?;
    if !sync_writes {
        db.enable_async_writes()?;
    }
    {
        let dash_db = db.durable_view();
        let dash_metrics = metrics.clone();
        let dash_pruning = cb_dashboard::PruningInfo {
            enabled: keep_blocks > 0,
            keep_recent: keep_blocks,
        };
        std::thread::Builder::new()
            .name("cb-dashboard".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("dashboard runtime");
                if let Err(e) = rt.block_on(cb_dashboard::serve(
                    dash_db,
                    dashboard_listen,
                    dash_metrics,
                    dash_pruning,
                )) {
                    tracing::error!("dashboard stopped: {e}");
                }
            })?;
    }
    let announce_handle = p2p_handle.as_ref().map(|h| {
        tokio::task::spawn_local(cb_node::p2p::announce_new_heads(
            db.durable_view(),
            h.clone(),
            notify.clone(),
        ))
    });
    tracing::info!(%listen, %dashboard_listen, %me, round_timeout_ms, idle_interval_ms, max_block_txs, benchmark, keep_blocks, p2p = p2p.enabled(), "validator running");
    let stack = cb_node::validator::ValidatorStack {
        db,
        ctx,
        notify,
        metrics,
        key: Arc::new(sk),
        me,
        p2p: p2p_handle,
        apply_switch,
        inbound,
        bypass,
        pending_nonces: Some(pending_nonces),
        verifier: Some(verifier),
        outbox,
    };
    let cfg = cb_node::validator::ValidatorCfg {
        round_timeout_ms,
        idle_interval_ms,
        max_block_txs,
        keep_blocks: (keep_blocks > 0).then_some(keep_blocks),
    };
    let run = tokio::task::spawn_local(cb_node::validator::run_validator(stack, cfg));
    match announce_handle {
        Some(announce) => {
            tokio::select! {
                r = run => { r??; }
                r = announce => { r?; }
            }
        }
        None => run.await??,
    }
    Ok(())
}

/// Run this node as the leader until it stops.
async fn run_sequencer_stack(db: Database, opts: SequencerOpts) -> anyhow::Result<()> {
    let SequencerOpts {
        listen,
        dashboard_listen,
        block_interval_ms,
        max_block_txs,
        rpc_threads,
        verify_threads,
        sync_writes,
        key_file,
        dev_key_seed,
        benchmark,
        bench_workload,
        bench_traders,
        bench_seed_prefix,
        bench_rng_seed,
        bench_workers,
        bench_channel_capacity,
        keep_blocks,
        pruning,
        p2p,
    } = opts;
    anyhow::ensure!(
        !Meta::new(&db).is_bft(),
        "this chain was initialised with `--consensus bft`; run `cb-node validator`, not `sequencer`"
    );
    let notify = Arc::new(Notify::new());
    let metrics = Arc::new(Metrics::new());
    let ctx = ExecCtx {
        treasury_signer: Meta::new(&db).company_addr(),
        ..Default::default()
    };

    let (proposer_sk, proposer_addr) = load_or_dev_key(key_file.as_deref(), &dev_key_seed)?;
    // May this key lead right now?
    //
    // The network accepts a block at view V only from `validators[V % N]`, so
    // that is the question to ask, and `Meta::view()` is where the chain's
    // current view is recorded — set by genesis, advanced by every block a
    // node applies, and set by a node that has just taken over. Asking
    // `Meta::proposer_addr` instead, as this did, means only the validator
    // genesis named can ever produce: a node that correctly took over view 1
    // then refused to start, which is how failover was found to be half
    // wired.
    //
    // With no validator set — a single-node dev chain — the genesis proposer
    // is still the only answer there is.
    let view = Meta::new(&db).view();
    let validators = Validators::new(&db);
    let expected = match validators.leader_for_view(view) {
        Some(v) => v.addr,
        None => Meta::new(&db).proposer_addr(),
    };
    if expected == cb_types::Address::ZERO {
        // Funding ticks and settlements must be signed by the
        // recorded proposer. With none recorded no key can sign
        // them, and the chain would stop at its first funding
        // boundary; better to say so now than 3,600 blocks in.
        anyhow::bail!(
            "genesis recorded no proposer address (Meta::proposer_addr is zero); \
             run `init` with a genesis that lists at least one validator"
        );
    }
    if expected != proposer_addr {
        anyhow::bail!(
            "this key derives {proposer_addr}, but view {view} belongs to {expected} \
             — refusing to produce blocks the network would reject. The validator \
             that leads the next view takes over on its own; see --failover-after-ms."
        );
    }
    tracing::info!(view, leader = %proposer_addr, "leading this view");
    let proposer_key = Arc::new(proposer_sk);
    // `pack_seq` numbers transactions within a block in sixteen
    // bits; a larger block gives two orders one book-index key.
    let max_block_txs = if max_block_txs > cb_block::MAX_BLOCK_TXS {
        tracing::warn!(
            requested = max_block_txs,
            clamped = cb_block::MAX_BLOCK_TXS,
            "--max-block-txs exceeds the block size limit; clamping"
        );
        cb_block::MAX_BLOCK_TXS
    } else {
        max_block_txs
    };

    // Shared vote pool: leader self-votes after each block, followers
    // submit theirs via submitVote RPC. QC formation persists to CF_QC.
    let vote_pool = cb_node::sequencer::new_vote_pool();

    // Peer-to-peer comes up before the RPC server so a locally
    // submitted order can be handed straight to gossip.
    let p2p_handle = if p2p.enabled() {
        Some(cb_node::p2p::start(&db, ctx, &p2p, &dev_key_seed, Some(notify.clone())).await?)
    } else {
        None
    };
    let tx_observer: Option<cb_rpc::TxObserver> = p2p_handle.as_ref().map(|h| {
        let h = h.clone();
        std::sync::Arc::new(move |hash, bytes| h.announce_tx(hash, bytes)) as cb_rpc::TxObserver
    });

    // One nonce ledger, shared by the RPC server that admits
    // transactions and the block loop that learns which of them
    // the executor refused. Without the second half, a rejected
    // transaction leaves the ledger permanently ahead of the chain
    // and the account can never submit again.
    let pending_nonces = std::sync::Arc::new(cb_rpc::PendingNonces::new());

    let verifier = cb_rpc::Verifier::spawn(
        cb_rpc::AdmissionCtx {
            db: db.clone(),
            notify: notify.clone(),
            pending_nonces: Some(pending_nonces.clone()),
            tx_observer: tx_observer.clone(),
        },
        verify_threads,
    )?;
    // Refusals the sequencer drops from its blocks, published on
    // `streamRejected` so a gateway can report them instead of waiting.
    let rejected_log = cb_rpc::RejectedLog::new(4096);
    let server = cb_rpc::SequencerServer {
        verifier: Some(verifier),
        db: db.clone(),
        notify: notify.clone(),
        rejected: Some(rejected_log.clone()),
        vote_handler: Some(std::sync::Arc::new(
            cb_node::sequencer::SequencerVoteHandler {
                // A follower votes for a block it received from
                // the durable stream, so its vote is checked
                // against the same view: a vote must never be
                // counted for a block that is not yet on disk.
                db: db.durable_view(),
                pool: vote_pool.clone(),
            },
        )),
        tx_observer,
        pending_nonces: Some(pending_nonces.clone()),
    };

    let bypass = if benchmark {
        let chain_id = Meta::new(&db).chain_id();
        let workers = if bench_workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(bench_traders as usize)
                .max(1)
        } else {
            bench_workers
        };
        let bench_cfg = cb_node::benchmark::BenchConfig {
            workload: bench_workload,
            chain_id,
            num_traders: bench_traders,
            seed_prefix: bench_seed_prefix,
            rng_seed: bench_rng_seed,
            workers,
            channel_capacity: bench_channel_capacity,
        };
        Some(cb_node::benchmark::spawn_benchmark(
            &db,
            metrics.clone(),
            bench_cfg,
        )?)
    } else {
        None
    };

    // `--pruning` is meaningless now; accept it and say so once.
    if pruning {
        tracing::warn!(
            "--pruning is deprecated: pruning is on by default. Use --keep-blocks N, or --keep-blocks 0 to keep every block."
        );
    }
    let pruning_cfg = cb_node::sequencer::PruningCfg {
        keep_recent: (keep_blocks > 0).then_some(keep_blocks),
    };
    let dash_pruning = cb_dashboard::PruningInfo {
        enabled: keep_blocks > 0,
        keep_recent: keep_blocks,
    };

    // RPC gets its own threads. It used to share this `LocalSet`
    // with the block builder, which meant a block spent scanning
    // was a block during which no transaction could be admitted.
    let rpc_threads = if rpc_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(1, 8))
            .unwrap_or(2)
    } else {
        rpc_threads
    };
    let _rpc_pool = cb_rpc::run_local_serve_multi(listen, server, rpc_threads)?;
    if !sync_writes {
        db.enable_async_writes()?;
    }
    let seq_handle = tokio::task::spawn_local(sequencer::run_sequencer(
        db.clone(),
        ctx,
        notify.clone(),
        block_interval_ms,
        max_block_txs,
        metrics.clone(),
        bypass,
        pruning_cfg,
        proposer_key,
        Some(vote_pool),
        Some(pending_nonces),
        Some(rejected_log),
    ));
    // The dashboard gets a thread of its own. It used to share
    // the block builder's `LocalSet`, and under a saturating load
    // the builder never yielded long enough for an HTTP request
    // to be answered — the one moment the dashboard is needed.
    {
        let dash_db = db.durable_view();
        let dash_metrics = metrics.clone();
        std::thread::Builder::new()
            .name("cb-dashboard".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("dashboard runtime");
                if let Err(e) = rt.block_on(cb_dashboard::serve(
                    dash_db,
                    dashboard_listen,
                    dash_metrics,
                    dash_pruning,
                )) {
                    tracing::error!("dashboard stopped: {e}");
                }
            })?;
    }

    // Gossip the head as it moves. A watcher rather than a hook
    // inside `run_sequencer`: block production must not be able to
    // block on the network, and the producer already notifies.
    let announce_handle = p2p_handle.as_ref().map(|h| {
        tokio::task::spawn_local(cb_node::p2p::announce_new_heads(
            db.durable_view(),
            h.clone(),
            notify.clone(),
        ))
    });

    tracing::info!(%listen, %dashboard_listen, block_interval_ms, %proposer_addr, benchmark, keep_blocks, rpc_threads, async_writes = !sync_writes, p2p = p2p.enabled(), "sequencer running");
    match announce_handle {
        Some(announce) => {
            tokio::select! {
                r = seq_handle => { r??; }
                r = announce => { r?; }
            }
        }
        None => {
            seq_handle.await??;
        }
    }
    Ok(())
}

#[derive(Subcommand)]
enum Mode {
    /// Initialize the chain from a genesis TOML.
    /// Print this node's consensus state: the view, the head, the highest
    /// certified block, and the validator set in the order that decides who
    /// leads which view. The question an operator asks when a leader dies is
    /// "who is next", and this answers it without a running node.
    Consensus,
    Init {
        #[arg(long)]
        genesis: PathBuf,
        /// How this chain orders blocks: `sequencer` (one block producer,
        /// replicas follow it) or `bft` (the validators in genesis run
        /// `cb-node validator`; blocks are ordered by quorum, ADR 0008).
        /// Fixed for the life of the data directory.
        #[arg(long, default_value = "sequencer")]
        consensus: String,
    },
    /// Run as the sequencer (single block producer).
    Sequencer {
        #[arg(long, default_value = "0.0.0.0:7700")]
        listen: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:8080")]
        dashboard_listen: SocketAddr,
        #[arg(long, default_value_t = 1000)]
        block_interval_ms: u64,
        #[arg(long, default_value_t = 10_000)]
        max_block_txs: usize,
        /// Path to a hex-encoded 32-byte secp256k1 secret key. If absent, a deterministic
        /// dev key derived from the `--dev-key-seed` is used and a warning is logged.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Seed used to derive a dev key when `--key-file` is not provided. Must match
        /// one of the `seed` values in genesis.jsonl's `validator` records.
        #[arg(long, default_value = "cb-validator-0")]
        dev_key_seed: String,
        /// Run an in-process benchmark generator alongside the sequencer.
        #[arg(long, default_value_t = false)]
        benchmark: bool,
        /// What the generator signs: `transfers` (transaction throughput) or
        /// `orders` (buyers and sellers, pre-verified, straight into the
        /// matcher — the pure stress test).
        #[arg(long, default_value = "transfers")]
        bench_workload: cb_node::benchmark::BenchWorkload,
        /// Number of synthetic traders for the in-process benchmark. Genesis must
        /// pre-fund at least this many under `bench_seed_prefix`.
        #[arg(long, default_value_t = 8)]
        bench_traders: u32,
        #[arg(long, default_value = "cb-sim-trader-")]
        bench_seed_prefix: String,
        #[arg(long, default_value_t = 0xC0FFEE)]
        bench_rng_seed: u64,
        /// Number of OS threads producing signed txs in parallel. 0 = auto-detect from
        /// available CPU cores.
        #[arg(long, default_value_t = 0)]
        bench_workers: usize,
        /// Bounded channel capacity feeding the sequencer (= max in-flight pre-verified
        /// txs). Producers block on send when full → automatic backpressure.
        #[arg(long, default_value_t = 50_000)]
        bench_channel_capacity: usize,
        /// Threads serving RPC connections. Each drives its own connections on
        /// its own runtime, so signature recovery at admission scales with
        /// clients instead of sharing the block builder's thread. 0 picks half
        /// the machine's cores, capped at 8.
        #[arg(long, default_value_t = 0)]
        rpc_threads: usize,
        /// Threads recovering signatures at admission, across all connections
        /// (0 = every core). Admission itself stays in arrival order; see
        /// `cb_rpc::verify`.
        #[arg(long, default_value_t = 0)]
        verify_threads: usize,
        /// Write every block to RocksDB before producing the next one. By
        /// default the write happens on its own thread while the matcher goes
        /// on; nothing is announced until it lands either way.
        #[arg(long, default_value_t = false)]
        sync_writes: bool,
        /// How many recent blocks keep their bodies. `0` keeps every block
        /// for ever.
        ///
        /// Pruning is on by default because a node does not need old block
        /// bodies to serve the chain: account state, the tries, the markets
        /// and the head are never pruned, and everything a client asks about
        /// the present is answered from those. Bodies are the archive, and an
        /// archive is a choice a node makes, not a default it inherits. A
        /// stress run writes tens of thousands of orders into every block;
        /// keeping them all took one afternoon's testing to 79 GB.
        #[arg(long, default_value_t = 10, alias = "pruning-keep-recent")]
        keep_blocks: u64,
        /// Deprecated: pruning is on by default now. Kept so existing
        /// commands still parse; pass `--keep-blocks 0` to keep everything.
        #[arg(long, default_value_t = false, hide = true)]
        pruning: bool,
        /// Peer-to-peer networking. Off unless a listen address, a seed or a
        /// config file is supplied; the legacy RPC block stream keeps working
        /// either way, so an existing cluster is unaffected by this flag's
        /// absence.
        #[command(flatten)]
        p2p: cb_node::p2p::P2pArgs,
    },
    /// Run as one validator of a BFT cluster: propose in turn, vote on the
    /// others' proposals, execute what a quorum commits. Needs a data
    /// directory initialised with `init --consensus bft` and, with more than
    /// one validator, the peer-to-peer flags to reach the others.
    Validator {
        #[arg(long, default_value = "0.0.0.0:7700")]
        listen: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:8080")]
        dashboard_listen: SocketAddr,
        /// Most transactions in one proposal.
        #[arg(long, default_value_t = 10_000)]
        max_block_txs: usize,
        /// Path to a hex-encoded 32-byte secp256k1 secret key: this
        /// validator's identity in genesis.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Seed for the deterministic dev key when `--key-file` is absent;
        /// one of the `seed` values in genesis.jsonl's `validator` records.
        #[arg(long, default_value = "cb-validator-0")]
        dev_key_seed: String,
        /// Run an in-process benchmark generator alongside the validator.
        /// Its transactions enter this validator's proposals pre-verified;
        /// the other validators recover every signature before voting.
        #[arg(long, default_value_t = false)]
        benchmark: bool,
        #[arg(long, default_value = "transfers")]
        bench_workload: cb_node::benchmark::BenchWorkload,
        #[arg(long, default_value_t = 8)]
        bench_traders: u32,
        #[arg(long, default_value = "cb-sim-trader-")]
        bench_seed_prefix: String,
        #[arg(long, default_value_t = 0xC0FFEE)]
        bench_rng_seed: u64,
        #[arg(long, default_value_t = 0)]
        bench_workers: usize,
        #[arg(long, default_value_t = 50_000)]
        bench_channel_capacity: usize,
        #[arg(long, default_value_t = 0)]
        rpc_threads: usize,
        #[arg(long, default_value_t = 0)]
        verify_threads: usize,
        #[arg(long, default_value_t = false)]
        sync_writes: bool,
        /// How many recent blocks keep their bodies. `0` keeps every block.
        #[arg(long, default_value_t = 10)]
        keep_blocks: u64,
        /// A round with no certificate by then is timed out and the next
        /// validator leads.
        #[arg(long, default_value_t = 1000)]
        round_timeout_ms: u64,
        /// How long an idle leader waits before an empty round on top of an
        /// empty parent. A lone order does not wait: a non-empty parent is
        /// followed at once, since the next certificate commits it.
        #[arg(long, default_value_t = 100)]
        idle_interval_ms: u64,
        #[command(flatten)]
        p2p: cb_node::p2p::P2pArgs,
    },
    /// Follow the chain without producing blocks: either from one configured
    /// sequencer (the legacy star) or from the peer-to-peer network.
    Replica {
        /// Legacy star topology: pull blocks from this one sequencer over RPC.
        /// Mutually exclusive with the peer-to-peer flags — running both would
        /// put two writers on the same database.
        #[arg(long)]
        sequencer: Option<SocketAddr>,
        /// Optional HTTP dashboard so verification scripts can poll head_height /
        /// state_root from this replica directly. If omitted, the replica logs its
        /// progress to stderr but exposes no HTTP endpoint.
        #[arg(long)]
        dashboard_listen: Option<SocketAddr>,
        /// Path to a hex-encoded 32-byte secp256k1 secret key. Used to sign
        /// votes that go back to the leader. If absent, the dev key derived
        /// from `--dev-key-seed` is used.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Seed for the deterministic dev voter key when `--key-file` is not
        /// provided. Must match this replica's entry in genesis.jsonl.
        #[arg(long, default_value = "cb-validator-1")]
        dev_key_seed: String,
        /// If true, do NOT submit votes after applying blocks. Useful when a
        /// non-validator observer is running as a pure follower.
        #[arg(long, default_value_t = false)]
        no_voting: bool,
        /// Strict-sync mode: decode + re-execute every block, recompute
        /// state_root, await each vote ack before pulling the next block.
        /// Default (false) is fast-mirror: store the leader's bytes, fire a
        /// vote, move on. Strict mode is required when the replica must serve
        /// account-state queries that match the leader byte-for-byte (e.g. the
        /// supervisord integration test); fast-mirror mode tracks the leader's
        /// claimed state_root in Meta but never updates the accounts CF.
        #[arg(long, default_value_t = false)]
        strict_sync: bool,
        /// Reserved for a future async-verify path: when set together with
        /// !strict_sync, run a background apply task to catch leader divergence
        /// without blocking the receive loop. Currently a no-op flag — the
        /// fast-mirror path does not verify in any mode.
        #[arg(long, default_value_t = false)]
        async_verify: bool,
        /// Accept client transactions on this address. Only meaningful in
        /// peer-to-peer mode, where a submitted order is gossiped onward to
        /// the sequencer instead of dying in a mempool nobody drains.
        #[arg(long)]
        rpc_listen: Option<SocketAddr>,
        /// How many recent blocks keep their bodies. `0` keeps every block.
        /// A replica used to keep all of them while the leader pruned.
        #[arg(long, default_value_t = 10, alias = "pruning-keep-recent")]
        keep_blocks: u64,
        /// Other validators' RPC addresses, repeatable. When the leader
        /// goes, a follower that is not next in line looks here for whoever
        /// is: the validator set records identities, not endpoints.
        #[arg(long = "peer")]
        peers: Vec<SocketAddr>,
        /// Where this node listens for transactions once it takes over. A
        /// standby with no RPC address can lead but nothing can reach it, so
        /// failover without this is only half a handover.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Block interval to use once this node takes over.
        #[arg(long, default_value_t = 100)]
        block_interval_ms: u64,
        /// Take over as leader when no block has arrived for this many
        /// milliseconds and this node is the validator that leads the next
        /// view. `0` never takes over.
        ///
        /// The window has to clear the quiet periods a live chain has anyway,
        /// so it is a flag and not a constant. Needs `--strict-sync`: a
        /// mirror keeps no state to lead from. See `cb_node::failover`.
        #[arg(long, default_value_t = 0)]
        failover_after_ms: u64,

        /// Peer-to-peer networking. Supplying any of these switches the
        /// replica off the legacy `--sequencer` star and onto gossip, where
        /// every block is re-executed and its state root checked before it is
        /// adopted.
        #[command(flatten)]
        p2p: cb_node::p2p::P2pArgs,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir)?;
    let db = Database::open(&cli.data_dir)?;
    {
        // A directory whose genesis ran under another state-commitment scheme
        // has roots no node running this binary will reproduce. Refuse it by
        // name rather than fail at the first block with a root mismatch.
        let meta = Meta::new(&db);
        let scheme = meta.state_hash_scheme();
        anyhow::ensure!(
            meta.chain_id() == 0 || scheme == cb_state::STATE_HASH_SCHEME,
            "{} was built with state-commitment scheme {scheme} (0 = merkle tries); this node \
             uses scheme {} (a hash chain over each block's changes, ADR 0007). \
             Move the directory aside and run genesis again.",
            cli.data_dir.display(),
            cb_state::STATE_HASH_SCHEME,
        );
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = LocalSet::new();

    runtime.block_on(local.run_until(async move {
        match cli.mode {
            Mode::Consensus => {
                let m = Meta::new(&db);
                let v = Validators::new(&db);
                let n = v.count();
                println!("chain_id    {}", m.chain_id());
                println!("head        {}", m.head_height());
                println!("last_qc     {}", m.last_qc_height());
                println!("view        {}", m.view());
                println!(
                    "safe to lead {}",
                    if m.head_height() == m.last_qc_height() {
                        "yes"
                    } else {
                        "no: head is above the last certified block"
                    }
                );
                println!("validators  {n}");
                for (i, info) in v.list().iter().enumerate() {
                    let leads_now = n > 0 && (m.view() % n) == i as u64;
                    let leads_next = n > 0 && ((m.view() + 1) % n) == i as u64;
                    let mark = match (leads_now, leads_next) {
                        (true, _) => "  <- leads this view",
                        (_, true) => "  <- leads the next view",
                        _ => "",
                    };
                    println!("  [{i}] {}{mark}", info.addr);
                }
            }
            Mode::Init { genesis, consensus } => {
                let scheme = match consensus.as_str() {
                    "sequencer" => cb_state::CONSENSUS_SEQUENCER,
                    "bft" => cb_state::CONSENSUS_BFT,
                    other => anyhow::bail!("--consensus must be `sequencer` or `bft`, not `{other}`"),
                };
                let spec = cb_genesis::load(&genesis)?;
                let _ = cb_genesis::apply(&db, &spec)?;
                Meta::new(&db).set_consensus_scheme(scheme);
                let m = Meta::new(&db);
                let v = Validators::new(&db);
                tracing::info!(
                    chain_id = m.chain_id(),
                    state_root = %m.state_root(),
                    proposer = %m.proposer_addr(),
                    validators = v.count(),
                    consensus = %consensus,
                    "genesis applied"
                );
                for info in v.list() {
                    tracing::info!(addr = %info.addr, weight = info.weight, "  validator");
                }
            }
            Mode::Sequencer {
                listen,
                dashboard_listen,
                block_interval_ms,
                max_block_txs,
                rpc_threads,
                verify_threads,
                sync_writes,
                key_file,
                dev_key_seed,
                benchmark,
                bench_workload,
                bench_traders,
                bench_seed_prefix,
                bench_rng_seed,
                bench_workers,
                bench_channel_capacity,
                keep_blocks,
                pruning,
                p2p,
            } => {
                run_sequencer_stack(
                    db,
                    SequencerOpts {
                        listen,
                        dashboard_listen,
                        block_interval_ms,
                        max_block_txs,
                        rpc_threads,
                        verify_threads,
                        sync_writes,
                        key_file,
                        dev_key_seed,
                        benchmark,
                        bench_workload,
                        bench_traders,
                        bench_seed_prefix,
                        bench_rng_seed,
                        bench_workers,
                        bench_channel_capacity,
                        keep_blocks,
                        pruning,
                        p2p,
                    },
                )
                .await?;
            }
            Mode::Validator {
                listen,
                dashboard_listen,
                max_block_txs,
                key_file,
                dev_key_seed,
                benchmark,
                bench_workload,
                bench_traders,
                bench_seed_prefix,
                bench_rng_seed,
                bench_workers,
                bench_channel_capacity,
                rpc_threads,
                verify_threads,
                sync_writes,
                keep_blocks,
                round_timeout_ms,
                idle_interval_ms,
                p2p,
            } => {
                run_validator_stack(
                    db,
                    ValidatorOpts {
                        listen,
                        dashboard_listen,
                        max_block_txs,
                        key_file,
                        dev_key_seed,
                        benchmark,
                        bench_workload,
                        bench_traders,
                        bench_seed_prefix,
                        bench_rng_seed,
                        bench_workers,
                        bench_channel_capacity,
                        rpc_threads,
                        verify_threads,
                        sync_writes,
                        keep_blocks,
                        round_timeout_ms,
                        idle_interval_ms,
                        p2p,
                    },
                )
                .await?;
            }
            Mode::Replica {
                sequencer,
                dashboard_listen,
                key_file,
                dev_key_seed,
                no_voting,
                strict_sync,
                async_verify,
                rpc_listen,
                keep_blocks,
                peers,
                listen,
                block_interval_ms,
                failover_after_ms,
                p2p,
            } => {
                let ctx = ExecCtx {
                    treasury_signer: Meta::new(&db).company_addr(),
                    ..Default::default()
                };
                // The two sync paths both write blocks and the head pointer.
                // Running them together would race; refusing is the only
                // honest answer.
                if sequencer.is_some() && p2p.enabled() {
                    anyhow::bail!(
                        "--sequencer (legacy star sync) and the --p2p-* flags are mutually                          exclusive: both write blocks and the head pointer"
                    );
                }
                if sequencer.is_none() && !p2p.enabled() {
                    anyhow::bail!(
                        "a replica needs a source of blocks: pass --sequencer for the legacy                          star, or --p2p-listen/--p2p-seed for peer-to-peer sync"
                    );
                }
                // The address is kept as well as the key: failover asks who
                // this node is, and without an identity in the validator set
                // it can never be anyone's next leader.
                let mut voter_addr: Option<cb_types::Address> = None;
                let voter_key = if no_voting {
                    None
                } else {
                    let (sk, addr) = load_or_dev_key(key_file.as_deref(), &dev_key_seed)?;
                    // If we know the validator set, refuse to start with a key
                    // that isn't in it — much cheaper to fail-fast here than
                    // have every vote rejected.
                    if Validators::new(&db).count() > 0
                        && Validators::new(&db).get_by_addr(&addr).is_none()
                    {
                        anyhow::bail!(
                            "loaded voter key derives address {addr} but it is not in the \
                             genesis validator set — refusing to start"
                        );
                    }
                    tracing::info!(%addr, "voter key loaded");
                    voter_addr = Some(addr);
                    Some(Arc::new(sk))
                };
                let (dash_handle, metrics_for_replica) = if let Some(addr) = dashboard_listen {
                    let metrics = Arc::new(Metrics::new());
                    let dash_pruning = cb_dashboard::PruningInfo {
                        enabled: keep_blocks > 0,
                        keep_recent: keep_blocks,
                    };
                    let handle = tokio::task::spawn_local(cb_dashboard::serve(
                        db.clone(),
                        addr,
                        metrics.clone(),
                        dash_pruning,
                    ));
                    (Some(handle), Some(metrics))
                } else {
                    (None, None)
                };
                if let Some(sequencer) = sequencer {
                    tracing::info!(
                        %sequencer,
                        strict_sync,
                        async_verify,
                        "replica connecting (legacy star sync); no RPC tx ingress"
                    );
                    // Failover needs to know who this node is; without a
                    // voter key it has no identity in the validator set and
                    // cannot be anyone's next leader.
                    let failover = match (failover_after_ms, voter_addr) {
                        (0, _) => None,
                        (_, None) => {
                            tracing::warn!(
                                "--failover-after-ms is set but this replica has no voter key, so it has no identity in the validator set; it will not take over"
                            );
                            None
                        }
                        (ms, Some(me)) => Some(replica::FailoverCfg {
                            me,
                            silence: std::time::Duration::from_millis(ms),
                        }),
                    };
                    let replica_handle = tokio::task::spawn_local(replica::run_replica(
                        db.clone(),
                        ctx,
                        sequencer,
                        voter_key,
                        metrics_for_replica,
                        strict_sync,
                        async_verify,
                        (keep_blocks > 0).then_some(keep_blocks),
                        failover,
                        peers,
                    ));
                    // The dashboard is held, not consumed: if this node
                    // takes over it must stop serving on that address before
                    // the sequencer stack tries to bind the same one.
                    let mut dash_task = dash_handle;
                    let exit = match dash_task.take() {
                        Some(dash) => {
                            tokio::select! {
                                r = replica_handle => { r?? }
                                r = dash => { r??; replica::Exit::StreamEnded }
                            }
                        }
                        None => replica_handle.await??,
                    };
                    if let replica::Exit::Promote { view } = exit {
                        // Take over here, in this process, on this database.
                        // Nothing is restarted and nothing is handed to an
                        // operator: the node records the view it is leading —
                        // which is what makes every follower check its
                        // signature against the right validator — and enters
                        // the same code path `cb-node sequencer` enters.
                        let Some(listen) = listen else {
                            anyhow::bail!(
                                "this node leads view {view} but has no --listen address, so nothing could submit to it. Give every standby a --listen address."
                            );
                        };
                        // The replica's dashboard is still bound to the
                        // address the sequencer stack is about to take.
                        if let Some(d) = dash_task.take() {
                            d.abort();
                            let _ = d.await;
                        }
                        cb_node::failover::take_view(&db, view);
                        tracing::warn!(
                            view,
                            %listen,
                            "taking over as leader in this process"
                        );
                        return run_sequencer_stack(
                            db,
                            SequencerOpts {
                                listen,
                                // The address it was already serving on: an
                                // operator watching this node should not have
                                // to find a new port because it was promoted.
                                dashboard_listen: dashboard_listen
                                    .unwrap_or_else(|| "127.0.0.1:0".parse().expect("valid addr")),
                                block_interval_ms,
                                max_block_txs: 10_000,
                                rpc_threads: 0,
                                verify_threads: 0,
                                sync_writes: false,
                                key_file,
                                dev_key_seed,
                                benchmark: false,
                                bench_workload: cb_node::benchmark::BenchWorkload::Transfers,
                                bench_traders: 8,
                                bench_seed_prefix: "cb-sim-trader-".to_string(),
                                bench_rng_seed: 0xC0FFEE,
                                bench_workers: 0,
                                bench_channel_capacity: 50_000,
                                keep_blocks,
                                pruning: false,
                                p2p,
                            },
                        )
                        .await;
                    }
                } else {
                    // Peer-to-peer sync. Unlike the legacy fast-mirror path,
                    // every block here is re-executed and its state root
                    // recomputed before it is adopted — there is no mode in
                    // which a gossiped block is taken on a peer's word.
                    let notify = Arc::new(Notify::new());
                    let handle =
                        cb_node::p2p::start(&db, ctx, &p2p, &dev_key_seed, Some(notify.clone()))
                            .await?;
                    tracing::info!(
                        node_id = %handle.node_id(),
                        listen = ?handle.listen_addr(),
                        "replica running peer-to-peer; every block is verified before adoption"
                    );
                    let rpc_handle = match rpc_listen {
                        Some(addr) => {
                            let h = handle.clone();
                            let observer: cb_rpc::TxObserver =
                                std::sync::Arc::new(move |hash, bytes| h.announce_tx(hash, bytes));
                            let mut server =
                                cb_rpc::SequencerServer::without_voting(db.clone(), notify.clone());
                            server.tx_observer = Some(observer);
                            tracing::info!(%addr, "replica accepting client transactions; they                                              are gossiped onward to the sequencer");
                            Some(tokio::task::spawn_local(cb_rpc::serve(addr, server)))
                        }
                        None => None,
                    };
                    // Known gap, stated rather than hidden: consensus votes
                    // still travel to the leader over its RPC, and a
                    // peer-to-peer replica holds no such connection. Carrying
                    // votes and quorum certificates over gossip is consensus
                    // work, not sync work, and is deliberately out of scope
                    // here — but an operator must not be left thinking this
                    // node is voting when it is not.
                    if voter_key.is_some() {
                        tracing::warn!(
                            "peer-to-peer replicas do not submit consensus votes yet; this node                              follows and verifies the chain but contributes no quorum weight"
                        );
                    }
                    if strict_sync || async_verify {
                        tracing::warn!(
                            "--strict-sync and --async-verify describe the legacy star sync and                              have no effect in peer-to-peer mode, where every block is verified"
                        );
                    }
                    let _ = metrics_for_replica;
                    match (dash_handle, rpc_handle) {
                        (Some(dash), Some(rpc)) => tokio::select! {
                            r = dash => { r??; }
                            r = rpc => { r??; }
                        },
                        (Some(dash), None) => dash.await??,
                        (None, Some(rpc)) => rpc.await??,
                        (None, None) => std::future::pending::<()>().await,
                    }
                }
            }
        }
        anyhow::Ok(())
    }))?;
    Ok(())
}

mod sequencer {
    pub use cb_node::sequencer::*;
}
mod replica {
    pub use cb_node::replica::*;
}
