//! The leaderboard: who is winning, recomputed from account state.
//!
//! ## Equity is computed, not stored
//!
//! There is no "equity" on chain. It is assembled per player from balances,
//! the cross-margin collateral pool, unrealised PnL and the marked value of
//! any non-USD holdings. That means a recomputation is `O(players)` RPC calls,
//! which is why it runs on a timer and writes a ranking, rather than being
//! computed per request.
//!
//! Ranks shift whenever any player's equity moves, so the ranking is replaced
//! wholesale — [`cb_ports::LeaderboardRepository::replace_window`] exists for
//! exactly that reason, and a partial update would leave the table
//! inconsistent.
//!
//! ## Window PnL, and its honest limitation
//!
//! "PnL over the last 24 hours" needs the player's equity as it was 24 hours
//! ago. Nothing persists that — no repository in `cb-ports` stores an equity
//! time series — so this module keeps an in-memory ring of periodic samples
//! and interpolates from the oldest sample inside the window. The consequence
//! is worth stating plainly: **window PnL resets when the gateway restarts,
//! and reads as all-time PnL until the ring has filled.** All-time PnL has no
//! such problem, because its baseline is what the faucet granted, which *is*
//! persisted.
//!
//! ## Bots are flagged, not hidden
//!
//! Bot accounts keep the books alive and will often out-trade the humans. The
//! contract carries `is_bot` on every row so the frontend can present a human
//! ranking without the gateway deciding on its behalf.

use crate::auth::AuthService;
use crate::error::ApiResult;
use crate::markets::MarketCatalog;
use crate::node::NodeClient;
use crate::symbols::{InstrumentKind, MarketRef};
use crate::tracker::OrderTracker;
use cb_ports::{AppDatabase, Clock, LeaderboardEntry, LeaderboardWindow};
use cb_types::{Address, AssetId};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// How many equity samples to keep per player.
const SAMPLE_RING: usize = 512;

/// One observation of a player's equity.
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: DateTime<Utc>,
    equity_micro_usd: i128,
}

/// Recomputes and serves the leaderboard.
pub struct LeaderboardService {
    db: Arc<dyn AppDatabase>,
    node: Arc<dyn NodeClient>,
    markets: Arc<MarketCatalog>,
    tracker: Arc<OrderTracker>,
    clock: Arc<dyn Clock>,
    /// Needed to turn a stored login address into the trading account that
    /// actually holds the money.
    auth: Arc<AuthService>,
    samples: Mutex<HashMap<String, VecDeque<Sample>>>,
}

impl std::fmt::Debug for LeaderboardService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderboardService")
            .field("sampled_players", &self.samples.lock().len())
            .finish_non_exhaustive()
    }
}

impl LeaderboardService {
    /// Build the service.
    pub fn new(
        db: Arc<dyn AppDatabase>,
        node: Arc<dyn NodeClient>,
        markets: Arc<MarketCatalog>,
        tracker: Arc<OrderTracker>,
        clock: Arc<dyn Clock>,
        auth: Arc<AuthService>,
    ) -> Self {
        Self {
            db,
            node,
            markets,
            tracker,
            clock,
            auth,
            samples: Mutex::new(HashMap::new()),
        }
    }

    /// Recompute every window and store the result.
    pub async fn recompute(&self, max_players: u32) -> ApiResult<()> {
        let now = self.clock.now();
        let players = self.db.users().list(true, max_players).await?;
        let prices = self.mark_prices().await;

        let mut rows = Vec::with_capacity(players.len());
        for player in &players {
            // The stored address is the login address; the money is at the
            // derived trading address.
            let Ok((_key, address)) = self.auth.trading_key_of(player) else {
                tracing::warn!(
                    target: "cb_gateway::leaderboard",
                    user = %player.user_id,
                    "skipping a player whose stored address is malformed"
                );
                continue;
            };
            let equity = self.equity_of(address, &prices).await?;
            self.record_sample(&player.user_id, now, equity);
            rows.push((player.clone(), equity));
        }

        for window in [
            LeaderboardWindow::Day,
            LeaderboardWindow::Week,
            LeaderboardWindow::AllTime,
        ] {
            let mut entries = Vec::with_capacity(rows.len());
            for (player, equity) in &rows {
                let baseline = self.baseline_for(&player.user_id, window, now).await?;
                let pnl = equity - baseline;
                let (volume, trades) = self.activity_of(&player.user_id, window, now);
                entries.push(LeaderboardEntry {
                    // Filled in once sorted.
                    rank: 0,
                    handle: self.auth.effective_handle(player),
                    address: player.address.clone(),
                    equity: micro_usd(*equity),
                    pnl: micro_usd(pnl),
                    pnl_pct: percentage(pnl, baseline),
                    volume: micro_usd(volume),
                    trades,
                    is_bot: player.is_bot,
                });
            }
            entries.sort_by(|a, b| b.pnl.cmp(&a.pnl).then_with(|| a.handle.cmp(&b.handle)));
            for (index, entry) in entries.iter_mut().enumerate() {
                entry.rank = index as u32 + 1;
            }
            self.db
                .leaderboard()
                .replace_window(window, now, &entries)
                .await?;
        }
        Ok(())
    }

