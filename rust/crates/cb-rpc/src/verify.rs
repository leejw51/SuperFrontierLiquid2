//! Signature recovery spread across every core, with admission kept in
//! arrival order.
//!
//! # Why
//!
//! `submitTx` used to recover the sender on the thread of the connection the
//! transaction arrived on. Recovery is ~63 µs of secp256k1 per transaction,
//! so one connection was one core's worth of admission — about 14,000
//! transactions a second — and the gateway, which holds exactly one
//! connection, could never offer the node more than that however fast the
//! node could apply. Measured on this class of machine: 15,981 recoveries a
//! second on one core, 88,983 across ten with rayon.
//!
//! # How
//!
//! Every connection thread pushes its submissions, in the order they arrived,
//! into one bounded queue and returns a promise. One coordinator thread takes
//! the queue in batches: it recovers the batch's senders in parallel on a
//! rayon pool, then walks the batch **in order** doing what admission did
//! before — chain id, the nonce ledger, the mempool — and answers each promise.
//!
//! Order is the whole point. The nonce ledger expects a sender's
//! transactions in sequence; a client pipelining nonces 7, 8, 9 must see them
//! admitted 7, 8, 9. Each connection pushes sequentially and the queue is
//! FIFO, so arrival order is preserved per sender, and the coordinator's
//! sequential walk admits in that order. Recovery is the only part that runs
//! concurrently, and recovery has no state.
//!
//! When the queue is full the submission is refused with `mempoolFull`
//! rather than making the connection thread wait: a wait there stalls every
//! request on that Cap'n Proto stream. `cb-sim` and the gateway both treat a
//! refusal as "re-read this sender's nonce," so a burst that overruns the
//! queue costs a resync, not a stuck client.

use crate::admission::PendingNonces;
use cb_schema::rpc_capnp::SubmitErrorCode;
use cb_state::{Database, Mempool, Meta};
use cb_tx::SignedTx;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify};

/// What admission decided, carried back to the connection thread.
#[derive(Debug, Clone)]
pub enum Admission {
    Ok,
    Rejected {
        code: SubmitErrorCode,
        message: String,
    },
}

struct Job {
    signed: SignedTx,
    /// `None` for a transaction another validator forwarded: nobody is
    /// waiting on the answer.
    reply: Option<oneshot::Sender<Admission>>,
    /// Whether to tell the observer (gossip, or a validator's forwarder)
    /// about it once admitted. Off for forwarded transactions, or every
    /// validator would forward every transaction for ever.
    forward: bool,
}

/// Submissions waiting for a verifier, across all connections.
pub const QUEUE_CAPACITY: usize = 65_536;
/// Most transactions one batch recovers before admitting them. Keeps the
/// admission latency of a saturated queue around one block interval.
pub const MAX_BATCH: usize = 8_192;

/// The verification pipeline: one queue, one coordinator, a rayon pool.
pub struct Verifier {
    tx: SyncSender<Job>,
}

/// Everything the coordinator needs to admit a transaction. The same values
/// `SequencerServer` holds; admission moved here so it can run in one place
/// for every connection.
#[derive(Clone)]
pub struct AdmissionCtx {
    pub db: Database,
    pub notify: Arc<Notify>,
    pub pending_nonces: Option<Arc<PendingNonces>>,
    pub tx_observer: Option<crate::server::TxObserver>,
}

impl Verifier {
    /// Start the pipeline with `threads` recovery threads (0 = every core).
    pub fn spawn(ctx: AdmissionCtx, threads: usize) -> anyhow::Result<Arc<Verifier>> {
        let threads = if threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            threads
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("cb-verify-{i}"))
            .build()?;
        let (tx, rx) = sync_channel::<Job>(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("cb-verify-coord".to_string())
            .spawn(move || coordinator(rx, pool, ctx))?;
        tracing::info!(
            threads,
            queue = QUEUE_CAPACITY,
            "signature verification pool running"
        );
        Ok(Arc::new(Verifier { tx }))
    }

    /// Queue a transaction. Returns the receiver the connection thread awaits,
    /// or an immediate refusal if the queue is full.
    pub fn submit(&self, signed: SignedTx) -> Result<oneshot::Receiver<Admission>, Admission> {
        let (reply, rx) = oneshot::channel();
        match self.tx.try_send(Job {
            signed,
            reply: Some(reply),
            forward: true,
        }) {
            Ok(()) => Ok(rx),
            Err(TrySendError::Full(_)) => Err(Admission::Rejected {
                code: SubmitErrorCode::MempoolFull,
                message: format!(
                    "verification queue is full ({QUEUE_CAPACITY} transactions waiting); retry"
                ),
            }),
            Err(TrySendError::Disconnected(_)) => Err(Admission::Rejected {
                code: SubmitErrorCode::Internal,
                message: "the verifier has stopped".to_string(),
            }),
        }
    }
}

