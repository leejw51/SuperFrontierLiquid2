//! The gossip state machine over real TCP sockets.
//!
//! # Shape
//!
//! One task owns the [`P2pEngine`]. Every other task — the listener, one
//! reader and one writer per connection, the verifier — talks to it through
//! channels of [`Cmd`]: a bounded one for what peers cause and an unbounded
//! one for what this node causes (see [`Channels`]). Nothing else touches
//! engine state, so there is no lock around it. Everything one connection
//! sends arrives in order, because one reader task sends it on one channel.
//!
//! Every queue a peer can fill is bounded. Readers wait for room in the engine
//! queue, which stops them reading and lets TCP push back on the sender. The
//! engine never waits on a peer: each peer's writer queue is bounded by frames
//! and bytes, and a peer that lets it fill is disconnected
//! ([`PeerConn::offer`]), because blocking the engine on one slow reader would
//! stall consensus for every other peer.
//!
//! Re-execution runs on [`tokio::task::spawn_blocking`], not on the event
//! loop. Verifying a block is CPU-bound work measured in milliseconds; running
//! it inline would stall every other peer behind whichever peer happened to
//! send a body. The engine already serialises verification to one block at a
//! time, so at most one blocking task is outstanding for blocks.
//!
//! # Framing
//!
//! Each Cap'n Proto message is prefixed with a 4-byte big-endian length. The
//! length is validated against [`codec::MAX_FRAME_BYTES`] *before* anything is
//! read, and the buffer then **grows with the bytes that actually arrive**
//! rather than being allocated at the declared size. Declaring a large frame
//! and sending nothing therefore costs the sender's bytes, not the receiver's
//! credulity. Before the first message has decoded — the handshake — the cap
//! is [`MAX_HANDSHAKE_FRAME_BYTES`] instead, because a `Hello` is tiny and
//! nothing legitimate is large before one.
//!
//! Inbound connections are additionally capped at `max_peers` *before* a
//! reader is spawned ([`InboundSlots`]), and a connection that does not
//! complete a first frame inside [`HANDSHAKE_TIMEOUT`] is dropped. Both exist
//! because the engine's own `max_peers` check fires on the decoded `Hello`,
//! which a silent attacker never sends. After the handshake, a connection
//! that delivers no byte for [`ConnTimeouts::idle`] is dropped by its reader,
//! and a connection the engine disconnects is torn down end to end
//! ([`PeerConn`]), so an inbound slot is only ever held by a socket the
//! engine is actually talking to.
//!
//! Frames are encoded once, length prefix included ([`codec::encode_frame`]),
//! and shared between every peer they go to as [`Bytes`].
//!
//! # What this layer does not do
//!
//! There is no encryption and no transport authentication beyond the signed
//! handshake, which proves key possession but not freshness (see
//! [`crate::p2p::message::hello_signing_bytes`]). A network adversary can read
//! and modify gossip traffic. This is survivable precisely because gossip
//! confers no authority: a modified block fails re-execution, and a forged
//! node id buys nothing but a peer-table slot. It would still be the first
//! thing to add before running this on a public network.

use crate::p2p::chain::ChainAccess;
use crate::p2p::codec::{self, MAX_FRAME_BYTES};
use crate::p2p::config::P2pConfig;
use crate::p2p::engine::{Action, Event, LocalNode, P2pEngine, P2pStats};
use crate::p2p::message::{DisconnectReason, Message};
use crate::p2p::peer_table::PeerId;
use bytes::Bytes;
use cb_types::H256;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// How often the engine is ticked. Everything time-driven — dialing,
/// discovery, request timeouts, pings — happens on this cadence, so it bounds
/// the resolution of every timeout in [`P2pConfig`].
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// Largest frame accepted before a connection has produced a decodable
/// message.
///
/// The first frame is the handshake, and a `Hello` is a few hundred bytes. The
/// full [`MAX_FRAME_BYTES`] only becomes available once a peer has shown it
/// speaks the protocol.
pub const MAX_HANDSHAKE_FRAME_BYTES: usize = 16 * 1024;

/// How long a connection may go without producing its first frame, by
/// default.
///
/// A running node uses its [`P2pConfig::request_timeout`], whose default this
/// is (see [`ConnTimeouts::from_config`]). Without it, a peer that
/// connects and says nothing holds an inbound slot and a task for as long as
/// it likes — which is the cheapest possible way to exhaust the connection
/// cap this module just added.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many bytes are copied per read while a frame is being assembled.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// How many messages every reader together may have queued for the engine.
///
/// Readers `await` a slot, so a full queue stops them reading and TCP flow
/// control pushes back on the senders. The alternative — an unbounded queue —
/// let a peer that sends faster than the engine drains grow this node's
/// memory without limit, since every frame it sent was decoded and parked
/// whether or not the engine would ever get to it.
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

/// How many frames may wait for one peer's socket.
pub const WRITER_QUEUE_FRAMES: usize = 1024;

/// How many bytes may wait for one peer's socket.
///
/// Two maximum frames: room for a full catch-up range reply with a frame's
/// worth of consensus traffic queued behind it, so an honest peer that is
/// being served a range is not dropped for also being sent proposals. A
/// frame is always admitted to an empty queue as well. A peer that is further
/// behind than this on reading is dropped rather than buffered for; see
/// [`PeerConn::offer`].
pub const WRITER_QUEUE_BYTES: usize = 2 * MAX_FRAME_BYTES;

/// A counting semaphore over inbound connections.
///
/// The engine rejects a peer over `max_peers` when it decodes the peer's
/// `Hello`. A connection that never sends one is never rejected, so the cap
/// has to exist at `accept` time as well. This is that cap: cheap, and it
/// releases on drop so a closed socket frees its slot without bookkeeping.
#[derive(Debug, Clone)]
pub struct InboundSlots {
    active: Arc<AtomicUsize>,
    limit: usize,
}

/// A held inbound slot. Releases on drop.
#[derive(Debug)]
pub struct InboundSlot {
    active: Arc<AtomicUsize>,
}