    /// The stored ranking.
    pub async fn top(
        &self,
        window: LeaderboardWindow,
        limit: u32,
    ) -> ApiResult<Vec<LeaderboardEntry>> {
        Ok(self.db.leaderboard().top(window, limit).await?)
    }

    /// When a window was last recomputed.
    pub async fn computed_at(&self, window: LeaderboardWindow) -> ApiResult<Option<DateTime<Utc>>> {
        Ok(self.db.leaderboard().computed_at(window).await?)
    }

    /// A player's equity in micro-USD.
    ///
    /// USD balances count at face value; the margin pool counts with its
    /// unrealised PnL; everything else is marked against the perpetual for
    /// that underlying, which is the only price the chain publishes.
    pub async fn equity_of(
        &self,
        address: Address,
        prices: &HashMap<AssetId, u128>,
    ) -> ApiResult<i128> {
        let account = self.node.account(address).await?;
        let margin = self.node.margin_account(address).await?;

        let mut equity: i128 = 0;
        // The chain's balance is the total, locks included, so a locked amount
        // is already in it and must not be added again. An asset that appears
        // only in the lock map is a corrupted account rather than a holding the
        // balance map forgot, and counting it would inflate the equity this
        // leaderboard ranks people by.
        for (asset, total) in account.balances.iter() {
            equity += match asset {
                AssetId::Usd => *total as i128,
                other => marked_value(*total, *other, prices) as i128,
            };
        }
        equity += margin.collateral as i128;
        equity += margin.unrealized_pnl;
        equity -= margin.bad_debt as i128;
        Ok(equity.max(0))
    }

    /// Mark prices for every underlying the node lists a perpetual for.
    pub(crate) async fn mark_prices(&self) -> HashMap<AssetId, u128> {
        let mut prices = HashMap::new();
        let catalog = self.markets.snapshot();
        for entry in catalog.symbols.entries() {
            if entry.kind != InstrumentKind::Perp {
                continue;
            }
            let MarketRef::Deriv(id) = entry.market else {
                continue;
            };
            if let Ok(mark) = self.node.mark_price(id).await {
                if mark.px_q8 > 0 {
                    prices.insert(entry.base, mark.px_q8);
                }
            }
        }
        prices
    }

    /// The equity a window's PnL is measured against.
    async fn baseline_for(
        &self,
        user_id: &str,
        window: LeaderboardWindow,
        now: DateTime<Utc>,
    ) -> ApiResult<i128> {
        let since = match window {
            LeaderboardWindow::Day => Some(now - Duration::hours(24)),
            LeaderboardWindow::Week => Some(now - Duration::days(7)),
            LeaderboardWindow::AllTime => None,
        };
        if let Some(since) = since {
            if let Some(sample) = self.oldest_sample_since(user_id, since) {
                return Ok(sample);
            }
        }
        // All-time, or a window with no sample yet: the player started with
        // whatever the faucet gave them, and everything above that is profit.
        let granted = self
            .db
            .ledger()
            .net_by_asset(user_id, DateTime::from_timestamp(0, 0).unwrap_or(now), now)
            .await?;
        let baseline: i128 = granted
            .iter()
            .filter(|(asset, _)| asset == "USD")
            .map(|(_, amount)| decimal_to_micro_usd(*amount))
            .sum();
        Ok(baseline)
    }

    /// Notional traded and trade count inside a window, from the fills the
    /// gateway observed for that user.
    fn activity_of(
        &self,
        user_id: &str,
        window: LeaderboardWindow,
        now: DateTime<Utc>,
    ) -> (i128, u64) {
        let since = match window {
            LeaderboardWindow::Day => now - Duration::hours(24),
            LeaderboardWindow::Week => now - Duration::days(7),
            LeaderboardWindow::AllTime => DateTime::from_timestamp(0, 0).unwrap_or(now),
        };
        let fills = self.tracker.fills_for_user(user_id, None, usize::MAX);
        let mut volume: i128 = 0;
        let mut trades = 0;
        for fill in fills.iter().filter(|f| f.at >= since) {
            let catalog = self.markets.snapshot();
            if let Some(entry) = catalog.entry(&fill.symbol) {
                volume += crate::orders::notional_micro_usd(entry, fill.price, fill.qty) as i128;
            }
            trades += 1;
        }
        (volume, trades)
    }

