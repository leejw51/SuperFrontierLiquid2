//! Property tests for every encode/decode pair `cb-types` owns.
//!
//! Each pair gets the same two questions, asked over generated inputs rather
//! than a handful of literals:
//!
//! 1. **Round trip.** Whatever the type can hold, encoding then decoding gives
//!    it back unchanged. A pair that loses or invents value on some input is a
//!    silent balance change the first time that input shows up.
//! 2. **Arbitrary input never panics.** Every decoder here sits behind a
//!    network or a file: order bodies, genesis lines, CF rows, RPC frames. A
//!    decoder that panics on a crafted input is a remote crash. The result on
//!    garbage is not asserted beyond "it returned".
//!
//! The types involved are consensus-load-bearing (see `ids.rs`), so the
//! bincode layouts are also pinned against a hand-written encoding where the
//! layout is simple enough to write down.

use cb_types::amount::{format_scaled, parse_scaled};
use cb_types::scales::{
    format_contract_qty, format_price_q8, format_spot_qty, parse_contract_qty, parse_price_q8,
    parse_spot_qty,
};
use cb_types::symbols::{all_instruments, format_symbol};
use cb_types::{
    parse_symbol, Address, AssetId, DerivId, FundingIndex, FutureMarket, Instrument,
    LiquidationEntry, MarginAccount, MarketId, PerpOrder, Position, Side, Signed, Tif, H256,
};
use proptest::prelude::*;

// ---------------------------------------------------------------- strategies --

fn address() -> impl Strategy<Value = Address> {
    any::<[u8; 20]>().prop_map(Address)
}

fn h256() -> impl Strategy<Value = H256> {
    any::<[u8; 32]>().prop_map(H256)
}

fn asset() -> impl Strategy<Value = AssetId> {
    prop::sample::select(AssetId::ALL.to_vec())
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn tif() -> impl Strategy<Value = Tif> {
    prop_oneof![Just(Tif::Gtc), Just(Tif::Ioc), Just(Tif::Fok)]
}

fn deriv_id() -> impl Strategy<Value = DerivId> {
    prop_oneof![
        any::<u16>().prop_map(DerivId::perp),
        any::<u16>().prop_map(DerivId::future)
    ]
}

/// Every value `Signed` can canonically hold: all of `i128`, plus magnitudes
/// past `i128::MAX` that only the sign-magnitude form can express.
fn signed() -> impl Strategy<Value = Signed> {
    prop_oneof![
        any::<i128>().prop_map(Signed::from_i128),
        (prop_oneof![Just(-1i8), Just(1i8)], 1u128..=u128::MAX)
            .prop_map(|(s, a)| Signed::new(s, a).expect("canonical by construction")),
    ]
}

fn bincode_round_trip<T>(v: &T) -> Result<(), TestCaseError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = bincode::serialize(v).expect("encodes");
    let back: T = bincode::deserialize(&bytes).expect("decodes what it encoded");
    prop_assert_eq!(&back, v);
    Ok(())
}

// ------------------------------------------------------------ identifiers --

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every byte either names an asset whose ordinal is that byte, or is
    /// refused. Nothing in between: no default, no wrap.
    #[test]
    fn asset_and_market_ordinals_decode_exactly_or_not_at_all(b in any::<u8>()) {
        match AssetId::from_u8(b) {
            Ok(a) => prop_assert_eq!(a.as_u8(), b),
            Err(_) => prop_assert!(b as usize >= AssetId::ALL.len()),
        }
        match MarketId::from_u8(b) {
            Ok(m) => prop_assert_eq!(m.as_u8(), b),
            Err(_) => prop_assert!(b as usize >= MarketId::ALL.len()),
        }
    }

    /// Four arbitrary bytes: a `DerivId` comes back only when the kind is
    /// known and the reserved byte is clear, and then it is those bytes.
    #[test]
    fn deriv_id_from_arbitrary_bytes_is_exact_or_refused(b in any::<[u8; 4]>()) {
        match DerivId::from_bytes(b) {
            Ok(d) => {
                prop_assert_eq!(d.as_bytes(), b);
                prop_assert!(b[3] == 0 && (b[0] == 1 || b[0] == 2));
                prop_assert_eq!(d.index(), u16::from_le_bytes([b[1], b[2]]));
                prop_assert!(d.kind().is_ok());
            }
            Err(_) => prop_assert!(b[3] != 0 || !(b[0] == 1 || b[0] == 2)),
        }
    }

    #[test]
    fn deriv_id_round_trips_through_bytes_and_bincode(d in deriv_id()) {
        prop_assert_eq!(DerivId::from_bytes(d.as_bytes()).unwrap(), d);
        bincode_round_trip(&d)?;
    }

    /// Names: arbitrary text never panics the `FromStr` impls, and anything
    /// accepted is a real asset or market whose own spelling parses to it.
    #[test]
    fn asset_and_market_names_never_panic_and_accept_only_real_names(s in "\\PC{0,12}") {
        if let Ok(a) = s.parse::<AssetId>() {
            prop_assert!(a.symbol().eq_ignore_ascii_case(&s));
        }
        if let Ok(m) = s.parse::<MarketId>() {
            let lower = s.to_ascii_lowercase();
            prop_assert!(lower == m.genesis_id() || lower == m.genesis_id().replace('_', ""));
        }
    }
}