impl InboundSlots {
    /// A cap of `limit` simultaneous inbound connections.
    pub fn new(limit: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit: limit.max(1),
        }
    }

    /// Take a slot, or `None` when the cap is reached.
    pub fn try_acquire(&self) -> Option<InboundSlot> {
        let mut current = self.active.load(Ordering::Relaxed);
        loop {
            if current >= self.limit {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(InboundSlot {
                        active: Arc::clone(&self.active),
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// How many inbound connections are live.
    pub fn in_use(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

impl Drop for InboundSlot {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The clocks a connection's reader runs on.
#[derive(Clone, Copy, Debug)]
pub struct ConnTimeouts {
    /// Budget for the whole first frame, header and body.
    pub handshake: Duration,
    /// Longest a handshaken connection may go without a single byte arriving,
    /// and longest one write to it may take.
    ///
    /// The engine pings every handshaken peer each `ping_interval`, and a live
    /// peer answers, so silence this long means the peer is gone or is
    /// holding the socket open on purpose. Either way the socket, its task and
    /// its inbound slot are released here, by the reader, rather than waiting
    /// on the engine: the engine can only time out a peer it still has a
    /// record for.
    pub idle: Duration,
}

impl ConnTimeouts {
    /// The timeouts a node built from `cfg` runs with.
    pub fn from_config(cfg: &P2pConfig) -> Self {
        Self {
            handshake: cfg.request_timeout,
            idle: cfg.peer_timeout,
        }
    }
}

/// Why reading a frame stopped.
#[derive(Debug)]
enum FrameError {
    /// The socket failed or closed.
    Io,
    /// Nothing arrived within the allowed time.
    Idle,
    /// The declared length is outside what this connection may send.
    OutOfRange(usize),
}

/// Read exactly `len` bytes into `buf`, growing it as the bytes arrive.
///
/// The point of taking `&mut Vec<u8>` rather than returning one: the buffer's
/// *capacity* is the thing under test. `vec![0u8; len]` commits the declared
/// size before the peer has sent a byte, which is how 96 bytes of attacker
/// input became ~940 MB of resident memory across 24 sockets. Here a peer that
/// declares 64 MiB and sends 100 bytes costs 100 bytes and an error.
///
/// Every individual read must make progress within `idle`, so a peer that
/// sends one byte a minute cannot hold the reader forever either.
async fn read_frame_into<R>(
    reader: &mut R,
    len: usize,
    buf: &mut Vec<u8>,
    idle: Duration,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    buf.clear();
    buf.reserve(len.min(READ_CHUNK_BYTES));
    let mut chunk = vec![0u8; len.min(READ_CHUNK_BYTES)];
    while buf.len() < len {
        let want = (len - buf.len()).min(chunk.len());
        let read = match tokio::time::timeout(idle, reader.read(&mut chunk[..want])).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "peer stalled mid-frame",
                ))
            }
        };
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed mid-frame",
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Ok(())
}

/// Read one length-prefixed frame of at most `cap` bytes into `buf`.
async fn read_one_frame<R>(
    reader: &mut R,
    cap: usize,
    idle: Duration,
    buf: &mut Vec<u8>,
) -> Result<(), FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match tokio::time::timeout(idle, reader.read_exact(&mut len_buf)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => return Err(FrameError::Io),
        Err(_) => return Err(FrameError::Idle),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > cap {
        return Err(FrameError::OutOfRange(len));
    }
    read_frame_into(reader, len, buf, idle).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::TimedOut {
            FrameError::Idle
        } else {
            FrameError::Io
        }
    })
}

/// The engine's end of one connection.
///
/// Owning this is what keeps the connection open. Dropping it — because the
/// engine disconnected the peer, or the peer disconnected, or the node is
/// shutting down — closes the writer's queue, so the writer flushes what is
/// already queued (a `Disconnect` reason, typically) and shuts its half down,
/// and fires `reader_close`, which ends the reader task, drops the read half
/// and releases the inbound slot.
///
/// Before this, disconnecting a peer only dropped the writer's queue. The
/// reader went on reading, with no timeout once past the handshake, and held
/// its inbound slot for as long as the remote end kept the socket open: a
/// few dozen peers that sent a rejected `Hello` and then sat idle occupied
/// every inbound slot this node had.
struct PeerConn {
    frames: mpsc::Sender<Bytes>,
    queued: Arc<AtomicUsize>,
    writer: tokio::task::AbortHandle,
    _reader_close: oneshot::Sender<()>,
}

/// What happened to a frame offered to a peer.
#[derive(Debug, PartialEq, Eq)]
enum Offer {
    Queued,
    /// The writer has already stopped; the reader will report the close.
    Gone,
    /// The peer is not reading fast enough. Drop it.
    Overflow,
}

impl PeerConn {
    /// Queue a frame for this peer without waiting.
    ///
    /// Never blocks: the engine task calls this, and one peer that has stopped
    /// reading must not stall consensus for everyone else. A peer whose queue
    /// is full, by frame count or by bytes, is reported as an overflow and the
    /// caller drops it. An empty queue always admits a frame, so a legitimate
    /// maximum-size frame is never refused on size alone.
    fn offer(&self, frame: Bytes) -> Offer {
        let len = frame.len();
        let before = self.queued.fetch_add(len, Ordering::AcqRel);
        if before != 0 && before.saturating_add(len) > WRITER_QUEUE_BYTES {
            self.queued.fetch_sub(len, Ordering::AcqRel);
            return Offer::Overflow;
        }
        match self.frames.try_send(frame) {
            Ok(()) => Offer::Queued,
            Err(e) => {
                self.queued.fetch_sub(len, Ordering::AcqRel);
                match e {
                    mpsc::error::TrySendError::Full(_) => Offer::Overflow,
                    mpsc::error::TrySendError::Closed(_) => Offer::Gone,
                }
            }
        }
    }

    /// Drop a peer that fell behind: stop its writer at once rather than
    /// letting it drain a queue the peer was not reading.
    fn abort(self) {
        self.writer.abort();
    }
}

/// A message to the engine task.
enum Cmd {
    Register {
        peer: PeerId,
        addr: Option<SocketAddr>,
        outbound: bool,
        conn: PeerConn,
    },
    Event(Event),
    /// A transaction this node accepted locally: remember the body so peers
    /// can fetch it, then announce it.
    LocalTx {
        hash: H256,
        bytes: Vec<u8>,
    },
    Stats(oneshot::Sender<P2pStats>),
    /// Send a consensus message to every peer.
    ConsensusBroadcast(Vec<u8>),
    /// Send a consensus message to one peer.
    ConsensusTo {
        peer: PeerId,
        bytes: Vec<u8>,
    },
    /// Send a consensus message to any one peer.
    ConsensusAny(Vec<u8>),
    /// Where inbound consensus messages go from now on.
    ConsensusSink(mpsc::UnboundedSender<ConsensusInbound>),
    /// Fetch announced blocks and sync ranges, or not.
    FetchBlocks(bool),
    Shutdown,
}

/// The two ways into the engine task.
///
/// `events` carries everything a remote peer causes — registration, decoded
/// messages, closes — and is bounded, so peers are pushed back on when the
/// engine is behind. `ctl` carries what this node itself causes: the handle's
/// calls, verifier verdicts, and the engine's own `Disconnected` bookkeeping.
/// It stays unbounded on purpose. Its producers are local and each is bounded
/// by something else (one block verification at a time, the node's own
/// consensus rate), the handle's methods are synchronous and cannot wait, and
/// the engine posting to a bounded queue that only it drains would deadlock
/// the moment the queue filled.
#[derive(Clone)]
struct Channels {
    ctl: mpsc::UnboundedSender<Cmd>,
    events: mpsc::Sender<Cmd>,
}

/// A consensus message from a peer, as the driver receives it.
#[derive(Clone, Debug)]
pub struct ConsensusInbound {
    /// The connection it came in on; pass it back to
    /// [`P2pHandle::send_consensus`] to answer.
    pub from: PeerId,
    /// `cb_consensus::bft::Message` bincode bytes, unverified.
    pub bytes: Vec<u8>,
}

/// A handle to a running peer-to-peer node.
///
/// Cloneable and `Send`, so the sequencer loop and the RPC server can both
/// push local announcements in without sharing engine state.
#[derive(Clone)]
pub struct P2pHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    node_id: crate::p2p::identity::NodeId,
    listen: Option<SocketAddr>,
}

impl P2pHandle {
    /// Announce a block this node produced or applied locally. Gossip picks it
    /// up from here exactly as it would a block that arrived from a peer.
    pub fn announce_block(&self, hash: H256, height: u64) {
        let _ = self.tx.send(Cmd::Event(Event::BlockAccepted {
            hash,
            height,
            from: None,
        }));
    }

    /// Announce a transaction this node accepted from a local client, so an
    /// order submitted to any node reaches the sequencer.
    ///
    /// The body is handed over with the hash, not just the hash: gossip is
    /// announce-then-fetch, so a node that announces something it cannot
    /// subsequently serve makes every peer wait out a timeout and is scored
    /// down by all of them for it.
    pub fn announce_tx(&self, hash: H256, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::LocalTx { hash, bytes });
    }

    /// Send a consensus message to every handshaked peer.
    pub fn broadcast_consensus(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::ConsensusBroadcast(bytes));
    }

    /// Send a consensus message to one handshaked peer, a different one
    /// each time: for a request any of them can answer.
    pub fn send_consensus_any(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::ConsensusAny(bytes));
    }

    /// Send a consensus message to one peer, by the connection an inbound
    /// message named.
    pub fn send_consensus(&self, peer: PeerId, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::ConsensusTo { peer, bytes });
    }

    /// Fetch announced blocks and sync ranges (true), or only serve them.
    pub fn set_fetch_blocks(&self, on: bool) {
        let _ = self.tx.send(Cmd::FetchBlocks(on));
    }

    /// Route inbound consensus messages to `sink`. Until one is installed
    /// they are dropped, which is what a node that runs no consensus wants.
    pub fn set_consensus_sink(&self, sink: mpsc::UnboundedSender<ConsensusInbound>) {
        let _ = self.tx.send(Cmd::ConsensusSink(sink));
    }

    /// This node's network identity.
    pub fn node_id(&self) -> crate::p2p::identity::NodeId {
        self.node_id
    }

    /// The address inbound peers are accepted on, if any.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen
    }

    /// Read the engine's counters. Returns `None` if the engine has stopped.
    pub async fn stats(&self) -> Option<P2pStats> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Stats(tx)).ok()?;
        rx.await.ok()
    }

    /// Ask the engine task to stop.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