    fn record_sample(&self, user_id: &str, at: DateTime<Utc>, equity_micro_usd: i128) {
        let mut samples = self.samples.lock();
        let ring = samples.entry(user_id.to_string()).or_default();
        ring.push_back(Sample {
            at,
            equity_micro_usd,
        });
        while ring.len() > SAMPLE_RING {
            ring.pop_front();
        }
    }

    fn oldest_sample_since(&self, user_id: &str, since: DateTime<Utc>) -> Option<i128> {
        let samples = self.samples.lock();
        let ring = samples.get(user_id)?;
        // Only meaningful once a sample actually predates the window; before
        // that, the oldest sample is "now" and would report zero PnL.
        let oldest = ring.front()?;
        if oldest.at > since {
            return None;
        }
        // The baseline is the player's equity *as the window opened*: the
        // newest sample at or before that instant. Taking the first sample
        // inside the window instead would use the equity that has already
        // moved, and report zero PnL for everyone.
        ring.iter()
            .rfind(|s| s.at <= since)
            .map(|s| s.equity_micro_usd)
            .or(Some(oldest.equity_micro_usd))
    }
}

/// Value of a non-USD holding, marked against its perpetual.
///
/// An asset with no listed perpetual has no price the chain publishes, so it
/// contributes nothing rather than an invented number.
fn marked_value(amount: u128, asset: AssetId, prices: &HashMap<AssetId, u128>) -> u128 {
    let Some(px_q8) = prices.get(&asset) else {
        return 0;
    };
    // amount is in the asset's own base units; rescale to the 1e8 contract
    // scale before applying the q8 price, so the micro-USD result is right for
    // an eighteen-decimal asset as well as an eight-decimal one.
    let decimals = asset.decimals() as u32;
    let contracts = if decimals >= 8 {
        amount / 10u128.pow(decimals - 8)
    } else {
        amount.saturating_mul(10u128.pow(8 - decimals))
    };
    contracts.saturating_mul(*px_q8) / 10u128.pow(10)
}

/// A micro-USD integer as the `Decimal` the port model stores.
// `Decimal::from(i128)` panics past 96 bits; the checked conversion is kept
// on purpose even though clippy calls it infallible.
#[allow(clippy::unnecessary_fallible_conversions)]
fn micro_usd(value: i128) -> Decimal {
    Decimal::try_from(value)
        .map(|d| d / Decimal::from(1_000_000))
        .unwrap_or_default()
}

fn decimal_to_micro_usd(value: Decimal) -> i128 {
    (value * Decimal::from(1_000_000))
        .round()
        .try_into()
        .unwrap_or(0)
}