// ----------------------------------------------------------------- symbols --

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Arbitrary text, including symbol-shaped text: parsing never panics, and
    /// whatever resolves formats back to the same symbol up to case.
    #[test]
    fn a_symbol_that_parses_formats_back_to_itself(
        s in prop_oneof![
            "\\PC{0,16}",
            "[A-Za-z]{1,5}-(USD|PERP|[0-9]{6})",
        ]
    ) {
        if let Ok(i) = parse_symbol(&s) {
            let back = format_symbol(i).expect("a parsed instrument is listed");
            prop_assert!(back.eq_ignore_ascii_case(&s), "`{}` -> {:?} -> `{}`", s, i, back);
        }
    }

    /// A deriv id outside the listed universe never gets a symbol, so no
    /// display path can print a real-looking name for a contract that is not
    /// listed.
    #[test]
    fn only_listed_instruments_have_symbols(d in deriv_id()) {
        let listed = all_instruments();
        for i in [Instrument::Perp(d), Instrument::Future(d)] {
            prop_assert_eq!(format_symbol(i).is_some(), listed.contains(&i));
        }
    }
}

// ----------------------------------------------------------- scaled amounts --

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Spot quantities round-trip at every asset's own decimals, over the
    /// whole u128 range.
    #[test]
    fn spot_quantities_round_trip_for_every_asset(a in asset(), v in any::<u128>()) {
        let text = format_spot_qty(a, v).unwrap();
        prop_assert_eq!(parse_spot_qty(a, &text).unwrap(), v, "{} via `{}`", a, text);
    }

    #[test]
    fn contract_quantities_and_q8_prices_round_trip(v in any::<u128>()) {
        prop_assert_eq!(parse_contract_qty(&format_contract_qty(v).unwrap()).unwrap(), v);
        prop_assert_eq!(parse_price_q8(&format_price_q8(v).unwrap()).unwrap(), v);
    }

    /// Parsing is exact: a decimal written with at most `d` places parses to
    /// `whole * 10^d + frac * 10^(d - places)`, computed here independently
    /// of the parser with plain integer arithmetic. `whole` spans all of u64
    /// and `d` stops at 18, so the expected value always fits in a u128
    /// (`u64::MAX * 10^18 < 2^128`) and every case is a real parse, not an
    /// overflow.
    #[test]
    fn parsing_matches_an_independent_computation(
        whole in any::<u64>(),
        digits in "[0-9]{0,18}",
        d in 0u8..=18,
    ) {
        let frac_digits = &digits[..digits.len().min(d as usize)];
        let text = if frac_digits.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{frac_digits}")
        };
        let frac: u128 = if frac_digits.is_empty() { 0 } else { frac_digits.parse().unwrap() };
        let pad = 10u128.pow((d as usize - frac_digits.len()) as u32);
        let expected = whole as u128 * 10u128.pow(d as u32) + frac * pad;
        prop_assert_eq!(parse_scaled(&text, d).unwrap(), expected, "`{}` at {}", text, d);
    }

    /// Adding one fractional digit beyond the scale is always refused, never
    /// rounded, whatever the digit is.
    #[test]
    fn one_digit_too_many_is_always_refused(v in any::<u64>(), d in 0u8..=18, extra in 0u8..=9) {
        let base = format_scaled(v as u128, d).unwrap();
        let (int, frac) = base.split_once('.').unwrap_or((&base, ""));
        let padded = format!("{int}.{frac:0<width$}{extra}", width = d as usize);
        prop_assert!(parse_scaled(&padded, d).is_err(), "`{}` at {} decimals parsed", padded, d);
    }
}