/// Start a peer-to-peer node: bind the listener, dial the seeds, and run the
/// engine until [`P2pHandle::shutdown`] is called.
///
/// `chain` is shared with the caller because the node's own block producer
/// writes to the same database; the mutex is held only for the duration of one
/// read or one apply.
pub async fn spawn<C>(
    cfg: P2pConfig,
    local: LocalNode,
    chain: Arc<Mutex<C>>,
) -> anyhow::Result<P2pHandle>
where
    C: ChainAccess + Send + 'static,
{
    cfg.validate()?;
    let head = lock(&chain).head_height();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let channels = Channels {
        ctl: ctl_tx.clone(),
        events: events_tx,
    };
    let peer_ids = Arc::new(AtomicU64::new(1));
    let timeouts = ConnTimeouts::from_config(&cfg);

    let listener = match cfg.listen {
        Some(addr) => {
            let l = TcpListener::bind(addr).await?;
            tracing::info!(%addr, node_id = %local.node_id, "p2p listening");
            Some(l)
        }
        None => {
            tracing::info!(node_id = %local.node_id, "p2p running outbound-only");
            None
        }
    };
    // Re-read the bound address: a config asking for port 0 must advertise the
    // port the OS actually gave us, or peers will be told to dial port 0.
    let bound = match &listener {
        Some(l) => Some(l.local_addr()?),
        None => None,
    };
    let mut cfg = cfg;
    cfg.listen = bound;

    let handle = P2pHandle {
        tx: ctl_tx,
        node_id: local.node_id,
        listen: bound,
    };

    if let Some(listener) = listener {
        let events = channels.events.clone();
        let peer_ids = peer_ids.clone();
        // The cap is applied here, before a reader task exists, because the
        // engine's own `max_peers` rejection needs a decoded `Hello` and a peer
        // that intends to exhaust us never sends one.
        let slots = InboundSlots::new(cfg.max_peers);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        let Some(slot) = slots.try_acquire() else {
                            tracing::warn!(
                                %addr,
                                in_use = slots.in_use(),
                                "refusing an inbound p2p connection: at the connection cap"
                            );
                            drop(stream);
                            continue;
                        };
                        let peer = peer_ids.fetch_add(1, Ordering::Relaxed);
                        spawn_connection(
                            stream,
                            peer,
                            Some(addr),
                            false,
                            events.clone(),
                            Some(slot),
                            timeouts,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(?e, "p2p accept failed");
                        return;
                    }
                }
            }
        });
    }

    let engine = P2pEngine::new(cfg, local, head, 0);
    tokio::spawn(engine_loop(
        engine, chain, ctl_rx, events_rx, channels, peer_ids, timeouts,
    ));
    Ok(handle)
}

