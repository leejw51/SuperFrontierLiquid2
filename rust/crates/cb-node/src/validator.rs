//! The validator: one node's driver around the BFT engine.
//!
//! `cb_consensus::bft::Engine` decides ordering and knows nothing about
//! transactions, databases or sockets. This module is everything around it:
//!
//! * **Payloads.** A proposal's payload is `bincode(Vec<SignedTx>)`, in the
//!   canonical intra-block order for its parent's seed. Before the engine
//!   sees a peer's proposal, the payload is decoded, every signature is
//!   recovered on the rayon pool, the chain id and static rules are checked,
//!   the order is checked, and the proposer's claimed executed root is
//!   compared with this node's own. Any failure is a *veto*: the engine
//!   records the proposal but never votes for it.
//!
//! * **Proposing.** When the engine says this node leads the round, the
//!   driver drains the mempool (and the benchmark generator's channel, when
//!   one is attached), orders the transactions, and proposes. An empty
//!   mempool proposes an empty round at once if the parent carried
//!   transactions (the next certificate is what commits the parent), or
//!   after `idle_interval` otherwise, so an idle chain turns over slowly
//!   and a lone order still commits in three message hops.
//!
//! * **Execution.** Committed proposals go to an executor thread, in order,
//!   through the same pipelined `execute_and_hand_off` the sequencer uses,
//!   with the origin (seed, proposer) taken from the proposal and the
//!   finality proof sealed into the header. Nothing on the consensus path
//!   waits for execution, and nothing executed is ever rolled back.
//!
//! * **Restart.** A validator starts its tree at the last block it
//!   executed, read back from that block's finality proof, and asks peers
//!   for the proposals since. Beyond the peers' retention window it needs
//!   block sync first, which is not wired for validators yet.

use crate::bft_writer::{BlockWriter, Outcome, Roots};
use cb_block::{order_for_block, Block, ExecCtx, ExecError};
use cb_consensus::bft::{
    Action, Committed, Config as BftConfig, Engine, Finality, Message, Proposal, ProposalHeader,
    QuorumCert, Reputation, Root, Round, ValidatorSet,
};
use cb_crypto::{keccak256, SecretKey};
use cb_dashboard::Metrics;
use cb_net::p2p::identity::GenesisId;
use cb_net::p2p::{ConsensusInbound, P2pHandle};
use cb_rpc::PendingNonces;
use cb_state::{Blocks, Database, Mempool, Meta, Validators};
use cb_tx::{MempoolRecord, SignedTx, VerifiedTx};
use cb_types::{Address, H256};
use crossbeam_channel::Receiver;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

/// Tunables for one validator.
#[derive(Clone, Debug)]
pub struct ValidatorCfg {
    /// Round timer; a round with no certificate by then is timed out.
    pub round_timeout_ms: u64,
    /// How long an idle leader waits before proposing an empty round on top
    /// of an empty parent.
    pub idle_interval_ms: u64,
    pub max_block_txs: usize,
    /// Block bodies kept; `None` keeps every one.
    pub keep_blocks: Option<u64>,
}

/// What the node hands the driver.
pub struct ValidatorStack {
    pub db: Database,
    pub ctx: ExecCtx,
    /// Woken by the RPC server and gossip when a transaction arrives, and by
    /// the executor when a block lands.
    pub notify: Arc<Notify>,
    pub metrics: Arc<Metrics>,
    pub key: Arc<SecretKey>,
    pub me: Address,
    pub p2p: Option<P2pHandle>,
    /// The gossip chain's apply switch: on while catching up by executed
    /// blocks, off otherwise.
    pub apply_switch: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub inbound: mpsc::UnboundedReceiver<ConsensusInbound>,
    /// Pre-verified transactions from the in-process benchmark generator.
    pub bypass: Option<Receiver<VerifiedTx>>,
    pub pending_nonces: Option<Arc<PendingNonces>>,
    /// The admission pipeline, for transactions other validators forward.
    pub verifier: Option<Arc<cb_rpc::Verifier>>,
    /// Transactions this node admitted and has not yet forwarded to the
    /// other validators. The RPC server's observer fills it; the driver
    /// flushes it as one `TxBatch` every few milliseconds.
    pub outbox: Outbox,
}

/// Admitted transactions waiting to be forwarded, as `SignedTx` bincode.
pub type Outbox = Arc<Mutex<Vec<Vec<u8>>>>;

/// An observer for the RPC admission pipeline that fills `outbox`.
pub fn outbox_observer(outbox: Outbox) -> cb_rpc::TxObserver {
    Arc::new(move |_hash: H256, bytes: Vec<u8>| {
        outbox.lock().expect("outbox").push(bytes);
    })
}

/// `payload_hash` for the engine: the transaction root of the payload.
///
/// # Why it no longer decodes the payload
///
/// The transaction root is `keccak(bincode(tx_0) ‖ … ‖ bincode(tx_n))`
/// ([`Block::compute_tx_root`]), and a payload is `bincode(Vec<SignedTx>)`,
/// which is an 8-byte little-endian count followed by exactly those
/// encodings with no framing between them. So the root is the keccak of the
/// payload after its first eight bytes. This used to deserialise the whole
/// payload and re-encode every transaction to get there, and the engine
/// calls it twice for each of the leader's own proposals and once for every
/// peer's, all on the consensus thread: at ten thousand transactions that
/// was two decodes and two re-encodes of a block per proposal on the path
/// every commit waits for.
///
/// The identity holds for any payload that is the canonical encoding of the
/// transactions it decodes to, which is every payload an honest proposer
/// builds ([`encode_payload`]) and the only kind [`validate_proposal`] lets
/// a vote through for: it refuses a payload with trailing bytes or a
/// transaction that does not re-encode to the bytes it was read from. A
/// payload that is not canonical may hash differently here than under the
/// old definition, and it is vetoed either way, so no certificate — and no
/// executed block — can carry one. `payload_root_is_the_tx_root_of_the_decoded_payload`
/// pins the identity against `compute_tx_root`.
pub fn payload_root(bytes: &[u8]) -> H256 {
    match bytes.get(8..) {
        Some(body) => keccak256(body),
        None => keccak256(bytes),
    }
}

/// A proposal payload for `txs`, in their order, and each transaction's
/// hash.
///
/// The bytes are `bincode::serialize(&Vec<SignedTx>)` exactly — the count,
/// then each transaction's encoding — written straight from `txs` so the
/// block's transactions are neither cloned into a `Vec<SignedTx>` first nor
/// encoded a second time to hash them: each hash is the keccak of the slice
/// of the payload that transaction occupies, which is what
/// [`SignedTx::hash`] computes. An empty list is an empty payload, as the
/// engine expects for an empty round. Pinned by
/// `encode_payload_matches_bincode_and_signed_tx_hash`.
pub fn encode_payload(txs: &[VerifiedTx]) -> (Vec<u8>, Vec<H256>) {
    use rayon::prelude::*;
    if txs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut payload = Vec::with_capacity(8 + txs.len() * 192);
    payload.extend_from_slice(&(txs.len() as u64).to_le_bytes());
    let mut offsets = Vec::with_capacity(txs.len() + 1);
    offsets.push(payload.len());
    for v in txs {
        bincode::serialize_into(&mut payload, &v.signed).expect("SignedTx encode");
        offsets.push(payload.len());
    }
    let hashes = offsets
        .par_windows(2)
        .map(|w| keccak256(&payload[w[0]..w[1]]))
        .collect();
    (payload, hashes)
}

/// The genesis identity every round-0 certificate names: the chain
/// fingerprint the peer-to-peer handshake also uses.
pub fn genesis_id(db: &Database) -> H256 {
    GenesisId::from_db(db).0
}

/// Where this node's tree starts: the proposal behind the last executed
/// block, or the genesis on a fresh chain.
pub fn root_from_db(db: &Database) -> Root {
    let head = Meta::new(db).head_height();
    if head > 0 {
        if let Some(bytes) = Blocks::new(db).get(head) {
            if let Ok(block) = Block::decode(&bytes) {
                if let Ok(fin) = Finality::decode(&block.header.parent_qc) {
                    return Root::from_finality(&fin);
                }
            }
        }
        tracing::warn!(
            head,
            "the executed head carries no finality proof; starting the tree at genesis"
        );
    }
    Root::genesis(genesis_id(db))
}

/// Where the safety record lives: the highest round this validator voted
/// or timed out in, and its highest certificate at that moment.
const SAFETY_KEY: &[u8] = b"bft_safety";

/// Write the safety record before a vote, timeout or proposal leaves the
/// process (see `Engine::restore_safety` for why a restart needs it).
///
/// Straight to RocksDB, not through the pending-write queue the executor
/// uses: a write that is still queued when the process dies is lost, and
/// this one must not be. It goes through RocksDB's write-ahead log without
/// an fsync, so it survives the process being killed; losing power before
/// the operating system flushes it is not covered. The record only ever
/// moves up, so writing it on every message is never wrong, only a few
/// hundred bytes a round.
pub fn persist_safety(db: &Database, engine: &Engine) -> anyhow::Result<()> {
    let bytes = bincode::serialize(&(engine.last_voted_round(), engine.high_qc()))?;
    db.put(cb_state::CF_META, SAFETY_KEY, &bytes)?;
    Ok(())
}

/// The safety record a previous run wrote, if any.
pub fn load_safety(db: &Database) -> Option<(Round, QuorumCert)> {
    let bytes = db.get(cb_state::CF_META, SAFETY_KEY).ok()??;
    match bincode::deserialize(&bytes) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!(
                ?e,
                "the stored consensus safety record does not decode; ignoring it"
            );
            None
        }
    }
}

/// Where the leader-reputation snapshot lives: who stands aside from
/// leading as of this validator's commit point (see `Engine::leader_for`).
const REPUTATION_KEY: &[u8] = b"bft_reputation";

/// Write the reputation snapshot at the engine's commit point. The driver
/// calls this whenever the commit point moves, before the commits it
/// produced reach the executor, so the snapshot on disk is never older
/// than the executed head a restart starts from. Unlike the safety record
/// it guards liveness only: a lost write leaves a restarted validator
/// disagreeing about the schedule for a while, never voting twice.
pub fn persist_reputation(db: &Database, rep: &Reputation) -> anyhow::Result<()> {
    let bytes = bincode::serialize(rep)?;
    db.put(cb_state::CF_META, REPUTATION_KEY, &bytes)?;
    Ok(())
}

/// The reputation snapshot a previous run wrote, if any.
pub fn load_reputation(db: &Database) -> Option<Reputation> {
    let bytes = db.get(cb_state::CF_META, REPUTATION_KEY).ok()??;
    match bincode::deserialize(&bytes) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(
                ?e,
                "the stored leader-reputation snapshot does not decode; starting without it"
            );
            None
        }
    }
}

/// A new engine with the safety record and the reputation snapshot
/// applied, if there are any.
#[allow(clippy::too_many_arguments)]
fn engine_with_safety(
    db: &Database,
    cfg: BftConfig,
    set: ValidatorSet,
    me: Address,
    key: SecretKey,
    genesis: H256,
    root: Root,
    now_ms: u64,
) -> Engine {
    let root = match load_reputation(db) {
        Some(rep) => root.with_reputation(rep),
        None => root,
    };
    let mut engine = Engine::new(cfg, set, me, key, genesis, root, payload_root, now_ms);
    if let Some((voted, high_qc)) = load_safety(db) {
        tracing::info!(
            voted_round = voted,
            high_qc_round = high_qc.round,
            "restoring the consensus safety record"
        );
        engine.restore_safety(voted, high_qc);
    }
    engine
}

/// The validator set in genesis order.
pub fn validator_set(db: &Database) -> anyhow::Result<ValidatorSet> {
    let addrs: Vec<Address> = Validators::new(db)
        .list()
        .into_iter()
        .map(|v| v.addr)
        .collect();
    Ok(ValidatorSet::new(addrs)?)
}

/// Work for the executor thread (see `crate::bft_writer`). Every job but
/// `Resync` carries the tree it came from: a catch-up starts a new tree, and
/// jobs of the old one still queued must not touch the blocks the catch-up
/// lands.
#[allow(clippy::large_enum_variant)]
enum ExecJob {
    /// A committed proposal whose certificate is still to come: execute it
    /// now, seal it when `Seal` names it.
    Execute {
        header: ProposalHeader,
        txs: Vec<VerifiedTx>,
        generation: u64,
    },
    /// The certificate for the proposal `Execute` handed over last.
    Seal {
        committed: Committed,
        generation: u64,
    },
    /// A committed proposal with its certificate: execute and seal it.
    Commit {
        committed: Committed,
        txs: Vec<VerifiedTx>,
        generation: u64,
    },
    /// Blocks are about to land, or have landed, by another path (catch-up
    /// sync): drop the unsealed block, flush, re-read the head, and say so.
    Resync { done: std::sync::mpsc::Sender<()> },
}