// ------------------------------------------------------ persisted records --

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn signed_round_trips_and_reconstitutes_its_i128(v in any::<i128>()) {
        let s = Signed::from_i128(v);
        prop_assert_eq!(s.to_i128(), v);
        bincode_round_trip(&s)?;
    }

    /// 17 arbitrary bytes are a `Signed`'s whole encoding. Decoding either
    /// fails or yields a canonical value, which re-encodes to the same bytes:
    /// no two encodings for one number.
    #[test]
    fn signed_decodes_arbitrary_bytes_to_canonical_values_only(b in any::<[u8; 17]>()) {
        if let Ok(s) = bincode::deserialize::<Signed>(&b) {
            prop_assert!(matches!(s.sign, -1..=1));
            prop_assert_eq!(s.sign == 0, s.abs == 0);
            prop_assert_eq!(bincode::serialize(&s).unwrap(), b.to_vec());
        }
    }

    #[test]
    fn position_round_trips(
        owner in address(), market in deriv_id(), side in side(), qty in any::<u128>(),
        f in signed(), px in any::<u128>(), mode in 0u8..=1, iso in any::<u128>(), h in any::<u64>(),
    ) {
        bincode_round_trip(&Position {
            owner, market, side, qty, entry_funding_index: f, entry_px_q8: px,
            margin_mode: mode, isolated_margin: iso, last_funding_height: h,
        })?;
    }

    #[test]
    fn margin_funding_and_liquidation_records_round_trip(
        owner in address(), other in address(), market in deriv_id(), a in any::<u128>(),
        b in any::<u128>(), s in signed(), h in any::<u64>(), rate in any::<i64>(), seq in any::<u64>(),
    ) {
        bincode_round_trip(&MarginAccount { owner, collateral: a, unrealized_pnl: s, bad_debt: b })?;
        bincode_round_trip(&FundingIndex { market, cumulative: s, last_tick_height: h, rate_q9: rate })?;
        bincode_round_trip(&LiquidationEntry {
            owner, market, triggered_by: other, triggered_at_height: h, mark_at_trigger_q8: a, seq,
        })?;
    }

    #[test]
    fn perp_orders_and_future_markets_round_trip(
        id in h256(), coid in h256(), owner in address(), market in deriv_id(), side in side(),
        tif in tif(), px in any::<u128>(), q in any::<u128>(), rem in any::<u128>(),
        flags in any::<(bool, bool, bool)>(), seq in any::<u64>(), h in any::<u64>(),
        u in asset(), settle in proptest::option::of(any::<u128>()),
    ) {
        bincode_round_trip(&PerpOrder {
            order_id: id, owner, market, side, price_q8: px, qty: q, remaining_qty: rem, tif,
            post_only: flags.0, reduce_only: flags.1, seq, placed_at_height: h, client_order_id: coid,
        })?;
        bincode_round_trip(&FutureMarket {
            id: market, underlying: u, tick_size: px, lot_size: q, max_leverage_x100: seq as u32,
            initial_margin_bps: 1, maintenance_margin_bps: 2, taker_fee_bps: 3,
            maker_fee_bps_signed: -1, expiry_height: h, settlement_method: flags.2 as u8,
            settlement_window_blocks: 7, oracle_authority: owner, paused: flags.0,
            final_settlement_px_q8: settle, last_trade_px_q8: rem, paused_reduce_only_only: flags.1,
            last_trade_height: h,
        })?;
    }

    /// Arbitrary bytes fed to every record decoder: an error is fine, a panic
    /// or an attempt to allocate a claimed gigantic length is not. The length
    /// cap is what bincode's own `Options` would enforce on the node; here it
    /// only has to not crash.
    #[test]
    fn record_decoders_survive_arbitrary_bytes(b in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = bincode::deserialize::<Position>(&b);
        let _ = bincode::deserialize::<MarginAccount>(&b);
        let _ = bincode::deserialize::<FundingIndex>(&b);
        let _ = bincode::deserialize::<LiquidationEntry>(&b);
        let _ = bincode::deserialize::<PerpOrder>(&b);
        let _ = bincode::deserialize::<FutureMarket>(&b);
        let _ = bincode::deserialize::<AssetId>(&b);
        let _ = bincode::deserialize::<MarketId>(&b);
        let _ = bincode::deserialize::<DerivId>(&b);
    }
}

// ------------------------------------------------------- pinned layouts --

#[test]
fn deriv_id_bincode_is_its_four_bytes() {
    // A fixed array encodes with no length prefix. The CF key and the
    // serialised record agree byte for byte, which the book index relies on.
    assert_eq!(
        bincode::serialize(&DerivId::future(0x0102)).unwrap(),
        vec![2, 0x02, 0x01, 0]
    );
}

#[test]
fn address_and_hash_encode_as_raw_bytes() {
    assert_eq!(
        bincode::serialize(&Address([0xab; 20])).unwrap(),
        vec![0xab; 20]
    );
    assert_eq!(
        bincode::serialize(&H256([0xcd; 32])).unwrap(),
        vec![0xcd; 32]
    );
}
