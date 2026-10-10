//! Cap'n Proto encoding for the gossip protocol.
//!
//! The codec is deliberately separate from [`crate::p2p::engine`]: the state
//! machine is tested against [`crate::p2p::message::Message`] values, and this
//! module is tested by round-tripping bytes. Neither test can hide a bug in
//! the other.
//!
//! Cap'n Proto rather than bincode, even though bincode is one derive away:
//! the rest of this system's wire formats are Cap'n Proto, the schema is a
//! reviewable artifact that a non-Rust implementation can read, and additive
//! field evolution is a property of the format rather than a convention. The
//! cost is this file — a few hundred lines of mechanical conversion that
//! bincode would not have needed.
//!
//! Framing is a 4-byte big-endian length prefix in front of each Cap'n Proto
//! message, applied by [`crate::p2p::transport`]. The prefix is checked
//! against [`MAX_FRAME_BYTES`] before a single byte is buffered, so an
//! adversary cannot make us allocate a gigabyte by lying about a length.

use crate::p2p::identity::{GenesisId, NodeId};
use crate::p2p::message::{DisconnectReason, Hello, Message, PeerAddr};
use cb_schema::p2p_capnp;
use cb_types::{Address, H256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Largest frame accepted or produced, in bytes.
///
/// Sized to hold a full catch-up range of large blocks with headroom. A frame
/// larger than this is treated as a protocol violation rather than grown into:
/// the alternative is letting a peer choose our memory usage.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Bytes of length prefix in front of every frame on the wire.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// Deepest pointer nesting a legitimate message has.
///
/// The deepest path in the schema is envelope -> `peers` list -> element ->
/// `endpoint`, four levels. Sixteen leaves room for additive evolution and is
/// still a quarter of the Cap'n Proto default, which exists for generic
/// readers that do not know their schema.
pub const NESTING_LIMIT: i32 = 16;

/// The smallest a list element of ours can legitimately be on the wire.
///
/// Both struct lists in the schema (`peers` and `blockRange`) have elements of
/// at least two words. An element count is a claim; the frame length is a
/// fact, so a list can never hold more than `frame bytes / this` elements
/// that are real, and output buffers are sized by that bound and never by the
/// claim.
const MIN_LIST_ELEMENT_BYTES: usize = 16;

/// Reader limits for one frame of `frame_len` bytes.
///
/// The traversal limit is what stops a list of zero-sized structs: such a
/// list occupies no words, so Cap'n Proto charges its element count against
/// the traversal budget instead. The default budget is 8 Mi words whatever
/// the frame size, which let a 112-byte frame declare eight million elements
/// and pass. Sizing the budget to the frame itself — each word of a
/// legitimate message is traversed once; twice is headroom — makes a claim
/// larger than the bytes that carry it fail at the pointer, before any code
/// of ours sees a length.
pub fn reader_options(frame_len: usize) -> capnp::message::ReaderOptions {
    let words = (frame_len / 8).saturating_mul(2).saturating_add(64);
    let cap = MAX_FRAME_BYTES / 8 * 2;
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words.min(cap)))
        .nesting_limit(NESTING_LIMIT);
    opts
}

/// How many elements to reserve for a decoded list: the claimed count, but
/// never more than the frame could actually carry.
fn bounded_capacity(claimed: u32, frame_len: usize) -> usize {
    (claimed as usize).min(frame_len / MIN_LIST_ELEMENT_BYTES)
}

/// A message could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The underlying Cap'n Proto layer rejected the message.
    #[error("capnp: {0}")]
    Capnp(String),
    /// A field held the wrong number of bytes for its type.
    #[error("field {field} has length {len}, expected {expected}")]
    BadLength {
        /// Which field.
        field: &'static str,
        /// What we got.
        len: usize,
        /// What we needed.
        expected: usize,
    },
    /// A frame's declared length exceeds [`MAX_FRAME_BYTES`].
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit")]
    FrameTooLarge(usize),
    /// An IP address field was neither 4 nor 16 bytes.
    #[error("malformed ip address of {0} bytes")]
    BadIpAddress(usize),
}