/// Lock a mutex, recovering from a poisoned one.
///
/// A panic in a verifier task must not take the whole gossip layer down with
/// it: the chain behind the mutex is a database handle, not an invariant that
/// a panic could have left half-updated, so continuing is safe and dropping
/// every peer is not.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!("p2p chain mutex was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Everything `exec` needs besides the actions themselves.
struct Driver<'a, C> {
    chain: &'a Arc<Mutex<C>>,
    writers: &'a mut HashMap<PeerId, PeerConn>,
    channels: &'a Channels,
    peer_ids: &'a Arc<AtomicU64>,
    sink: &'a Option<mpsc::UnboundedSender<ConsensusInbound>>,
    timeouts: ConnTimeouts,
}

#[allow(clippy::too_many_arguments)]
async fn engine_loop<C>(
    mut engine: P2pEngine,
    chain: Arc<Mutex<C>>,
    mut ctl_rx: mpsc::UnboundedReceiver<Cmd>,
    mut events_rx: mpsc::Receiver<Cmd>,
    channels: Channels,
    peer_ids: Arc<AtomicU64>,
    timeouts: ConnTimeouts,
) where
    C: ChainAccess + Send + 'static,
{
    let start = Instant::now();
    let mut writers: HashMap<PeerId, PeerConn> = HashMap::new();
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sink: Option<mpsc::UnboundedSender<ConsensusInbound>> = None;

    macro_rules! run {
        ($actions:expr) => {{
            let actions = $actions;
            exec(
                Driver {
                    chain: &chain,
                    writers: &mut writers,
                    channels: &channels,
                    peer_ids: &peer_ids,
                    sink: &sink,
                    timeouts,
                },
                actions,
            );
        }};
    }

    run!(engine.bootstrap());

    loop {
        let cmd = tokio::select! {
            cmd = ctl_rx.recv() => match cmd {
                Some(c) => c,
                None => return,
            },
            // `events` never closes while this task lives: it holds a sender.
            Some(cmd) = events_rx.recv() => cmd,
            _ = ticker.tick() => {
                let now_ms = start.elapsed().as_millis() as u64;
                run!(engine.handle(Event::Tick { now_ms }));
                continue;
            }
        };
        match cmd {
            Cmd::Shutdown => {
                // Dropping every connection closes every writer queue and ends
                // every reader.
                writers.clear();
                return;
            }
            Cmd::LocalTx { hash, bytes } => {
                lock(&chain).remember_tx(hash, bytes);
                run!(engine.handle(Event::TxAccepted { hash, from: None }));
            }
            Cmd::Stats(reply) => {
                let _ = reply.send(engine.stats());
            }
            Cmd::ConsensusBroadcast(bytes) => run!(engine.broadcast_consensus(bytes)),
            Cmd::ConsensusTo { peer, bytes } => run!(engine.send_consensus(peer, bytes)),
            Cmd::ConsensusAny(bytes) => run!(engine.send_consensus_any(bytes)),
            Cmd::ConsensusSink(s) => {
                sink = Some(s);
            }
            Cmd::FetchBlocks(on) => {
                engine.set_fetch_blocks(on);
            }
            Cmd::Register {
                peer,
                addr,
                outbound,
                conn,
            } => {
                writers.insert(peer, conn);
                run!(engine.handle(Event::Connected {
                    peer,
                    addr,
                    outbound,
                }));
            }
            Cmd::Event(event) => {
                let from_peer = match &event {
                    Event::Disconnected { peer } => {
                        writers.remove(peer);
                        None
                    }
                    Event::Message { peer, .. } => Some(*peer),
                    _ => None,
                };
                run!(engine.handle(event));
                // A socket stays open only while the engine has state for it.
                // This catches the paths where the engine forgets a peer
                // without asking for a disconnect — chiefly a peer's own
                // `Disconnect`, after which it could otherwise keep the socket
                // and its inbound slot indefinitely.
                if let Some(peer) = from_peer {
                    if !engine.is_tracking(peer) {
                        writers.remove(&peer);
                    }
                }
            }
        }
    }
}