impl Verifier {
    /// Queue a transaction another validator forwarded. Verified and
    /// admitted exactly as a client's submission, but not forwarded again
    /// and not answered. Dropped silently when the queue is full: the
    /// forwarder still has it, and its own proposal will carry it.
    pub fn submit_forwarded(&self, signed: SignedTx) {
        let _ = self.tx.try_send(Job {
            signed,
            reply: None,
            forward: false,
        });
    }
}

fn coordinator(rx: Receiver<Job>, pool: rayon::ThreadPool, ctx: AdmissionCtx) {
    use rayon::prelude::*;
    // Genesis sets the chain id and nothing changes it: read once.
    let chain_id = Meta::new(&ctx.db).chain_id();
    let mut batch: Vec<Job> = Vec::with_capacity(MAX_BATCH);
    loop {
        // Block for the first, then take whatever else has arrived, in order.
        let first = match rx.recv() {
            Ok(j) => j,
            Err(_) => {
                tracing::error!("verifier queue closed; no more submissions will be admitted");
                return;
            }
        };
        batch.clear();
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(j) => batch.push(j),
                Err(_) => break,
            }
        }
        // A transaction this node already holds, or committed moments ago,
        // is answered as admitted without a look at its signature: a
        // forwarded copy that arrived after the proposal it was in, or a
        // client resubmitting, costs one hash. Under load most of what a
        // validator receives by forwarding is exactly that.
        let known: Vec<bool> = {
            let mempool = Mempool::new(&ctx.db);
            batch
                .iter()
                .map(|j| mempool.is_known(&j.signed.hash()))
                .collect()
        };
        // Recovery in parallel, results in batch order. Recovery is ~16 µs;
        // a batch of a few hundred split eighteen ways is more rayon than
        // crypto. Chunks of 32 keep each job a fraction of a millisecond and
        // a small batch on one or two threads.
        let senders: Vec<Option<Result<cb_types::Address, ()>>> = pool.install(|| {
            batch
                .par_iter()
                .zip(known.par_iter())
                .with_min_len(32)
                .map(|(j, known)| (!known).then(|| j.signed.sender().map_err(|_| ())))
                .collect()
        });
        // Admission in order, one notification for the batch.
        let mut admitted_any = false;
        for (job, sender) in batch.drain(..).zip(senders) {
            let outcome = match sender {
                None => Admission::Ok,
                Some(Err(())) => Admission::Rejected {
                    code: SubmitErrorCode::BadSignature,
                    message: "signature recovery failed".to_string(),
                },
                Some(Ok(sender)) => admit_with(&ctx, chain_id, job.signed, sender, job.forward),
            };
            if matches!(outcome, Admission::Ok) {
                admitted_any = true;
            }
            // A dropped receiver means the connection went away; nothing to do.
            if let Some(reply) = job.reply {
                let _ = reply.send(outcome);
            }
        }
        if admitted_any {
            ctx.notify.notify_waiters();
        }
    }
}

/// The checks that used to live inline in `submitTx`, minus recovery and
/// minus the notification, which the caller batches. Used by the coordinator
/// and by the inline path a server without a verifier still takes.
pub fn admit(
    ctx: &AdmissionCtx,
    chain_id: u32,
    signed: SignedTx,
    sender: cb_types::Address,
) -> Admission {
    admit_with(ctx, chain_id, signed, sender, true)
}

