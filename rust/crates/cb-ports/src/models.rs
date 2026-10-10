//! Application-domain records.
//!
//! These are the rows the exchange keeps *off* chain: who joined, what they
//! were granted, what the candles looked like. Nothing here is consensus
//! state — the chain is authoritative for balances, orders and positions, and
//! these records are derived views, audit trails and user-account plumbing.
//!
//! Money is [`rust_decimal::Decimal`] throughout. Binary floating point has no
//! place in a ledger, and the on-chain fixed-point integers are the gateway's
//! concern, not the database's.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A registered player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Stable internal id.
    pub user_id: String,
    /// Display handle, unique case-insensitively.
    pub handle: String,
    /// On-chain address the gateway controls for this user.
    pub address: String,
    /// Argon2 hash of the recovery code. Never the code itself.
    pub recovery_hash: String,
    /// Whether this account is driven by a simulation bot.
    pub is_bot: bool,
    /// When the account was created.
    pub created_at: DateTime<Utc>,
    /// Last time a session was issued or used.
    pub last_seen_at: DateTime<Utc>,
}

/// An issued session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Hash of the bearer token. The plaintext token exists only in transit.
    pub token_hash: String,
    /// Subject.
    pub user_id: String,
    /// When issued.
    pub created_at: DateTime<Utc>,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
    /// Whether it has been explicitly revoked before expiry.
    pub revoked: bool,
}

/// One faucet grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaucetClaim {
    /// Stable claim id.
    pub claim_id: String,
    /// Who claimed.
    pub user_id: String,
    /// Asset granted.
    pub asset: String,
    /// Amount granted.
    pub amount: Decimal,
    /// Hash of the on-chain transaction that credited the account.
    pub tx_hash: String,
    /// When granted.
    pub claimed_at: DateTime<Utc>,
}

/// One OHLCV bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    /// Market symbol.
    pub symbol: String,
    /// Bucket width, such as `1m`.
    pub interval: String,
    /// Bucket start, aligned to the interval.
    pub open_time: DateTime<Utc>,
    /// First trade price in the bucket.
    pub open: Decimal,
    /// Highest trade price.
    pub high: Decimal,
    /// Lowest trade price.
    pub low: Decimal,
    /// Last trade price.
    pub close: Decimal,
    /// Base-asset volume traded.
    pub volume: Decimal,
    /// Number of trades.
    pub trade_count: u64,
}

/// A movement in a user's off-chain ledger view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Stable entry id.
    pub entry_id: String,
    /// Whose ledger.
    pub user_id: String,
    /// What kind of movement: `faucet`, `fee`, `funding`, `realized_pnl`,
    /// `liquidation`, `settlement`, `transfer_in`, `transfer_out`.
    pub kind: String,
    /// Asset moved.
    pub asset: String,
    /// Signed amount. Negative means the user paid.
    pub amount: Decimal,
    /// Free balance after the movement, as the gateway observed it.
    pub balance_after: Decimal,
    /// Free-text note.
    pub memo: Option<String>,
    /// On-chain transaction, when one caused this.
    pub tx_hash: Option<String>,
    /// Block height, when known.
    pub height: Option<u64>,
    /// When recorded.
    pub at: DateTime<Utc>,
}

/// One row of the leaderboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    /// Position, 1-based.
    pub rank: u32,
    /// Player handle.
    pub handle: String,
    /// Player address.
    pub address: String,
    /// Account equity at evaluation time.
    pub equity: Decimal,
    /// Profit or loss over the window.
    pub pnl: Decimal,
    /// Profit or loss as a percentage of starting equity.
    pub pnl_pct: Decimal,
    /// Notional traded over the window.
    pub volume: Decimal,
    /// Trades executed over the window.
    pub trades: u64,
    /// Whether a bot holds this position.
    pub is_bot: bool,
}

/// Cached metadata describing a tradable market.
///
/// The chain defines which markets exist. This record carries the display and
/// validation facts a frontend needs without a chain round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketMeta {
    /// Market symbol, such as `BTC-PERP`.
    pub symbol: String,
    /// `spot`, `perp` or `future`.
    pub kind: String,
    /// `crypto` or `equity`.
    pub asset_class: String,
    /// Base asset.
    pub base: String,
    /// Quote asset.
    pub quote: String,
    /// Human-facing name.
    pub display_name: String,
    /// `trading`, `halted`, `expired` or `settling`.
    pub status: String,
    /// Minimum price increment.
    pub tick_size: Decimal,
    /// Minimum quantity increment.
    pub lot_size: Decimal,
    /// Smallest permitted order notional.
    pub min_notional: Decimal,
    /// Decimal places to display for price.
    pub price_decimals: u32,
    /// Decimal places to display for size.
    pub size_decimals: u32,
    /// Maximum leverage. `None` for spot.
    pub max_leverage: Option<Decimal>,
    /// Initial margin requirement in basis points. `None` for spot.
    pub initial_margin_bps: Option<u32>,
    /// Maintenance margin requirement in basis points. `None` for spot.
    pub maintenance_margin_bps: Option<u32>,
    /// Seconds between funding payments. `None` for spot and futures.
    pub funding_interval_seconds: Option<u64>,
    /// Expiry instant. `Some` only for dated futures.
    pub expiry: Option<DateTime<Utc>>,
    /// Maker fee in basis points; negative means a rebate.
    pub maker_fee_bps: i32,
    /// Taker fee in basis points.
    pub taker_fee_bps: i32,
}