impl From<capnp::Error> for CodecError {
    fn from(e: capnp::Error) -> Self {
        CodecError::Capnp(e.to_string())
    }
}

impl From<capnp::NotInSchema> for CodecError {
    fn from(e: capnp::NotInSchema) -> Self {
        CodecError::Capnp(format!("unknown union variant {}", e.0))
    }
}

fn fixed<const N: usize>(field: &'static str, data: &[u8]) -> Result<[u8; N], CodecError> {
    if data.len() != N {
        return Err(CodecError::BadLength {
            field,
            len: data.len(),
            expected: N,
        });
    }
    let mut out = [0u8; N];
    out.copy_from_slice(data);
    Ok(out)
}

fn write_endpoint(mut b: p2p_capnp::endpoint::Builder<'_>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(v4) => b.set_ip(&v4.octets()),
        IpAddr::V6(v6) => b.set_ip(&v6.octets()),
    }
    b.set_port(addr.port());
}

fn read_endpoint(r: p2p_capnp::endpoint::Reader<'_>) -> Result<SocketAddr, CodecError> {
    let ip = r.get_ip()?;
    let addr = match ip.len() {
        4 => IpAddr::V4(Ipv4Addr::from(fixed::<4>("ip", ip)?)),
        16 => IpAddr::V6(Ipv6Addr::from(fixed::<16>("ip", ip)?)),
        n => return Err(CodecError::BadIpAddress(n)),
    };
    Ok(SocketAddr::new(addr, r.get_port()))
}

fn reason_to_code(r: DisconnectReason) -> p2p_capnp::DisconnectCode {
    use p2p_capnp::DisconnectCode as C;
    match r {
        DisconnectReason::ChainIdMismatch => C::ChainIdMismatch,
        DisconnectReason::GenesisMismatch => C::GenesisMismatch,
        DisconnectReason::VersionMismatch => C::VersionMismatch,
        DisconnectReason::SelfConnection => C::SelfConnection,
        DisconnectReason::DuplicateConnection => C::DuplicateConnection,
        DisconnectReason::BadIdentity => C::BadIdentity,
        DisconnectReason::Banned => C::Banned,
        DisconnectReason::Misbehaving => C::Misbehaving,
        DisconnectReason::TooManyPeers => C::TooManyPeers,
        DisconnectReason::ProtocolViolation => C::ProtocolViolation,
        DisconnectReason::Timeout => C::Timeout,
        DisconnectReason::Shutdown => C::Shutdown,
    }
}

fn code_to_reason(c: p2p_capnp::DisconnectCode) -> DisconnectReason {
    use p2p_capnp::DisconnectCode as C;
    match c {
        C::ChainIdMismatch => DisconnectReason::ChainIdMismatch,
        C::GenesisMismatch => DisconnectReason::GenesisMismatch,
        C::VersionMismatch => DisconnectReason::VersionMismatch,
        C::SelfConnection => DisconnectReason::SelfConnection,
        C::DuplicateConnection => DisconnectReason::DuplicateConnection,
        C::BadIdentity => DisconnectReason::BadIdentity,
        C::Banned => DisconnectReason::Banned,
        C::Misbehaving => DisconnectReason::Misbehaving,
        C::TooManyPeers => DisconnectReason::TooManyPeers,
        C::ProtocolViolation => DisconnectReason::ProtocolViolation,
        C::Timeout => DisconnectReason::Timeout,
        C::Shutdown => DisconnectReason::Shutdown,
    }
}