fn exec<C>(d: Driver<'_, C>, actions: Vec<Action>)
where
    C: ChainAccess + Send + 'static,
{
    let Driver {
        chain,
        writers,
        channels,
        peer_ids,
        sink,
        timeouts,
    } = d;
    let mut overflowed: Vec<PeerId> = Vec::new();
    for action in actions {
        match action {
            Action::Dial { addr } => {
                let peer = peer_ids.fetch_add(1, Ordering::Relaxed);
                let events = channels.events.clone();
                tokio::spawn(async move {
                    match TcpStream::connect(addr).await {
                        // Outbound connections are bounded by the engine's own
                        // dialing policy, so they take no inbound slot.
                        Ok(stream) => {
                            spawn_connection(stream, peer, Some(addr), true, events, None, timeouts)
                        }
                        Err(e) => tracing::debug!(%addr, ?e, "p2p dial failed"),
                    }
                });
            }
            Action::Send { peer, msg } => {
                if let Some(frame) = frame_of(&msg) {
                    send_frame(writers, peer, frame, &mut overflowed);
                }
            }
            Action::Broadcast { peers, msg } => {
                // Encoded once; every peer's queue holds the same buffer.
                if let Some(frame) = frame_of(&msg) {
                    for peer in peers {
                        send_frame(writers, peer, frame.clone(), &mut overflowed);
                    }
                }
            }
            Action::Disconnect { peer, reason } => {
                tracing::debug!(peer, %reason, "p2p disconnecting");
                if let Some(frame) = frame_of(&Message::Disconnect(reason)) {
                    send_frame(writers, peer, frame, &mut overflowed);
                }
                writers.remove(&peer);
                let _ = channels.ctl.send(Cmd::Event(Event::Disconnected { peer }));
            }
            Action::VerifyBlock {
                from,
                height,
                bytes,
            } => {
                let chain = chain.clone();
                let ctl = channels.ctl.clone();
                // Off the event loop: this is the expensive, load-bearing
                // re-execution, and the engine will not issue another until
                // this one reports back.
                tokio::task::spawn_blocking(move || {
                    let result = lock(&chain).verify_and_apply(&bytes);
                    let event = match result {
                        Ok((hash, applied)) => Event::BlockAccepted {
                            hash,
                            height: applied,
                            from,
                        },
                        Err(reason) => Event::BlockRejected {
                            from,
                            height,
                            reason,
                        },
                    };
                    let _ = ctl.send(Cmd::Event(event));
                });
            }
            Action::VerifyTx { from, bytes } => {
                let chain = chain.clone();
                let ctl = channels.ctl.clone();
                tokio::task::spawn_blocking(move || {
                    let event = match lock(&chain).accept_tx(&bytes) {
                        Ok(hash) => Event::TxAccepted {
                            hash,
                            from: Some(from),
                        },
                        Err(reason) => Event::TxRejected {
                            from: Some(from),
                            reason,
                        },
                    };
                    let _ = ctl.send(Cmd::Event(event));
                });
            }
            Action::ServeBlock { peer, hash } => {
                let found = {
                    let c = lock(chain);
                    c.height_of(&hash)
                        .and_then(|h| c.block_bytes(h).map(|b| (h, b)))
                };
                if let Some((height, bytes)) = found {
                    if let Some(frame) = frame_of(&Message::BlockBody { height, bytes }) {
                        send_frame(writers, peer, frame, &mut overflowed);
                    }
                }
            }
            Action::ServeBlockRange { peer, from, to } => {
                let blocks = {
                    let c = lock(chain);
                    let mut out = Vec::new();
                    for h in from..=to {
                        match c.block_bytes(h) {
                            Some(b) => out.push((h, b)),
                            // Stop at the first gap: a pruned peer answers
                            // with what it has rather than with an error.
                            None => break,
                        }
                    }
                    out
                };
                if let Some(frame) = frame_of(&Message::BlockRange { blocks }) {
                    send_frame(writers, peer, frame, &mut overflowed);
                }
            }
            Action::Consensus { from, bytes } => {
                if let Some(s) = sink {
                    let _ = s.send(ConsensusInbound { from, bytes });
                }
            }
            Action::ServeTx { peer, hash } => {
                let bytes = lock(chain).tx_bytes(&hash);
                if let Some(bytes) = bytes {
                    if let Some(frame) = frame_of(&Message::TxBody { bytes }) {
                        send_frame(writers, peer, frame, &mut overflowed);
                    }
                }
            }
        }
    }
    for peer in overflowed {
        if let Some(conn) = writers.remove(&peer) {
            tracing::warn!(peer, "dropping a p2p peer that is not reading what we send");
            conn.abort();
            let _ = channels.ctl.send(Cmd::Event(Event::Disconnected { peer }));
        }
    }
}

/// Encode a message into its wire frame, logging rather than failing.
fn frame_of(msg: &Message) -> Option<Bytes> {
    match codec::encode_frame(msg) {
        Ok(frame) => Some(frame),
        Err(e) => {
            tracing::warn!(kind = msg.kind(), ?e, "could not encode p2p message");
            None
        }
    }
}

/// Queue `frame` for `peer`, noting the peer for removal if it is too far
/// behind to take it. Dropping is deferred to the end of the batch so one
/// batch never sends to a connection it has already torn down.
fn send_frame(
    writers: &HashMap<PeerId, PeerConn>,
    peer: PeerId,
    frame: Bytes,
    overflowed: &mut Vec<PeerId>,
) {
    let Some(conn) = writers.get(&peer) else {
        return;
    };
    if conn.offer(frame) == Offer::Overflow && !overflowed.contains(&peer) {
        overflowed.push(peer);
    }
}

/// Wire a connected socket up to the engine: a writer task draining a queue,
/// and a reader task turning frames into events.
fn spawn_connection(
    stream: TcpStream,
    peer: PeerId,
    addr: Option<SocketAddr>,
    outbound: bool,
    events: mpsc::Sender<Cmd>,
    slot: Option<InboundSlot>,
    timeouts: ConnTimeouts,
) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!(peer, ?e, "could not disable Nagle on a p2p socket");
    }
    let (read_half, write_half) = stream.into_split();
    wire_connection(
        read_half, write_half, peer, addr, outbound, events, slot, timeouts,
    );
}

