//! Canonical key encoders for derivatives column families.
//!
//! Keys are explicit byte strings — never `bincode(struct)`. Field order in
//! a bincode encoding depends on the struct layout; a refactor that reorders
//! fields would silently change every key. Hand-rolling the encoders pins the
//! layout to the protocol design doc, not the source-code field order.

use cb_types::{Address, DerivId};

/// `(owner: 20 || deriv_id: 4)` — total order on `(owner, market)`.
pub fn position_key(owner: &Address, market: &DerivId) -> [u8; 24] {
    let mut k = [0u8; 24];
    k[..20].copy_from_slice(&owner.0);
    k[20..].copy_from_slice(&market.as_bytes());
    k
}

/// `owner: 20`.
pub fn margin_account_key(owner: &Address) -> [u8; 20] {
    owner.0
}

/// `deriv_id: 4`.
pub fn deriv_key(market: &DerivId) -> [u8; 4] {
    market.as_bytes()
}

/// `(deriv_id: 4 || kind: u8)`. `kind` is `ORACLE_KIND_ORACLE` or
/// `ORACLE_KIND_MARK` from `oracle_state`.
pub fn oracle_state_key(market: &DerivId, kind: u8) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[..4].copy_from_slice(&market.as_bytes());
    k[4] = kind;
    k
}

/// `(deriv_id: 4 || kind=2: u8 || height: u64 BE)` — mark-history ring buffer
/// (M5). Lives in `CF_ORACLE_STATE` alongside the singleton oracle / mark
/// rows; the kind byte separates them. Big-endian height gives us
/// price-time-priority traversal (oldest first) under prefix iteration with
/// `[deriv_id || kind=2]`.
pub fn mark_history_key(market: &DerivId, height: u64) -> [u8; 13] {
    let mut k = [0u8; 13];
    k[..4].copy_from_slice(&market.as_bytes());
    k[4] = 2; // ORACLE_KIND_MARK_HISTORY — pinned by protocol-design Appendix B.
    k[5..].copy_from_slice(&height.to_be_bytes());
    k
}

/// 5-byte prefix for the mark-history scan: `[deriv_id || kind=2]`.
pub fn mark_history_prefix(market: &DerivId) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[..4].copy_from_slice(&market.as_bytes());
    k[4] = 2;
    k
}

/// `(height: u64 BE || seq: u64 BE)` — sorts in `(height, seq)` order under
/// RocksDB's default lex comparator.
pub fn liquidation_queue_key(height: u64, seq: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&height.to_be_bytes());
    k[8..].copy_from_slice(&seq.to_be_bytes());
    k
}

/// `(deriv_id: 4 || side: 1 || price_q8: 16 BE || seq: 8 BE)`. Big-endian
/// price/seq give correct lex order for price-time-priority traversal under
/// a `[deriv_id][side]` prefix scan.
pub fn perp_book_index_key(
    market: &DerivId,
    side: cb_types::Side,
    price_q8: u128,
    seq: u64,
) -> [u8; 29] {
    let mut k = [0u8; 29];
    k[..4].copy_from_slice(&market.as_bytes());
    k[4] = side_byte(side);
    k[5..21].copy_from_slice(&price_q8.to_be_bytes());
    k[21..].copy_from_slice(&seq.to_be_bytes());
    k
}

/// Prefix for prefix-iteration over a market's book on one side: 5 bytes.
pub fn perp_book_index_prefix(market: &DerivId, side: cb_types::Side) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[..4].copy_from_slice(&market.as_bytes());
    k[4] = side_byte(side);
    k
}

fn side_byte(s: cb_types::Side) -> u8 {
    match s {
        cb_types::Side::Buy => 0,
        cb_types::Side::Sell => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_types::Side;

    #[test]
    fn position_key_orders_owner_then_market() {
        let a = Address([0; 20]);
        let b = Address([1; 20]);
        let m0 = DerivId::perp(0);
        let m1 = DerivId::perp(1);
        // Same owner, different market: market drives order.
        assert!(position_key(&a, &m0) < position_key(&a, &m1));
        // Different owner: owner drives order.
        assert!(position_key(&a, &m1) < position_key(&b, &m0));
    }

    #[test]
    fn liquidation_queue_key_sorts_by_height_then_seq() {
        let a = liquidation_queue_key(1, 5);
        let b = liquidation_queue_key(1, 6);
        let c = liquidation_queue_key(2, 0);
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn perp_book_index_key_sorts_buy_low_first_sell_low_first() {
        let m = DerivId::perp(0);
        // Buys (side=0): sorted ascending by price (we'll iterate descending).
        let lo_buy = perp_book_index_key(&m, Side::Buy, 100, 0);
        let hi_buy = perp_book_index_key(&m, Side::Buy, 200, 0);
        assert!(lo_buy < hi_buy);

        // Sells (side=1) sit after all buys for the same market.
        let lo_sell = perp_book_index_key(&m, Side::Sell, 50, 0);
        assert!(hi_buy < lo_sell);

        // Same price, lower seq sorts first (time priority).
        let early = perp_book_index_key(&m, Side::Buy, 100, 1);
        let later = perp_book_index_key(&m, Side::Buy, 100, 2);
        assert!(early < later);
    }

    #[test]
    fn oracle_state_key_separates_oracle_and_mark() {
        let m = DerivId::perp(0);
        let o = oracle_state_key(&m, 0);
        let mk = oracle_state_key(&m, 1);
        assert_ne!(o, mk);
        assert_eq!(&o[..4], &m.as_bytes()[..]);
        assert_eq!(o[4], 0);
        assert_eq!(mk[4], 1);
    }
}
