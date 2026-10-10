//! The mock backend's data, lifted from the design reference.
//!
//! Every figure a reader can check against `docs/design/reference.html` is the
//! reference's own: the book levels, the tape, the three positions and their
//! liquidation distances, the portfolio totals, the leaderboard, the chain
//! statistics. Those numbers were chosen to be internally consistent — the
//! position P&L really is the mark minus the entry times the size — and keeping
//! them lets a reviewer put the two side by side.
//!
//! Where a screen needs more rows than the reference printed (the ledger and the
//! fills blotter both have to exceed two hundred rows for virtualisation to be
//! worth anything), the extra rows are generated from a seeded, deterministic
//! sequence so that every reload shows the same history.
//!
//! All arithmetic here is [`Decimal`]. The generator's randomness produces
//! integers, which are then scaled by a decimal tick — a price is never the
//! result of a floating-point multiply.

use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::clock::rfc3339;

use super::types::*;

/// Builds a plausible twenty-byte address from a seed.
///
/// Fixture addresses have to look like addresses: a zero-padded counter would
/// truncate to the same few characters everywhere the interface shortens one,
/// which is not what the production treatment looks like.
#[must_use]
pub fn pseudo_address(seed: u64) -> String {
    let mut rng = Rng::new(seed | 1);
    let mut bytes = [0_u8; 20];
    for chunk in bytes.chunks_mut(8) {
        let word = rng.next_u64().to_be_bytes();
        let take = chunk.len();
        chunk.copy_from_slice(&word[..take]);
    }
    crate::wallet::to_eip55(&bytes)
}

/// Parses a decimal literal from the reference document.
///
/// A malformed literal is a bug in this file, not in user input, and it would be
/// caught the first time the screen renders. Falling back to zero keeps a typo
/// from taking the whole application down with a wasm panic, which renders as a
/// blank page.
#[must_use]
pub fn dec(literal: &str) -> Decimal {
    Decimal::from_str(literal).unwrap_or_default()
}

/// A deterministic xorshift generator.
///
/// Seeded from a constant, so the demo is the same on every reload and a
/// screenshot can be compared with the one before it.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Creates a generator from a seed. Zero is remapped, since xorshift has a
    /// fixed point there.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// The next raw value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..range`, or zero when the range is empty.
    pub fn below(&mut self, range: u64) -> u64 {
        if range == 0 {
            0
        } else {
            self.next_u64() % range
        }
    }

    /// A signed value in `-span..=span`.
    pub fn centred(&mut self, span: i64) -> i64 {
        if span <= 0 {
            return 0;
        }
        let width = (span * 2 + 1) as u64;
        self.below(width) as i64 - span
    }

    /// A decimal in `-span..=span` scaled by `tick`.
    pub fn jitter(&mut self, span: i64, tick: Decimal) -> Decimal {
        Decimal::from(self.centred(span)) * tick
    }
}

// ------------------------------------------------------------------- markets

/// Builds a spot market definition.
// One argument per column of the market table this mirrors; a struct here would
// exist only to be destructured at the single call site.
#[allow(clippy::too_many_arguments)]
fn spot(
    symbol: &str,
    class: AssetClass,
    base: &str,
    name: &str,
    tick: &str,
    lot: &str,
    price_dp: u32,
    size_dp: u32,
) -> Market {
    Market {
        symbol: symbol.into(),
        kind: MarketKind::Spot,
        asset_class: class,
        base: base.into(),
        quote: "USD".into(),
        display_name: name.into(),
        status: MarketStatus::Trading,
        tick_size: dec(tick),
        lot_size: dec(lot),
        min_notional: dec("10.00"),
        price_decimals: price_dp,
        size_decimals: size_dp,
        max_leverage: None,
        initial_margin_bps: None,
        maintenance_margin_bps: None,
        funding_interval_seconds: None,
        expiry: None,
        maker_fee_bps: -1,
        taker_fee_bps: 4,
    }
}

/// Builds a perpetual market definition.
// One argument per column of the market table this mirrors; grouping them into
// a struct would add a type whose only purpose is to be destructured here.
#[allow(clippy::too_many_arguments)]
fn perp(
    symbol: &str,
    class: AssetClass,
    base: &str,
    name: &str,
    tick: &str,
    lot: &str,
    price_dp: u32,
    size_dp: u32,
    max_leverage: &str,
    status: MarketStatus,
) -> Market {
    Market {
        symbol: symbol.into(),
        kind: MarketKind::Perp,
        asset_class: class,
        base: base.into(),
        quote: "USD".into(),
        display_name: name.into(),
        status,
        tick_size: dec(tick),
        lot_size: dec(lot),
        min_notional: dec("10.00"),
        price_decimals: price_dp,
        size_decimals: size_dp,
        max_leverage: Some(dec(max_leverage)),
        initial_margin_bps: Some(200),
        maintenance_margin_bps: Some(100),
        funding_interval_seconds: Some(3_600),
        expiry: None,
        maker_fee_bps: -1,
        taker_fee_bps: 4,
    }
}

/// Builds a dated future definition.
fn future(symbol: &str, base: &str, name: &str, expiry: &str, status: MarketStatus) -> Market {
    Market {
        symbol: symbol.into(),
        kind: MarketKind::Future,
        asset_class: AssetClass::Crypto,
        base: base.into(),
        quote: "USD".into(),
        display_name: name.into(),
        status,
        tick_size: dec("0.05"),
        lot_size: dec("0.0100"),
        min_notional: dec("10.00"),
        price_decimals: 2,
        size_decimals: 4,
        max_leverage: Some(dec("20")),
        initial_margin_bps: Some(500),
        maintenance_margin_bps: Some(250),
        funding_interval_seconds: None,
        expiry: Some(expiry.into()),
        maker_fee_bps: -1,
        taker_fee_bps: 4,
    }
}

