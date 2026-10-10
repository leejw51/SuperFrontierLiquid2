//! OHLCV aggregation.
//!
//! Trades arrive one at a time and are folded into a bar per interval. The
//! in-progress bar for each `(symbol, interval)` is held in memory so the
//! WebSocket `candles:` channel can publish it on every trade, and written
//! through [`cb_ports::CandleRepository`] as it changes so a restart resumes
//! from storage rather than from nothing.
//!
//! ## Bucket alignment is not reimplemented
//!
//! `cb_ports::CandleInterval::bucket_start` aligns buckets to the Unix epoch,
//! including — carefully — for pre-epoch instants. Writing that division
//! again here would be a second implementation to keep in agreement, so this
//! module calls it and does not think about it.
//!
//! ## Where the trades come from
//!
//! The node publishes no trade stream, so [`crate::marketdata`] derives trade
//! prints from per-block changes. Prices are exact wherever the chain
//! publishes a last-trade price; quantities are estimated. Volume in these
//! bars inherits that: it is a good estimate, not a settled figure, and
//! nothing financial should be computed from it.

use crate::error::ApiResult;
use cb_ports::{AppDatabase, Candle, CandleInterval, Clock};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;

/// Aggregates trades into bars.
pub struct CandleAggregator {
    db: Arc<dyn AppDatabase>,
    clock: Arc<dyn Clock>,
    /// Keyed by `(symbol, interval-name)`: `CandleInterval` is not `Hash`, and
    /// its wire name is a perfectly good stable key.
    live: Mutex<HashMap<(String, &'static str), Candle>>,
}

impl std::fmt::Debug for CandleAggregator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandleAggregator")
            .field("live_bars", &self.live.lock().len())
            .finish_non_exhaustive()
    }
}

