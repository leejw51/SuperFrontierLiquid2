//! Canonical Cap'n Proto encoding + hashing helpers.
//!
//! Uses `capnp` 0.20's `Reader::canonicalize()` API, which produces true canonical form
//! per the Cap'n Proto specification (single segment, no packing, sorted fields).
//! The framed bytes are produced by serializing the canonical words with a segment table.

use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;
use capnp::Word;
use cb_crypto::keccak256;
use cb_types::H256;

/// Canonical-encode a builder root.
///
/// Returns deterministic, canonical-form bytes (with standard segment-table framing)
/// suitable for hashing or wire transmission.
///
/// Internally this:
/// 1. Serialises the builder to framed bytes.
/// 2. Reads those bytes back as a `Reader`.
/// 3. Calls `Reader::canonicalize()` (capnp 0.20) to get a single canonical segment.
/// 4. Re-frames those canonical words with a segment table.
pub fn canonical_bytes(builder: &Builder<HeapAllocator>) -> capnp::Result<Vec<u8>> {
    // Step 1 – framed bytes from the builder.
    let framed = serialize::write_message_to_words(builder);

    // Step 2 – read back.
    let mut cursor = std::io::Cursor::new(&framed);
    let reader = serialize::read_message(&mut cursor, ReaderOptions::new())?;

    // Step 3 – produce canonical words (no framing, single segment).
    let canonical_words: Vec<Word> = reader.canonicalize()?;

    // Step 4 – re-frame: build a one-segment canonical message.
    // We build a fresh Builder whose sole segment is the canonical words, then serialise it.
    let canonical_word_bytes = Word::words_to_bytes(&canonical_words);

    // Use a HeapAllocator-backed builder with set_root_canonical so the output
    // is guaranteed single-segment and canonical.
    // Re-read the canonical words as a flat-slice message, then re-frame via write_message_to_words.
    // Canonical form has no segment table; wrap raw words in a single-segment framed message.
    let reframed = frame_single_segment(canonical_word_bytes);
    Ok(reframed)
}

/// Frame a single raw segment (no segment table) into standard Cap'n Proto framing.
///
/// Format: [segment_count-1 (u32le)] [segment_0_word_count (u32le)] [padding (u32le 0)] [segment data]
fn frame_single_segment(segment_bytes: &[u8]) -> Vec<u8> {
    assert_eq!(segment_bytes.len() % 8, 0, "segment must be word-aligned");
    let word_count = (segment_bytes.len() / 8) as u32;
    // Segment table for 1 segment: 4 bytes count (0 = one segment), 4 bytes word count.
    // Total table = 1 word = 8 bytes.
    let mut out = Vec::with_capacity(8 + segment_bytes.len());
    out.extend_from_slice(&0u32.to_le_bytes()); // segment_count - 1 = 0
    out.extend_from_slice(&word_count.to_le_bytes()); // words in segment 0
    out.extend_from_slice(segment_bytes);
    out
}

/// keccak256 over canonical bytes.
pub fn canonical_hash(builder: &Builder<HeapAllocator>) -> capnp::Result<H256> {
    Ok(keccak256(&canonical_bytes(builder)?))
}

/// Read a typed message from framed (canonical or non-canonical) bytes.
pub fn read_message(
    bytes: &[u8],
) -> capnp::Result<capnp::message::Reader<serialize::OwnedSegments>> {
    let mut cursor = std::io::Cursor::new(bytes);
    serialize::read_message(&mut cursor, ReaderOptions::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_capnp;
    use crate::types_capnp;
    use capnp::message::Builder;

    fn build_sample() -> Builder<HeapAllocator> {
        let mut b: Builder<HeapAllocator> = Builder::new_default();
        {
            let mut tx = b.init_root::<tx_capnp::transaction::Builder<'_>>();
            tx.set_nonce(7);
            tx.set_chain_id(1);
            let body = tx.reborrow().init_body();
            let mut transfer = body.init_transfer();
            {
                let mut to = transfer.reborrow().init_to();
                to.set_bytes(&[1u8; 20]);
            }
            transfer.reborrow().set_asset(types_capnp::AssetId::Usd);
            {
                let mut amt = transfer.init_amount();
                amt.set_hi(0);
                amt.set_lo(1_000_000);
            }
        }
        b
    }

    #[test]
    fn canonical_hash_is_stable_across_two_builds() {
        let b1 = build_sample();
        let b2 = build_sample();
        let h1 = canonical_hash(&b1).unwrap();
        let h2 = canonical_hash(&b2).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let b = build_sample();
        let bytes = canonical_bytes(&b).unwrap();
        let reader = read_message(&bytes).unwrap();
        let r: tx_capnp::transaction::Reader = reader.get_root().unwrap();
        assert_eq!(r.get_nonce(), 7);
        assert_eq!(r.get_chain_id(), 1);
    }

    #[test]
    fn canonical_bytes_produce_valid_canonical_message() {
        let b = build_sample();
        let bytes = canonical_bytes(&b).unwrap();
        // Re-read and check that the result is canonical.
        let mut cursor = std::io::Cursor::new(&bytes);
        let reader = serialize::read_message(&mut cursor, ReaderOptions::new()).unwrap();
        assert!(reader.is_canonical().unwrap());
    }

    // ---------------- perp tx canonical encoding ----------------

    fn build_perp_sample() -> Builder<HeapAllocator> {
        let mut b: Builder<HeapAllocator> = Builder::new_default();
        {
            let mut tx = b.init_root::<tx_capnp::transaction::Builder<'_>>();
            tx.set_nonce(7);
            tx.set_chain_id(1);
            let body = tx.reborrow().init_body();
            let mut p = body.init_place_perp_order();
            {
                // DerivId::perp(0) = [1, 0, 0, 0]
                let mut m = p.reborrow().init_market();
                m.set_bytes(&[1u8, 0, 0, 0]);
            }
            p.reborrow().set_side(types_capnp::Side::Buy);
            {
                let mut px = p.reborrow().init_price_q8();
                px.set_hi(0);
                px.set_lo(5_000_000_000_000);
            }
            {
                let mut q = p.reborrow().init_qty();
                q.set_hi(0);
                q.set_lo(1_000_000);
            }
            p.reborrow().set_tif(types_capnp::Tif::Gtc);
            p.reborrow().set_post_only(false);
            p.reborrow().set_reduce_only(false);
            {
                let mut id = p.init_client_order_id();
                id.set_bytes(&[0xab; 32]);
            }
        }
        b
    }

    #[test]
    fn place_perp_order_canonical_hash_is_stable() {
        let h1 = canonical_hash(&build_perp_sample()).unwrap();
        let h2 = canonical_hash(&build_perp_sample()).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn place_perp_order_canonical_round_trip() {
        let b = build_perp_sample();
        let bytes = canonical_bytes(&b).unwrap();
        let reader = read_message(&bytes).unwrap();
        let r: tx_capnp::transaction::Reader = reader.get_root().unwrap();
        assert_eq!(r.get_nonce(), 7);
        match r.get_body().which().unwrap() {
            tx_capnp::transaction::body::Which::PlacePerpOrder(p) => {
                let p = p.unwrap();
                let market = p.get_market().unwrap();
                let mb = market.get_bytes().unwrap();
                assert_eq!(mb, &[1u8, 0, 0, 0]);
                assert!(matches!(p.get_side().unwrap(), types_capnp::Side::Buy));
            }
            _ => panic!("wrong variant"),
        }
    }
}
