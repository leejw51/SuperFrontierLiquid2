//! Malformed input. Everything a validator decodes arrives from the
//! network, and a panic in the decoder or anywhere behind it takes the
//! validator down — a crash fault any peer could cause at will, and `f + 1`
//! of those halt the chain.
//!
//! Three layers: arbitrary bytes into every decoder of a wire type;
//! valid encodings with bytes flipped, cut, inserted or overwritten with
//! extreme integers, decoded and, when they still decode, fed to a live
//! engine; and structurally valid messages full of nonsense (rounds near
//! `u64::MAX`, signatures of every length, member-signed headers around
//! garbage certificates) fed to a live engine in bulk.

use super::*;
use proptest::prelude::*;

/// A valid example of every message variant and of a finality proof, from
/// a four-validator chain with a timeout certificate in it.
fn samples(kit: &Kit) -> (Vec<Vec<u8>>, Vec<u8>) {
    let chain = kit.chain(2);
    let qc1 = kit.qc_for(&chain[0]);
    let p3 = kit.proposal(
        3,
        1,
        qc1.clone(),
        Some(kit.tc(2, &[qc1.clone(), kit.genesis_qc()])),
        b"p3 payload",
    );
    let msgs = vec![
        Message::Proposal(chain[0].clone()),
        Message::Proposal(p3.clone()),
        Message::Vote(kit.vote(1, 3, p3.header.id())),
        Message::Timeout(kit.timeout(2, 3, qc1.clone())),
        Message::ProposalRequest {
            id: p3.header.id(),
            ancestors: 64,
        },
        Message::TxBatch {
            txs: vec![vec![1, 2, 3], vec![], vec![9; 40]],
        },
    ];
    let fin = Finality {
        header: chain[0].header.clone(),
        cert: qc1,
    };
    fin.verify(&kit.set, kit.genesis).unwrap();
    (msgs.iter().map(|m| m.encode()).collect(), fin.encode())
}

/// An engine with some history: two proposals in, round 2, one vote cast.
fn live_engine(kit: &Kit) -> Engine {
    let mut e = kit.engine(3);
    let _ = feed(&mut e, &kit.chain(2), 0);
    e
}

/// Every decoder a wire byte string can reach, plus the finality check on
/// whatever decodes.
fn decode_everything(kit: &Kit, bytes: &[u8]) {
    let _ = Message::decode(bytes);
    if let Ok(f) = Finality::decode(bytes) {
        let _ = f.verify(&kit.set, kit.genesis);
        let _ = f.header.id();
    }
    let _ = crate::QuorumCert::decode(bytes);
    let _ = bincode::deserialize::<Vote>(bytes);
    let _ = bincode::deserialize::<Timeout>(bytes);
    let _ = bincode::deserialize::<TimeoutCert>(bytes);
    let _ = bincode::deserialize::<QuorumCert>(bytes);
    let _ = bincode::deserialize::<ProposalHeader>(bytes);
    let _ = bincode::deserialize::<Proposal>(bytes);
}

fn kit4() -> &'static Kit {
    static KIT: std::sync::OnceLock<Kit> = std::sync::OnceLock::new();
    KIT.get_or_init(|| Kit::new(4))
}

#[derive(Clone, Debug)]
enum Mutation {
    Flip(usize, u8),
    Truncate(usize),
    Insert(usize, Vec<u8>),
    /// Overwrite eight bytes with an extreme integer: where a length
    /// prefix or a round sits, this is the value an attacker would pick.
    Extreme(usize, u64),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), 1u8..=255).prop_map(|(p, b)| Mutation::Flip(p, b)),
        any::<usize>().prop_map(Mutation::Truncate),
        (
            any::<usize>(),
            proptest::collection::vec(any::<u8>(), 1..16)
        )
            .prop_map(|(p, b)| Mutation::Insert(p, b)),
        (
            any::<usize>(),
            prop_oneof![
                Just(u64::MAX),
                Just(u64::MAX - 1),
                Just(1u64 << 32),
                Just(1u64 << 62),
                Just(0u64),
                any::<u64>()
            ]
        )
            .prop_map(|(p, v)| Mutation::Extreme(p, v)),
    ]
}