impl CandleAggregator {
    /// Build an aggregator.
    pub fn new(db: Arc<dyn AppDatabase>, clock: Arc<dyn Clock>) -> Self {
        Self {
            db,
            clock,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Fold one trade into every interval, and persist what changed.
    ///
    /// Returns the bars that moved, so the caller can publish them to the
    /// `candles:` channel without asking again.
    pub async fn apply_trade(
        &self,
        symbol: &str,
        price: Decimal,
        qty: Decimal,
        at: DateTime<Utc>,
    ) -> ApiResult<Vec<Candle>> {
        let mut changed = Vec::with_capacity(CandleInterval::ALL.len());
        {
            let mut live = self.live.lock();
            for interval in CandleInterval::ALL {
                let open_time = interval.bucket_start(at);
                let key = (symbol.to_string(), interval.as_str());
                let bar = live.entry(key).or_insert_with(|| Candle {
                    symbol: symbol.to_string(),
                    interval: interval.as_str().to_string(),
                    open_time,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: Decimal::ZERO,
                    trade_count: 0,
                });
                if bar.open_time != open_time {
                    // The bucket rolled. The previous bar was already written
                    // on its last update, so it is safe to replace outright.
                    *bar = Candle {
                        symbol: symbol.to_string(),
                        interval: interval.as_str().to_string(),
                        open_time,
                        open: price,
                        high: price,
                        low: price,
                        close: price,
                        volume: Decimal::ZERO,
                        trade_count: 0,
                    };
                }
                bar.high = bar.high.max(price);
                bar.low = bar.low.min(price);
                bar.close = price;
                bar.volume += qty;
                bar.trade_count += 1;
                changed.push(bar.clone());
            }
        }
        self.db.candles().upsert_many(&changed).await?;
        Ok(changed)
    }

    /// The in-progress bar, if the aggregator has seen a trade this bucket.
    pub fn in_progress(&self, symbol: &str, interval: CandleInterval) -> Option<Candle> {
        let live = self.live.lock();
        let bar = live.get(&(symbol.to_string(), interval.as_str()))?;
        // A bar whose bucket has passed is history, not progress.
        (bar.open_time == interval.bucket_start(self.clock.now())).then(|| bar.clone())
    }

    /// Stored history, oldest first.
    pub async fn history(
        &self,
        symbol: &str,
        interval: CandleInterval,
        limit: u32,
    ) -> ApiResult<Vec<Candle>> {
        Ok(self
            .db
            .candles()
            .range(symbol, interval, None, None, limit)
            .await?)
    }

    /// Drop bars older than the retention each interval is worth keeping.
    ///
    /// A minute bar from a year ago serves nobody and costs a row; a daily bar
    /// from a year ago is the chart. Retention therefore scales with the
    /// bucket width rather than being one number for all six.
    pub async fn prune(&self, buckets_to_keep: i64) -> ApiResult<u64> {
        let now = self.clock.now();
        let mut removed = 0;
        for interval in CandleInterval::ALL {
            let cutoff = now - chrono::Duration::seconds(interval.seconds() * buckets_to_keep);
            removed += self.db.candles().prune(interval, cutoff).await?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryDatabase;
    use cb_ports::FixedClock;
    /// Read a decimal from its text, so the tests state money the way the
    /// wire does.
    fn dec(text: &str) -> Decimal {
        use std::str::FromStr;
        Decimal::from_str(text).expect("decimal literal")
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("static timestamp")
            .with_timezone(&Utc)
    }

    fn aggregator(now: &str) -> (FixedClock, Arc<InMemoryDatabase>, CandleAggregator) {
        let clock = FixedClock::new(at(now));
        let db = InMemoryDatabase::shared();
        let aggregator = CandleAggregator::new(db.clone(), Arc::new(clock.clone()));
        (clock, db, aggregator)
    }

    #[tokio::test]
    async fn one_trade_opens_a_bar_in_every_interval() {
        let (_clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        let changed = aggregator
            .apply_trade(
                "BTC-PERP",
                dec("64250.50"),
                dec("0.25"),
                at("2026-09-18T12:00:30Z"),
            )
            .await
            .expect("applied");
        assert_eq!(changed.len(), CandleInterval::ALL.len());
        let minute = changed
            .iter()
            .find(|c| c.interval == "1m")
            .expect("a minute bar");
        assert_eq!(minute.open_time, at("2026-09-18T12:00:00Z"));
        assert_eq!(minute.open, dec("64250.50"));
        assert_eq!(minute.close, dec("64250.50"));
        assert_eq!(minute.volume, dec("0.25"));
        assert_eq!(minute.trade_count, 1);
    }

    #[tokio::test]
    async fn successive_trades_track_open_high_low_close_and_volume() {
        let (_clock, _db, aggregator) = aggregator("2026-09-18T12:00:59Z");
        for (price, qty, when) in [
            ("64200.00", "1", "2026-09-18T12:00:01Z"),
            ("64280.00", "2", "2026-09-18T12:00:20Z"),
            ("64180.00", "3", "2026-09-18T12:00:40Z"),
            ("64250.50", "4", "2026-09-18T12:00:59Z"),
        ] {
            aggregator
                .apply_trade("BTC-PERP", dec(price), dec(qty), at(when))
                .await
                .expect("applied");
        }
        let bar = aggregator
            .in_progress("BTC-PERP", CandleInterval::M1)
            .expect("in progress");
        assert_eq!(bar.open, dec("64200.00"));
        assert_eq!(bar.high, dec("64280.00"));
        assert_eq!(bar.low, dec("64180.00"));
        assert_eq!(bar.close, dec("64250.50"));
        assert_eq!(bar.volume, dec("10"));
        assert_eq!(bar.trade_count, 4);
    }

    #[tokio::test]
    async fn crossing_a_bucket_boundary_starts_a_new_bar_and_keeps_the_old_one() {
        let (clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        aggregator
            .apply_trade("BTC-PERP", dec("100"), dec("1"), at("2026-09-18T12:00:30Z"))
            .await
            .expect("applied");
        clock.set(at("2026-09-18T12:01:30Z"));
        aggregator
            .apply_trade("BTC-PERP", dec("110"), dec("2"), at("2026-09-18T12:01:30Z"))
            .await
            .expect("applied");

        let history = aggregator
            .history("BTC-PERP", CandleInterval::M1, 10)
            .await
            .expect("history");
        assert_eq!(history.len(), 2, "the closed bar survives");
        assert_eq!(history[0].open_time, at("2026-09-18T12:00:00Z"));
        assert_eq!(history[0].close, dec("100"));
        assert_eq!(history[1].open_time, at("2026-09-18T12:01:00Z"));
        assert_eq!(history[1].volume, dec("2"), "volume does not carry over");

        // The five-minute bar spans both trades and keeps accumulating.
        let five = aggregator
            .in_progress("BTC-PERP", CandleInterval::M5)
            .expect("in progress");
        assert_eq!(five.volume, dec("3"));
        assert_eq!(five.open, dec("100"));
        assert_eq!(five.close, dec("110"));
    }

    #[tokio::test]
    async fn an_in_progress_bar_from_a_past_bucket_is_not_reported_as_current() {
        let (clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        aggregator
            .apply_trade("BTC-PERP", dec("100"), dec("1"), at("2026-09-18T12:00:30Z"))
            .await
            .expect("applied");
        assert!(aggregator
            .in_progress("BTC-PERP", CandleInterval::M1)
            .is_some());
        clock.set(at("2026-09-18T12:05:00Z"));
        assert!(
            aggregator
                .in_progress("BTC-PERP", CandleInterval::M1)
                .is_none(),
            "a stale bar is history, and history comes from storage"
        );
    }

    #[tokio::test]
    async fn markets_do_not_bleed_into_each_other() {
        let (_clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        aggregator
            .apply_trade("BTC-PERP", dec("100"), dec("1"), at("2026-09-18T12:00:30Z"))
            .await
            .expect("applied");
        aggregator
            .apply_trade("ETH-PERP", dec("5"), dec("7"), at("2026-09-18T12:00:30Z"))
            .await
            .expect("applied");
        assert_eq!(
            aggregator
                .in_progress("BTC-PERP", CandleInterval::M1)
                .expect("btc")
                .volume,
            dec("1")
        );
        assert_eq!(
            aggregator
                .in_progress("ETH-PERP", CandleInterval::M1)
                .expect("eth")
                .close,
            dec("5")
        );
    }

    #[tokio::test]
    async fn history_returns_the_most_recent_bars_oldest_first() {
        let (clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        for minute in 0..5 {
            let when = at(&format!("2026-09-18T12:0{minute}:30Z"));
            clock.set(when);
            aggregator
                .apply_trade("BTC-PERP", Decimal::from(100 + minute), dec("1"), when)
                .await
                .expect("applied");
        }
        let history = aggregator
            .history("BTC-PERP", CandleInterval::M1, 3)
            .await
            .expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].close, Decimal::from(102));
        assert_eq!(history[2].close, Decimal::from(104));
    }

    #[tokio::test]
    async fn pruning_drops_bars_older_than_the_retention() {
        let (clock, _db, aggregator) = aggregator("2026-09-18T12:00:30Z");
        aggregator
            .apply_trade("BTC-PERP", dec("100"), dec("1"), at("2026-09-18T12:00:30Z"))
            .await
            .expect("applied");
        clock.set(at("2026-09-19T12:00:30Z"));
        let removed = aggregator.prune(10).await.expect("prune");
        assert!(
            removed > 0,
            "a day-old minute bar is past a ten-bucket retention"
        );
        assert!(aggregator
            .history("BTC-PERP", CandleInterval::M1, 10)
            .await
            .expect("history")
            .is_empty());
        assert_eq!(
            aggregator
                .history("BTC-PERP", CandleInterval::D1, 10)
                .await
                .expect("history")
                .len(),
            1,
            "a daily bar is still inside its own retention"
        );
    }
}
