//! Canonical key encoders for the spot column families.
//!
//! Hand-rolled byte-string keys (never `bincode(struct)`): bincode's field
//! order is a function of the struct definition, so a refactor that reorders
//! fields would silently change every key. Pinning the layout here keeps the
//! on-disk format tied to the protocol design, not the source layout.
//!
//! S1 layout for `CF_BOOK_INDEX` (per `docs/pm/plan-client-trade.md` §S1):
//!
//! ```text
//! [market_id: 8 BE || side: 1 || price: 16 BE || seq: 8 BE]  -> order_id (32 bytes)
//! ```
//!
//! Total key width: 33 bytes. Big-endian price + seq give correct lex order
//! for price-time-priority traversal under a `[market || side]` prefix scan.
//! 8 bytes for the market id is wider than `MarketId::as_u8` strictly needs
//! (today only 0/1 are valid), but it leaves headroom for a future widening
//! of the market id space without a re-genesis.

use cb_types::{MarketId, Side};

/// Width of the encoded market id prefix, in bytes.
pub const MARKET_ID_BYTES: usize = 8;
/// Side discriminator width.
pub const SIDE_BYTES: usize = 1;
/// Price width (matches `u128` BE encoding).
pub const PRICE_BYTES: usize = 16;
/// Seq width (matches `u64` BE encoding).
pub const SEQ_BYTES: usize = 8;

/// `[market_id_bytes || side_byte]` — 9-byte prefix used for prefix-iteration
/// over a single side of one market's book.
pub const BOOK_INDEX_PREFIX_LEN: usize = MARKET_ID_BYTES + SIDE_BYTES;
/// Full `[market || side || price || seq]` key width.
pub const BOOK_INDEX_KEY_LEN: usize = MARKET_ID_BYTES + SIDE_BYTES + PRICE_BYTES + SEQ_BYTES;

/// 8-byte big-endian encoding of a `MarketId`. Pads `as_u8()` into a `u64` so
/// the on-disk key has stable width across future widenings of `MarketId`.
pub fn market_id_bytes(market: MarketId) -> [u8; MARKET_ID_BYTES] {
    (market.as_u8() as u64).to_be_bytes()
}

/// `0` for `Buy`, `1` for `Sell`. Matches the perp encoding.
pub fn side_byte(s: Side) -> u8 {
    match s {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

/// `[market_id (8) || side (1) || price (16 BE) || seq (8 BE)]`.
pub fn book_index_key(
    market: MarketId,
    side: Side,
    price: u128,
    seq: u64,
) -> [u8; BOOK_INDEX_KEY_LEN] {
    let mut k = [0u8; BOOK_INDEX_KEY_LEN];
    k[..MARKET_ID_BYTES].copy_from_slice(&market_id_bytes(market));
    k[MARKET_ID_BYTES] = side_byte(side);
    let p_lo = MARKET_ID_BYTES + SIDE_BYTES;
    k[p_lo..p_lo + PRICE_BYTES].copy_from_slice(&price.to_be_bytes());
    let s_lo = p_lo + PRICE_BYTES;
    k[s_lo..].copy_from_slice(&seq.to_be_bytes());
    k
}

/// 9-byte `[market_id || side]` prefix for prefix-iteration on one side.
pub fn book_index_prefix(market: MarketId, side: Side) -> [u8; BOOK_INDEX_PREFIX_LEN] {
    let mut k = [0u8; BOOK_INDEX_PREFIX_LEN];
    k[..MARKET_ID_BYTES].copy_from_slice(&market_id_bytes(market));
    k[MARKET_ID_BYTES] = side_byte(side);
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_widths_match_constants() {
        let k = book_index_key(MarketId::BtcUsd, Side::Buy, 0, 0);
        assert_eq!(k.len(), 33);
        let p = book_index_prefix(MarketId::BtcUsd, Side::Buy);
        assert_eq!(p.len(), 9);
    }

    #[test]
    fn market_then_side_then_price_then_seq_lex_order() {
        let m = MarketId::BtcUsd;
        // Same side (Buy=0): lower price sorts first under default lex order.
        let lo = book_index_key(m, Side::Buy, 100, 0);
        let hi = book_index_key(m, Side::Buy, 200, 0);
        assert!(lo < hi);

        // Same price, lower seq sorts first (time priority within a level).
        let early = book_index_key(m, Side::Buy, 100, 1);
        let later = book_index_key(m, Side::Buy, 100, 2);
        assert!(early < later);

        // Sells (side=1) sort after every Buy entry for the same market.
        let max_buy = book_index_key(m, Side::Buy, u128::MAX, u64::MAX);
        let min_sell = book_index_key(m, Side::Sell, 0, 0);
        assert!(max_buy < min_sell);

        // Different markets are wholly disjoint at the prefix level.
        let other = book_index_key(MarketId::EthUsd, Side::Buy, 0, 0);
        let same_max = book_index_key(MarketId::BtcUsd, Side::Sell, u128::MAX, u64::MAX);
        assert!(same_max < other);
    }

    #[test]
    fn prefix_is_full_key_prefix() {
        let m = MarketId::EthUsd;
        let pref = book_index_prefix(m, Side::Sell);
        let full = book_index_key(m, Side::Sell, 42, 7);
        assert!(full.starts_with(&pref));
    }

    #[test]
    fn market_id_bytes_are_be_u64_pad() {
        let b = market_id_bytes(MarketId::EthUsd);
        // EthUsd::as_u8() == 1; BE u64 → seven leading zero bytes then 1.
        assert_eq!(b, [0, 0, 0, 0, 0, 0, 0, 1]);
    }
}