fn mutate(mut b: Vec<u8>, ms: &[Mutation]) -> Vec<u8> {
    for m in ms {
        if b.is_empty() {
            break;
        }
        match m {
            Mutation::Flip(p, x) => {
                let i = p % b.len();
                b[i] ^= x;
            }
            Mutation::Truncate(p) => b.truncate(p % b.len()),
            Mutation::Insert(p, bytes) => {
                let i = p % (b.len() + 1);
                b.splice(i..i, bytes.iter().copied());
            }
            Mutation::Extreme(p, v) => {
                let i = p % b.len();
                for (k, byte) in v.to_le_bytes().iter().enumerate() {
                    if i + k < b.len() {
                        b[i + k] = *byte;
                    }
                }
            }
        }
    }
    b
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Arbitrary bytes, empty to a few kilobytes, through every decoder:
    /// an error or a value, never a panic, and never an allocation sized by
    /// a length prefix the bytes do not back.
    #[test]
    fn arbitrary_bytes_never_panic_a_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        decode_everything(kit4(), &bytes);
    }

    /// Arbitrary bytes behind a valid variant tag, so the decoder gets past
    /// the first byte and into each variant's fields.
    #[test]
    fn arbitrary_bytes_behind_a_valid_tag_never_panic(
        tag in 0u32..5,
        rest in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut bytes = tag.to_le_bytes().to_vec();
        bytes.extend(rest);
        decode_everything(kit4(), &bytes);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 384, ..ProptestConfig::default() })]

    /// Valid encodings, mutated. Whatever still decodes goes to a live
    /// engine, which must refuse it or handle it without panicking and
    /// without committing: nothing a mutation produces carries a quorum's
    /// signatures over anything new.
    #[test]
    fn mutated_valid_encodings_never_panic_the_engine(
        which in 0usize..7,
        ms in proptest::collection::vec(mutation(), 1..6),
    ) {
        let kit = kit4();
        let (msgs, fin) = samples(kit);
        let original = if which < msgs.len() { msgs[which].clone() } else { fin };
        let bytes = mutate(original, &ms);
        decode_everything(kit, &bytes);
        if let Ok(m) = Message::decode(&bytes) {
            let mut e = live_engine(kit);
            let committed = e.committed_round();
            let _ = e.on_message(m.clone(), None, 0);
            let _ = e.on_message(m, Some("vetoed"), 1);
            let _ = e.on_tick(1_000_000);
            prop_assert_eq!(e.committed_round(), committed);
        }
    }
}