/// [`spawn_connection`] over any byte stream, so the reader and writer can be
/// tested against an in-memory pipe.
#[allow(clippy::too_many_arguments)]
fn wire_connection<R, W>(
    mut read_half: R,
    mut write_half: W,
    peer: PeerId,
    addr: Option<SocketAddr>,
    outbound: bool,
    events: mpsc::Sender<Cmd>,
    slot: Option<InboundSlot>,
    timeouts: ConnTimeouts,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (wtx, mut wrx) = mpsc::channel::<Bytes>(WRITER_QUEUE_FRAMES);
    let queued = Arc::new(AtomicUsize::new(0));
    let (close_tx, close_rx) = oneshot::channel::<()>();

    let writer_queued = Arc::clone(&queued);
    let idle = timeouts.idle;
    let writer = tokio::spawn(async move {
        while let Some(frame) = wrx.recv().await {
            // The frame already carries its length prefix: one write per
            // frame, and the same buffer every other peer is being sent.
            let len = frame.len();
            let written = tokio::time::timeout(idle, write_half.write_all(&frame)).await;
            writer_queued.fetch_sub(len, Ordering::AcqRel);
            if !matches!(written, Ok(Ok(()))) {
                break;
            }
        }
        let _ = tokio::time::timeout(idle, write_half.shutdown()).await;
    })
    .abort_handle();

    let conn = PeerConn {
        frames: wtx,
        queued,
        writer,
        _reader_close: close_tx,
    };

    tokio::spawn(async move {
        // Held for the life of the reader: the slot is released when this task
        // ends, whatever ends it.
        let _slot = slot;
        // Registration precedes every message from this connection because it
        // is sent from the same task, on the same channel, before the read
        // loop starts.
        if events
            .send(Cmd::Register {
                peer,
                addr,
                outbound,
                conn,
            })
            .await
            .is_err()
        {
            return;
        }
        let ended_by_peer = tokio::select! {
            // The engine dropped this connection. It already knows; say
            // nothing, and let the read half go with this future.
            _ = close_rx => false,
            () = read_loop(&mut read_half, peer, &events, timeouts) => true,
        };
        if ended_by_peer {
            let _ = events.send(Cmd::Event(Event::Disconnected { peer })).await;
        }
    });
}