/// [`admit`], with a say in whether the observer hears about it.
pub fn admit_with(
    ctx: &AdmissionCtx,
    chain_id: u32,
    signed: SignedTx,
    sender: cb_types::Address,
    forward: bool,
) -> Admission {
    // A transaction is signed for one specific chain, so one minted for
    // another is not a nonce problem or a balance problem: it is the wrong
    // transaction.
    if signed.tx.chain_id != chain_id {
        return Admission::Rejected {
            code: SubmitErrorCode::NotAuthorized,
            message: format!(
                "transaction is signed for chain {}, this chain is {chain_id}",
                signed.tx.chain_id
            ),
        };
    }
    // `expected` is the chain's nonce advanced past whatever this sender
    // already has in the mempool, so pipelining within one block interval
    // still works; what is refused is a nonce the chain cannot reach.
    if let Some(pending) = &ctx.pending_nonces {
        let expected = pending.expected(&ctx.db, &sender);
        if signed.tx.nonce != expected {
            // The first few, so an operator can tell a client replaying an
            // old nonce (got < expected) from one whose transaction went
            // missing (got > expected).
            static SHOWN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
                tracing::warn!(
                    sender = %hex::encode(sender.0),
                    expected,
                    got = signed.tx.nonce,
                    chain = cb_state::Accounts::new(&ctx.db).get(&sender).map(|a| a.nonce).unwrap_or(0),
                    "admission refused a nonce"
                );
            }
            return Admission::Rejected {
                code: SubmitErrorCode::BadNonce,
                message: format!("bad nonce: expected {expected}, got {}", signed.tx.nonce),
            };
        }
    }
    let (hash, mut bytes, signed_len) = match encode_admitted(&signed, &sender) {
        Ok(v) => v,
        Err(e) => {
            return Admission::Rejected {
                code: SubmitErrorCode::Internal,
                message: format!("encode mempool record: {e}"),
            }
        }
    };
    let nonce = signed.tx.nonce;
    // The recovered sender rides along so the block builder never re-runs
    // ECDSA on a drained transaction. `try_put` copies what it keeps, and
    // refuses when the mempool or this sender's share of it is full — before
    // the nonce ledger advances, so a refused nonce is the one expected next.
    if let Err(full) = Mempool::new(&ctx.db).try_put(hash, sender, nonce, &bytes) {
        return Admission::Rejected {
            code: SubmitErrorCode::MempoolFull,
            message: full.to_string(),
        };
    }
    if let Some(pending) = &ctx.pending_nonces {
        pending.admitted(sender, nonce);
    }
    if forward {
        if let Some(observe) = &ctx.tx_observer {
            // The canonical `SignedTx` bytes for gossip and forwarding are
            // the record's prefix: cut the sender off rather than encode
            // the transaction again.
            bytes.truncate(signed_len);
            observe(hash, bytes);
        }
    }
    Admission::Ok
}