/// A small deterministic generator, so a failing structured case is
/// reproducible from the proptest seed that drew it.
struct Gen(u64);
impl Gen {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn round(&mut self) -> Round {
        match self.below(6) {
            0 => self.below(6),
            1 => u64::MAX - self.below(3),
            2 => 1 << self.below(64),
            3 => 255 + self.below(4),
            _ => self.next(),
        }
    }
    fn bytes(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Nonsense wrapped in structurally valid messages. Headers, votes and
/// timeouts are often signed by a real member — validator 1, the one
/// Byzantine member four validators tolerate — so the nonsense gets past
/// the signature check and into the certificate, justification, payload,
/// height and tally logic behind it. Only one key signs, so no amount of
/// nonsense adds up to a genuine quorum.
fn nonsense(g: &mut Gen, kit: &Kit, ids: &[H256]) -> Message {
    let id = |g: &mut Gen| -> H256 {
        if g.below(2) == 0 {
            ids[g.below(ids.len() as u64) as usize]
        } else {
            H256([g.next() as u8; 32])
        }
    };
    const BYZ: usize = 1;
    let member = |g: &mut Gen| g.below(kit.n() as u64) as usize;
    let sig = |g: &mut Gen, digest_ok: Option<Vec<u8>>| -> Vec<u8> {
        match (g.below(4), digest_ok) {
            (0 | 1, Some(s)) => s,
            (2, _) => {
                let mut s = vec![0u8; 65];
                for b in &mut s {
                    *b = g.next() as u8;
                }
                s
            }
            _ => g.bytes(80),
        }
    };
    let qc = |g: &mut Gen| -> QuorumCert {
        let round = g.round();
        let qid = id(g);
        let votes = (0..g.below(6))
            .map(|_| {
                let v = kit.vote(BYZ, if g.below(3) == 0 { g.round() } else { round }, qid);
                let s = sig(g, Some(v.sig.clone()));
                Vote { sig: s, ..v }
            })
            .collect();
        QuorumCert {
            round,
            id: qid,
            votes,
        }
    };
    match g.below(4) {
        0 => {
            let q = qc(g);
            let tc = if g.below(2) == 0 {
                let round = g.round();
                Some(TimeoutCert {
                    round,
                    timeouts: (0..g.below(5))
                        .map(|_| {
                            let hq = qc(g);
                            let t = kit.timeout(BYZ, round, hq);
                            let s = sig(g, Some(t.sig.clone()));
                            Timeout { sig: s, ..t }
                        })
                        .collect(),
                })
            } else {
                None
            };
            let payload = g.bytes(64);
            let mut h = ProposalHeader {
                round: g.round(),
                height: g.round(),
                parent_id: if g.below(3) == 0 { id(g) } else { q.id },
                payload_hash: if g.below(2) == 0 {
                    keccak256(&payload)
                } else {
                    id(g)
                },
                payload_len: g.next() as u32 % 4,
                timestamp: g.round(),
                exec_height: g.round(),
                exec_root: id(g),
                qc: q,
                tc,
                proposer: kit.addr(member(g)),
                sig: Vec::new(),
            };
            if g.below(4) != 0 {
                h.proposer = kit.addr(BYZ);
                h.sign_with(&kit.keys[BYZ]);
            } else {
                h.sig = g.bytes(70);
            }
            Message::Proposal(Proposal { header: h, payload })
        }
        1 => {
            let v = kit.vote(BYZ, g.round(), id(g));
            let s = sig(g, Some(v.sig.clone()));
            Message::Vote(Vote { sig: s, ..v })
        }
        2 => {
            let hq = qc(g);
            let t = kit.timeout(BYZ, g.round(), hq);
            let s = sig(g, Some(t.sig.clone()));
            Message::Timeout(Timeout { sig: s, ..t })
        }
        _ => Message::ProposalRequest {
            id: id(g),
            ancestors: g.next() as u32,
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Sequences of structured nonsense into one engine with history. No
    /// panic, no commit, and the engine's round moves only if some message
    /// carried a genuine quorum — which nonsense never does, so the round
    /// may move only through the engine's own timer.
    #[test]
    fn structured_nonsense_never_panics_or_commits(seed in any::<u64>(), len in 1usize..24) {
        let kit = kit4();
        let chain = kit.chain(2);
        let ids: Vec<H256> = chain
            .iter()
            .map(|p| p.header.id())
            .chain([kit.genesis])
            .collect();
        let mut g = Gen(seed | 1);
        let mut e = live_engine(kit);
        let before = snapshot(&e);
        for k in 0..len {
            let m = nonsense(&mut g, kit, &ids);
            // Round-trip through the wire format as a real peer would.
            let m = Message::decode(&m.encode()).unwrap();
            let veto = if k % 5 == 0 { Some("vetoed") } else { None };
            let _ = e.on_message(m, veto, k as u64);
        }
        let _ = e.on_tick(u64::MAX / 2);
        let after = snapshot(&e);
        prop_assert_eq!(after.2, before.2, "committed on nonsense");
        prop_assert_eq!(after.4, before.4);
        prop_assert!(after.1 == before.1, "high certificate moved on nonsense");
    }
}