/// `pnl / baseline` as a percentage, without touching a float.
#[allow(clippy::unnecessary_fallible_conversions)]
fn percentage(pnl: i128, baseline: i128) -> Decimal {
    if baseline == 0 {
        return Decimal::ZERO;
    }
    let scaled = pnl.saturating_mul(10_000) / baseline;
    Decimal::try_from(scaled).unwrap_or_default() / Decimal::from(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::SpotDefaults;
    use crate::memory::InMemoryDatabase;
    use crate::node::{MockNode, PerpMarketSnapshot};
    use cb_ports::{FixedClock, LedgerEntry, User};
    use cb_types::DerivId;

    struct Harness {
        node: Arc<MockNode>,
        db: Arc<InMemoryDatabase>,
        clock: FixedClock,
        auth: Arc<crate::auth::AuthService>,
        service: LeaderboardService,
    }

    async fn harness() -> Harness {
        let node = Arc::new(MockNode::new());
        node.edit(|state| {
            state.perp_markets = vec![PerpMarketSnapshot {
                id: DerivId::perp(0),
                underlying: AssetId::Btc,
                tick_size: 1,
                lot_size: 1,
                max_leverage_x100: 5_000,
                initial_margin_bps: 200,
                maintenance_margin_bps: 100,
                taker_fee_bps: 4,
                maker_fee_bps: -1,
                funding_interval_blocks: 36_000,
                max_position_notional: 0,
                paused: false,
                reduce_only_mode: false,
                last_trade_px_q8: 0,
                last_trade_height: 0,
            }];
            state.marks.insert(
                DerivId::perp(0),
                crate::node::MarkSnapshot {
                    px_q8: 6_400_000_000_000,
                    computed_at_height: 1,
                    premium_ema_q9: 0,
                },
            );
        });
        let db = InMemoryDatabase::shared();
        let clock = FixedClock::new(
            DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let markets = Arc::new(MarketCatalog::new(
            node.clone(),
            db.clone(),
            Arc::new(clock.clone()),
            100,
            SpotDefaults::default(),
        ));
        markets.refresh(1).await.expect("refresh");
        let auth = Arc::new(
            crate::auth::AuthService::new(
                db.clone(),
                Arc::new(crate::directory::Directory::new(Arc::new(
                    crate::memory::InMemoryKv::new(),
                ))),
                Arc::new(clock.clone()),
                chrono::Duration::hours(24),
                "leaderboard-test-secret-4f81c90d2ab7",
            )
            .expect("the test secret clears the floor"),
        );
        let service = LeaderboardService::new(
            db.clone(),
            node.clone(),
            markets,
            OrderTracker::new(),
            Arc::new(clock.clone()),
            Arc::clone(&auth),
        );
        Harness {
            node,
            db,
            clock,
            auth,
            service,
        }
    }

    /// Add a player and return the trading account their money lives at.
    ///
    /// The stored address is the login address; equity is read from the
    /// derived trading address, so a test that funds the wrong one proves
    /// nothing.
    async fn add_player(
        h: &Harness,
        handle: &str,
        login_address: Address,
        is_bot: bool,
    ) -> Address {
        let user = User {
            user_id: format!("usr_{handle}"),
            handle: handle.into(),
            address: crate::auth::eth::checksum_address(&login_address),
            recovery_hash: "hash".into(),
            is_bot,
            created_at: h.clock.now(),
            last_seen_at: h.clock.now(),
        };
        h.db.users().create(&user).await.expect("create");
        h.auth.trading_key_of(&user).expect("derive").1
    }

    async fn grant(h: &Harness, user_id: &str, usd: i64) {
        h.db.ledger()
            .append(&LedgerEntry {
                entry_id: format!("led_{user_id}"),
                user_id: user_id.into(),
                kind: "faucet".into(),
                asset: "USD".into(),
                amount: Decimal::from(usd),
                balance_after: Decimal::ZERO,
                memo: None,
                tx_hash: None,
                height: None,
                at: h.clock.now(),
            })
            .await
            .expect("ledger");
    }

    #[tokio::test]
    async fn equity_sums_balances_collateral_and_unrealised_pnl() {
        let h = harness().await;
        let address = Address([1u8; 20]);
        h.node.edit(|state| {
            // The chain's model: `balances` is the TOTAL and `locked` is the
            // part of it reserved against resting orders. 100k held, 6k of it
            // spoken for, 94k spendable.
            let account = state.accounts.entry(address).or_default();
            account.balances.insert(AssetId::Usd, 100_000_000_000);
            account.locked.insert(AssetId::Usd, 6_000_000_000);
            state.margins.insert(
                address,
                crate::node::MarginSnapshot {
                    collateral: 0,
                    unrealized_pnl: 4_250_000_000,
                    bad_debt: 0,
                },
            );
        });
        let prices = h.service.mark_prices().await;
        let equity = h.service.equity_of(address, &prices).await.expect("equity");
        assert_eq!(
            equity, 104_250_000_000,
            "100k held (6k of it locked) + 4.25k PnL"
        );
    }

    #[tokio::test]
    async fn a_crypto_holding_is_marked_against_its_perpetual() {
        let h = harness().await;
        let address = Address([2u8; 20]);
        h.node.fund(address, AssetId::Btc, 100_000_000); // 1 BTC
        let prices = h.service.mark_prices().await;
        let equity = h.service.equity_of(address, &prices).await.expect("equity");
        assert_eq!(equity, 64_000_000_000, "1 BTC at the $64,000 mark");
    }

    #[tokio::test]
    async fn an_asset_with_no_listed_market_contributes_nothing_rather_than_a_guess() {
        let h = harness().await;
        let address = Address([3u8; 20]);
        h.node
            .fund(address, AssetId::Eth, 1_000_000_000_000_000_000);
        let prices = h.service.mark_prices().await;
        assert_eq!(
            h.service.equity_of(address, &prices).await.expect("equity"),
            0
        );
    }

    #[tokio::test]
    async fn bad_debt_reduces_equity_and_it_never_goes_negative() {
        let h = harness().await;
        let address = Address([4u8; 20]);
        h.node.edit(|state| {
            state.margins.insert(
                address,
                crate::node::MarginSnapshot {
                    collateral: 1_000_000,
                    unrealized_pnl: 0,
                    bad_debt: 9_000_000,
                },
            );
        });
        let prices = h.service.mark_prices().await;
        assert_eq!(
            h.service.equity_of(address, &prices).await.expect("equity"),
            0
        );
    }

    #[tokio::test]
    async fn the_ranking_is_by_profit_and_bots_are_flagged() {
        let h = harness().await;
        let winner = add_player(&h, "winner", Address([1u8; 20]), false).await;
        let loser = add_player(&h, "botty", Address([2u8; 20]), true).await;
        grant(&h, "usr_winner", 100_000).await;
        grant(&h, "usr_botty", 100_000).await;
        h.node.fund(winner, AssetId::Usd, 184_250_000_000);
        h.node.fund(loser, AssetId::Usd, 50_000_000_000);

        h.service.recompute(100).await.expect("recompute");
        let rows = h
            .service
            .top(LeaderboardWindow::AllTime, 10)
            .await
            .expect("top");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].rank, 1);
        assert_eq!(rows[0].handle, "winner");
        assert!(!rows[0].is_bot);
        assert_eq!(rows[0].equity.to_string(), "184250");
        assert_eq!(rows[0].pnl.to_string(), "84250");
        assert_eq!(rows[0].pnl_pct.to_string(), "84.25");
        assert_eq!(rows[1].rank, 2);
        assert!(rows[1].is_bot, "bots must be flagged, not hidden");
        assert_eq!(rows[1].pnl.to_string(), "-50000");
    }

    #[tokio::test]
    async fn a_player_who_never_claimed_has_no_baseline_and_no_percentage() {
        let h = harness().await;
        add_player(&h, "quiet", Address([5u8; 20]), false).await;
        h.service.recompute(100).await.expect("recompute");
        let rows = h
            .service
            .top(LeaderboardWindow::AllTime, 10)
            .await
            .expect("top");
        assert_eq!(rows[0].pnl_pct, Decimal::ZERO, "no division by zero");
    }

    #[tokio::test]
    async fn window_pnl_uses_a_sample_once_one_predates_the_window() {
        let h = harness().await;
        let address = add_player(&h, "trader", Address([6u8; 20]), false).await;
        grant(&h, "usr_trader", 100_000).await;
        h.node.fund(address, AssetId::Usd, 100_000_000_000);

        // First sample: the ring has nothing older than the window, so the
        // day window falls back to the all-time baseline.
        h.service.recompute(100).await.expect("recompute");
        let day = h
            .service
            .top(LeaderboardWindow::Day, 10)
            .await
            .expect("top");
        assert_eq!(day[0].pnl, Decimal::ZERO);

        // A day later the player is up, and the old sample is the baseline.
        h.clock.advance(Duration::hours(25));
        h.node.edit(|state| {
            state
                .accounts
                .entry(address)
                .or_default()
                .balances
                .insert(AssetId::Usd, 150_000_000_000);
        });
        h.service.recompute(100).await.expect("recompute");
        let day = h
            .service
            .top(LeaderboardWindow::Day, 10)
            .await
            .expect("top");
        assert_eq!(day[0].pnl.to_string(), "50000");
    }

    #[tokio::test]
    async fn a_malformed_stored_address_is_skipped_rather_than_failing_the_run() {
        let h = harness().await;
        h.db.users()
            .create(&User {
                user_id: "usr_bad".into(),
                handle: "bad".into(),
                address: "not-an-address".into(),
                recovery_hash: "hash".into(),
                is_bot: false,
                created_at: h.clock.now(),
                last_seen_at: h.clock.now(),
            })
            .await
            .expect("create");
        add_player(&h, "good", Address([9u8; 20]), false).await;
        h.service.recompute(100).await.expect("recompute survives");
        let rows = h
            .service
            .top(LeaderboardWindow::AllTime, 10)
            .await
            .expect("top");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].handle, "good");
    }

    #[tokio::test]
    async fn an_unreachable_node_fails_the_recompute_without_clearing_the_table() {
        let h = harness().await;
        add_player(&h, "trader", Address([1u8; 20]), false).await;
        h.service.recompute(100).await.expect("first");
        h.node.take_offline("restarting");
        assert!(h.service.recompute(100).await.is_err());
        assert_eq!(
            h.service
                .top(LeaderboardWindow::AllTime, 10)
                .await
                .expect("top")
                .len(),
            1,
            "the previous ranking survives"
        );
    }
}