/// Serialize a message into a Cap'n Proto frame (without the length prefix).
pub fn encode(msg: &Message) -> Result<Vec<u8>, CodecError> {
    let mut builder = capnp::message::Builder::new_default();
    {
        let root = builder.init_root::<p2p_capnp::envelope::Builder>();
        write_envelope(root, msg);
    }
    let bytes = capnp::serialize::write_message_to_words(&builder);
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Serialize a message into exactly the bytes that go on the wire: the 4-byte
/// big-endian length prefix followed by the Cap'n Proto message.
///
/// This is what the transport sends, and it exists so a frame is built once
/// however many peers receive it. The result is a [`bytes::Bytes`], so handing
/// it to ten writers is ten reference-count increments rather than ten copies
/// of a two-megabyte proposal. The payload is copied twice in total — into
/// the Cap'n Proto builder, and from the builder into this buffer with the
/// prefix already in front — where the per-peer path used to copy it three
/// times per peer.
///
/// The output is byte-for-byte `len.to_be_bytes() ++ encode(msg)`; the tests
/// pin that, because a peer running the previous build must not be able to
/// tell the difference.
pub fn encode_frame(msg: &Message) -> Result<bytes::Bytes, CodecError> {
    let mut builder = capnp::message::Builder::new_default();
    {
        let root = builder.init_root::<p2p_capnp::envelope::Builder>();
        write_envelope(root, msg);
    }
    let body_len = capnp::serialize::compute_serialized_size_in_words(&builder).saturating_mul(8);
    if body_len > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(body_len));
    }
    let prefix = u32::try_from(body_len).map_err(|_| CodecError::FrameTooLarge(body_len))?;
    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + body_len);
    out.extend_from_slice(&prefix.to_be_bytes());
    capnp::serialize::write_message(&mut out, &builder)?;
    debug_assert_eq!(out.len(), LENGTH_PREFIX_BYTES + body_len);
    Ok(bytes::Bytes::from(out))
}

fn write_envelope(root: p2p_capnp::envelope::Builder<'_>, msg: &Message) {
    match msg {
        Message::Hello(h) => {
            let mut b = root.init_hello();
            b.set_protocol(h.protocol);
            b.set_chain_id(h.chain_id);
            b.set_genesis(&h.genesis.0 .0);
            b.set_node_id(&h.node_id.0 .0);
            b.set_software(h.software.as_str());
            b.set_head_height(h.head_height);
            b.set_has_listen(h.listen.is_some());
            b.set_nonce(h.nonce);
            b.set_sig(&h.sig);
            let listen = h
                .listen
                .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
            write_endpoint(b.init_listen(), &listen);
        }
        Message::Disconnect(reason) => {
            let mut root = root;
            root.set_disconnect(reason_to_code(*reason));
        }
        Message::GetPeers => {
            let mut root = root;
            root.set_get_peers(());
        }
        Message::Peers(list) => {
            let mut b = root.init_peers(u32::try_from(list.len()).unwrap_or(u32::MAX));
            for (i, p) in list.iter().enumerate() {
                let Ok(i) = u32::try_from(i) else { break };
                let mut e = b.reborrow().get(i);
                e.set_node_id(&p.node_id.0 .0);
                write_endpoint(e.init_endpoint(), &p.addr);
            }
        }
        Message::BlockAnnounce { hash, height } => {
            let mut b = root.init_block_announce();
            b.set_hash(&hash.0);
            b.set_height(*height);
        }
        Message::GetBlock { hash } => {
            let mut root = root;
            root.set_get_block(&hash.0);
        }
        Message::BlockBody { height, bytes } => {
            let mut b = root.init_block_body();
            b.set_height(*height);
            b.set_bytes(bytes);
        }
        Message::TxAnnounce { hash } => {
            let mut root = root;
            root.set_tx_announce(&hash.0);
        }
        Message::GetTx { hash } => {
            let mut root = root;
            root.set_get_tx(&hash.0);
        }
        Message::TxBody { bytes } => {
            let mut root = root;
            root.set_tx_body(bytes);
        }
        Message::Consensus { bytes } => {
            let mut root = root;
            root.set_consensus(bytes);
        }
        Message::GetBlockRange { from, to } => {
            let mut b = root.init_get_block_range();
            b.set_from(*from);
            b.set_to(*to);
        }
        Message::BlockRange { blocks } => {
            let mut b = root.init_block_range(u32::try_from(blocks.len()).unwrap_or(u32::MAX));
            for (i, (height, bytes)) in blocks.iter().enumerate() {
                let Ok(i) = u32::try_from(i) else { break };
                let mut e = b.reborrow().get(i);
                e.set_height(*height);
                e.set_bytes(bytes);
            }
        }
        Message::Ping { nonce } => {
            let mut root = root;
            root.set_ping(*nonce);
        }
        Message::Pong { nonce } => {
            let mut root = root;
            root.set_pong(*nonce);
        }
    }
}