/// An append-only record of something worth being able to prove later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// Stable record id.
    pub record_id: String,
    /// Who acted, when known. `None` for system actions.
    pub actor: Option<String>,
    /// What happened, such as `order.place` or `faucet.claim`.
    pub action: String,
    /// What it happened to.
    pub subject: String,
    /// Structured context, serialised as JSON.
    pub detail: serde_json::Value,
    /// Client address, when the action arrived over the network.
    pub source_ip: Option<String>,
    /// When it happened.
    pub at: DateTime<Utc>,
}

/// Window over which leaderboard statistics are computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaderboardWindow {
    /// Trailing 24 hours.
    Day,
    /// Trailing 7 days.
    Week,
    /// Since genesis.
    AllTime,
}

impl LeaderboardWindow {
    /// Parse the wire representation used by the HTTP API.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "24h" => Some(Self::Day),
            "7d" => Some(Self::Week),
            "all" => Some(Self::AllTime),
            _ => None,
        }
    }

    /// The wire representation used by the HTTP API.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::AllTime => "all",
        }
    }
}

/// Candle bucket widths the exchange aggregates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandleInterval {
    /// One minute.
    M1,
    /// Five minutes.
    M5,
    /// Fifteen minutes.
    M15,
    /// One hour.
    H1,
    /// Four hours.
    H4,
    /// One day.
    D1,
}

impl CandleInterval {
    /// Every supported interval, shortest first.
    pub const ALL: [CandleInterval; 6] =
        [Self::M1, Self::M5, Self::M15, Self::H1, Self::H4, Self::D1];

    /// Parse the wire representation used by the HTTP API.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1m" => Some(Self::M1),
            "5m" => Some(Self::M5),
            "15m" => Some(Self::M15),
            "1h" => Some(Self::H1),
            "4h" => Some(Self::H4),
            "1d" => Some(Self::D1),
            _ => None,
        }
    }

    /// The wire representation used by the HTTP API.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }

    /// Bucket width in seconds.
    pub fn seconds(&self) -> i64 {
        match self {
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::H1 => 3_600,
            Self::H4 => 14_400,
            Self::D1 => 86_400,
        }
    }

    /// The start of the bucket containing `at`.
    ///
    /// Buckets are aligned to the Unix epoch, so every node computes the same
    /// boundaries regardless of when it started.
    pub fn bucket_start(&self, at: DateTime<Utc>) -> DateTime<Utc> {
        let secs = at.timestamp();
        let width = self.seconds();
        let aligned = secs.div_euclid(width) * width;
        DateTime::from_timestamp(aligned, 0).expect("aligned timestamp is in range")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candle_intervals_round_trip() {
        for i in CandleInterval::ALL {
            assert_eq!(CandleInterval::parse(i.as_str()), Some(i));
        }
        assert_eq!(CandleInterval::parse("3m"), None);
    }

    #[test]
    fn leaderboard_windows_round_trip() {
        for w in [
            LeaderboardWindow::Day,
            LeaderboardWindow::Week,
            LeaderboardWindow::AllTime,
        ] {
            assert_eq!(LeaderboardWindow::parse(w.as_str()), Some(w));
        }
        assert_eq!(LeaderboardWindow::parse("1h"), None);
    }

    #[test]
    fn buckets_align_to_the_epoch_not_to_now() {
        let at = DateTime::parse_from_rfc3339("2026-09-18T12:34:56Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            CandleInterval::M1.bucket_start(at).to_rfc3339(),
            "2026-09-18T12:34:00+00:00"
        );
        assert_eq!(
            CandleInterval::M15.bucket_start(at).to_rfc3339(),
            "2026-09-18T12:30:00+00:00"
        );
        assert_eq!(
            CandleInterval::H4.bucket_start(at).to_rfc3339(),
            "2026-09-18T12:00:00+00:00"
        );
        assert_eq!(
            CandleInterval::D1.bucket_start(at).to_rfc3339(),
            "2026-09-18T00:00:00+00:00"
        );
    }

    #[test]
    fn a_timestamp_exactly_on_a_boundary_is_its_own_bucket() {
        let at = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(CandleInterval::H1.bucket_start(at), at);
    }

    #[test]
    fn buckets_align_correctly_before_the_epoch() {
        // div_euclid, not integer division: truncation toward zero would put
        // a pre-epoch instant in the bucket *after* the one containing it.
        let at = DateTime::from_timestamp(-30, 0).unwrap();
        assert_eq!(
            CandleInterval::M1.bucket_start(at),
            DateTime::from_timestamp(-60, 0).unwrap()
        );
    }
}