/// The seventeen markets the landing page advertises.
///
/// The set deliberately covers every combination the interface has to render:
/// spot and derivative, crypto and equity, and all four values of
/// `MarketStatus`, so that the halted, settling and settled treatments are
/// reachable from the mock without any special mode.
#[must_use]
pub fn markets() -> Vec<Market> {
    vec![
        perp(
            "BTC-PERP",
            AssetClass::Crypto,
            "BTC",
            "Bitcoin Perpetual",
            "0.10",
            "0.0001",
            2,
            4,
            "50",
            MarketStatus::Trading,
        ),
        perp(
            "ETH-PERP",
            AssetClass::Crypto,
            "ETH",
            "Ether Perpetual",
            "0.05",
            "0.0100",
            2,
            4,
            "50",
            MarketStatus::Trading,
        ),
        perp(
            "SOL-PERP",
            AssetClass::Crypto,
            "SOL",
            "Solana Perpetual",
            "0.01",
            "0.1000",
            2,
            4,
            "25",
            MarketStatus::Trading,
        ),
        perp(
            "AAPL-PERP",
            AssetClass::Equity,
            "AAPL",
            "Apple Perpetual",
            "0.01",
            "1.0000",
            2,
            4,
            "10",
            MarketStatus::Trading,
        ),
        perp(
            "TSLA-PERP",
            AssetClass::Equity,
            "TSLA",
            "Tesla Perpetual",
            "0.01",
            "1.0000",
            2,
            4,
            "10",
            MarketStatus::Halted,
        ),
        perp(
            "NVDA-PERP",
            AssetClass::Equity,
            "NVDA",
            "Nvidia Perpetual",
            "0.01",
            "1.0000",
            2,
            4,
            "10",
            MarketStatus::Trading,
        ),
        spot(
            "BTC-USD",
            AssetClass::Crypto,
            "BTC",
            "Bitcoin",
            "0.10",
            "0.0001",
            2,
            4,
        ),
        spot(
            "ETH-USD",
            AssetClass::Crypto,
            "ETH",
            "Ether",
            "0.05",
            "0.0100",
            2,
            4,
        ),
        spot(
            "SOL-USD",
            AssetClass::Crypto,
            "SOL",
            "Solana",
            "0.01",
            "0.1000",
            2,
            4,
        ),
        spot(
            "AAPL-USD",
            AssetClass::Equity,
            "AAPL",
            "Apple",
            "0.01",
            "1.0000",
            2,
            4,
        ),
        spot(
            "NVDA-USD",
            AssetClass::Equity,
            "NVDA",
            "Nvidia",
            "0.01",
            "1.0000",
            2,
            4,
        ),
        spot(
            "TSLA-USD",
            AssetClass::Equity,
            "TSLA",
            "Tesla",
            "0.01",
            "1.0000",
            2,
            4,
        ),
        spot(
            "MSFT-USD",
            AssetClass::Equity,
            "MSFT",
            "Microsoft",
            "0.01",
            "1.0000",
            2,
            4,
        ),
        spot(
            "SPY-USD",
            AssetClass::Equity,
            "SPY",
            "S&P 500 tracker",
            "0.01",
            "1.0000",
            2,
            4,
        ),
        future(
            "ETH-260626",
            "ETH",
            "Ether June 2026",
            "2026-06-26T08:00:00Z",
            MarketStatus::Expired,
        ),
        future(
            "ETH-261225",
            "ETH",
            "Ether December 2026",
            "2026-12-25T08:00:00Z",
            MarketStatus::Trading,
        ),
        future(
            "BTC-261225",
            "BTC",
            "Bitcoin December 2026",
            "2026-12-25T08:00:00Z",
            MarketStatus::Settling,
        ),
    ]
}