/// Parse a Cap'n Proto frame (without the length prefix) into a message.
pub fn decode(frame: &[u8]) -> Result<Message, CodecError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(frame.len()));
    }
    let mut slice = frame;
    let reader =
        capnp::serialize::read_message_from_flat_slice(&mut slice, reader_options(frame.len()))?;
    let root = reader.get_root::<p2p_capnp::envelope::Reader>()?;
    read_envelope(root, frame.len())
}

fn read_envelope(
    root: p2p_capnp::envelope::Reader<'_>,
    frame_len: usize,
) -> Result<Message, CodecError> {
    use p2p_capnp::envelope::Which;
    Ok(match root.which()? {
        Which::Hello(h) => {
            let h = h?;
            let listen = if h.get_has_listen() {
                Some(read_endpoint(h.get_listen()?)?)
            } else {
                None
            };
            Message::Hello(Hello {
                protocol: h.get_protocol(),
                chain_id: h.get_chain_id(),
                genesis: GenesisId(H256(fixed::<32>("genesis", h.get_genesis()?)?)),
                node_id: NodeId(Address(fixed::<20>("nodeId", h.get_node_id()?)?)),
                software: h
                    .get_software()?
                    .to_str()
                    .map_err(|e| CodecError::Capnp(e.to_string()))?
                    .to_string(),
                head_height: h.get_head_height(),
                listen,
                nonce: h.get_nonce(),
                sig: h.get_sig()?.to_vec(),
            })
        }
        Which::Disconnect(code) => Message::Disconnect(code_to_reason(code?)),
        Which::GetPeers(()) => Message::GetPeers,
        Which::Peers(list) => {
            let list = list?;
            let mut out = Vec::with_capacity(bounded_capacity(list.len(), frame_len));
            for e in list.iter() {
                out.push(PeerAddr {
                    node_id: NodeId(Address(fixed::<20>("nodeId", e.get_node_id()?)?)),
                    addr: read_endpoint(e.get_endpoint()?)?,
                });
            }
            Message::Peers(out)
        }
        Which::BlockAnnounce(b) => {
            let b = b?;
            Message::BlockAnnounce {
                hash: H256(fixed::<32>("hash", b.get_hash()?)?),
                height: b.get_height(),
            }
        }
        Which::GetBlock(h) => Message::GetBlock {
            hash: H256(fixed::<32>("hash", h?)?),
        },
        Which::BlockBody(b) => {
            let b = b?;
            Message::BlockBody {
                height: b.get_height(),
                bytes: b.get_bytes()?.to_vec(),
            }
        }
        Which::TxAnnounce(h) => Message::TxAnnounce {
            hash: H256(fixed::<32>("hash", h?)?),
        },
        Which::GetTx(h) => Message::GetTx {
            hash: H256(fixed::<32>("hash", h?)?),
        },
        Which::TxBody(b) => Message::TxBody { bytes: b?.to_vec() },
        Which::Consensus(b) => Message::Consensus { bytes: b?.to_vec() },
        Which::GetBlockRange(r) => {
            let r = r?;
            Message::GetBlockRange {
                from: r.get_from(),
                to: r.get_to(),
            }
        }
        Which::BlockRange(list) => {
            let list = list?;
            let mut out = Vec::with_capacity(bounded_capacity(list.len(), frame_len));
            for e in list.iter() {
                out.push((e.get_height(), e.get_bytes()?.to_vec()));
            }
            Message::BlockRange { blocks: out }
        }
        Which::Ping(n) => Message::Ping { nonce: n },
        Which::Pong(n) => Message::Pong { nonce: n },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let bytes = encode(&msg).expect("encode");
        let back = decode(&bytes).expect("decode");
        assert_eq!(msg, back);
    }

    #[test]
    fn every_variant_round_trips() {
        let (sk, _) = cb_crypto::keypair_from_seed(b"codec-test");
        let hello = Hello::signed(
            &sk,
            1,
            42,
            GenesisId(H256([3; 32])),
            "cb-node/test".into(),
            99,
            Some("10.0.0.1:7701".parse().expect("addr")),
            0xDEAD_BEEF,
        )
        .expect("sign");
        roundtrip(Message::Hello(hello));
        roundtrip(Message::Disconnect(DisconnectReason::GenesisMismatch));
        roundtrip(Message::GetPeers);
        roundtrip(Message::Peers(vec![PeerAddr {
            node_id: NodeId(Address([5; 20])),
            addr: "127.0.0.1:7700".parse().expect("addr"),
        }]));
        roundtrip(Message::BlockAnnounce {
            hash: H256([1; 32]),
            height: 7,
        });
        roundtrip(Message::GetBlock {
            hash: H256([2; 32]),
        });
        roundtrip(Message::BlockBody {
            height: 7,
            bytes: vec![1, 2, 3],
        });
        roundtrip(Message::TxAnnounce {
            hash: H256([4; 32]),
        });
        roundtrip(Message::GetTx {
            hash: H256([4; 32]),
        });
        roundtrip(Message::TxBody { bytes: vec![9; 64] });
        roundtrip(Message::Consensus {
            bytes: vec![7; 300],
        });
        roundtrip(Message::GetBlockRange { from: 2, to: 9 });
        roundtrip(Message::BlockRange {
            blocks: vec![(2, vec![0; 8]), (3, vec![1; 8])],
        });
        roundtrip(Message::Ping { nonce: 5 });
        roundtrip(Message::Pong { nonce: 5 });
    }

    #[test]
    fn ipv6_endpoints_survive() {
        roundtrip(Message::Peers(vec![PeerAddr {
            node_id: NodeId(Address([6; 20])),
            addr: "[2001:db8::1]:7700".parse().expect("addr"),
        }]));
    }

    #[test]
    fn a_hello_without_a_listen_address_stays_without_one() {
        let (sk, _) = cb_crypto::keypair_from_seed(b"codec-test-2");
        let hello = Hello::signed(&sk, 1, 1, GenesisId(H256([0; 32])), "x".into(), 0, None, 1)
            .expect("sign");
        let bytes = encode(&Message::Hello(hello)).expect("encode");
        match decode(&bytes).expect("decode") {
            Message::Hello(h) => assert!(h.listen.is_none()),
            other => panic!("expected hello, got {other:?}"),
        }
    }

    /// Every variant, once, with non-trivial contents.
    fn one_of_each() -> Vec<Message> {
        let (sk, _) = cb_crypto::keypair_from_seed(b"codec-frame");
        let hello = Hello::signed(
            &sk,
            1,
            42,
            GenesisId(H256([3; 32])),
            "cb-node/frame".into(),
            99,
            Some("10.0.0.1:7701".parse().expect("addr")),
            7,
        )
        .expect("sign");
        vec![
            Message::Hello(hello),
            Message::Disconnect(DisconnectReason::Timeout),
            Message::GetPeers,
            Message::Peers(vec![PeerAddr {
                node_id: NodeId(Address([5; 20])),
                addr: "[2001:db8::1]:7700".parse().expect("addr"),
            }]),
            Message::BlockAnnounce {
                hash: H256([1; 32]),
                height: 7,
            },
            Message::GetBlock {
                hash: H256([2; 32]),
            },
            Message::BlockBody {
                height: 7,
                bytes: vec![1; 1_000],
            },
            Message::TxAnnounce {
                hash: H256([4; 32]),
            },
            Message::GetTx {
                hash: H256([4; 32]),
            },
            Message::TxBody { bytes: vec![9; 65] },
            Message::Consensus {
                bytes: (0..=255u8).cycle().take(100_003).collect(),
            },
            Message::GetBlockRange { from: 2, to: 9 },
            Message::BlockRange {
                blocks: vec![(2, vec![0; 8]), (3, vec![1; 9])],
            },
            Message::Ping { nonce: 5 },
            Message::Pong { nonce: 5 },
        ]
    }

    #[test]
    fn a_shared_frame_is_byte_identical_to_the_prefix_and_message_it_replaced() {
        // The transport used to write `len.to_be_bytes()` and then
        // `encode(msg)` per peer. It now writes one pre-built `encode_frame`
        // to every peer. A peer on the previous build reads the same bytes.
        for msg in one_of_each() {
            let body = encode(&msg).expect("encode");
            let mut before = (body.len() as u32).to_be_bytes().to_vec();
            before.extend_from_slice(&body);
            let after = encode_frame(&msg).expect("encode_frame");
            assert_eq!(
                &after[..],
                &before[..],
                "{} changed on the wire",
                msg.kind()
            );
        }
    }

    #[test]
    fn frames_match_bytes_captured_from_the_previous_encoder() {
        // Hex captured from the per-peer path before frames were shared, so
        // this fails if `encode` and `encode_frame` ever drift together.
        let hex = |f: &[u8]| f.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let cases = [
            (
                Message::Consensus {
                    bytes: (0u8..=40).collect(),
                },
                "00000058000000000a00000000000000020001000e000000000000000000000000000000\
                 010000004a010000000102030405060708090a0b0c0d0e0f101112131415161718191a1b\
                 1c1d1e1f20212223242526272800000000000000",
            ),
            (
                Message::BlockRange {
                    blocks: vec![(7, vec![1, 2, 3]), (8, vec![4, 5, 6, 7, 8, 9, 10, 11, 12])],
                },
                "00000068000000000c00000000000000020001000b000000000000000000000000000000\
                 010000002700000008000000010001000700000000000000090000001a00000008000000\
                 00000000050000004a00000001020300000000000405060708090a0b0c00000000000000",
            ),
            (
                Message::Ping {
                    nonce: 0x0102_0304_0506_0708,
                },
                "00000028000000000400000000000000020001000c000000000000000807060504030201\
                 0000000000000000",
            ),
        ];
        for (msg, want) in cases {
            assert_eq!(
                hex(&encode_frame(&msg).expect("frame")),
                want,
                "{}",
                msg.kind()
            );
        }
    }

    #[test]
    fn the_frame_bounded_traversal_limit_still_admits_large_legitimate_frames() {
        // A tight budget must not refuse what the protocol really sends: a
        // multi-megabyte proposal, and a range of many small blocks — the
        // shape with the most pointers per byte.
        let proposal = Message::Consensus {
            bytes: vec![0xab; 8 * 1024 * 1024],
        };
        roundtrip(proposal);
        let many = Message::BlockRange {
            blocks: (0..20_000u64).map(|h| (h, vec![h as u8])).collect(),
        };
        roundtrip(many);
    }

    #[test]
    fn a_list_reserves_no_more_elements_than_the_frame_could_hold() {
        assert_eq!(bounded_capacity(8_000_000, 112), 7);
        assert_eq!(bounded_capacity(3, 1_000_000), 3);
        assert_eq!(bounded_capacity(u32::MAX, 0), 0);
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        assert!(decode(&[0xff; 16]).is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn a_signed_hello_survives_the_wire_and_still_verifies() {
        let (sk, _) = cb_crypto::keypair_from_seed(b"codec-test-3");
        let hello = Hello::signed(
            &sk,
            1,
            77,
            GenesisId(H256([8; 32])),
            "cb-node/wire".into(),
            1234,
            Some("127.0.0.1:7700".parse().expect("addr")),
            7,
        )
        .expect("sign");
        let bytes = encode(&Message::Hello(hello)).expect("encode");
        match decode(&bytes).expect("decode") {
            Message::Hello(h) => assert!(h.verify_identity()),
            other => panic!("expected hello, got {other:?}"),
        }
    }
}