/// Read frames until the connection fails, stalls, or misbehaves.
async fn read_loop<R>(
    read_half: &mut R,
    peer: PeerId,
    events: &mpsc::Sender<Cmd>,
    timeouts: ConnTimeouts,
) where
    R: AsyncRead + Unpin,
{
    let mut handshaken = false;
    loop {
        // Declared per iteration on purpose: the buffer's capacity is the
        // peer's to influence, so a connection that once sent a large
        // legitimate frame should not go on holding that much for the rest of
        // its life.
        let mut frame = Vec::new();
        let result = if handshaken {
            read_one_frame(read_half, MAX_FRAME_BYTES, timeouts.idle, &mut frame).await
        } else {
            // The whole first frame is on one clock, so trickling it a byte
            // at a time does not extend it.
            match tokio::time::timeout(
                timeouts.handshake,
                read_one_frame(
                    read_half,
                    MAX_HANDSHAKE_FRAME_BYTES,
                    timeouts.handshake,
                    &mut frame,
                ),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(FrameError::Idle),
            }
        };
        match result {
            Ok(()) => {}
            Err(FrameError::Io) => return,
            Err(FrameError::Idle) => {
                if handshaken {
                    tracing::debug!(peer, "p2p connection went silent; closing it");
                } else {
                    tracing::warn!(peer, "p2p connection sent no handshake in time");
                }
                return;
            }
            Err(FrameError::OutOfRange(len)) => {
                tracing::warn!(peer, len, handshaken, "p2p frame length out of range");
                let _ = events
                    .send(Cmd::Event(Event::Message {
                        peer,
                        // Feeding a protocol violation through the normal path
                        // means the peer is scored for it, not just dropped.
                        msg: Message::Disconnect(DisconnectReason::ProtocolViolation),
                    }))
                    .await;
                return;
            }
        }
        match codec::decode(&frame) {
            Ok(msg) => {
                handshaken = true;
                if events
                    .send(Cmd::Event(Event::Message { peer, msg }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(peer, ?e, "undecodable p2p frame");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_slots_cap_connections_and_release_on_drop() {
        let slots = InboundSlots::new(2);
        let first = slots.try_acquire().expect("one");
        let second = slots.try_acquire().expect("two");
        assert_eq!(slots.in_use(), 2);
        assert!(
            slots.try_acquire().is_none(),
            "a third connection must be refused before a reader is spawned"
        );
        drop(first);
        assert_eq!(slots.in_use(), 1);
        assert!(slots.try_acquire().is_some());
        drop(second);
    }

    #[tokio::test]
    async fn a_declared_frame_costs_the_bytes_that_arrive_not_the_bytes_claimed() {
        // Finding 4, in one assertion: 24 connections each declaring 64 MiB
        // and sending nothing took a node from 15 MB to ~940 MB, because the
        // reader allocated the declared size up front.
        let (mut client, mut server) = tokio::io::duplex(4096);
        let declared = MAX_FRAME_BYTES;
        tokio::spawn(async move {
            let _ = client.write_all(&[7u8; 100]).await;
            // Then close, standing in for a peer that declares and stalls.
        });
        let mut buf = Vec::new();
        let result = read_frame_into(&mut server, declared, &mut buf, Duration::from_secs(5)).await;
        assert!(result.is_err(), "a peer that closes mid-frame is an error");
        assert_eq!(buf.len(), 100, "only what arrived was kept");
        assert!(
            buf.capacity() < 1024 * 1024,
            "the buffer grew with the bytes received, not the {declared} declared:              capacity was {}",
            buf.capacity()
        );
    }

    #[tokio::test]
    async fn a_complete_frame_is_read_exactly() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let payload: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        tokio::spawn(async move {
            let _ = client.write_all(&payload).await;
        });
        let mut buf = Vec::new();
        read_frame_into(
            &mut server,
            expected.len(),
            &mut buf,
            Duration::from_secs(5),
        )
        .await
        .expect("a complete frame");
        assert_eq!(buf, expected);
    }

    /// One connection wired over an in-memory pipe, as the engine would see
    /// it: the remote end, the engine's handle on the connection, the queue
    /// the reader reports into, and the inbound slot counter.
    struct Wired {
        remote: tokio::io::DuplexStream,
        conn: Option<PeerConn>,
        events: mpsc::Receiver<Cmd>,
        slots: InboundSlots,
    }

    async fn wired(timeouts: ConnTimeouts) -> Wired {
        wired_with_pipe(timeouts, 1024 * 1024).await
    }

    async fn wired_with_pipe(timeouts: ConnTimeouts, pipe_bytes: usize) -> Wired {
        let (remote, local) = tokio::io::duplex(pipe_bytes);
        let (r, w) = tokio::io::split(local);
        let (events_tx, mut events) = mpsc::channel(16);
        let slots = InboundSlots::new(1);
        let slot = slots.try_acquire().expect("a slot");
        wire_connection(r, w, 7, None, false, events_tx, Some(slot), timeouts);
        let conn = match events.recv().await {
            Some(Cmd::Register { conn, .. }) => conn,
            _ => panic!("registration comes first"),
        };
        Wired {
            remote,
            conn: Some(conn),
            events,
            slots,
        }
    }

    fn short(idle_ms: u64) -> ConnTimeouts {
        ConnTimeouts {
            handshake: Duration::from_secs(5),
            idle: Duration::from_millis(idle_ms),
        }
    }

    /// Poll until `f` holds, failing after `secs`.
    async fn eventually(secs: u64, label: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for: {label}");
    }

    async fn send_frame_to(remote: &mut tokio::io::DuplexStream, msg: &Message) {
        let frame = codec::encode_frame(msg).expect("frame");
        remote.write_all(&frame).await.expect("write");
    }

    #[tokio::test]
    async fn a_handshaken_peer_that_goes_silent_is_closed_and_frees_its_slot() {
        // Past the handshake the reader used to wait forever. A peer that
        // spoke once and then held the socket open kept its task and its
        // inbound slot until the engine's peer timeout — and indefinitely if
        // the engine had already forgotten it.
        let mut w = wired(short(200)).await;
        send_frame_to(&mut w.remote, &Message::Ping { nonce: 1 }).await;
        assert!(matches!(
            w.events.recv().await,
            Some(Cmd::Event(Event::Message { peer: 7, .. }))
        ));
        assert_eq!(w.slots.in_use(), 1);
        // Now nothing, with the socket still open.
        let closed = tokio::time::timeout(Duration::from_secs(5), w.events.recv())
            .await
            .expect("the reader gave up on a silent peer");
        assert!(matches!(
            closed,
            Some(Cmd::Event(Event::Disconnected { peer: 7 }))
        ));
        let slots = w.slots.clone();
        eventually(5, "slot released", || slots.in_use() == 0).await;
    }

    #[tokio::test]
    async fn a_peer_that_trickles_a_frame_is_held_to_the_idle_timeout_per_read() {
        let mut w = wired(short(200)).await;
        send_frame_to(&mut w.remote, &Message::Ping { nonce: 1 }).await;
        let _ = w.events.recv().await;
        // Declare a frame and send part of it, then stall.
        w.remote
            .write_all(&100u32.to_be_bytes())
            .await
            .expect("len");
        w.remote.write_all(&[0u8; 10]).await.expect("part");
        let closed = tokio::time::timeout(Duration::from_secs(5), w.events.recv())
            .await
            .expect("a stalled frame is abandoned");
        assert!(matches!(
            closed,
            Some(Cmd::Event(Event::Disconnected { peer: 7 }))
        ));
    }

    #[tokio::test]
    async fn dropping_the_connection_ends_the_reader_even_while_the_peer_holds_the_socket() {
        // Finding: after `Action::Disconnect` the engine dropped only the
        // writer's queue. The reader kept reading a socket the remote end
        // never closed, so ~32 peers that were rejected at the handshake and
        // then sat idle filled every inbound slot. The idle timeout here is an
        // hour, so only the teardown can release the slot in time.
        let mut w = wired(short(3_600_000)).await;
        send_frame_to(&mut w.remote, &Message::Ping { nonce: 1 }).await;
        let _ = w.events.recv().await;
        assert_eq!(w.slots.in_use(), 1);

        let conn = w.conn.take().expect("conn");
        assert_eq!(
            conn.offer(
                codec::encode_frame(&Message::Disconnect(DisconnectReason::Banned)).expect("frame")
            ),
            Offer::Queued
        );
        drop(conn);

        let slots = w.slots.clone();
        eventually(5, "slot released by teardown", || slots.in_use() == 0).await;
        // The reason still reached the peer before the close.
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), w.remote.read_to_end(&mut got)).await;
        let expected =
            codec::encode_frame(&Message::Disconnect(DisconnectReason::Banned)).expect("frame");
        assert_eq!(
            &got[..],
            &expected[..],
            "the disconnect reason was flushed first"
        );
        // The engine asked for the close, so the reader does not report one.
        assert!(
            w.events.try_recv().is_err(),
            "no Disconnected for a close the engine made itself"
        );
    }

    #[tokio::test]
    async fn the_writer_puts_exactly_the_shared_frame_on_the_wire() {
        // The same bytes the per-peer path produced: a big-endian length and
        // then the Cap'n Proto message, in one piece.
        let mut w = wired(short(3_600_000)).await;
        let msg = Message::Consensus {
            bytes: (0..=255u8).cycle().take(300_000).collect(),
        };
        let frame = codec::encode_frame(&msg).expect("frame");
        let conn = w.conn.take().expect("conn");
        assert_eq!(conn.offer(frame.clone()), Offer::Queued);
        assert_eq!(conn.offer(frame), Offer::Queued);
        drop(conn);
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), w.remote.read_to_end(&mut got))
            .await
            .expect("writer finished")
            .expect("read");
        let body = codec::encode(&msg).expect("encode");
        let mut one = (body.len() as u32).to_be_bytes().to_vec();
        one.extend_from_slice(&body);
        let mut want = one.clone();
        want.extend_from_slice(&one);
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_overflows_instead_of_blocking() {
        // The remote end never reads, so the pipe fills and the writer stalls.
        // Offers must keep returning immediately, and turn into `Overflow`
        // once the queue's byte budget is spent.
        let mut w = wired(short(3_600_000)).await;
        let conn = w.conn.take().expect("conn");
        let big = Bytes::from(vec![0u8; 4 * 1024 * 1024]);
        let mut outcomes = Vec::new();
        let started = Instant::now();
        for _ in 0..64 {
            outcomes.push(conn.offer(big.clone()));
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "offering never waits on the peer"
        );
        assert!(outcomes.contains(&Offer::Overflow), "{outcomes:?}");
        let queued_ok = outcomes.iter().filter(|o| **o == Offer::Queued).count();
        assert!(
            queued_ok * big.len() <= WRITER_QUEUE_BYTES + big.len() + 2 * 1024 * 1024,
            "no more than the byte budget was admitted: {queued_ok} frames"
        );
        conn.abort();
        drop(w.remote);
    }

    #[tokio::test]
    async fn an_empty_queue_admits_one_maximum_size_frame() {
        let w = wired(short(3_600_000)).await;
        let conn = w.conn.as_ref().expect("conn");
        let huge = Bytes::from(vec![0u8; MAX_FRAME_BYTES]);
        assert_eq!(conn.offer(huge), Offer::Queued);
    }

    #[tokio::test]
    async fn the_frame_count_is_bounded_as_well_as_the_bytes() {
        // Tiny frames never trip the byte budget; the count cap must. The
        // pipe holds 64 bytes and the remote never reads, so the writer is
        // stuck after a dozen frames and everything else stays queued.
        let w = wired_with_pipe(short(3_600_000), 64).await;
        let conn = w.conn.as_ref().expect("conn");
        let tiny = Bytes::from_static(&[0, 0, 0, 1, 0]);
        let mut admitted = 0usize;
        let mut overflowed = false;
        for _ in 0..(WRITER_QUEUE_FRAMES * 4) {
            match conn.offer(tiny.clone()) {
                Offer::Queued => admitted += 1,
                Offer::Overflow => {
                    overflowed = true;
                    break;
                }
                Offer::Gone => panic!("the writer is alive"),
            }
            tokio::task::yield_now().await;
        }
        assert!(overflowed, "the frame cap was never reached");
        assert!(
            admitted <= WRITER_QUEUE_FRAMES + 64,
            "admitted {admitted} frames"
        );
    }

    #[tokio::test]
    async fn the_engine_queue_is_bounded_and_pushes_back_on_the_reader() {
        // A peer sending faster than the engine drains stops being read,
        // rather than having every frame decoded and parked in memory.
        let (remote, local) = tokio::io::duplex(1024 * 1024);
        let (r, w) = tokio::io::split(local);
        let (events_tx, mut events) = mpsc::channel(4);
        wire_connection(r, w, 9, None, false, events_tx, None, short(3_600_000));
        let _register = events.recv().await;
        let mut remote = remote;
        for n in 0..200u64 {
            send_frame_to(&mut remote, &Message::Ping { nonce: n }).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The queue holds its capacity and no more; the rest is still on the
        // socket, not in memory.
        let mut drained = 0;
        while events.try_recv().is_ok() {
            drained += 1;
        }
        assert!(drained <= 4 + 1, "the queue held {drained} events");
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

            /// Whatever a peer writes and then closes, the reader ends, reports
            /// the close, releases its slot, and forwards only messages that
            /// decoded — never a panic, never a hang.
            #[test]
            fn any_byte_stream_ends_the_reader_cleanly(
                prefix_frames in proptest::collection::vec(0u64..4, 0..3),
                junk in proptest::collection::vec(any::<u8>(), 0..4_096),
                declared in prop_oneof![Just(None), any::<u32>().prop_map(Some)],
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    let (mut remote, local) = tokio::io::duplex(64 * 1024);
                    let (r, w) = tokio::io::split(local);
                    let (events_tx, mut events) = mpsc::channel(64);
                    let slots = InboundSlots::new(1);
                    let slot = slots.try_acquire().expect("slot");
                    wire_connection(r, w, 3, None, false, events_tx, Some(slot), short(2_000));
                    let writer = tokio::spawn(async move {
                        for n in &prefix_frames {
                            let f = codec::encode_frame(&Message::Ping { nonce: *n }).expect("frame");
                            let _ = remote.write_all(&f).await;
                        }
                        if let Some(len) = declared {
                            let _ = remote.write_all(&len.to_be_bytes()).await;
                        }
                        let _ = remote.write_all(&junk).await;
                        // Closing is the peer's last act.
                    });
                    let mut saw_register = false;
                    let mut saw_close = false;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < deadline {
                        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                            Ok(Some(Cmd::Register { conn, .. })) => {
                                saw_register = true;
                                // Keep the engine's end alive, as the engine would
                                // until it hears the close.
                                std::mem::forget(conn);
                            }
                            Ok(Some(Cmd::Event(Event::Disconnected { peer: 3 }))) => {
                                saw_close = true;
                                break;
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => break,
                        }
                    }
                    let _ = writer.await;
                    assert!(saw_register, "registration comes first");
                    assert!(saw_close, "the reader always reports the end of the stream");
                    eventually(5, "slot released", || slots.in_use() == 0).await;
                });
            }
        }
    }

    #[test]
    fn the_handshake_cap_is_far_below_the_frame_cap() {
        // A `Hello` is a few hundred bytes; nothing legitimate is large before
        // one has been decoded.
        const { assert!(MAX_HANDSHAKE_FRAME_BYTES < MAX_FRAME_BYTES / 1_000) };
    }
}