/// One encoding of an admitted transaction, from which everything admission
/// stores or announces is cut: its hash, its mempool record and its gossip
/// bytes.
///
/// This used to be three encodings of the same transaction on the one
/// coordinator thread every submission goes through: `SignedTx::hash`
/// (encode, then keccak), the gossip bytes (encode again), and the
/// `MempoolRecord` (encode a third time, wrapped with the sender). They are
/// one encoding because `bincode` writes a struct's fields in order with no
/// framing, and an [`Address`] is a fixed `[u8; 20]` with no length prefix,
/// so `bincode(MempoolRecord { signed, sender })` is exactly
/// `bincode(signed) ‖ sender`. The hash is keccak over the first part, which
/// is what `SignedTx::hash` computes. `one_encoding_gives_the_same_hash_record_and_gossip_bytes`
/// pins all three equalities against the library's own encoders, so a change
/// to either type's layout fails the test rather than the mempool.
///
/// Returns `(hash, bincode(signed) ‖ sender, bincode(signed).len())`.
fn encode_admitted(
    signed: &SignedTx,
    sender: &cb_types::Address,
) -> Result<(cb_types::H256, Vec<u8>, usize), bincode::Error> {
    let mut bytes = bincode::serialize(signed)?;
    let signed_len = bytes.len();
    let hash = cb_crypto::keccak256(&bytes);
    bytes.extend_from_slice(&sender.0);
    Ok((hash, bytes, signed_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_crypto::{address_from_pubkey, generate_keypair};
    use cb_tx::{MempoolRecord, Transaction, TxBody};
    use cb_types::{Address, AssetId};

    fn signed_transfer(nonce: u64) -> (SignedTx, Address) {
        let (sk, pk) = generate_keypair();
        let signed = SignedTx::sign_with(
            &sk,
            Transaction {
                nonce,
                chain_id: 1,
                body: TxBody::Transfer {
                    to: Address([9; 20]),
                    asset: AssetId::Usd,
                    amount: 1_234_567,
                },
            },
        )
        .unwrap();
        (signed, address_from_pubkey(&pk))
    }

    /// The layout `encode_admitted` relies on: one encoding of the
    /// transaction yields the same hash, mempool record and gossip bytes
    /// as the three separate encodings it replaced.
    #[test]
    fn one_encoding_gives_the_same_hash_record_and_gossip_bytes() {
        for nonce in [0u64, 1, 7, u64::MAX] {
            let (signed, sender) = signed_transfer(nonce);
            let (hash, bytes, signed_len) = encode_admitted(&signed, &sender).unwrap();

            assert_eq!(hash, signed.hash(), "hash is SignedTx::hash");
            let record = MempoolRecord {
                signed: signed.clone(),
                sender,
            };
            assert_eq!(
                bytes,
                bincode::serialize(&record).unwrap(),
                "record bytes are bincode(MempoolRecord)"
            );
            assert_eq!(
                &bytes[..signed_len],
                bincode::serialize(&signed).unwrap().as_slice(),
                "gossip bytes are bincode(SignedTx)"
            );
            assert_eq!(&bytes[signed_len..], &sender.0, "the sender is the suffix");
            // And the record decodes back to what was admitted, sender included.
            let back: MempoolRecord = bincode::deserialize(&bytes).unwrap();
            assert_eq!(back.signed, signed);
            assert_eq!(back.sender, sender);
        }
    }

    /// Run `body` on its own thread and fail — rather than hang the whole
    /// test binary — if it has not finished within `limit`. A lock taken
    /// twice on one thread blocks for ever without this.
    fn within(limit: std::time::Duration, body: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            body();
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(limit) {
            Ok(()) => worker.join().expect("test body"),
            // The body panicked: surface its panic, not a timeout.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(p) = worker.join() {
                    std::panic::resume_unwind(p);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("test body did not finish within {limit:?}: a deadlock")
            }
        }
    }

    /// What the mempool holds and what the observer is told, end to end
    /// through `admit_with`, match the encoders they replaced.
    ///
    /// # Why the observer's log is read by copying it out
    ///
    /// An earlier version held `announced.lock()` in a local while it went
    /// on to admit the forwarded transaction and then locked `announced`
    /// again to count it. `std::sync::Mutex` is not re-entrant, so the
    /// second lock on the same thread waited on the first for ever and the
    /// test binary hung instead of failing. The log is now copied out under
    /// a guard that dies at the end of its statement, no guard is alive
    /// across any other call, and the body runs under [`within`] so any
    /// future deadlock is a failure with a message rather than a hang.
    #[test]
    fn admission_stores_the_record_and_announces_the_signed_bytes() {
        within(std::time::Duration::from_secs(30), || {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path()).unwrap();
            Meta::new(&db).set_chain_id(1);
            type Log = Arc<std::sync::Mutex<Vec<(cb_types::H256, Vec<u8>)>>>;
            let announced: Log = Arc::new(std::sync::Mutex::new(Vec::new()));
            // Poison-tolerant: a failed assertion elsewhere must not turn
            // every later read into a second panic.
            let snapshot = |log: &Log| log.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let sink = announced.clone();
            let ctx = AdmissionCtx {
                db: db.clone(),
                notify: Arc::new(Notify::new()),
                pending_nonces: None,
                tx_observer: Some(Arc::new(move |h, b| {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push((h, b))
                })),
            };
            let (signed, sender) = signed_transfer(3);
            assert!(matches!(
                admit_with(&ctx, 1, signed.clone(), sender, true),
                Admission::Ok
            ));
            let hash = signed.hash();
            let record = MempoolRecord {
                signed: signed.clone(),
                sender,
            };
            assert_eq!(
                Mempool::new(&db).get(&hash),
                Some(bincode::serialize(&record).unwrap())
            );
            let got = snapshot(&announced);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, hash);
            assert_eq!(got[0].1, bincode::serialize(&signed).unwrap());

            // A forwarded transaction is stored the same way and never
            // announced.
            let (fwd, fwd_sender) = signed_transfer(4);
            assert!(matches!(
                admit_with(&ctx, 1, fwd.clone(), fwd_sender, false),
                Admission::Ok
            ));
            let fwd_record = MempoolRecord {
                signed: fwd.clone(),
                sender: fwd_sender,
            };
            assert_eq!(
                Mempool::new(&db).get(&fwd.hash()),
                Some(bincode::serialize(&fwd_record).unwrap())
            );
            assert_eq!(snapshot(&announced).len(), 1);
        });
    }

    /// The watchdog itself: a body that takes the same lock twice fails
    /// within the limit instead of hanging.
    #[test]
    fn a_self_deadlock_fails_instead_of_hanging() {
        let r = std::panic::catch_unwind(|| {
            within(std::time::Duration::from_millis(200), || {
                let m = std::sync::Mutex::new(0u8);
                let _first = m.lock().unwrap();
                let _second = m.lock(); // blocks for ever
            })
        });
        let msg = r.expect_err("a deadlock must fail");
        let msg = msg.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("deadlock"), "{msg}");
    }
}