/// The verdict of validating a peer's payload, back on the driver thread.
struct Validated {
    proposal: Proposal,
    veto: Option<String>,
    txs: Vec<VerifiedTx>,
    /// `txs[i].signed.hash()`, computed once during validation.
    hashes: Arc<[H256]>,
    /// Microseconds spent decoding, recovering and checking the payload.
    validate_us: u64,
    /// The tree it was validated for.
    generation: u64,
}

/// A payload this node validated or proposed, held until it commits or is
/// abandoned. Shared between `payloads` and `own` for this node's own
/// proposals, so a proposal's transaction list exists once, not three
/// times.
struct Held {
    round: u64,
    txs: Vec<VerifiedTx>,
    hashes: Arc<[H256]>,
}

impl Held {
    /// The transactions and hashes, without a copy when this is the last
    /// reference (the usual case: `own` lets go first).
    fn into_parts(held: Arc<Held>) -> (Vec<VerifiedTx>, Arc<[H256]>) {
        match Arc::try_unwrap(held) {
            Ok(h) => (h.txs, h.hashes),
            Err(shared) => (shared.txs.clone(), shared.hashes.clone()),
        }
    }
}

/// Run the validator until the process stops.
pub async fn run_validator(stack: ValidatorStack, cfg: ValidatorCfg) -> anyhow::Result<()> {
    let ValidatorStack {
        db,
        ctx,
        notify,
        metrics,
        key,
        me,
        p2p,
        apply_switch,
        mut inbound,
        bypass,
        pending_nonces,
        verifier,
        outbox,
    } = stack;
    if let Some(keep) = cfg.keep_blocks {
        db.flush_pending()?;
        crate::sequencer::startup_prune(&db, keep);
    }
    let set = validator_set(&db)?;
    anyhow::ensure!(
        set.contains(&me),
        "this key ({me}) is not in the validator set"
    );
    let genesis = genesis_id(&db);
    let root = root_from_db(&db);
    let started = Instant::now();
    let now = || started.elapsed().as_millis() as u64;
    tracing::info!(
        %me,
        validators = set.len(),
        faults_tolerated = set.faults_tolerated(),
        quorum = set.quorum(),
        root_round = root.round,
        root_height = root.height,
        "validator starting"
    );
    let bft_cfg = BftConfig {
        base_timeout_ms: cfg.round_timeout_ms,
        max_timeout_ms: cfg.round_timeout_ms.saturating_mul(16),
        retain_rounds: RETAIN_ROUNDS,
    };
    let mut engine = engine_with_safety(
        &db,
        bft_cfg.clone(),
        set.clone(),
        me,
        *key,
        genesis,
        root,
        now(),
    );
    // Catching up by executed blocks: see `CATCH_UP_GAP`.
    let mut catching_up: Option<CatchUp> = None;
    let mut catch_up_failures: u32 = 0;
    // The highest round any inbound proposal named, verified or not: only
    // a hint for when catch-up has brought the head within reach of it.
    let mut net_round: u64 = 0;
    let mut catch_up_ticks: u32 = 0;
    // Proposal requests answered per peer in the current 100 ms window.
    let mut served_requests: HashMap<cb_net::p2p::PeerId, (Instant, u32)> = HashMap::new();
    let mut last_commit_at = Instant::now();
    // The proposal last handed over as `Action::Ordered`, whose certificate
    // the next `Action::Commit` brings.
    let mut ordered_tip: Option<H256> = None;

    // The executor thread and what it reports back.
    let roots: Roots = Arc::new(Mutex::new(BTreeMap::new()));
    let (exec_tx, exec_rx) = crossbeam_channel::unbounded::<ExecJob>();
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let generation = generation.clone();
        let db = db.clone();
        let notify = notify.clone();
        let metrics = metrics.clone();
        let roots = roots.clone();
        let pending_nonces = pending_nonces.clone();
        let keep = cfg.keep_blocks;
        std::thread::Builder::new()
            .name("cb-executor".to_string())
            .spawn(move || {
                executor_loop(
                    db,
                    ctx,
                    exec_rx,
                    notify,
                    metrics,
                    roots,
                    pending_nonces,
                    keep,
                    generation,
                )
            })?;
    }

    // Validation of peers' payloads runs on one thread of its own, in
    // arrival order, and comes back here. One thread, not the blocking
    // pool: a validator catching up receives hundreds of proposals at once,
    // and hundreds of concurrent validations each fanning out onto the
    // rayon pool nest stolen jobs on the pool threads' stacks until one
    // overflows. In order is also what the tree wants.
    let (validated_tx, mut validated_rx) = mpsc::unbounded_channel::<Validated>();
    let (to_validate_tx, to_validate_rx) = crossbeam_channel::unbounded::<(u64, Proposal)>();
    {
        let db = db.clone();
        let roots = roots.clone();
        let generation = generation.clone();
        std::thread::Builder::new()
            .name("cb-validate".to_string())
            .spawn(move || {
                while let Ok((gen, p)) = to_validate_rx.recv() {
                    if gen != generation.load(std::sync::atomic::Ordering::Relaxed) {
                        // Queued for a tree a catch-up has replaced.
                        continue;
                    }
                    let started = Instant::now();
                    let (veto, txs, hashes) = validate_proposal(&db, &roots, &p);
                    let validate_us = started.elapsed().as_micros() as u64;
                    if validated_tx
                        .send(Validated {
                            proposal: p,
                            veto,
                            txs,
                            hashes,
                            validate_us,
                            generation: gen,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })?;
    }

    let p2p_send = |handle: &Option<P2pHandle>, msg: &Message| {
        if let Some(h) = handle {
            h.broadcast_consensus(msg.encode());
        }
    };

    // Payloads this node validated or proposed, by proposal id, so commit
    // does not recover signatures a second time.
    let mut payloads: HashMap<H256, Arc<Held>> = HashMap::new();
    // This node's own proposals not yet committed, so an abandoned one can
    // give its transactions back to the mempool.
    let mut own: BTreeMap<u64, (H256, Arc<Held>)> = BTreeMap::new();
    // Transactions seen in any proposal recently, by the round that carried
    // them. A forwarded transaction can reach this node's mempool after the
    // proposal that carries it was validated here and its copy deleted;
    // without this it would be proposed a second time.
    let mut recent: HashMap<H256, u64> = HashMap::new();
    let mut recent_rounds: std::collections::VecDeque<(u64, Arc<[H256]>)> = Default::default();
    let mut committed_nonces = CommittedNonces::default();
    let mut ready: Option<(u64, bool)> = None; // (round, parent_empty)
    let mut idle_until: Option<Instant> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // One line every few seconds with what the protocol is doing, and the
    // time from a proposal's timestamp to its commit here: the finality
    // latency, as measured on one machine where every clock is the same.
    let mut status_at = Instant::now() + STATUS_EVERY;
    let mut latencies: Vec<u64> = Vec::new();
    let mut validate_times: Vec<u64> = Vec::new();
    let mut committed_txs: u64 = 0;
    let mut forwarded_txs: u64 = 0;
    let mut committed_rounds: u64 = 0;
    let mut timeouts_seen: u64 = 0;

    let mut actions = engine.take_initial_actions();
    // The commit point the reputation snapshot on disk was taken at.
    let mut reputation_saved_at = engine.committed_round();
    loop {
        // ---- persist the reputation snapshot ---------------------------
        // Before the commits below go to the executor: see
        // `persist_reputation`.
        if engine.committed_round() != reputation_saved_at {
            reputation_saved_at = engine.committed_round();
            if let Err(e) = persist_reputation(&db, &engine.committed_reputation()) {
                tracing::warn!(?e, "cannot write the leader-reputation snapshot");
            }
        }
        // ---- drain actions ---------------------------------------------
        for a in actions.drain(..) {
            match a {
                Action::Broadcast(msg) => {
                    if matches!(
                        msg,
                        Message::Vote(_) | Message::Timeout(_) | Message::Proposal(_)
                    ) {
                        // Nothing that commits this node to a round leaves
                        // before the record of it is on disk. Failing to
                        // write it is not survivable: sending anyway would
                        // let a restart vote twice.
                        if let Err(e) = persist_safety(&db, &engine) {
                            tracing::error!(
                                ?e,
                                "cannot write the consensus safety record; stopping"
                            );
                            return Err(e);
                        }
                    }
                    p2p_send(&p2p, &msg)
                }
                Action::ReadyToPropose {
                    round,
                    parent_empty,
                    ..
                } => {
                    ready = Some((round, parent_empty));
                    idle_until = None;
                }
                Action::RequestProposal { id, ancestors } => {
                    // One peer, not all: any of them can answer, and an
                    // answer with ancestors is not small.
                    if let Some(h) = &p2p {
                        h.send_consensus_any(Message::ProposalRequest { id, ancestors }.encode());
                    }
                }
                a @ (Action::Commit(_) | Action::Ordered(_)) => {
                    // A proposal reaches this point once, at commit: as
                    // `Ordered` (execute now, seal when its certificate
                    // comes) or, when its certificate is already known, in
                    // a `Commit` (execute and seal). A `Commit` entry for
                    // the proposal last `Ordered` is only its certificate.
                    // While blocks land from peers the old tree's commits
                    // are theirs to execute; the bookkeeping below still
                    // runs, so what they carry leaves the mempool.
                    let executing = catching_up.is_none();
                    if !executing {
                        ordered_tip = None;
                    }
                    let gen = generation.load(std::sync::atomic::Ordering::Relaxed);
                    let fresh: Vec<(Option<Committed>, ProposalHeader, Vec<u8>)> = match a {
                        Action::Commit(list) => list
                            .into_iter()
                            .filter_map(|mut c| {
                                if ordered_tip == Some(c.header.id()) {
                                    ordered_tip = None;
                                    // Only the certificate is needed.
                                    c.payload = Vec::new();
                                    let _ = exec_tx.send(ExecJob::Seal {
                                        committed: c,
                                        generation: gen,
                                    });
                                    return None;
                                }
                                let header = c.header.clone();
                                // The executor needs the certificate, not
                                // the payload a second time.
                                let payload = std::mem::take(&mut c.payload);
                                Some((Some(c), header, payload))
                            })
                            .collect(),
                        Action::Ordered(o) => {
                            if executing {
                                ordered_tip = Some(o.header.id());
                            }
                            vec![(None, o.header, o.payload)]
                        }
                        _ => unreachable!("matched above"),
                    };
                    let Some(last_round) = fresh.iter().map(|(_, h, _)| h.round).max() else {
                        continue;
                    };
                    let wall = wall_ms();
                    for (committed, header, payload) in fresh {
                        let id = header.id();
                        own.remove(&header.round);
                        committed_rounds += 1;
                        if header.tc.is_some() {
                            timeouts_seen += 1;
                        }
                        if header.is_empty() {
                            continue;
                        }
                        committed_txs += u64::from(header.payload_len);
                        latencies.push(wall.saturating_sub(header.timestamp));
                        let (txs, hashes) = match payloads.remove(&id) {
                            Some(held) => Held::into_parts(held),
                            None => match decode_and_recover(&payload) {
                                Ok(t) => {
                                    let h: Arc<[H256]> =
                                        t.iter().map(|v| v.signed.hash()).collect();
                                    (t, h)
                                }
                                Err(e) => {
                                    tracing::error!(
                                        round = header.round,
                                        ?e,
                                        "committed payload does not decode; skipping"
                                    );
                                    continue;
                                }
                            },
                        };
                        // Whatever this node still holds of the payload
                        // has been ordered; it is not this node's to
                        // propose again.
                        Mempool::new(&db).delete_many(&hashes);
                        Mempool::new(&db).mark_committed(&hashes);
                        committed_nonces.record(header.round, &txs);
                        if !executing {
                            continue;
                        }
                        let _ = exec_tx.send(match committed {
                            Some(committed) => ExecJob::Commit {
                                committed,
                                txs,
                                generation: gen,
                            },
                            None => ExecJob::Execute {
                                header,
                                txs,
                                generation: gen,
                            },
                        });
                    }
                    last_commit_at = Instant::now();
                    // Own proposals older than the commit point that did
                    // not commit were abandoned. Their transactions go
                    // back to the mempool for the next round this node
                    // leads.
                    let abandoned: Vec<u64> = own.range(..last_round).map(|(r, _)| *r).collect();
                    for r in abandoned {
                        if let Some((id, held)) = own.remove(&r) {
                            payloads.remove(&id);
                            for h in held.hashes.iter() {
                                recent.remove(h);
                            }
                            requeue(&db, &held.txs, &held.hashes);
                            tracing::info!(
                                round = r,
                                txs = held.txs.len(),
                                "proposal abandoned; transactions returned to the mempool"
                            );
                        }
                    }
                    // Payloads of proposals the commit point has passed
                    // without committing them were abandoned; they must not
                    // count as in flight.
                    payloads.retain(|_, h| h.round > last_round);
                }
            }
        }

        // ---- propose, if it is this node's turn ------------------------
        if let Some((round, parent_empty)) = ready {
            if round != engine.round() {
                ready = None;
                idle_until = None;
            } else {
                let mut txs = select_from_mempool(
                    &db,
                    cfg.max_block_txs,
                    &recent,
                    payloads.values().map(|h| h.txs.as_slice()),
                    &mut committed_nonces,
                    round,
                );
                if let Some(rx) = &bypass {
                    let cap = cfg.max_block_txs.saturating_sub(txs.len());
                    for _ in 0..cap {
                        match rx.try_recv() {
                            Ok(v) => txs.push(v),
                            Err(_) => break,
                        }
                    }
                    metrics.set_channel_depth(rx.len() as u64);
                }
                let idle_over = idle_until.map(|t| Instant::now() >= t).unwrap_or(false);
                if !txs.is_empty() || !parent_empty || idle_over {
                    let seed = engine.high_qc().id;
                    let txs = order_for_block(txs, seed);
                    // One encoding of the block: the payload, and every
                    // transaction's hash cut from it.
                    let (payload, hashes) = encode_payload(&txs);
                    let n_txs = txs.len() as u32;
                    let mut held = Some((txs, hashes));
                    let (exec_height, exec_root) = executed_head(&db, &roots);
                    let ts = wall_ms();
                    let acts = engine.propose(payload, n_txs, ts, exec_height, exec_root, now());
                    for a in &acts {
                        if let Action::Broadcast(Message::Proposal(p)) = a {
                            let id = p.header.id();
                            if let Some((txs, hashes)) = held.take().filter(|(t, _)| !t.is_empty())
                            {
                                let hashes: Arc<[H256]> = hashes.into();
                                Mempool::new(&db).delete_many(&hashes);
                                Mempool::new(&db).mark_committed(&hashes);
                                for h in hashes.iter() {
                                    recent.insert(*h, round);
                                }
                                recent_rounds.push_back((round, hashes.clone()));
                                trim_recent(&mut recent, &mut recent_rounds);
                                let held = Arc::new(Held { round, txs, hashes });
                                payloads.insert(id, held.clone());
                                own.insert(round, (id, held));
                            }
                            tracing::debug!(round, txs = p.header.payload_len, "proposed");
                        }
                    }
                    actions.extend(acts);
                    ready = None;
                    idle_until = None;
                    continue;
                } else if idle_until.is_none() {
                    idle_until = Some(Instant::now() + Duration::from_millis(cfg.idle_interval_ms));
                }
            }
        }

        // ---- wait for something ----------------------------------------
        let idle_sleep = idle_until
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        tokio::select! {
            biased;
            Some(v) = validated_rx.recv() => {
                if v.generation != generation.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                tracing::debug!(round = v.proposal.header.round, veto = ?v.veto, "validated");
                if let Some(reason) = &v.veto {
                    tracing::warn!(round = v.proposal.header.round, reason, "vetoing a proposal");
                } else {
                    // Transactions this node is about to vote for leave its
                    // mempool now rather than at commit: gossip has put the
                    // same transactions in every validator's mempool, and
                    // the next leader must not propose them again while
                    // this proposal is still in flight. If the proposal is
                    // abandoned its proposer requeues them.
                    if !v.txs.is_empty() {
                        let round = v.proposal.header.round;
                        Mempool::new(&db).delete_many(&v.hashes);
                        Mempool::new(&db).mark_committed(&v.hashes);
                        for h in v.hashes.iter() {
                            recent.insert(*h, round);
                        }
                        recent_rounds.push_back((round, v.hashes.clone()));
                        trim_recent(&mut recent, &mut recent_rounds);
                    }
                    if !v.proposal.header.is_empty() {
                        validate_times.push(v.validate_us);
                    }
                    payloads.insert(
                        v.proposal.header.id(),
                        Arc::new(Held {
                            round: v.proposal.header.round,
                            txs: v.txs,
                            hashes: v.hashes,
                        }),
                    );
                }
                actions = engine.on_message(Message::Proposal(v.proposal), v.veto.as_deref(), now());
            }
            Some(m) = inbound.recv() => {
                if tracing::enabled!(tracing::Level::DEBUG) {
                    let kind = match Message::decode(&m.bytes) {
                        Ok(Message::Proposal(p)) => format!("proposal r{}", p.header.round),
                        Ok(Message::Vote(v)) => format!("vote r{}", v.round),
                        Ok(Message::Timeout(t)) => format!("timeout r{}", t.round),
                        Ok(Message::ProposalRequest { .. }) => "request".into(),
                        Ok(Message::TxBatch { txs }) => format!("txbatch {}", txs.len()),
                        Err(_) => "undecodable".into(),
                    };
                    tracing::debug!(peer = m.from, kind, catching_up = catching_up.is_some(), "inbound");
                }
                if catching_up.is_some() {
                    // Not in the protocol until the tree can reach the tip;
                    // only note how far the tip has got.
                    if let Ok(Message::Proposal(p)) = Message::decode(&m.bytes) {
                        net_round = net_round.max(p.header.round);
                    }
                    continue;
                }
                match Message::decode(&m.bytes) {
                    Ok(Message::Proposal(p)) => {
                        // One already held — a repeat, or an ancestor served
                        // twice — would cost a second validation for
                        // nothing; the engine would drop it as a duplicate.
                        if engine.holds(&p.header.id()) {
                            continue;
                        }
                        // Validate off this thread; the verdict comes back
                        // through `validated_rx`.
                        let gen = generation.load(std::sync::atomic::Ordering::Relaxed);
                        let _ = to_validate_tx.send((gen, p));
                    }
                    Ok(Message::ProposalRequest { id, ancestors }) => {
                        // Answered at a bounded rate per peer: a validator
                        // rebuilding its tree asks in bursts, and answering
                        // every burst in full would flood this node's own
                        // outbound queue with megabytes of proposals.
                        let now_i = Instant::now();
                        let (window, served) = served_requests.entry(m.from).or_insert((now_i, 0));
                        if now_i.duration_since(*window) > Duration::from_millis(100) {
                            *window = now_i;
                            *served = 0;
                        }
                        if *served < MAX_REQUESTS_PER_WINDOW {
                            *served += 1;
                            if let Some(h) = &p2p {
                                for p in engine.proposal_chain(&id, ancestors.min(MAX_SERVED_ANCESTORS)) {
                                    h.send_consensus(m.from, Message::Proposal(p).encode());
                                }
                            }
                        }
                    }
                    Ok(Message::TxBatch { txs }) => {
                        // Another validator's clients. Through the same
                        // admission as this node's own, minus the forwarding.
                        if let Some(v) = &verifier {
                            for bytes in txs {
                                if let Ok(signed) = bincode::deserialize::<SignedTx>(&bytes) {
                                    v.submit_forwarded(signed);
                                }
                            }
                        }
                    }
                    Ok(msg) => {
                        actions = engine.on_message(msg, None, now());
                    }
                    Err(e) => tracing::debug!(peer = m.from, ?e, "undecodable consensus message"),
                }
            }
            _ = ticker.tick() => {
                actions = engine.on_tick(now());
                // ---- catch-up by executed blocks ----------------------
                //
                // A validator that missed more rounds than its peers keep
                // proposals for cannot rebuild its tree by asking for
                // them. It syncs executed blocks through gossip instead,
                // then starts a fresh tree at its new head.
                match &mut catching_up {
                    None => {
                        let gap = engine.behind_by();
                        let wait = CATCH_UP_AFTER * (1 + catch_up_failures.min(10));
                        if gap > CATCH_UP_GAP && last_commit_at.elapsed() > wait {
                            let head = executed_head(&db, &roots).0;
                            tracing::warn!(
                                gap,
                                head,
                                "too far behind to rejoin by proposals; syncing executed blocks"
                            );
                            // Whatever the old tree still has queued for
                            // the executor is void from here on. And the
                            // block it executed but has not sealed goes
                            // before any block lands from a peer: that
                            // block's parent is the sealed head, and a
                            // peer's copy of the unsealed one will land in
                            // its place.
                            generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            ordered_tip = None;
                            // Landing a peer's block on top of an unsealed
                            // one would never fit: rather not start.
                            anyhow::ensure!(
                                resync_executor(&exec_tx),
                                "the executor did not drop its unsealed block before a catch-up"
                            );
                            if let Some(s) = &apply_switch {
                                s.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            if let Some(h) = &p2p {
                                h.set_fetch_blocks(true);
                            }
                            catching_up = Some(CatchUp {
                                started: Instant::now(),
                                start_head: head,
                                last_head: head,
                                last_move: Instant::now(),
                            });
                        }
                    }
                    Some(c) => {
                        let head = executed_head(&db, &roots).0;
                        if head > c.last_head {
                            c.last_head = head;
                            c.last_move = Instant::now();
                        }
                        // Done when the executed head's round is within
                        // reach of the rounds the network is on (checked
                        // every few ticks: it reads the head block), or
                        // when the head has stopped moving — the tip, or
                        // nobody has more. Either way, rejoin from here.
                        catch_up_ticks += 1;
                        let within_reach = catch_up_ticks % 20 == 0
                            && head > c.start_head
                            && root_from_db(&db).round + CATCH_UP_GAP / 2 >= net_round;
                        let stalled = c.last_move.elapsed() > CATCH_UP_SETTLE
                            && c.started.elapsed() > CATCH_UP_SETTLE;
                        if within_reach || stalled {
                            let moved = c.last_head > executed_head_start(c);
                            if moved {
                                catch_up_failures = 0;
                            } else {
                                catch_up_failures += 1;
                                tracing::error!(
                                    head,
                                    "no peer served the blocks this validator is missing; they \
                                     have pruned past them. Sync from an archive node, or start \
                                     the validators with a larger --keep-blocks. Retrying later."
                                );
                            }
                            if let Some(s) = &apply_switch {
                                s.store(false, std::sync::atomic::Ordering::Relaxed);
                            }
                            if let Some(h) = &p2p {
                                h.set_fetch_blocks(false);
                            }
                            anyhow::ensure!(
                                resync_executor(&exec_tx),
                                "the executor did not settle after a catch-up"
                            );
                            ordered_tip = None;
                            let root = root_from_db(&db);
                            tracing::warn!(
                                head,
                                root_round = root.round,
                                "caught up; rejoining consensus from the executed head"
                            );
                            engine = engine_with_safety(
                                &db,
                                bft_cfg.clone(),
                                set.clone(),
                                me,
                                *key,
                                genesis,
                                root,
                                now(),
                            );
                            payloads.clear();
                            own.clear();
                            recent.clear();
                            recent_rounds.clear();
                            ready = None;
                            idle_until = None;
                            catching_up = None;
                            last_commit_at = Instant::now();
                            actions.extend(engine.take_initial_actions());
                        }
                    }
                }
                // Forward what admission let in since the last tick.
                let batch: Vec<Vec<u8>> = std::mem::take(&mut *outbox.lock().expect("outbox"));
                if !batch.is_empty() {
                    forwarded_txs += batch.len() as u64;
                    p2p_send(&p2p, &Message::TxBatch { txs: batch });
                }
                if Instant::now() >= status_at {
                    status_at = Instant::now() + STATUS_EVERY;
                    latencies.sort_unstable();
                    let pct = |p: f64| -> u64 {
                        if latencies.is_empty() {
                            0
                        } else {
                            latencies[((latencies.len() - 1) as f64 * p) as usize]
                        }
                    };
                    let (exec_height, _) = executed_head(&db, &roots);
                    validate_times.sort_unstable();
                    let validate_ms_max = validate_times.last().copied().unwrap_or(0) / 1000;
                    let validate_ms_p50 = validate_times
                        .get(validate_times.len() / 2)
                        .copied()
                        .unwrap_or(0)
                        / 1000;
                    let mut drops: BTreeMap<String, usize> = BTreeMap::new();
                    for (_, why) in engine.dropped.drain(..) {
                        *drops.entry(format!("{why:?}")).or_default() += 1;
                    }
                    tracing::info!(
                        round = engine.round(),
                        committed_round = engine.committed_round(),
                        drops = ?drops,
                        executed_height = exec_height,
                        rounds = committed_rounds,
                        blocks = latencies.len(),
                        txs = committed_txs,
                        timeouts = timeouts_seen,
                        commit_ms_p50 = pct(0.5),
                        commit_ms_p99 = pct(0.99),
                        commit_ms_max = latencies.last().copied().unwrap_or(0),
                        validate_ms_p50,
                        validate_ms_max,
                        forwarded = forwarded_txs,
                        committed_nonces = committed_nonces.len(),
                        mempool = Mempool::new(&db).len(),
                        "consensus status (last {}s)",
                        STATUS_EVERY.as_secs()
                    );
                    latencies.clear();
                    validate_times.clear();
                    committed_txs = 0;
                    forwarded_txs = 0;
                    committed_rounds = 0;
                    timeouts_seen = 0;
                }
            }
            _ = tokio::time::sleep(idle_sleep), if idle_until.is_some() => {}
            _ = notify.notified(), if idle_until.is_some() => {}
        }
    }
}

const STATUS_EVERY: Duration = Duration::from_secs(5);
/// Most ancestors served with one proposal request, whatever was asked.
const MAX_SERVED_ANCESTORS: u32 = 64;
/// Most proposal requests answered per peer per 100 ms.
const MAX_REQUESTS_PER_WINDOW: u32 = 16;
/// Rounds peers keep proposals for.
const RETAIN_ROUNDS: u64 = 4_096;
/// Missing more rounds than this, with no commit for `CATCH_UP_AFTER`, and a
/// validator syncs executed blocks instead of asking for proposals.
const CATCH_UP_GAP: u64 = RETAIN_ROUNDS / 2;
const CATCH_UP_AFTER: Duration = Duration::from_secs(3);
/// Catch-up is over when the executed head has not moved for this long.
const CATCH_UP_SETTLE: Duration = Duration::from_secs(2);

struct CatchUp {
    started: Instant,
    start_head: u64,
    last_head: u64,
    last_move: Instant,
}

fn executed_head_start(c: &CatchUp) -> u64 {
    c.start_head
}
/// Rounds a transaction stays in the "seen in a proposal" set.
const RECENT_ROUNDS: usize = 512;

/// Forget transactions seen in proposals more than `RECENT_ROUNDS` rounds
/// ago. Called wherever a round is added, including this node's own
/// proposals: a lone validator never validates a peer's, and trimming only
/// there let the set grow for ever.
fn trim_recent(
    recent: &mut HashMap<H256, u64>,
    recent_rounds: &mut std::collections::VecDeque<(u64, Arc<[H256]>)>,
) {
    while recent_rounds.len() > RECENT_ROUNDS {
        if let Some((_, old)) = recent_rounds.pop_front() {
            for h in old.iter() {
                recent.remove(h);
            }
        }
    }
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The nonce each sender's next proposed transaction must carry, as far as
/// the payloads committed but not yet executed say.
///
/// Execution follows commit (ADR 0008), so at any moment a few committed
/// payloads are still on their way to the executor and the chain's account
/// nonce is behind what consensus has already ordered. The selection below
/// used to take the chain's nonce as its baseline, and the committed
/// transactions are already gone from the mempool, so every transaction
/// left there for such a sender looked "ahead" and waited. That bound the
/// proposal rate to the executor's latency: whenever execution fell a block
/// or two behind, every leader proposed a few dozen transactions a round
/// while the mempools held hundreds of thousands. This records, at commit,
/// the nonce after each sender's last committed transaction; a sender's
/// baseline is that or the chain's nonce, whichever is ahead.
///
/// An entry is dropped once the chain has caught up with it, and after
/// [`COMMITTED_NONCE_ROUNDS`] rounds regardless: a committed transaction
/// consumes its nonce whether it applied or not (`failure_consumes_nonce`),
/// so the chain does catch up, except for a payload no block came of, and
/// the expiry bounds that. Both checks run when the sender is next looked
/// at, so a sender that goes quiet keeps its entry: the map is bounded by
/// distinct senders seen, not by time. Fine for a machine's worth of
/// traders; on a public network it wants a sweep.
#[derive(Default)]
struct CommittedNonces {
    /// Sender → (the nonce after its last committed transaction, the round
    /// that transaction was committed in).
    next: HashMap<Address, (u64, u64)>,
}

/// How long a committed nonce is trusted over the chain's before it is
/// assumed the chain will never get there.
const COMMITTED_NONCE_ROUNDS: u64 = 4_096;

impl CommittedNonces {
    /// Fold the transactions of a payload committed in `round`.
    fn record(&mut self, round: u64, txs: &[VerifiedTx]) {
        for v in txs {
            if v.is_synthetic() {
                continue;
            }
            let e = self.next.entry(v.sender).or_insert((0, round));
            if v.signed.tx.nonce + 1 > e.0 {
                *e = (v.signed.tx.nonce + 1, round);
            }
        }
    }

    /// The baseline for `sender` at `round`, given the chain's nonce `chain`:
    /// the committed successor while it is still ahead of the chain and not
    /// expired, else the chain's, and then the entry goes.
    fn expected(&mut self, sender: Address, chain: u64, round: u64) -> u64 {
        match self.next.get(&sender) {
            Some(&(n, at)) if n > chain && round.saturating_sub(at) <= COMMITTED_NONCE_ROUNDS => n,
            Some(_) => {
                self.next.remove(&sender);
                chain
            }
            None => chain,
        }
    }

    fn len(&self) -> usize {
        self.next.len()
    }
}

/// What this node proposes: up to `max` transactions from its mempool, each
/// sender's as an unbroken run of nonces continuing from the chain (or from
/// what is committed ahead of it, see [`CommittedNonces`]) and from the
/// proposals already in flight.
///
/// # Why the run must be unbroken
///
/// Every validator's mempool holds the transactions its own clients sent
/// and the ones the other validators forwarded, and any of them may propose
/// any of them. Forwarding is a network path: a transaction can be dropped
/// or arrive after its successor. A validator that proposed nonce 8 while
/// nonce 7 still sat in another validator's mempool would have 8 executed
/// first and refused, its client's admission ledger reset, and the client
/// resubmitting from 7 — a cascade of duplicates that this rule prevents by
/// leaving 8 in the mempool until 7 has been proposed by whoever holds it.
///
/// Transactions already in a proposal (`recent`) are dropped from the
/// mempool here; the ones selected are deleted; the rest stay.
fn select_from_mempool<'a>(
    db: &Database,
    max: usize,
    recent: &HashMap<H256, u64>,
    in_flight: impl Iterator<Item = &'a [VerifiedTx]>,
    committed: &mut CommittedNonces,
    round: u64,
) -> Vec<VerifiedTx> {
    use rayon::prelude::*;
    let mempool = Mempool::new(db);
    let pending = mempool.drain(max.saturating_mul(2));
    if pending.is_empty() {
        return Vec::new();
    }
    // The nonce each sender's next proposed transaction must carry.
    let mut next: HashMap<Address, u64> = HashMap::new();
    for txs in in_flight {
        for v in txs {
            let e = next.entry(v.sender).or_insert(0);
            *e = (*e).max(v.signed.tx.nonce + 1);
        }
    }
    // The drained bytes are moved, not copied again, and the hash each was
    // queued under comes with them: every caller of `Mempool::put` keys it
    // by `SignedTx::hash`, so it need not be recomputed here.
    let (hashes, blobs): (Vec<H256>, Vec<Vec<u8>>) = pending.into_iter().unzip();
    let accounts = cb_state::Accounts::new(db);
    let mut out = Vec::with_capacity(max.min(blobs.len()));
    // Deleted once, under one lock, at the end.
    let mut consumed: Vec<H256> = Vec::with_capacity(max.min(blobs.len()));
    // Decoded a block's worth at a time, in parallel, and only as far as the
    // walk below gets: it usually stops at `max`, halfway through what was
    // drained. The walk itself is the sequential one it always was, so what
    // is selected and what is deleted are unchanged.
    let chunk = max.clamp(1, 16_384);
    let mut start = 0;
    'walk: while start < blobs.len() {
        let end = (start + chunk).min(blobs.len());
        let decoded: Vec<Option<VerifiedTx>> = blobs[start..end]
            .par_iter()
            .with_min_len(256)
            .map(|blob| {
                bincode::deserialize::<MempoolRecord>(blob)
                    .ok()
                    .map(|rec| rec.into_verified())
            })
            .collect();
        // `drain` yields each sender's transactions lowest nonce first.
        for (i, v) in decoded.into_iter().enumerate() {
            // A record that does not decode is skipped and left, as before.
            let Some(v) = v else { continue };
            let hash = hashes[start + i];
            debug_assert_eq!(hash, v.signed.hash(), "mempool keyed by SignedTx::hash");
            if recent.contains_key(&hash) {
                consumed.push(hash);
                continue;
            }
            if out.len() >= max {
                break 'walk;
            }
            let expected = match next.get(&v.sender) {
                Some(n) => *n,
                None => {
                    let chain = accounts.get(&v.sender).map(|a| a.nonce).unwrap_or(0);
                    let base = committed.expected(v.sender, chain, round);
                    next.insert(v.sender, base);
                    base
                }
            };
            if v.signed.tx.nonce == expected {
                next.insert(v.sender, expected + 1);
                consumed.push(hash);
                out.push(v);
            } else if v.signed.tx.nonce < expected {
                // Already consumed: a late copy.
                consumed.push(hash);
            }
            // A nonce ahead of the run waits for its predecessor.
        }
        start = end;
    }
    mempool.delete_many(&consumed);
    out
}

/// Put transactions back into the mempool. `hashes[i]` is `txs[i]`'s hash.
fn requeue(db: &Database, txs: &[VerifiedTx], hashes: &[H256]) {
    let mempool = Mempool::new(db);
    for (v, h) in txs.iter().zip(hashes) {
        let rec = MempoolRecord {
            signed: v.signed.clone(),
            sender: v.sender,
        };
        if let Ok(bytes) = bincode::serialize(&rec) {
            mempool.put(*h, v.sender, v.signed.tx.nonce, &bytes);
        }
    }
}

/// Have the executor drop any unsealed block, flush every write, and
/// re-read the head, and wait until it has: a catch-up is about to land
/// blocks, or has, and the head it reads next must be the sealed one.
/// Whether it confirmed within 30 s; if not, continuing would stall for
/// good (a peer's block never fits on an unsealed one), so the caller
/// stops the validator instead.
fn resync_executor(exec_tx: &crossbeam_channel::Sender<ExecJob>) -> bool {
    let (done, wait) = std::sync::mpsc::channel();
    exec_tx.send(ExecJob::Resync { done }).is_ok()
        && wait.recv_timeout(Duration::from_secs(30)).is_ok()
}

/// The executed head and its state root: the newest block whose root is
/// known, sealed or not (see `crate::bft_writer`). A block executed and
/// waiting for its certificate counts: its root does not depend on the
/// certificate, and a crash re-derives the same one.
fn executed_head(db: &Database, roots: &Roots) -> (u64, H256) {
    let durable = db.durable_view();
    let (h, root) = loop {
        let h = Meta::new(&durable).head_height();
        let root = Meta::new(&durable).state_root();
        if Meta::new(&durable).head_height() == h {
            break (h, root);
        }
    };
    let mut r = roots.lock().expect("roots");
    if h > 0 {
        r.insert(h, root);
    }
    match r.last_key_value() {
        Some((&top, &top_root)) if top > h => (top, top_root),
        _ => (h, root),
    }
}

fn recover_senders_refs(txs: &[&SignedTx]) -> Result<Vec<Address>, cb_crypto::CryptoError> {
    use rayon::prelude::*;
    // See `cb_rpc::verify`: recovery jobs of 32, not one per thread.
    txs.par_iter()
        .with_min_len(32)
        .map(|s| s.sender())
        .collect()
}

fn decode_and_recover(payload: &[u8]) -> anyhow::Result<Vec<VerifiedTx>> {
    let signed: Vec<SignedTx> = bincode::deserialize(payload)?;
    let senders = cb_tx::recover_senders_par(&signed)?;
    Ok(signed
        .into_iter()
        .zip(senders)
        .map(|(s, a)| VerifiedTx::from_parts_unchecked(s, a))
        .collect())
}

/// What [`validate_proposal`] returns: a veto (or none), and the payload's
/// transactions with their hashes when it passed.
type Verdict = (Option<String>, Vec<VerifiedTx>, Arc<[H256]>);

fn veto(reason: impl Into<String>) -> Verdict {
    (Some(reason.into()), Vec::new(), Arc::from(Vec::new()))
}

/// Check a peer's proposal: the payload decodes to what the header says,
/// is the canonical encoding of what it decodes to, every signature
/// recovers, every transaction is for this chain and statically valid, the
/// order is canonical for the parent's seed, and the proposer's executed
/// root agrees with ours where we can tell.
///
/// Each transaction is encoded and hashed once here, and the hashes go back
/// to the consensus thread with the verdict so nothing there hashes the
/// block again.
fn validate_proposal(db: &Database, roots: &Roots, p: &Proposal) -> Verdict {
    use rayon::prelude::*;
    let h = &p.header;
    // The audit of execution: a proposer whose root at a height we have
    // executed differs from ours has diverged, or we have. Either way this
    // node must not vote for what it cannot vouch for.
    if h.exec_height > 0 {
        let ours = roots
            .lock()
            .expect("roots")
            .get(&h.exec_height)
            .copied()
            .or_else(|| {
                Blocks::new(db)
                    .get(h.exec_height)
                    .and_then(|b| Block::decode(&b).ok())
                    .map(|b| b.header.state_root)
            });
        if let Some(ours) = ours {
            if ours != h.exec_root {
                tracing::error!(
                    height = h.exec_height,
                    proposer = %h.proposer,
                    theirs = %h.exec_root,
                    ours = %ours,
                    "EXECUTION DIVERGED from a proposer"
                );
                return veto("executed root differs from ours");
            }
        }
    }
    if h.is_empty() {
        return (None, Vec::new(), Arc::from(Vec::new()));
    }
    let signed: Vec<SignedTx> = match bincode::deserialize(&p.payload) {
        Ok(s) => s,
        Err(e) => return veto(format!("payload does not decode: {e}")),
    };
    if signed.len() != h.payload_len as usize {
        return veto("payload length differs from the header");
    }
    if signed.len() > cb_block::MAX_BLOCK_TXS {
        return veto("payload exceeds the block size limit");
    }
    let chain_id = Meta::new(db).chain_id();
    for s in &signed {
        if s.tx.chain_id != chain_id {
            return veto("a transaction is for another chain");
        }
        if s.is_synthetic() {
            return veto("a transaction is unsigned");
        }
        if let Err(e) = cb_tx::validate_static(&s.tx) {
            return veto(format!("a transaction is invalid: {e}"));
        }
    }
    // The payload must be exactly the encoding of what it decodes to: the
    // count, then each transaction's canonical bytes, and nothing after.
    // That is what lets `payload_root` hash the bytes instead of decoding
    // them, and it gives every transaction's hash as the keccak of its own
    // slice of the payload.
    let body = &p.payload[8..];
    let mut offsets = Vec::with_capacity(signed.len() + 1);
    offsets.push(0usize);
    for s in &signed {
        match bincode::serialized_size(s) {
            Ok(n) => offsets.push(offsets[offsets.len() - 1] + n as usize),
            Err(e) => return veto(format!("a transaction does not encode: {e}")),
        }
    }
    if offsets[signed.len()] != body.len() {
        return veto("payload is not canonically encoded");
    }
    let hashes: Option<Vec<H256>> = signed
        .par_iter()
        .with_min_len(256)
        .enumerate()
        .map(|(i, s)| {
            let slice = &body[offsets[i]..offsets[i + 1]];
            let ours = bincode::serialize(s).ok()?;
            (ours.as_slice() == slice).then(|| keccak256(slice))
        })
        .collect();
    let hashes = match hashes {
        Some(h) => h,
        None => return veto("payload is not canonically encoded"),
    };
    // The same transaction twice is never an honest leader's doing: its
    // mempool is keyed by hash and selection takes each sender's nonces
    // once. The executor would refuse the copy as a bad nonce, but a block
    // padded with copies is still a block of work for every validator.
    {
        let mut seen = std::collections::HashSet::with_capacity(hashes.len());
        if !hashes.iter().all(|h| seen.insert(*h)) {
            return veto("a transaction appears twice");
        }
    }
    // Senders this node's own admission already recovered — for a
    // forwarded or locally submitted transaction still in the mempool —
    // are taken from there; only the rest cost a recovery. Under load that
    // is nearly all of them, and it is what makes a round cheap.
    let known = Mempool::new(db).senders_of(&hashes);
    let unknown: Vec<&SignedTx> = signed
        .iter()
        .zip(known.iter())
        .filter(|(_, k)| k.is_none())
        .map(|(s, _)| s)
        .collect();
    let mut recovered = match recover_senders_refs(&unknown) {
        Ok(s) => s.into_iter(),
        Err(e) => return veto(format!("a signature does not recover: {e}")),
    };
    let txs: Vec<VerifiedTx> = signed
        .into_iter()
        .zip(known)
        .map(|(s, k)| {
            let a = match k {
                Some(a) => a,
                None => recovered.next().expect("one recovery per unknown"),
            };
            VerifiedTx::from_parts_unchecked(s, a)
        })
        .collect();
    match in_canonical_order(txs, h.parent_id) {
        Some(txs) => (None, txs, hashes.into()),
        None => veto("payload is not in canonical order"),
    }
}

/// `txs` back, unchanged, if it is already in the canonical order for
/// `seed`; `None` if not.
///
/// # Why this compares `(sender, nonce)` and not hashes
///
/// This used to clone the whole list into `order_for_block` and compare the
/// hash of every transaction before and after — two more encodings and
/// keccaks of the block and a copy of it. The list is moved in instead and
/// the check compares each position's `(sender, nonce)`, which is enough:
///
/// `order_for_block` groups by sender, stable-sorts each group by nonce, and
/// lays the groups out one after another. Suppose the output and the input
/// agree on `(sender, nonce)` at every position. Then each sender occupies
/// the same positions in both, so a sender's transactions in the output are
/// its transactions in the input at the same positions. The output's are
/// sorted by nonce and have the same nonces position by position, so the
/// input's were already sorted — and a stable sort of a sorted sequence
/// moves nothing. Every sender's transactions are therefore where they
/// were, and the output is the input, transaction for transaction.
/// Conversely, equal lists agree on `(sender, nonce)`. So this accepts
/// exactly what the hash comparison accepted.
/// `canonical_order_check_matches_the_hash_comparison` pins that against the
/// old check on shuffled and duplicated inputs.
fn in_canonical_order(txs: Vec<VerifiedTx>, seed: H256) -> Option<Vec<VerifiedTx>> {
    let given: Vec<(Address, u64)> = txs.iter().map(|v| (v.sender, v.signed.tx.nonce)).collect();
    let ordered = order_for_block(txs, seed);
    let same = ordered
        .iter()
        .map(|v| (v.sender, v.signed.tx.nonce))
        .eq(given.iter().copied());
    same.then_some(ordered)
}

/// Execute committed proposals in order, on their own thread.
#[allow(clippy::too_many_arguments)]
fn executor_loop(
    db: Database,
    ctx: ExecCtx,
    rx: Receiver<ExecJob>,
    notify: Arc<Notify>,
    metrics: Arc<Metrics>,
    roots: Roots,
    pending_nonces: Option<Arc<PendingNonces>>,
    keep_blocks: Option<u64>,
    generation: Arc<std::sync::atomic::AtomicU64>,
) {
    let on_sealed: Arc<dyn Fn(u64, H256) + Send + Sync> = {
        let notify = notify.clone();
        Arc::new(move |_height, _hash| notify.notify_waiters())
    };
    let mut writer = BlockWriter::new(db.clone(), ctx, roots, on_sealed);
    let current = |g: u64| g == generation.load(std::sync::atomic::Ordering::Relaxed);
    while let Ok(job) = rx.recv() {
        let (header, txs, result) = match job {
            ExecJob::Resync { done } => {
                writer.reset();
                tracing::info!(
                    head = Meta::new(&db).head_height(),
                    "executor dropped any unsealed block and re-read the head"
                );
                let _ = done.send(());
                continue;
            }
            ExecJob::Seal {
                committed,
                generation: g,
            } => {
                if current(g) {
                    match writer.seal(&committed) {
                        Ok(true) => {}
                        Ok(false) => tracing::debug!(
                            round = committed.header.round,
                            "certificate for a proposal that is not the unsealed one"
                        ),
                        Err(e) => tracing::error!(
                            round = committed.header.round,
                            ?e,
                            "sealing a committed block failed"
                        ),
                    }
                }
                continue;
            }
            ExecJob::Execute {
                header,
                txs,
                generation: g,
            } => {
                if !current(g) || txs.is_empty() {
                    continue;
                }
                let senders: Vec<Address> = txs.iter().map(|v| v.sender).collect();
                let r = writer.execute(&header, txs);
                (header, senders, r)
            }
            ExecJob::Commit {
                committed,
                txs,
                generation: g,
            } => {
                if !current(g) || txs.is_empty() {
                    continue;
                }
                let senders: Vec<Address> = txs.iter().map(|v| v.sender).collect();
                let r = writer.execute_sealed(&committed, txs);
                (committed.header, senders, r)
            }
        };
        let round = header.round;
        let senders = txs;
        match result {
            Ok(Outcome {
                height,
                applied,
                rejected,
                execute_us,
            }) => {
                let n = applied.len() as u64;
                let orders = applied.iter().filter(|(_, is_order, _)| *is_order).count() as u64;
                for (side, market) in applied.iter().filter_map(|(_, _, sm)| *sm) {
                    metrics.record_applied_order(side, market);
                }
                metrics.record_block(n, orders);
                // Why the refused ones were refused, by kind, so a load
                // run can tell an order that found no liquidity from a
                // failure.
                let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
                for r in &rejected {
                    let kind = format!("{:?}", r.error);
                    let kind = kind.split(['{', '(', ' ']).next().unwrap_or("").to_string();
                    *reasons.entry(kind).or_insert(0) += 1;
                }
                tracing::info!(
                    height,
                    round,
                    txs = n,
                    rejected = rejected.len(),
                    ?reasons,
                    execute_us,
                    "executed committed proposal"
                );
                if let Some(p) = &pending_nonces {
                    // A refusal from the body consumed the nonce like an
                    // applied transaction, so the sender's admission ledger
                    // — and the client's pipelined window behind it — is
                    // still right. Only a refusal from before the nonce
                    // check means the ledger and the chain disagree.
                    for r in &rejected {
                        if !cb_block::error_consumes_nonce(&r.error) {
                            p.forget(&r.sender);
                        }
                    }
                    p.retire_settled(&db, &senders);
                }
                if let Some(keep) = keep_blocks {
                    crate::sequencer::prune_one(&db, height, keep);
                }
            }
            Err(ExecError::NoApplicableTxs { rejected }) => {
                tracing::warn!(
                    round,
                    rejected = rejected.len(),
                    "committed proposal had no applicable transaction; no block"
                );
                if let Some(p) = &pending_nonces {
                    for a in &senders {
                        p.forget(a);
                    }
                }
            }
            Err(e) => {
                tracing::error!(round, ?e, "committed proposal failed to execute; every honest validator skips it the same way");
                if let Some(p) = &pending_nonces {
                    for a in &senders {
                        p.forget(a);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_crypto::{address_from_pubkey, keypair_from_seed};

    /// The safety record written before a vote survives a reopen of the
    /// database, and an engine built from it will not vote again in the
    /// round it voted in.
    #[test]
    fn the_safety_record_survives_a_reopen_and_blocks_a_second_vote() {
        let dir = tempfile::tempdir().unwrap();
        let (sk, pk) = keypair_from_seed(b"safety-test");
        let me = address_from_pubkey(&pk);
        let set = ValidatorSet::new(vec![me]).unwrap();
        let genesis = H256([5; 32]);
        let cfg = BftConfig::default();
        let (voted, high) = {
            let db = Database::open(dir.path()).unwrap();
            assert!(load_safety(&db).is_none());
            let mut e = engine_with_safety(
                &db,
                cfg.clone(),
                set.clone(),
                me,
                sk,
                genesis,
                Root::genesis(genesis),
                0,
            );
            let _ = e.take_initial_actions();
            // A lone validator proposes, votes and certifies at once.
            let acts = e.propose(Vec::new(), 0, 1, 0, H256::ZERO, 1);
            assert!(acts
                .iter()
                .any(|a| matches!(a, Action::Broadcast(Message::Vote(_)))));
            persist_safety(&db, &e).unwrap();
            (e.last_voted_round(), e.high_qc().clone())
        };
        assert_eq!(voted, 1);
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(load_safety(&db), Some((voted, high.clone())));
        let mut e = engine_with_safety(&db, cfg, set, me, sk, genesis, Root::genesis(genesis), 0);
        let _ = e.take_initial_actions();
        assert_eq!(e.last_voted_round(), voted);
        assert!(e.round() > voted);
        assert_eq!(e.high_qc(), &high);
    }

    /// The leader-reputation snapshot written at the commit point survives a
    /// reopen, and an engine built from the database starts from it.
    #[test]
    fn the_reputation_snapshot_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (sk, pk) = keypair_from_seed(b"reputation-test");
        let me = address_from_pubkey(&pk);
        let set = ValidatorSet::new(vec![me]).unwrap();
        let genesis = H256([6; 32]);
        let cfg = BftConfig::default();
        let saved = {
            let db = Database::open(dir.path()).unwrap();
            assert!(load_reputation(&db).is_none());
            let mut e = engine_with_safety(
                &db,
                cfg.clone(),
                set.clone(),
                me,
                sk,
                genesis,
                Root::genesis(genesis),
                0,
            );
            let _ = e.take_initial_actions();
            // A lone validator certifies each proposal at once; three
            // rounds commit two.
            for t in 1..=3 {
                let _ = e.propose(Vec::new(), 0, t, 0, H256::ZERO, t);
            }
            assert!(e.committed_round() >= 1);
            let rep = e.committed_reputation();
            assert_ne!(rep, Reputation::default());
            persist_reputation(&db, &rep).unwrap();
            rep
        };
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(load_reputation(&db), Some(saved.clone()));
        let e = engine_with_safety(&db, cfg, set, me, sk, genesis, Root::genesis(genesis), 0);
        assert_eq!(e.committed_reputation(), saved);
    }

    #[test]
    fn recent_transactions_are_forgotten_after_the_window() {
        let mut recent = HashMap::new();
        let mut rounds = std::collections::VecDeque::new();
        for r in 0..(RECENT_ROUNDS as u64 + 10) {
            let h = H256([(r % 251) as u8; 32]);
            let h = keccak256(&[h.0.as_slice(), &r.to_be_bytes()].concat());
            recent.insert(h, r);
            rounds.push_back((r, Arc::from(vec![h])));
            trim_recent(&mut recent, &mut rounds);
        }
        assert_eq!(rounds.len(), RECENT_ROUNDS);
        assert_eq!(recent.len(), RECENT_ROUNDS);
    }
}

/// The proposal hot path: payload building, the payload root, selection
/// from the mempool and proposal validation. Each rewrite is pinned against
/// the implementation it replaced, kept here verbatim as the reference.
#[cfg(test)]
mod hot_path_tests {
    use super::*;
    use cb_consensus::bft::{ProposalHeader, QuorumCert as BftQc};
    use cb_crypto::{address_from_pubkey, keypair_from_seed};
    use cb_tx::{Transaction, TxBody};
    use cb_types::{AssetId, DerivId, MarketId, Side, Tif};
    use proptest::prelude::*;

    // ---- the replaced implementations, as references -------------------

    fn old_payload_root(bytes: &[u8]) -> H256 {
        match bincode::deserialize::<Vec<SignedTx>>(bytes) {
            Ok(txs) => Block::compute_tx_root(&txs),
            Err(_) => keccak256(bytes),
        }
    }

    fn old_payload(txs: &[VerifiedTx]) -> Vec<u8> {
        let signed: Vec<SignedTx> = txs.iter().map(|v| v.signed.clone()).collect();
        if signed.is_empty() {
            Vec::new()
        } else {
            bincode::serialize(&signed).expect("payload encode")
        }
    }

    fn old_order_ok(txs: &[VerifiedTx], seed: H256) -> bool {
        let given: Vec<H256> = txs.iter().map(|v| v.signed.hash()).collect();
        let canonical: Vec<H256> = order_for_block(txs.to_vec(), seed)
            .iter()
            .map(|v| v.signed.hash())
            .collect();
        given == canonical
    }

    fn old_select_from_mempool<'a>(
        db: &Database,
        max: usize,
        recent: &HashMap<H256, u64>,
        in_flight: impl Iterator<Item = &'a [VerifiedTx]>,
    ) -> Vec<VerifiedTx> {
        let mempool = Mempool::new(db);
        let pending = mempool.drain(max.saturating_mul(2));
        if pending.is_empty() {
            return Vec::new();
        }
        let mut next: HashMap<Address, u64> = HashMap::new();
        for txs in in_flight {
            for v in txs {
                let e = next.entry(v.sender).or_insert(0);
                *e = (*e).max(v.signed.tx.nonce + 1);
            }
        }
        let blobs: Vec<Vec<u8>> = pending.iter().map(|(_, v)| v.clone()).collect();
        let decoded = cb_tx::decode_mempool_batch(&blobs);
        let accounts = cb_state::Accounts::new(db);
        let mut out = Vec::with_capacity(max.min(decoded.len()));
        for v in decoded {
            let hash = v.signed.hash();
            if recent.contains_key(&hash) {
                mempool.delete(&hash);
                continue;
            }
            if out.len() >= max {
                break;
            }
            let expected = match next.get(&v.sender) {
                Some(n) => *n,
                None => {
                    let chain = accounts.get(&v.sender).map(|a| a.nonce).unwrap_or(0);
                    next.insert(v.sender, chain);
                    chain
                }
            };
            if v.signed.tx.nonce == expected {
                next.insert(v.sender, expected + 1);
                mempool.delete(&hash);
                out.push(v);
            } else if v.signed.tx.nonce < expected {
                mempool.delete(&hash);
            }
        }
        out
    }

    // ---- fixtures ------------------------------------------------------

    /// `VerifiedTx` has no `PartialEq`; its parts do.
    fn parts(v: &[VerifiedTx]) -> Vec<(SignedTx, Address)> {
        v.iter().map(|t| (t.signed.clone(), t.sender)).collect()
    }

    const CHAIN: u32 = 1;

    fn key(i: u8) -> (SecretKey, Address) {
        let (sk, pk) = keypair_from_seed(&[b'k', i]);
        (sk, address_from_pubkey(&pk))
    }

    /// A body of each shape, so the payload carries every encoding kind:
    /// fixed fields, strings, enums, options-free structs, derivatives.
    fn body(kind: u8, n: u64) -> TxBody {
        match kind % 7 {
            0 => TxBody::Transfer {
                to: Address([n as u8; 20]),
                asset: AssetId::Usd,
                amount: 1 + n as u128,
            },
            1 => TxBody::Deposit {
                user: Address([3; 20]),
                asset: AssetId::Btc,
                amount: 7 + n as u128,
                external_tx_ref: format!("0x{:064x}", n),
            },
            2 => TxBody::Withdraw {
                asset: AssetId::Eth,
                amount: 9 + n as u128,
                external_address: "bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh".into(),
            },
            3 => TxBody::PlaceOrder {
                market: MarketId::BtcUsd,
                side: if n % 2 == 0 { Side::Buy } else { Side::Sell },
                price: 100 + n as u128,
                qty: 1 + n as u128,
                tif: Tif::Gtc,
                post_only: n % 3 == 0,
                client_order_id: H256([n as u8; 32]),
            },
            4 => TxBody::CancelOrder {
                order_id: H256([n as u8 ^ 0x5a; 32]),
            },
            5 => TxBody::PlacePerpOrder {
                market: DerivId::perp(0),
                side: Side::Buy,
                price_q8: 100_000_000 + n as u128,
                qty: 1 + n as u128,
                tif: Tif::Gtc,
                post_only: false,
                reduce_only: n % 2 == 1,
                client_order_id: H256([0x11; 32]),
            },
            _ => TxBody::BatchCancel {
                market: MarketId::EthUsd,
            },
        }
    }

    fn signed(sk: &SecretKey, nonce: u64, b: TxBody) -> SignedTx {
        SignedTx::sign_with(
            sk,
            Transaction {
                nonce,
                chain_id: CHAIN,
                body: b,
            },
        )
        .unwrap()
    }

    /// A representative block: several senders, contiguous nonces each,
    /// every body kind, in canonical order for `seed`.
    fn representative(seed: H256, senders: u8, per_sender: u64) -> Vec<VerifiedTx> {
        let mut txs = Vec::new();
        for s in 0..senders {
            let (sk, addr) = key(s);
            for n in 0..per_sender {
                let b = body(s.wrapping_add(n as u8), n);
                txs.push(VerifiedTx::from_parts_unchecked(signed(&sk, n, b), addr));
            }
        }
        order_for_block(txs, seed)
    }

    fn header(parent: H256, payload: &[u8], len: u32) -> ProposalHeader {
        ProposalHeader {
            round: 2,
            height: 1,
            parent_id: parent,
            payload_hash: if len == 0 {
                H256::ZERO
            } else {
                payload_root(payload)
            },
            payload_len: len,
            timestamp: 1,
            exec_height: 0,
            exec_root: H256::ZERO,
            qc: BftQc::genesis(parent),
            tc: None,
            proposer: key(0).1,
            sig: Vec::new(),
        }
    }

    fn proposal(parent: H256, payload: Vec<u8>, len: u32) -> Proposal {
        Proposal {
            header: header(parent, &payload, len),
            payload,
        }
    }

    fn chain_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).unwrap();
        Meta::new(&db).set_chain_id(CHAIN);
        (dir, db)
    }

    fn no_roots() -> Roots {
        Arc::new(Mutex::new(BTreeMap::new()))
    }

    // ---- payload bytes and root ----------------------------------------

    /// The pin the task asks for: for a representative block the new
    /// payload bytes, tx hashes and root are exactly the old ones.
    #[test]
    fn encode_payload_matches_bincode_and_signed_tx_hash() {
        let seed = H256([9; 32]);
        let txs = representative(seed, 5, 8);
        let (payload, hashes) = encode_payload(&txs);
        assert_eq!(payload, old_payload(&txs), "payload bytes unchanged");
        let expect: Vec<H256> = txs.iter().map(|v| v.signed.hash()).collect();
        assert_eq!(hashes, expect, "hashes are SignedTx::hash");
        let signed: Vec<SignedTx> = txs.iter().map(|v| v.signed.clone()).collect();
        assert_eq!(payload_root(&payload), Block::compute_tx_root(&signed));
        assert_eq!(payload_root(&payload), old_payload_root(&payload));
        // Empty is empty, as the engine expects of an empty round.
        assert_eq!(encode_payload(&[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn payload_root_is_the_tx_root_of_the_decoded_payload() {
        // One transaction, and the largest representative block this test
        // can afford, on top of the random ones below.
        for (senders, per) in [(1u8, 1u64), (12, 40)] {
            let txs = representative(H256([1; 32]), senders, per);
            let payload = old_payload(&txs);
            let decoded: Vec<SignedTx> = bincode::deserialize(&payload).unwrap();
            assert_eq!(payload_root(&payload), Block::compute_tx_root(&decoded));
            assert_eq!(payload_root(&payload), old_payload_root(&payload));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Random payloads: building a proposal and validating it returns
        /// the same bytes, the same root under old and new definitions, the
        /// same transactions and the same hashes.
        #[test]
        fn proposal_round_trip_preserves_bytes_root_and_txs(
            seed in any::<[u8; 32]>(),
            shape in prop::collection::vec((0u8..4, 0u8..7), 1..24),
        ) {
            let seed = H256(seed);
            let mut nonces = [0u64; 4];
            let mut txs = Vec::new();
            for (s, kind) in shape {
                let (sk, addr) = key(s);
                let n = nonces[s as usize];
                nonces[s as usize] += 1;
                // Keep every body statically valid for the validation half.
                let b = match body(kind, n) {
                    TxBody::PlacePerpOrder { .. } => body(3, n),
                    other => other,
                };
                txs.push(VerifiedTx::from_parts_unchecked(signed(&sk, n, b), addr));
            }
            let txs = order_for_block(txs, seed);
            let (payload, hashes) = encode_payload(&txs);
            prop_assert_eq!(&payload, &old_payload(&txs));
            prop_assert_eq!(payload_root(&payload), old_payload_root(&payload));

            let (_d, db) = chain_db();
            let p = proposal(seed, payload.clone(), txs.len() as u32);
            let (veto, back, back_hashes) = validate_proposal(&db, &no_roots(), &p);
            prop_assert_eq!(veto, None);
            prop_assert_eq!(parts(&back), parts(&txs));
            prop_assert_eq!(back_hashes.to_vec(), hashes);
            prop_assert_eq!(p.payload, payload);
        }

        /// The (sender, nonce) order check accepts exactly what the old
        /// hash comparison accepted — on canonical lists, shuffled lists,
        /// and lists with exact duplicates and same-nonce twins.
        #[test]
        fn canonical_order_check_matches_the_hash_comparison(
            seed in any::<[u8; 32]>(),
            items in prop::collection::vec((0u8..3, 0u64..4, 0u8..3), 0..14),
            shuffle in any::<bool>(),
            perm_seed in any::<u64>(),
        ) {
            let seed = H256(seed);
            let mut txs: Vec<VerifiedTx> = items
                .iter()
                .map(|(s, n, variant)| {
                    // Not really signed: ordering never checks signatures.
                    // The sender's byte goes into `sig_r` because a real
                    // sender is recovered from the signature, so two
                    // senders never share one transaction's bytes; and
                    // `variant` makes same-(sender, nonce) twins distinct.
                    let tx = SignedTx {
                        tx: Transaction {
                            nonce: *n,
                            chain_id: CHAIN,
                            body: body(*variant, *n),
                        },
                        sig_r: [*s; 32],
                        sig_s: [0; 32],
                        sig_recovery: 0,
                    };
                    VerifiedTx::from_parts_unchecked(tx, Address([*s; 20]))
                })
                .collect();
            if !shuffle {
                txs = order_for_block(txs, seed);
            } else {
                // A deterministic Fisher-Yates from `perm_seed`.
                let mut x = perm_seed | 1;
                for i in (1..txs.len()).rev() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    txs.swap(i, (x % (i as u64 + 1)) as usize);
                }
            }
            let old = old_order_ok(&txs, seed);
            let new = in_canonical_order(txs.clone(), seed);
            prop_assert_eq!(old, new.is_some());
            if let Some(back) = new {
                prop_assert_eq!(parts(&back), parts(&txs));
            }
        }
    }

    // ---- selection from the mempool ------------------------------------

    fn record(tx: &SignedTx, sender: Address) -> Vec<u8> {
        bincode::serialize(&MempoolRecord {
            signed: tx.clone(),
            sender,
        })
        .unwrap()
    }

    /// Everything the mempool holds, in drain order.
    fn contents(db: &Database) -> Vec<H256> {
        Mempool::new(db)
            .drain(usize::MAX / 4)
            .into_iter()
            .map(|(h, _)| h)
            .collect()
    }

    /// Execution follows commit, so a sender's next mempool nonce is often
    /// past the chain's while its predecessors are committed and waiting to
    /// execute. Those must be proposable; before `CommittedNonces` they
    /// waited until execution caught up, which bound every leader's
    /// proposals to the executor's latency.
    #[test]
    fn a_leader_proposes_past_what_is_committed_but_not_yet_executed() {
        let (_d, db) = chain_db();
        let sender = Address([7; 20]);
        // A non-zero signature: an all-zero one is what marks a synthetic
        // system transaction, which has no nonce and is never recorded.
        let mk = |n: u64| SignedTx {
            tx: Transaction {
                nonce: n,
                chain_id: CHAIN,
                body: body(0, n),
            },
            sig_r: [1; 32],
            sig_s: [1; 32],
            sig_recovery: 0,
        };
        // The chain is at nonce 0; nonces 0 and 1 are committed and not yet
        // executed; the mempool holds 2 and 3.
        for n in [2u64, 3] {
            let tx = mk(n);
            Mempool::new(&db).put(tx.hash(), sender, n, &record(&tx, sender));
        }
        let recent = HashMap::new();
        let mut committed = CommittedNonces::default();
        let got = select_from_mempool(&db, 10, &recent, std::iter::empty(), &mut committed, 6);
        assert!(got.is_empty(), "nothing continues from the chain's nonce 0");

        let ordered: Vec<VerifiedTx> = [0u64, 1]
            .iter()
            .map(|n| VerifiedTx::from_parts_unchecked(mk(*n), sender))
            .collect();
        committed.record(5, &ordered);
        let got = select_from_mempool(&db, 10, &recent, std::iter::empty(), &mut committed, 6);
        let nonces: Vec<u64> = got.iter().map(|v| v.signed.tx.nonce).collect();
        assert_eq!(nonces, vec![2, 3], "the run continues from the committed 1");
        assert_eq!(committed.len(), 1);

        // Once the chain has caught up the entry goes, and the chain's own
        // nonce is the baseline again.
        cb_state::Accounts::new(&db).put(
            &sender,
            &cb_state::AccountState {
                nonce: 4,
                ..Default::default()
            },
        );
        let tx = mk(4);
        Mempool::new(&db).put(tx.hash(), sender, 4, &record(&tx, sender));
        let got = select_from_mempool(&db, 10, &recent, std::iter::empty(), &mut committed, 7);
        assert_eq!(
            got.iter().map(|v| v.signed.tx.nonce).collect::<Vec<_>>(),
            vec![4]
        );
        assert_eq!(committed.len(), 0, "caught up: the entry is gone");

        // An entry the chain never reaches expires.
        committed.record(10, &ordered);
        assert_eq!(
            committed.expected(sender, 0, 10 + COMMITTED_NONCE_ROUNDS),
            2
        );
        assert_eq!(
            committed.expected(sender, 0, 11 + COMMITTED_NONCE_ROUNDS),
            0
        );
        assert_eq!(committed.len(), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// The rewritten selection picks the same transactions in the same
        /// order and leaves the mempool in the same state as the old one,
        /// for random mempools with nonce gaps, stale copies, transactions
        /// already seen in a proposal, in-flight proposals, corrupt records
        /// and caps below and above what is waiting.
        #[test]
        fn select_from_mempool_matches_the_old_selection(
            entries in prop::collection::vec((0u8..5, 0u64..12, 0u8..7), 0..60),
            chain_nonce in prop::collection::vec(0u64..4, 5),
            recent_mask in any::<u64>(),
            in_flight in prop::collection::vec((0u8..5, 0u64..6), 0..6),
            corrupt in 0usize..3,
            max in 1usize..40,
        ) {
            let (_d1, a) = chain_db();
            let (_d2, b) = chain_db();
            let mut recent = HashMap::new();
            for (i, (s, n, kind)) in entries.iter().enumerate() {
                let sender = Address([s + 1; 20]);
                let tx = SignedTx {
                    tx: Transaction { nonce: *n, chain_id: CHAIN, body: body(*kind, *n) },
                    sig_r: [0; 32],
                    sig_s: [0; 32],
                    sig_recovery: 0,
                };
                let h = tx.hash();
                let bytes = record(&tx, sender);
                Mempool::new(&a).put(h, sender, *n, &bytes);
                Mempool::new(&b).put(h, sender, *n, &bytes);
                if recent_mask >> (i % 64) & 1 == 1 && i % 3 == 0 {
                    recent.insert(h, 1u64);
                }
            }
            for c in 0..corrupt {
                let h = H256([0xc0 + c as u8; 32]);
                let sender = Address([c as u8 + 1; 20]);
                Mempool::new(&a).put(h, sender, 100 + c as u64, b"not a record");
                Mempool::new(&b).put(h, sender, 100 + c as u64, b"not a record");
            }
            for (i, n) in chain_nonce.iter().enumerate() {
                let st = cb_state::AccountState { nonce: *n, ..Default::default() };
                cb_state::Accounts::new(&a).put(&Address([i as u8 + 1; 20]), &st);
                cb_state::Accounts::new(&b).put(&Address([i as u8 + 1; 20]), &st);
            }
            let flight: Vec<VerifiedTx> = in_flight
                .iter()
                .map(|(s, n)| {
                    let tx = SignedTx {
                        tx: Transaction { nonce: *n, chain_id: CHAIN, body: body(0, *n) },
                        sig_r: [0; 32],
                        sig_s: [0; 32],
                        sig_recovery: 0,
                    };
                    VerifiedTx::from_parts_unchecked(tx, Address([s + 1; 20]))
                })
                .collect();
            let old = old_select_from_mempool(&a, max, &recent, std::iter::once(flight.as_slice()));
            let new = select_from_mempool(
                &b,
                max,
                &recent,
                std::iter::once(flight.as_slice()),
                &mut CommittedNonces::default(),
                0,
            );
            prop_assert_eq!(parts(&new), parts(&old));
            prop_assert_eq!(contents(&b), contents(&a));
        }
    }

    // ---- validation refuses each malformed class -----------------------

    fn two_sender_block(seed: H256) -> Vec<VerifiedTx> {
        let (sk0, a0) = key(0);
        let (sk1, a1) = key(1);
        order_for_block(
            vec![
                VerifiedTx::from_parts_unchecked(signed(&sk0, 0, body(0, 0)), a0),
                VerifiedTx::from_parts_unchecked(signed(&sk0, 1, body(3, 1)), a0),
                VerifiedTx::from_parts_unchecked(signed(&sk1, 0, body(0, 5)), a1),
                VerifiedTx::from_parts_unchecked(signed(&sk1, 1, body(4, 6)), a1),
            ],
            seed,
        )
    }

    fn veto_of(db: &Database, roots: &Roots, p: &Proposal) -> String {
        let (veto, txs, hashes) = validate_proposal(db, roots, p);
        assert!(
            txs.is_empty() && hashes.is_empty(),
            "a veto carries nothing"
        );
        veto.expect("expected a veto")
    }

    #[test]
    fn a_well_formed_proposal_passes_and_an_empty_one_needs_no_payload() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let txs = two_sender_block(seed);
        let (payload, _) = encode_payload(&txs);
        let (veto, back, _) = validate_proposal(&db, &no_roots(), &proposal(seed, payload, 4));
        assert_eq!(veto, None);
        assert_eq!(parts(&back), parts(&txs));
        let (veto, back, _) = validate_proposal(&db, &no_roots(), &proposal(seed, Vec::new(), 0));
        assert_eq!(veto, None);
        assert!(back.is_empty());
    }

    /// A block executed at commit and waiting for its certificate already
    /// counts for the audit: its state root is published once its rows are
    /// final, it is the executed head this node would claim, a proposer
    /// claiming another root at that height is vetoed, and the block sealed
    /// later carries the same root.
    #[test]
    fn the_audit_sees_an_executed_root_before_its_block_is_sealed() {
        let (_d, db) = chain_db();
        Meta::new(&db).set_consensus_scheme(cb_state::CONSENSUS_BFT);
        let (sk, a) = key(0);
        let mut st = cb_state::AccountState::default();
        st.add(AssetId::Usd, 1_000_000).unwrap();
        cb_state::Accounts::new(&db).put(&a, &st);
        let tx = VerifiedTx::from_parts_unchecked(
            SignedTx::sign_with(
                &sk,
                Transaction {
                    nonce: 0,
                    chain_id: CHAIN,
                    body: TxBody::Transfer {
                        to: Address([9; 20]),
                        asset: AssetId::Usd,
                        amount: 1,
                    },
                },
            )
            .unwrap(),
            a,
        );
        let (payload, _) = encode_payload(std::slice::from_ref(&tx));
        let p = proposal(H256([4; 32]), payload, 1);
        let roots = no_roots();
        let mut w = BlockWriter::new(
            db.clone(),
            ExecCtx {
                treasury_signer: Address::ZERO,
                ..Default::default()
            },
            roots.clone(),
            Arc::new(|_, _| {}),
        );
        let h = w.execute(&p.header, vec![tx]).unwrap().height;
        let root = *roots
            .lock()
            .unwrap()
            .get(&h)
            .expect("root published before the seal");
        assert!(Blocks::new(&db).get(h).is_none(), "sealed already");
        assert_eq!(executed_head(&db, &roots), (h, root));

        let seed = H256([5; 32]);
        let (other, _) = encode_payload(&two_sender_block(seed));
        let mut q = proposal(seed, other, 4);
        q.header.exec_height = h;
        q.header.exec_root = H256([2; 32]);
        assert!(veto_of(&db, &roots, &q).contains("executed root"));
        q.header.exec_root = root;
        assert_eq!(validate_proposal(&db, &roots, &q).0, None);

        let c = Committed {
            header: p.header.clone(),
            payload: p.payload.clone(),
            cert: BftQc {
                round: p.header.round,
                id: p.header.id(),
                votes: Vec::new(),
            },
        };
        assert!(w.seal(&c).unwrap());
        let block = Block::decode(&Blocks::new(&db).get(h).unwrap()).unwrap();
        assert_eq!(block.header.state_root, root);
        assert_eq!(executed_head(&db, &roots), (h, root));
    }

    #[test]
    fn a_proposer_whose_executed_root_differs_is_vetoed() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let roots = no_roots();
        roots.lock().unwrap().insert(3, H256([1; 32]));
        let (payload, _) = encode_payload(&two_sender_block(seed));
        let mut p = proposal(seed, payload, 4);
        p.header.exec_height = 3;
        p.header.exec_root = H256([2; 32]);
        assert!(veto_of(&db, &roots, &p).contains("executed root"));
        // The same root agrees.
        p.header.exec_root = H256([1; 32]);
        assert_eq!(validate_proposal(&db, &roots, &p).0, None);
    }

    #[test]
    fn a_duplicated_transaction_is_vetoed() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let (sk, a) = key(0);
        let t = VerifiedTx::from_parts_unchecked(signed(&sk, 0, body(0, 0)), a);
        let txs = order_for_block(vec![t.clone(), t], seed);
        let (payload, _) = encode_payload(&txs);
        assert!(veto_of(&db, &no_roots(), &proposal(seed, payload, 2)).contains("twice"));
    }

    #[test]
    fn a_signature_that_does_not_recover_is_vetoed() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let mut txs = two_sender_block(seed);
        // r at the field's top is not a valid curve point x-coordinate.
        txs[2].signed.sig_r = [0xff; 32];
        let (payload, _) = encode_payload(&txs);
        assert!(veto_of(&db, &no_roots(), &proposal(seed, payload, 4)).contains("recover"));
    }

    #[test]
    fn an_order_other_than_the_canonical_one_is_vetoed() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let mut txs = two_sender_block(seed);
        txs.reverse();
        let (payload, _) = encode_payload(&txs);
        assert!(veto_of(&db, &no_roots(), &proposal(seed, payload, 4)).contains("canonical order"));
        // The same transactions under another seed may order differently;
        // either way the check agrees with the old one.
        let txs = two_sender_block(seed);
        let other = H256([5; 32]);
        let (payload, _) = encode_payload(&txs);
        let verdict = validate_proposal(&db, &no_roots(), &proposal(other, payload, 4)).0;
        assert_eq!(verdict.is_none(), old_order_ok(&txs, other));
    }

    #[test]
    fn an_oversized_payload_is_vetoed_before_anything_is_recovered() {
        let (_d, db) = chain_db();
        let (sk, a) = key(0);
        let t = VerifiedTx::from_parts_unchecked(signed(&sk, 0, body(0, 0)), a);
        let n = cb_block::MAX_BLOCK_TXS + 1;
        let txs = vec![t; n];
        let (payload, _) = encode_payload(&txs);
        let seed = H256([4; 32]);
        assert!(
            veto_of(&db, &no_roots(), &proposal(seed, payload, n as u32)).contains("size limit")
        );
    }

    #[test]
    fn malformed_payloads_are_vetoed_with_their_reason() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let txs = two_sender_block(seed);
        let (payload, _) = encode_payload(&txs);
        let roots = no_roots();

        // A count that disagrees with the header.
        assert!(veto_of(&db, &roots, &proposal(seed, payload.clone(), 3)).contains("length"));
        // Bytes that do not decode.
        assert!(veto_of(&db, &roots, &proposal(seed, vec![0xff; 11], 1)).contains("decode"));
        // Trailing bytes: decodes, but is not the canonical encoding.
        let mut trailing = payload.clone();
        trailing.extend_from_slice(b"extra");
        assert!(veto_of(&db, &roots, &proposal(seed, trailing, 4)).contains("canonically"));
        // Another chain's transaction.
        let (sk, a) = key(0);
        let foreign = SignedTx::sign_with(
            &sk,
            Transaction {
                nonce: 0,
                chain_id: CHAIN + 1,
                body: body(0, 0),
            },
        )
        .unwrap();
        let (p, _) = encode_payload(&[VerifiedTx::from_parts_unchecked(foreign, a)]);
        assert!(veto_of(&db, &roots, &proposal(seed, p, 1)).contains("another chain"));
        // An unsigned transaction.
        let mut unsigned = txs[0].clone();
        unsigned.signed.sig_r = [0; 32];
        unsigned.signed.sig_s = [0; 32];
        let (p, _) = encode_payload(&[unsigned]);
        assert!(veto_of(&db, &roots, &proposal(seed, p, 1)).contains("unsigned"));
        // A statically invalid one.
        let zero = SignedTx::sign_with(
            &sk,
            Transaction {
                nonce: 0,
                chain_id: CHAIN,
                body: TxBody::Transfer {
                    to: Address([1; 20]),
                    asset: AssetId::Usd,
                    amount: 0,
                },
            },
        )
        .unwrap();
        let (p, _) = encode_payload(&[VerifiedTx::from_parts_unchecked(zero, a)]);
        assert!(veto_of(&db, &roots, &proposal(seed, p, 1)).contains("invalid"));
    }

    /// Senders admission already recovered are taken from the mempool, and
    /// the result is the same as recovering them.
    #[test]
    fn senders_known_to_the_mempool_are_used_and_agree() {
        let (_d, db) = chain_db();
        let seed = H256([4; 32]);
        let txs = two_sender_block(seed);
        for v in &txs[..2] {
            Mempool::new(&db).put(
                v.signed.hash(),
                v.sender,
                v.signed.tx.nonce,
                &record(&v.signed, v.sender),
            );
        }
        let (payload, _) = encode_payload(&txs);
        let (veto, back, _) = validate_proposal(&db, &no_roots(), &proposal(seed, payload, 4));
        assert_eq!(veto, None);
        assert_eq!(parts(&back), parts(&txs));
    }

    // ---- the safety record ---------------------------------------------

    #[test]
    fn a_corrupt_safety_record_is_ignored_and_a_later_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let (sk, pk) = keypair_from_seed(b"safety-2");
        let me = address_from_pubkey(&pk);
        let set = ValidatorSet::new(vec![me]).unwrap();
        let genesis = H256([6; 32]);
        {
            let db = Database::open(dir.path()).unwrap();
            db.put(cb_state::CF_META, SAFETY_KEY, b"garbage").unwrap();
            assert!(load_safety(&db).is_none(), "undecodable record is ignored");
            let mut e = engine_with_safety(
                &db,
                BftConfig::default(),
                set.clone(),
                me,
                sk,
                genesis,
                Root::genesis(genesis),
                0,
            );
            let _ = e.take_initial_actions();
            // Two rounds, persisted after each: the record only moves up.
            let _ = e.propose(Vec::new(), 0, 1, 0, H256::ZERO, 1);
            persist_safety(&db, &e).unwrap();
            let first = load_safety(&db).unwrap();
            let _ = e.propose(Vec::new(), 0, 2, 0, H256::ZERO, 2);
            persist_safety(&db, &e).unwrap();
            let second = load_safety(&db).unwrap();
            assert!(second.0 > first.0, "voted round advanced");
            assert!(second.1.round >= first.1.round);
        }
        let db = Database::open(dir.path()).unwrap();
        let (voted, high) = load_safety(&db).unwrap();
        let mut e = engine_with_safety(
            &db,
            BftConfig::default(),
            set,
            me,
            sk,
            genesis,
            Root::genesis(genesis),
            0,
        );
        let _ = e.take_initial_actions();
        assert_eq!(e.last_voted_round(), voted);
        assert_eq!(e.high_qc(), &high);
        // No second proposal (and vote) in a round already voted in.
        assert!(e.round() > voted);
    }

    /// Timings for the report, not an assertion: run with
    /// `cargo test -p cb-node --release --lib bench_hot_path -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_hot_path() {
        let seed = H256([9; 32]);
        let n_senders = 100u8;
        let per = 100u64;
        let mut txs = Vec::new();
        for s in 0..n_senders {
            let (sk, addr) = key(s);
            for n in 0..per {
                txs.push(VerifiedTx::from_parts_unchecked(
                    signed(&sk, n, body(3, n)),
                    addr,
                ));
            }
        }
        let txs = order_for_block(txs, seed);
        let reps = 20u32;
        let t = |f: &mut dyn FnMut()| {
            let start = Instant::now();
            for _ in 0..reps {
                f();
            }
            start.elapsed().as_micros() as f64 / reps as f64 / 1000.0
        };
        // Leader: build payload, engine hashes it twice, then hash each tx
        // for the mempool/recent bookkeeping.
        let old_leader = t(&mut || {
            let p = old_payload(&txs);
            let _ = old_payload_root(&p);
            let _ = old_payload_root(&p);
            let _: Vec<H256> = txs.iter().map(|v| v.signed.hash()).collect();
            let _ = (txs.clone(), txs.clone());
        });
        let new_leader = t(&mut || {
            let (p, _h) = encode_payload(&txs);
            let _ = payload_root(&p);
            let _ = payload_root(&p);
        });
        let (payload, _) = encode_payload(&txs);
        // Follower: the parts of validation that changed, plus the engine's
        // hash and the driver's per-tx hash for bookkeeping.
        let old_follower = t(&mut || {
            let signed: Vec<SignedTx> = bincode::deserialize(&payload).unwrap();
            let _: Vec<H256> = signed.iter().map(|s| s.hash()).collect();
            let _ = old_order_ok(&txs, seed);
            let _ = old_payload_root(&payload);
            let _: Vec<H256> = txs.iter().map(|v| v.signed.hash()).collect();
        });
        let (_d, db) = chain_db();
        for v in &txs {
            Mempool::new(&db).put(
                v.signed.hash(),
                v.sender,
                v.signed.tx.nonce,
                &record(&v.signed, v.sender),
            );
        }
        let p = proposal(seed, payload.clone(), txs.len() as u32);
        let full_new = t(&mut || {
            let _ = validate_proposal(&db, &no_roots(), &p);
            let _ = payload_root(&payload);
        });
        let new_follower_parts = t(&mut || {
            let signed: Vec<SignedTx> = bincode::deserialize(&payload).unwrap();
            let _ = in_canonical_order(txs.clone(), seed);
            let _ = payload_root(&payload);
            drop(signed);
        });
        // Selection: 20k waiting, 10k taken.
        let sel = |new: bool| {
            let (_d, db) = chain_db();
            let mut more = txs.clone();
            for s in 100..200u8 {
                let (sk, addr) = key(s);
                for n in 0..per {
                    more.push(VerifiedTx::from_parts_unchecked(
                        signed(&sk, n, body(3, n)),
                        addr,
                    ));
                }
            }
            for v in &more {
                Mempool::new(&db).put(
                    v.signed.hash(),
                    v.sender,
                    v.signed.tx.nonce,
                    &record(&v.signed, v.sender),
                );
            }
            let recent = HashMap::new();
            let start = Instant::now();
            let got = if new {
                select_from_mempool(
                    &db,
                    10_000,
                    &recent,
                    std::iter::empty(),
                    &mut CommittedNonces::default(),
                    0,
                )
            } else {
                old_select_from_mempool(&db, 10_000, &recent, std::iter::empty())
            };
            assert_eq!(got.len(), 10_000);
            start.elapsed().as_micros() as f64 / 1000.0
        };
        let (old_sel, new_sel) = (sel(false), sel(true));
        println!("10k-tx block, {} bytes", payload.len());
        println!(
            "leader payload+root+hashes+clones: old {old_leader:.2} ms, new {new_leader:.2} ms"
        );
        println!("follower decode+hash+order+root+rehash: old {old_follower:.2} ms, new comparable parts {new_follower_parts:.2} ms, full new validate_proposal+root {full_new:.2} ms");
        println!("select_from_mempool 20k->10k: old {old_sel:.2} ms, new {new_sel:.2} ms");
        {
            use rayon::prelude::*;
            let signed: Vec<SignedTx> = bincode::deserialize(&payload).unwrap();
            let decode = t(&mut || {
                let _: Vec<SignedTx> = bincode::deserialize(&payload).unwrap();
            });
            let statics = t(&mut || {
                for s in &signed {
                    let _ = cb_tx::validate_static(&s.tx);
                    let _ = s.is_synthetic();
                }
            });
            let sizes = t(&mut || {
                for s in &signed {
                    let _ = bincode::serialized_size(s);
                }
            });
            let enc = t(&mut || {
                let _: Vec<H256> = signed
                    .par_iter()
                    .map(|s| keccak256(&bincode::serialize(s).unwrap()))
                    .collect();
            });
            let hs: Vec<H256> = signed.iter().map(|s| s.hash()).collect();
            let known = t(&mut || {
                let _ = Mempool::new(&db).senders_of(&hs);
            });
            let order = t(&mut || {
                let _ = in_canonical_order(txs.clone(), seed);
            });
            let clone = t(&mut || {
                let _ = txs.clone();
            });
            println!("validate parts: decode {decode:.2}, static {statics:.2}, sizes {sizes:.2}, par encode+hash {enc:.2}, senders_of {known:.2}, order(incl clone) {order:.2}, clone alone {clone:.2} ms");
        }
        let start = Instant::now();
        for _ in 0..reps {
            let _ = keccak256(&payload[8..]);
        }
        println!(
            "one keccak over the payload: {:.2} ms",
            start.elapsed().as_micros() as f64 / reps as f64 / 1000.0
        );
    }
}