/// A market's symbol, its last price and its 24-hour change, as strings.
pub type Quote = (&'static str, &'static str, &'static str);

/// One side of the reference's printed book: price and quantity, as strings.
pub type PrintedLevel = (&'static str, &'static str);

/// The last price and 24-hour change the reference shows for each market.
///
/// Returned as strings so a reader can diff this table against the reference's
/// market list line by line.
#[must_use]
pub fn quotes() -> Vec<Quote> {
    vec![
        ("BTC-PERP", "64250.50", "2.41"),
        ("ETH-PERP", "3242.90", "1.08"),
        ("SOL-PERP", "141.05", "-4.82"),
        ("AAPL-PERP", "230.40", "0.71"),
        ("TSLA-PERP", "248.90", "0.00"),
        ("NVDA-PERP", "121.50", "-1.18"),
        ("BTC-USD", "64238.00", "2.38"),
        ("ETH-USD", "3241.14", "1.02"),
        ("SOL-USD", "140.92", "-4.90"),
        ("AAPL-USD", "230.14", "0.62"),
        ("NVDA-USD", "121.36", "-1.24"),
        ("TSLA-USD", "248.60", "0.40"),
        ("MSFT-USD", "418.22", "0.34"),
        ("SPY-USD", "566.20", "0.18"),
        ("ETH-260626", "3402.15", "0.00"),
        ("ETH-261225", "3318.40", "1.21"),
        ("BTC-261225", "65180.00", "2.10"),
    ]
}

/// A ticker built from the reference's figures for the given market.
#[must_use]
pub fn ticker(market: &Market, last: Decimal, change: Decimal, now: i64) -> Ticker {
    let mark = last - (last * dec("0.0000374")).round_dp(u32::from(market.price_decimals as u8));
    let index = last + (last * dec("0.0000078")).round_dp(u32::from(market.price_decimals as u8));
    let hundred = Decimal::from(100);
    let opening = if change == Decimal::ZERO {
        last
    } else {
        last / (Decimal::ONE + change / hundred)
    };
    let span = (last - opening).abs().max(last * dec("0.004"));
    Ticker {
        symbol: market.symbol.clone(),
        last_price: last,
        mark_price: mark.round_dp(market.price_decimals),
        index_price: index.round_dp(market.price_decimals),
        open_interest: (last * dec("0.02")).round_dp(market.size_decimals),
        volume_24h: (last * dec("2000")).round_dp(2),
        change_24h_pct: change,
        high_24h: (last + span).round_dp(market.price_decimals),
        low_24h: (last - span * dec("1.4")).round_dp(market.price_decimals),
        funding_rate: market.kind.is_derivative().then(|| {
            if change >= Decimal::ZERO {
                dec("0.000125")
            } else {
                dec("-0.000094")
            }
        }),
        next_funding_at: market.kind.is_derivative().then(|| rfc3339(next_hour(now))),
    }
}

/// The next whole hour, which is when funding is charged.
#[must_use]
pub fn next_hour(now: i64) -> i64 {
    now - now.rem_euclid(3_600) + 3_600
}

// ---------------------------------------------------------------- order book

/// The BTC-PERP book exactly as the reference prints it, best price first.
///
/// These nine levels a side are the visible ones. [`book`] extends them
/// deterministically so that the depth query and the cumulative totals behave
/// like a real book rather than running out after nine rows.
#[must_use]
pub fn reference_book() -> (Vec<PrintedLevel>, Vec<PrintedLevel>) {
    let asks = vec![
        ("64251.00", "0.8020"),
        ("64251.40", "1.0700"),
        ("64252.10", "3.1200"),
        ("64252.80", "0.9100"),
        ("64253.60", "1.0400"),
        ("64254.30", "2.4100"),
        ("64255.50", "1.0600"),
        ("64256.90", "2.0800"),
        ("64257.80", "1.9100"),
    ];
    let bids = vec![
        ("64250.00", "1.2500"),
        ("64249.50", "3.0000"),
        ("64249.10", "0.8400"),
        ("64248.40", "1.3400"),
        ("64247.90", "1.7700"),
        ("64247.20", "1.7500"),
        ("64246.30", "1.6800"),
        ("64245.10", "1.7400"),
        ("64244.40", "1.3800"),
    ];
    (bids, asks)
}

/// Builds a book of `depth` levels a side around `last`.
///
/// For BTC-PERP the first nine levels a side are the reference's own; beyond
/// that, and for every other market, levels are generated on the market's tick
/// with sizes that thicken away from the spread, which is what a real book looks
/// like and what makes the cumulative depth bars form a silhouette.
#[must_use]
pub fn book(
    market: &Market,
    last: Decimal,
    height: u64,
    depth: u32,
    seed: u64,
) -> OrderBookPayload {
    let mut rng = Rng::new(seed);
    let tick = market.tick_size;
    let lot = market.lot_size;
    let mut bids: Vec<Level> = Vec::new();
    let mut asks: Vec<Level> = Vec::new();

    if market.symbol == "BTC-PERP" {
        let (reference_bids, reference_asks) = reference_book();
        bids.extend(
            reference_bids
                .iter()
                .map(|(price, qty)| Level(dec(price), dec(qty))),
        );
        asks.extend(
            reference_asks
                .iter()
                .map(|(price, qty)| Level(dec(price), dec(qty))),
        );
    }

    let base_size = (last / dec("40000"))
        .max(lot)
        .round_dp(market.size_decimals);
    let mut bid_price = bids.last().map_or(last, |level| level.price());
    let mut ask_price = asks.last().map_or(last + tick, |level| level.price());

    while bids.len() < depth as usize {
        bid_price -= tick * Decimal::from(1 + rng.below(4));
        let size = (base_size
            * Decimal::from(1 + bids.len() as u64 / 4)
            * Decimal::from_u64(70 + rng.below(90)).unwrap_or(Decimal::ONE)
            / Decimal::from(100))
        .round_dp(market.size_decimals)
        .max(lot);
        bids.push(Level(bid_price.round_dp(market.price_decimals), size));
    }
    while asks.len() < depth as usize {
        ask_price += tick * Decimal::from(1 + rng.below(4));
        let size = (base_size
            * Decimal::from(1 + asks.len() as u64 / 4)
            * Decimal::from_u64(70 + rng.below(90)).unwrap_or(Decimal::ONE)
            / Decimal::from(100))
        .round_dp(market.size_decimals)
        .max(lot);
        asks.push(Level(ask_price.round_dp(market.price_decimals), size));
    }

    OrderBookPayload { height, bids, asks }
}

// ----------------------------------------------------------------- the tape

/// The sixteen trades the reference's tape shows, newest first.
#[must_use]
pub fn reference_tape() -> Vec<(&'static str, &'static str, Side)> {
    vec![
        ("64250.50", "0.2500", Side::Buy),
        ("64250.00", "1.1400", Side::Sell),
        ("64251.00", "0.0800", Side::Buy),
        ("64250.90", "3.2000", Side::Buy),
        ("64249.50", "0.4200", Side::Sell),
        ("64249.10", "0.6700", Side::Sell),
        ("64250.20", "0.1900", Side::Buy),
        ("64250.40", "2.0400", Side::Buy),
        ("64248.80", "0.9100", Side::Sell),
        ("64248.40", "1.5000", Side::Sell),
        ("64249.60", "0.3300", Side::Buy),
        ("64249.90", "0.7800", Side::Buy),
        ("64247.20", "4.1000", Side::Sell),
        ("64248.10", "0.2200", Side::Buy),
        ("64248.50", "0.6000", Side::Buy),
        ("64246.90", "1.2400", Side::Sell),
    ]
}

/// Builds a tape of `limit` trades ending at `now`.
#[must_use]
pub fn trades(market: &Market, last: Decimal, height: u64, limit: u32, now: i64) -> Vec<Trade> {
    let mut rng = Rng::new(0x7A5E_5EED ^ u64::from(limit));
    let mut out = Vec::with_capacity(limit as usize);
    let reference = reference_tape();
    for index in 0..limit as usize {
        let (price, qty, side) = if market.symbol == "BTC-PERP" && index < reference.len() {
            let (price, qty, side) = reference[index];
            (dec(price), dec(qty), side)
        } else {
            let price = (last + rng.jitter(6, market.tick_size)).round_dp(market.price_decimals);
            let qty = (market.lot_size * Decimal::from(1 + rng.below(4_000)))
                .round_dp(market.size_decimals);
            let side = if rng.below(2) == 0 {
                Side::Buy
            } else {
                Side::Sell
            };
            (price, qty, side)
        };
        out.push(Trade {
            id: format!("trd_{:06x}", 0x10_0000 - index),
            price,
            qty,
            side,
            at: rfc3339(now - index as i64),
            height: height.saturating_sub(index as u64),
        });
    }
    out
}

// ------------------------------------------------------------------ candles

/// Builds `limit` candles ending at `last`, on a deterministic random walk.
///
/// The walk is generated backwards from the known closing price so that the
/// final candle's close is exactly the last price the rest of the interface
/// shows. A chart whose right-hand edge disagrees with the market strip beside
/// it is worse than no chart.
#[must_use]
pub fn candles(
    market: &Market,
    last: Decimal,
    interval_seconds: i64,
    limit: u32,
    now: i64,
) -> Vec<Candle> {
    let mut rng = Rng::new(0x0CA0_D1E5 ^ market.symbol.len() as u64);
    let step = market.tick_size * Decimal::from(12);
    let mut close = last;
    let mut backwards: Vec<Candle> = Vec::with_capacity(limit as usize);
    let bucket = now - now.rem_euclid(interval_seconds);

    for index in 0..limit as i64 {
        let open = (close - rng.jitter(14, step)).round_dp(market.price_decimals);
        let high = open.max(close) + (rng.jitter(6, step)).abs();
        let low = open.min(close) - (rng.jitter(6, step)).abs();
        let volume = (market.lot_size * Decimal::from(200 + rng.below(1_600)))
            .round_dp(market.size_decimals);
        backwards.push(Candle {
            t: rfc3339(bucket - index * interval_seconds),
            o: open,
            h: high.round_dp(market.price_decimals),
            l: low.round_dp(market.price_decimals),
            c: close,
            v: volume,
        });
        close = open;
    }
    backwards.reverse();
    backwards
}

// ------------------------------------------------------------------ account

/// The account the reference's portfolio screen describes.
#[must_use]
pub fn account(login_address: &str, trading_address: &str) -> Account {
    Account {
        login_address: login_address.into(),
        trading_address: trading_address.into(),
        nonce: 42,
        balances: vec![
            Balance {
                asset: "USD".into(),
                free: dec("94000.00"),
                locked: dec("6000.00"),
                total: dec("100000.00"),
            },
            Balance {
                asset: "BTC".into(),
                free: dec("1.00000000"),
                locked: dec("0.00000000"),
                total: dec("1.00000000"),
            },
            Balance {
                asset: "ETH".into(),
                free: dec("10.00000000"),
                locked: dec("0.00000000"),
                total: dec("10.00000000"),
            },
        ],
        portfolio: Portfolio {
            equity: dec("104250.00"),
            available_margin: dec("56086.00"),
            used_margin: dec("48164.00"),
            unrealized_pnl: dec("-6490.85"),
            realized_pnl_24h: dec("820.00"),
            margin_ratio: dec("0.462"),
            liquidation_risk: LiquidationRisk::Medium,
        },
    }
}

/// The three positions the reference's portfolio screen shows, including the one
/// sitting a fraction of a percent from liquidation.
#[must_use]
pub fn positions(now: i64) -> Vec<Position> {
    vec![
        Position {
            symbol: "BTC-PERP".into(),
            side: PositionSide::Long,
            qty: dec("1.5000"),
            entry_price: dec("63000.00"),
            mark_price: dec("64248.10"),
            liquidation_price: dec("58200.00"),
            notional: dec("96372.15"),
            margin_mode: MarginMode::Cross,
            leverage: dec("10"),
            margin: dec("9637.21"),
            unrealized_pnl: dec("1872.15"),
            unrealized_pnl_pct: dec("19.42"),
            funding_paid: dec("-12.40"),
            opened_at: rfc3339(now - 7_400),
        },
        Position {
            symbol: "ETH-PERP".into(),
            side: PositionSide::Short,
            qty: dec("40.0000"),
            entry_price: dec("3180.40"),
            mark_price: dec("3242.90"),
            liquidation_price: dec("3318.00"),
            notional: dec("129716.00"),
            margin_mode: MarginMode::Isolated,
            leverage: dec("20"),
            margin: dec("6360.80"),
            unrealized_pnl: dec("-2500.00"),
            unrealized_pnl_pct: dec("-39.31"),
            funding_paid: dec("18.90"),
            opened_at: rfc3339(now - 21_000),
        },
        Position {
            symbol: "SOL-PERP".into(),
            side: PositionSide::Long,
            qty: dec("820.0000"),
            entry_price: dec("148.20"),
            mark_price: dec("141.05"),
            liquidation_price: dec("140.10"),
            notional: dec("115661.00"),
            margin_mode: MarginMode::Isolated,
            leverage: dec("25"),
            margin: dec("4861.44"),
            unrealized_pnl: dec("-5863.00"),
            unrealized_pnl_pct: dec("-96.40"),
            funding_paid: dec("-54.60"),
            opened_at: rfc3339(now - 40_000),
        },
    ]
}

/// The five orders the reference's blotter shows, covering pending, open,
/// partially filled, filled and rejected in one table.
#[must_use]
pub fn orders(now: i64) -> Vec<Order> {
    vec![
        Order {
            order_id: "0x4c1e88a0b1".into(),
            client_order_id: Some("cli_a1b2c3".into()),
            symbol: "BTC-PERP".into(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            price: Some(dec("64000.00")),
            qty: dec("1.0000"),
            filled_qty: dec("0.2500"),
            remaining_qty: dec("0.7500"),
            avg_fill_price: Some(dec("64000.00")),
            status: OrderStatus::PartiallyFilled,
            tif: TimeInForce::Gtc,
            post_only: true,
            reduce_only: false,
            created_at: rfc3339(now - 2_228),
            updated_at: rfc3339(now - 40),
            tx_hash: Some("0x7f2ad2b1c49c04".into()),
            reject_reason: None,
        },
        Order {
            order_id: "0x31a0f7c210".into(),
            client_order_id: Some("cli_d4e5f6".into()),
            symbol: "ETH-PERP".into(),
            side: Side::Sell,
            order_type: OrderType::Limit,
            price: Some(dec("3244.80")),
            qty: dec("12.0000"),
            filled_qty: dec("0.0000"),
            remaining_qty: dec("12.0000"),
            avg_fill_price: None,
            status: OrderStatus::Pending,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            created_at: rfc3339(now - 7),
            updated_at: rfc3339(now - 7),
            tx_hash: Some("0x93cc41b78b01de".into()),
            reject_reason: None,
        },
        Order {
            order_id: "0x9b12ee4402".into(),
            client_order_id: None,
            symbol: "AAPL-USD".into(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            price: Some(dec("228.40")),
            qty: dec("200.0000"),
            filled_qty: dec("120.0000"),
            remaining_qty: dec("80.0000"),
            avg_fill_price: Some(dec("228.36")),
            status: OrderStatus::PartiallyFilled,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            created_at: rfc3339(now - 2_596),
            updated_at: rfc3339(now - 300),
            tx_hash: Some("0xc41f0982a038a7".into()),
            reject_reason: None,
        },
        Order {
            order_id: "0x55b1f020e7".into(),
            client_order_id: None,
            symbol: "SOL-PERP".into(),
            side: Side::Sell,
            order_type: OrderType::Market,
            price: None,
            qty: dec("120.0000"),
            filled_qty: dec("120.0000"),
            remaining_qty: dec("0.0000"),
            avg_fill_price: Some(dec("141.05")),
            status: OrderStatus::Filled,
            tif: TimeInForce::Ioc,
            post_only: false,
            reduce_only: true,
            created_at: rfc3339(now - 3_599),
            updated_at: rfc3339(now - 3_599),
            tx_hash: Some("0x7f2ad2b1c49c04".into()),
            reject_reason: None,
        },
        Order {
            order_id: "0x2de50fb6c0".into(),
            client_order_id: None,
            symbol: "TSLA-PERP".into(),
            side: Side::Buy,
            order_type: OrderType::Limit,
            price: Some(dec("246.00")),
            qty: dec("80.0000"),
            filled_qty: dec("0.0000"),
            remaining_qty: dec("80.0000"),
            avg_fill_price: None,
            status: OrderStatus::Rejected,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            created_at: rfc3339(now - 3_737),
            updated_at: rfc3339(now - 3_737),
            tx_hash: None,
            reject_reason: Some("Market halted".into()),
        },
    ]
}

/// Builds a fills history long enough to need virtualising.
#[must_use]
pub fn fills(count: u32, height: u64, now: i64) -> Vec<Fill> {
    let symbols = [
        ("BTC-PERP", "64250.50", "0.0100"),
        ("ETH-PERP", "3242.90", "0.1000"),
        ("SOL-PERP", "141.05", "1.0000"),
        ("AAPL-USD", "230.14", "1.0000"),
    ];
    let mut rng = Rng::new(0xF111_5EED);
    (0..count)
        .map(|index| {
            let (symbol, price, lot) = symbols[(index as usize) % symbols.len()];
            let price = dec(price) + Decimal::from(rng.centred(40)) / Decimal::from(100);
            let qty = dec(lot) * Decimal::from(1 + rng.below(60));
            let side = if rng.below(2) == 0 {
                Side::Buy
            } else {
                Side::Sell
            };
            let role = if rng.below(3) == 0 {
                FillRole::Maker
            } else {
                FillRole::Taker
            };
            let fee = (price * qty * dec("0.0004")).round_dp(2);
            Fill {
                fill_id: format!("fil_{index:05}"),
                order_id: format!("0x{:010x}", 0x4c1e_88a0_u64 + u64::from(index)),
                symbol: symbol.into(),
                side,
                price: price.round_dp(2),
                qty: qty.round_dp(4),
                fee: if role == FillRole::Maker { -fee } else { fee },
                fee_asset: "USD".into(),
                role,
                realized_pnl: Decimal::from(rng.centred(900)) / Decimal::from(10),
                at: rfc3339(now - i64::from(index) * 47),
                height: height.saturating_sub(u64::from(index) * 3),
            }
        })
        .collect()
}

/// The six ledger lines the reference prints, followed by a generated history
/// long enough that the ledger table has to virtualise.
#[must_use]
pub fn ledger(count: u32, height: u64, now: i64) -> Vec<LedgerEntry> {
    let head = [
        (
            LedgerKind::RealizedPnl,
            "SOL-PERP close 120.0000",
            "624.05",
            "94000.00",
            "0x7f2ad2b1c49c04",
            20,
            0_i64,
        ),
        (
            LedgerKind::Fee,
            "SOL-PERP taker 0.04%",
            "-6.77",
            "93375.95",
            "0x7f2ad2b1c49c04",
            20,
            0,
        ),
        (
            LedgerKind::Funding,
            "BTC-PERP hourly",
            "-12.40",
            "93382.72",
            "0x1b8e772c9944af",
            230,
            2_480,
        ),
        (
            LedgerKind::Settlement,
            "ETH-260626 expired at 3,402.15",
            "2661.00",
            "93395.12",
            "0x93cc41b78b01de",
            1_110,
            16_880,
        ),
        (
            LedgerKind::Liquidation,
            "NVDA-PERP forced close",
            "-1402.80",
            "90734.12",
            "0x5ad0e91b7c772b",
            3_590,
            23_320,
        ),
        (
            LedgerKind::Faucet,
            "Hourly grant",
            "100000.00",
            "92136.92",
            "0xc41f0982a038a7",
            25_360,
            80_240,
        ),
    ];

    let mut entries: Vec<LedgerEntry> = head
        .iter()
        .enumerate()
        .map(
            |(index, (kind, memo, amount, after, tx, block_back, seconds_back))| LedgerEntry {
                id: format!("led_{index:05}"),
                kind: *kind,
                asset: "USD".into(),
                amount: dec(amount),
                balance_after: dec(after),
                memo: (*memo).to_string(),
                at: rfc3339(now - seconds_back),
                tx_hash: (*tx).to_string(),
                height: height.saturating_sub(*block_back),
            },
        )
        .collect();

    let mut rng = Rng::new(0x01ED_6E12);
    let kinds = [
        LedgerKind::Fee,
        LedgerKind::Funding,
        LedgerKind::RealizedPnl,
        LedgerKind::TransferOut,
        LedgerKind::TransferIn,
    ];
    let mut balance = dec("92136.92");
    for index in entries.len()..count as usize {
        let kind = kinds[index % kinds.len()];
        let amount = Decimal::from(rng.centred(4_200)) / Decimal::from(100);
        balance -= amount;
        entries.push(LedgerEntry {
            id: format!("led_{index:05}"),
            kind,
            asset: "USD".into(),
            amount,
            balance_after: balance.round_dp(2),
            memo: match kind {
                LedgerKind::Funding => "BTC-PERP hourly".into(),
                LedgerKind::Fee => "ETH-PERP maker rebate".into(),
                LedgerKind::TransferIn => "From 0x77ab8e021d40".into(),
                LedgerKind::TransferOut => "To 0x0b6142c39a7e".into(),
                _ => "AAPL-USD close".into(),
            },
            at: rfc3339(now - 80_240 - (index as i64) * 431),
            tx_hash: format!("0x{:012x}", 0x8f10_4b63_0000_u64 + index as u64 * 7),
            height: height.saturating_sub(25_360 + index as u64 * 31),
        });
    }
    entries
}

// ------------------------------------------------------------------- faucet

/// The faucet grant the reference shows.
#[must_use]
pub fn faucet_grant() -> Vec<Grant> {
    vec![
        Grant {
            asset: "USD".into(),
            amount: dec("100000.00"),
        },
        Grant {
            asset: "BTC".into(),
            amount: dec("1.00000000"),
        },
        Grant {
            asset: "ETH".into(),
            amount: dec("10.00000000"),
        },
    ]
}

/// The three grant claims the reference's history shows, newest first.
#[must_use]
pub fn faucet_history(height: u64, now: i64) -> Vec<LedgerEntry> {
    vec![
        ("clm_1f2e", "0x1b8e772c9944af", 23, 2_476_i64),
        ("clm_0c71", "0xc41f0982a038a7", 3_183, 80_248),
        ("clm_04a9", "0x2de50fb6c00000", 4_199, 96_101),
    ]
    .into_iter()
    .map(|(id, tx, back, seconds)| LedgerEntry {
        id: id.into(),
        kind: LedgerKind::Faucet,
        asset: "USD".into(),
        amount: dec("100000.00"),
        balance_after: dec("100000.00"),
        memo: "faucet claim".into(),
        at: rfc3339(now - seconds),
        tx_hash: tx.into(),
        height: height.saturating_sub(back),
    })
    .collect()
}

// -------------------------------------------------------------------- chain

/// The chain statistics the reference's explorer shows.
#[must_use]
pub fn chain_status(height: u64, now: i64) -> ChainStatus {
    let _ = now;
    ChainStatus {
        chain_id: 42,
        height,
        state_root: "0x4c1e8f2b7a9ba2".into(),
        block_time_ms: 100,
        tps_1m: dec("1240.5"),
        tx_count: 9_128_401,
        mempool_depth: 12,
        validators: 3,
        synced: true,
        uptime_seconds: 84_021,
        node_rpc: "127.0.0.1:7700".into(),
        gateway_listen: "127.0.0.1:8081".into(),
        ws_path: "/ws".into(),
        oracle_control: Some("http://127.0.0.1:8171".into()),
        sim_mode: SimMode::On,
    }
}

/// The kinds of transaction the fixture chain carries, in a cycle that puts
/// an order in most blocks and a funding tick in a few.
const TX_KINDS: [(&str, Option<&str>, Option<&str>); 8] = [
    ("PlaceOrder", Some("BTC-USD"), Some("buy")),
    ("PlaceOrder", Some("BTC-USD"), Some("sell")),
    ("Transfer", None, None),
    ("PlacePerpOrder", Some("BTC-PERP"), Some("buy")),
    ("OracleUpdate", Some("ETH-PERP"), None),
    ("CancelOrder", None, None),
    ("PlaceOrder", Some("ETH-USD"), Some("sell")),
    ("FundingTick", Some("BTC-PERP"), None),
];

/// Builds `limit` recent transactions, newest first, ending in block `height`.
#[must_use]
pub fn txs(height: u64, limit: u32, now_millis: i64) -> Vec<ChainTx> {
    (0..limit)
        .map(|index| {
            let h = height.saturating_sub(u64::from(index / 3));
            let (kind, market, side) = TX_KINDS[(index as usize) % TX_KINDS.len()];
            ChainTx {
                hash: format!(
                    "0x{:016x}",
                    0x7a1d_00c0_ffee_0000_u64 ^ (u64::from(index).wrapping_mul(0x9E37_79B9))
                ),
                height: h,
                index: index % 3,
                sender: format!("0x{:040x}", 0x8f3b_u64 + u64::from(index % 5)),
                kind: kind.into(),
                market: market.map(str::to_string),
                side: side.map(str::to_string),
                at: rfc3339((now_millis - i64::from(index / 3) * 100).div_euclid(1_000)),
            }
        })
        .collect()
}

/// The transaction counts the reference's twenty-four block chips carry.
pub const BLOCK_TX_COUNTS: [u32; 24] = [
    30, 52, 44, 68, 38, 81, 59, 47, 90, 62, 35, 71, 44, 57, 66, 29, 83, 51, 40, 75, 61, 48, 55, 74,
];

/// Builds `limit` recent blocks, newest first, ending at `height`.
#[must_use]
pub fn blocks(height: u64, limit: u32, now_millis: i64) -> Vec<Block> {
    (0..limit)
        .map(|index| {
            let h = height.saturating_sub(u64::from(index));
            let tx = BLOCK_TX_COUNTS[(h as usize) % BLOCK_TX_COUNTS.len()];
            Block {
                height: h,
                hash: format!(
                    "0x{:012x}",
                    0x4c1e_9ba2_0000_u64 ^ (h.wrapping_mul(0x9E37_79B9))
                ),
                state_root: format!(
                    "0x{:012x}",
                    0x91d0_ac74_0000_u64 ^ (h.wrapping_mul(0x85EB_CA6B))
                ),
                tx_count: tx,
                at: rfc3339((now_millis - i64::from(index) * 100).div_euclid(1_000)),
                proposer: "0xseq000000000001".into(),
            }
        })
        .collect()
}

// -------------------------------------------------------------- leaderboard

/// The leaderboard rows the reference prints, in rank order.
#[must_use]
pub fn leaderboard(handle: &str, limit: u32) -> Vec<LeaderboardEntry> {
    let head: Vec<LeaderboardEntry> = vec![
        (
            "satoshi",
            "0x8f3bc4d19ac21d",
            "184250.00",
            "84250.00",
            "84.25",
            "2840000.00",
            412,
            None,
        ),
        (
            "mm_alpha",
            "0x11c4a920b18e02",
            "162908.40",
            "62908.40",
            "62.91",
            "18402900.00",
            94_120,
            Some("market maker"),
        ),
        (
            "hkclock",
            "0x77ab8e021d40aa",
            "141220.10",
            "41220.10",
            "41.22",
            "1204800.00",
            288,
            None,
        ),
        (
            "momentum_3",
            "0x2f9077a15c1100",
            "128004.80",
            "28004.80",
            "28.00",
            "6920400.00",
            21_880,
            Some("momentum"),
        ),
        (
            "tram_cat",
            "0x0b6142c39a7e00",
            "118440.00",
            "18440.00",
            "18.44",
            "884100.00",
            201,
            None,
        ),
        (
            "noise_17",
            "0x6cd2b8f3330100",
            "104880.20",
            "4880.20",
            "4.88",
            "2104000.00",
            40_912,
            Some("noise"),
        ),
        (
            "lantern",
            "0x9e14aa02bb7200",
            "99120.00",
            "-880.00",
            "-0.88",
            "402900.00",
            88,
            None,
        ),
        (
            "liq_hunter_1",
            "0x4a0833c16f9500",
            "91402.00",
            "-8598.00",
            "-8.60",
            "3880200.00",
            12_004,
            Some("liquidation hunter"),
        ),
        (
            "gloucester",
            "0xd3f7b1c2240800",
            "72040.50",
            "-27959.50",
            "-27.96",
            "1940000.00",
            604,
            None,
        ),
        (
            "paperhand",
            "0x58bcd4a10e1900",
            "18220.00",
            "-81780.00",
            "-81.78",
            "5102400.00",
            1_880,
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    .map(
        |(index, (name, address, equity, pnl, pnl_pct, volume, trades, strategy))| {
            LeaderboardEntry {
                rank: index as u32 + 1,
                handle: if index == 0 {
                    handle.to_string()
                } else {
                    name.to_string()
                },
                address: address.into(),
                equity: dec(equity),
                pnl: dec(pnl),
                pnl_pct: dec(pnl_pct),
                volume: dec(volume),
                trades,
                is_bot: strategy.is_some(),
                bot_strategy: strategy.map(str::to_string),
            }
        },
    )
    .collect();

    let mut entries = head;
    let mut rng = Rng::new(0x1EAD_B0A2);
    let names = [
        "wanchai",
        "quarry_bay",
        "tin_hau",
        "jardine",
        "sogo",
        "hysan",
        "victoria",
        "kennedy",
        "shaukeiwan",
        "fortress",
    ];
    let strategies = ["market maker", "momentum", "noise", "liquidation hunter"];
    for rank in entries.len()..limit as usize {
        let is_bot = rng.below(10) < 4;
        let pnl = Decimal::from(-(rank as i64) * 830 - rng.centred(4_000));
        entries.push(LeaderboardEntry {
            rank: rank as u32 + 1,
            handle: format!("{}_{rank}", names[rank % names.len()]),
            address: pseudo_address(0x0058_bc0e_1977_u64 ^ (rank as u64).wrapping_mul(0x85EB_CA6B)),
            equity: (dec("100000.00") + pnl).max(Decimal::ZERO),
            pnl,
            pnl_pct: (pnl / Decimal::from(1_000)).round_dp(2),
            volume: Decimal::from(200_000 + rng.below(4_000_000)),
            trades: 40 + rng.below(9_000),
            is_bot,
            bot_strategy: is_bot.then(|| strategies[rank % strategies.len()].to_string()),
        });
    }
    entries
}

// ---------------------------------------------------- user page and admin

/// The profile shown on a user's own account page.
#[must_use]
pub fn profile(user_id: &str, handle: &str, address: &str, roles: &[String], now: i64) -> Profile {
    Profile {
        user_id: user_id.into(),
        handle: handle.into(),
        address: address.into(),
        joined_at: rfc3339(now - 1_480_000),
        last_seen_at: rfc3339(now - 79),
        is_bot: false,
        roles: roles.to_vec(),
        stats: ProfileStats {
            trades: 412,
            volume_30d: dec("2840000.00"),
            realized_pnl_30d: dec("8420.00"),
            markets_traded: 7,
            liquidations: 2,
            faucet_claims: 3,
            best_rank: Some(4),
        },
    }
}

/// The live sessions on an account, with the current one first.
#[must_use]
pub fn sessions(current_token: &str, now: i64) -> Vec<UserSession> {
    vec![
        UserSession {
            session_id: format!(
                "ses_{}",
                current_token.chars().rev().take(4).collect::<String>()
            ),
            issued_at: rfc3339(now - 600),
            expires_at: rfc3339(now + 86_400),
            last_used_at: rfc3339(now),
            user_agent: "This browser".into(),
            ip_prefix: "203.0.113.0/24".into(),
            current: true,
        },
        UserSession {
            session_id: "ses_9c04".into(),
            issued_at: rfc3339(now - 92_000),
            expires_at: rfc3339(now + 4_000),
            last_used_at: rfc3339(now - 4_100),
            user_agent: "cb-client 0.1.0 on macOS".into(),
            ip_prefix: "203.0.113.0/24".into(),
            current: false,
        },
        UserSession {
            session_id: "ses_44af".into(),
            issued_at: rfc3339(now - 180_000),
            expires_at: rfc3339(now + 1_200),
            last_used_at: rfc3339(now - 96_000),
            user_agent: "Mobile Safari on iOS 18".into(),
            ip_prefix: "198.51.100.0/24".into(),
            current: false,
        },
    ]
}

/// One printed activity row: kind, summary, symbol, amount, block-offset, age.
type ActivityRow = (
    ActivityKind,
    &'static str,
    Option<&'static str>,
    &'static str,
    u64,
    i64,
);

/// The unified activity feed: what a person actually wants to read.
#[must_use]
pub fn activity(count: u32, height: u64, now: i64) -> Vec<ActivityEntry> {
    let head: Vec<ActivityRow> = vec![
        (
            ActivityKind::Fill,
            "Bought 0.2500 BTC-PERP at 64,250.50",
            Some("BTC-PERP"),
            "-16062.63",
            0,
            1,
        ),
        (
            ActivityKind::Order,
            "Placed a post-only buy of 1.0000 BTC-PERP at 64,000.00",
            Some("BTC-PERP"),
            "0.00",
            2,
            40,
        ),
        (
            ActivityKind::Liquidation,
            "NVDA-PERP was closed out at 121.36",
            Some("NVDA-PERP"),
            "-1402.80",
            3_590,
            23_320,
        ),
        (
            ActivityKind::Funding,
            "Paid funding on BTC-PERP",
            Some("BTC-PERP"),
            "-12.40",
            230,
            2_480,
        ),
        (
            ActivityKind::Settlement,
            "ETH-260626 settled at 3,402.15",
            Some("ETH-260626"),
            "2661.00",
            1_110,
            16_880,
        ),
        (
            ActivityKind::Faucet,
            "Claimed an hourly grant",
            None,
            "100000.00",
            25_360,
            80_240,
        ),
        (
            ActivityKind::TransferOut,
            "Sent 100.00 USD to 0x77ab…1d40",
            None,
            "-100.00",
            26_000,
            84_000,
        ),
    ];
    let mut entries: Vec<ActivityEntry> = head
        .into_iter()
        .enumerate()
        .map(
            |(index, (kind, summary, symbol, amount, back, seconds))| ActivityEntry {
                id: format!("act_{index:05}"),
                kind,
                at: rfc3339(now - seconds),
                summary: summary.into(),
                symbol: symbol.map(str::to_string),
                amount: Some(dec(amount)),
                asset: Some("USD".into()),
                tx_hash: Some(format!(
                    "0x{:012x}",
                    0x7f2a_9c04_0000_u64 + index as u64 * 13
                )),
                height: height.saturating_sub(back),
            },
        )
        .collect();

    let mut rng = Rng::new(0x00AC_7117);
    let markets = ["BTC-PERP", "ETH-PERP", "SOL-PERP", "AAPL-USD"];
    for index in entries.len()..count as usize {
        let symbol = markets[index % markets.len()];
        let qty = Decimal::from(1 + rng.below(40)) / Decimal::from(10);
        let amount = Decimal::from(rng.centred(600_000)) / Decimal::from(100);
        entries.push(ActivityEntry {
            id: format!("act_{index:05}"),
            kind: if index % 5 == 0 {
                ActivityKind::Funding
            } else {
                ActivityKind::Fill
            },
            at: rfc3339(now - 84_000 - (index as i64) * 389),
            summary: if index % 5 == 0 {
                format!("Received funding on {symbol}")
            } else if index % 2 == 0 {
                format!("Bought {qty} {symbol}")
            } else {
                format!("Sold {qty} {symbol}")
            },
            symbol: Some(symbol.into()),
            amount: Some(amount.round_dp(2)),
            asset: Some("USD".into()),
            tx_hash: Some(format!(
                "0x{:012x}",
                0x2de5_b6c0_0000_u64 + index as u64 * 29
            )),
            height: height.saturating_sub(26_000 + index as u64 * 37),
        });
    }
    entries
}

/// The admin dashboard's single fetch.
#[must_use]
pub fn admin_overview(height: u64, markets: &[Market]) -> AdminOverview {
    let halted = markets
        .iter()
        .filter(|m| m.status == MarketStatus::Halted)
        .count() as u32;
    let expired = markets
        .iter()
        .filter(|m| matches!(m.status, MarketStatus::Expired | MarketStatus::Settling))
        .count() as u32;
    AdminOverview {
        chain: AdminChain {
            height,
            block_time_ms: 100,
            tps_1m: dec("1240.5"),
            mempool_depth: 12,
            validators: 3,
            synced: true,
            state_root: "0x4c1e8f2b7a9ba2".into(),
            uptime_seconds: 84_021,
        },
        users: AdminUsers {
            total: 1_284,
            humans: 412,
            bots: 872,
            active_24h: 318,
            new_24h: 41,
        },
        markets: AdminMarkets {
            total: markets.len() as u32,
            trading: markets.len() as u32 - halted - expired,
            halted,
            expired,
        },
        volume: AdminVolume {
            volume_24h: dec("128450000.00"),
            trades_24h: 84_021,
            fees_24h: dec("51380.00"),
        },
        faucet: AdminFaucetSummary {
            claims_24h: 96,
            granted_24h: vec![Grant {
                asset: "USD".into(),
                amount: dec("9600000.00"),
            }],
            total_granted: vec![Grant {
                asset: "USD".into(),
                amount: dec("412000000.00"),
            }],
        },
        risk: AdminRisk {
            open_interest: dec("48200000.00"),
            insurance_fund: dec("1000000.00"),
            accounts_below_maintenance: 3,
            liquidations_24h: 27,
        },
        health: vec![
            ComponentHealth {
                component: "node".into(),
                status: HealthStatus::Ok,
                detail: Some(format!("height {height}")),
            },
            ComponentHealth {
                component: "database".into(),
                status: HealthStatus::Ok,
                detail: Some("schema 1".into()),
            },
            ComponentHealth {
                component: "bus".into(),
                status: HealthStatus::Ok,
                detail: None,
            },
            ComponentHealth {
                component: "feed".into(),
                status: HealthStatus::Degraded,
                detail: Some("SPY stale 42s".into()),
            },
        ],
    }
}

/// The admin user list.
#[must_use]
pub fn admin_users(count: u32, now: i64) -> Vec<AdminUser> {
    let mut rng = Rng::new(0x0AD1_15E5);
    let names = [
        "satoshi",
        "mm_alpha",
        "hkclock",
        "momentum_3",
        "tram_cat",
        "noise_17",
        "lantern",
        "liq_hunter_1",
        "gloucester",
        "paperhand",
        "wanchai",
        "quarry_bay",
    ];
    (0..count as usize)
        .map(|index| {
            let is_bot = matches!(index, 1 | 3 | 5 | 7) || (index > 11 && rng.below(10) < 6);
            AdminUser {
                user_id: format!("usr_{index:06x}"),
                handle: names
                    .get(index)
                    .map_or_else(|| format!("trader_{index}"), |n| (*n).to_string()),
                address: pseudo_address(
                    0x008f_3bc2_1da0_u64 ^ (index as u64).wrapping_mul(0x9E37_79B9),
                ),
                is_bot,
                joined_at: rfc3339(now - 1_480_000 + index as i64 * 3_600),
                last_seen_at: rfc3339(now - (rng.below(9_000) as i64)),
                equity: Decimal::from(20_000 + rng.below(180_000)),
                open_positions: rng.below(6) as u32,
                trades_24h: rng.below(4_000),
                faucet_claims: rng.below(9) as u32,
                status: if index == 9 {
                    AccountStatus::Suspended
                } else {
                    AccountStatus::Active
                },
            }
        })
        .collect()
}

/// Every market with its live operational state.
#[must_use]
pub fn admin_markets(markets: &[Market], last: &dyn Fn(&str) -> Decimal) -> Vec<AdminMarket> {
    let mut rng = Rng::new(0x0AD4_A2E7);
    markets
        .iter()
        .map(|market| AdminMarket {
            symbol: market.symbol.clone(),
            status: market.status,
            last_price: last(&market.symbol),
            open_interest: (last(&market.symbol) * dec("0.02")).round_dp(market.size_decimals),
            mark_price: last(&market.symbol),
            oracle_age_blocks: if market.status.is_tradable() {
                Some(rng.below(4))
            } else {
                Some(42 + rng.below(600))
            },
            oracle_fresh: market.status.is_tradable(),
            resting_levels: 12 + rng.below(900),
            overridden: false,
            max_leverage: market.max_leverage,
            initial_margin_bps: market.initial_margin_bps,
            maintenance_margin_bps: market.maintenance_margin_bps,
        })
        .collect()
}

/// The operator audit trail, newest first.
#[must_use]
pub fn audit(now: i64) -> Vec<AuditRecord> {
    let rows: Vec<(&str, &str, &str, &str, i64)> = vec![
        (
            "market.halt",
            "TSLA-PERP",
            "reason",
            "oracle feed stale",
            160,
        ),
        ("faucet.update", "faucet", "cooldown_seconds", "3600", 7_400),
        (
            "user.suspend",
            "usr_000009",
            "reason",
            "abusive order flow",
            22_000,
        ),
        (
            "market.settle",
            "ETH-260626",
            "settlement_price",
            "3402.15",
            16_880,
        ),
        ("sim.scenario", "crash", "seed", "42", 44_000),
        (
            "market.risk",
            "SOL-PERP",
            "maintenance_margin_bps",
            "250",
            90_000,
        ),
    ];
    rows.into_iter()
        .enumerate()
        .map(|(index, (action, subject, key, value, back))| AuditRecord {
            record_id: format!("aud_{index:05}"),
            actor: "0x9858EfFD232B4033E47d90003D41EC34EcaEda94".into(),
            action: action.into(),
            subject: Some(subject.into()),
            detail: [(key.to_string(), value.to_string())].into_iter().collect(),
            at: rfc3339(now - back),
        })
        .collect()
}

/// The simulator's starting state.
#[must_use]
pub fn sim_status() -> SimStatus {
    SimStatus {
        running: true,
        mode: SimMode::On,
        scenario: Scenario::Calm,
        seed: 42,
        bots: [
            ("market_maker", 8),
            ("momentum", 16),
            ("noise", 32),
            ("whale", 2),
            ("liquidator", 4),
            ("arbitrageur", 4),
        ]
        .into_iter()
        .map(|(name, n)| (name.to_string(), n))
        .collect(),
        elapsed_seconds: 1_840,
        orders_submitted: 184_020,
        invariant_violations: 0,
    }
}

/// The faucet as an operator configures it.
#[must_use]
pub fn admin_faucet(enabled: bool, cooldown_seconds: i64, grant: &[Grant]) -> AdminFaucet {
    AdminFaucet {
        enabled,
        cooldown_seconds,
        grant: grant.to_vec(),
        max_claims_per_user: None,
        claims_24h: 96,
        granted_total: vec![Grant {
            asset: "USD".into(),
            amount: dec("412000000.00"),
        }],
        authority_balance: vec![Grant {
            asset: "USD".into(),
            amount: dec("9588000000.00"),
        }],
    }
}
