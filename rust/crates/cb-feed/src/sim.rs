//! A seeded geometric-Brownian-motion price simulator.
//!
//! # Why reproducibility is the headline property
//!
//! Every interesting failure in an exchange is a sequence: a position built
//! here, a price move there, a liquidation that fires one tick too late. A
//! simulator whose prices differ between runs turns each of those into a
//! sighting rather than a bug, because the run that produced it cannot be
//! repeated. So the contract is blunt: **the same seed produces the same
//! price path.**
//!
//! Three design consequences follow.
//!
//! 1. **One RNG per symbol, seeded from the symbol's name.** A single shared
//!    generator would make BTC's path depend on how many times anyone happened
//!    to ask about ETH, so the path would change when a caller added a symbol
//!    or reordered a loop. Deriving each symbol's seed from the master seed
//!    *and the symbol's name* also means adding an instrument to the universe
//!    does not perturb the others.
//! 2. **Advancing is separate from observing.** `latest()` never moves a
//!    price; only [`SimulatedFeed::step`] does. If reading moved the market,
//!    the path would depend on how many subscribers were attached, and a
//!    logging statement could change the outcome of a simulation.
//! 3. **Time comes from a [`Clock`] port.** A replay with a
//!    [`cb_ports::FixedClock`] stamps identical observation times, so a
//!    recorded run and its replay are diffable.
//!
//! The one thing not guaranteed is bit-identical results across platforms:
//! the model calls `f64::exp` and `f64::ln`, which are libm functions and are
//! not required to be correctly rounded. Within one toolchain and target — the
//! case that matters for reproducing a bug — paths are identical.
//!
//! # The model
//!
//! Standard geometric Brownian motion in discrete steps:
//!
//! ```text
//! S(t+dt) = S(t) · exp((μ − σ²/2)·dt + σ·√dt·Z),   Z ~ N(0,1)
//! ```
//!
//! GBM is the right level of realism here. It cannot go negative, its returns
//! compound multiplicatively, and its volatility scales with price — all
//! properties a trader's intuition assumes. It has no fat tails and no
//! volatility clustering, which is exactly what the scenarios in
//! [`crate::Scenario`] exist to supply: rather than complicate the process,
//! the interesting regimes are injected deliberately, which also makes them
//! reproducible on demand instead of waiting for one to occur.

use crate::universe::{lookup, AssetClass, AssetSpec, Scenario, UNIVERSE};
use async_trait::async_trait;
use cb_ports::clock::{Clock, SystemClock};
use cb_ports::feed::{FeedSubscription, MarketDataFeed, PriceTick};
use cb_ports::{PortError, PortResult};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Seconds in a 365-day year, the denominator that turns an annualised
/// volatility into a per-tick one.
const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// Simulated time per [`SimulatedFeed::step`] when none is configured.
///
/// One second matches the cadence a trading UI refreshes at, and gives a
/// per-tick standard deviation of about one basis point for BTC — small
/// enough that the tape looks like a market rather than a random-number
/// display.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

/// How strongly [`Scenario::Chop`] pulls back towards its anchor, per step, as
/// a fraction of the gap. Small, so the pull shapes the path over tens of
/// ticks instead of snapping the price back on the next one.
const CHOP_REVERSION_PER_STEP: f64 = 0.02;

/// A floor on the simulated price, as a fraction of the asset's start price.
///
/// GBM cannot reach zero, but a long crash scenario can drive a price low
/// enough that tick rounding collapses it to zero and every notional
/// calculation downstream divides by it. Clamping is a simulator concession,
/// not finance; it is documented rather than hidden.
const PRICE_FLOOR_FRACTION: f64 = 0.01;

/// A mocked market-data source with reproducible, scenario-driven prices.
///
/// Cheap to clone; clones share the same paths, so a bot and the gateway see
/// one market.
#[derive(Clone)]
pub struct SimulatedFeed {
    inner: Arc<FeedState>,
}

struct FeedState {
    source_id: String,
    clock: Arc<dyn Clock>,
    tick: Duration,
    /// Ordered so `supported_symbols` and iteration are stable run to run.
    paths: Mutex<BTreeMap<&'static str, SymbolPath>>,
    /// Bumped once per [`SimulatedFeed::step`]; subscriptions park on it.
    generation: watch::Sender<u64>,
}

struct SymbolPath {
    spec: &'static AssetSpec,
    rng: ChaCha8Rng,
    price: f64,
    scenario: Scenario,
    /// Remaining steps of the current scenario's opening shock.
    shock_steps_left: u32,
    shock_per_step: f64,
    /// Where [`Scenario::Chop`] pulls back to.
    anchor: f64,
    /// Observation time of the most recent step.
    observed_at: DateTime<Utc>,
}

impl std::fmt::Debug for SimulatedFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulatedFeed")
            .field("source_id", &self.inner.source_id)
            .field("symbols", &self.inner.paths.lock().len())
            .field("generation", &*self.inner.generation.borrow())
            .finish()
    }
}

impl SimulatedFeed {
    /// A feed over the whole universe, seeded deterministically.
    pub fn new(seed: u64) -> Self {
        Self::with_symbols(seed, UNIVERSE)
    }

    /// A feed over a chosen subset, for a test that only cares about one
    /// instrument.
    ///
    /// A symbol's path depends on the seed and its own name, never on which
    /// other symbols are present, so a subset produces the same prices as the
    /// full universe would.
    pub fn with_symbols(seed: u64, specs: &'static [AssetSpec]) -> Self {
        Self::build(seed, specs, DEFAULT_TICK, Arc::new(SystemClock))
    }

    /// A feed with an explicit tick width and clock.
    ///
    /// The tick width is simulated time per step, which sets how far a price
    /// moves per step; the clock supplies the observation timestamps.
    pub fn with_config(
        seed: u64,
        specs: &'static [AssetSpec],
        tick: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::build(seed, specs, tick, clock)
    }

    fn build(
        seed: u64,
        specs: &'static [AssetSpec],
        tick: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now();
        let mut paths = BTreeMap::new();
        for spec in specs {
            paths.insert(
                spec.symbol,
                SymbolPath {
                    spec,
                    rng: ChaCha8Rng::seed_from_u64(symbol_seed(seed, spec.symbol)),
                    price: spec.start_price,
                    scenario: Scenario::Calm,
                    shock_steps_left: 0,
                    shock_per_step: 1.0,
                    anchor: spec.start_price,
                    observed_at: now,
                },
            );
        }

        Self {
            inner: Arc::new(FeedState {
                source_id: format!("sim:{seed}"),
                clock,
                tick,
                paths: Mutex::new(paths),
                generation: watch::channel(0).0,
            }),
        }
    }

    /// Advance every symbol by one tick.
    pub fn step(&self) {
        self.advance(1);
    }

    /// Advance every symbol by `steps` ticks, waking subscribers once at the
    /// end rather than once per step.
    pub fn advance(&self, steps: u32) {
        if steps == 0 {
            return;
        }
        let now = self.inner.clock.now();
        let dt = self.inner.tick.as_secs_f64() / SECONDS_PER_YEAR;

        {
            let mut paths = self.inner.paths.lock();
            for path in paths.values_mut() {
                for _ in 0..steps {
                    path.advance_one(dt);
                }
                path.observed_at = now;
            }
        }

        let generation = *self.inner.generation.borrow() + u64::from(steps);
        self.inner.generation.send_replace(generation);
    }

    /// Put one symbol into a scenario, arming its opening shock.
    ///
    /// Returns [`PortError::NotFound`] for a symbol this feed does not carry,
    /// which keeps a typo in a scenario script from silently doing nothing.
    pub fn set_scenario(&self, symbol: &str, scenario: Scenario) -> PortResult<()> {
        let spec = lookup(symbol).ok_or_else(|| PortError::NotFound(symbol.to_string()))?;
        let mut paths = self.inner.paths.lock();
        let path = paths
            .get_mut(spec.symbol)
            .ok_or_else(|| PortError::NotFound(symbol.to_string()))?;
        path.enter(scenario);
        Ok(())
    }

    /// Put every symbol into a scenario.
    pub fn set_scenario_all(&self, scenario: Scenario) {
        let mut paths = self.inner.paths.lock();
        for path in paths.values_mut() {
            path.enter(scenario);
        }
    }

    /// The scenario a symbol is currently in.
    pub fn scenario(&self, symbol: &str) -> PortResult<Scenario> {
        let spec = lookup(symbol).ok_or_else(|| PortError::NotFound(symbol.to_string()))?;
        let paths = self.inner.paths.lock();
        paths
            .get(spec.symbol)
            .map(|p| p.scenario)
            .ok_or_else(|| PortError::NotFound(symbol.to_string()))
    }

    /// The current mid price as an `f64`, before rounding to the tick.
    ///
    /// Exposed for scenario assertions that need the unrounded path; ordinary
    /// callers want [`MarketDataFeed::latest`].
    pub fn raw_price(&self, symbol: &str) -> PortResult<f64> {
        let spec = lookup(symbol).ok_or_else(|| PortError::NotFound(symbol.to_string()))?;
        let paths = self.inner.paths.lock();
        paths
            .get(spec.symbol)
            .map(|p| p.price)
            .ok_or_else(|| PortError::NotFound(symbol.to_string()))
    }

    fn tick_for(&self, spec: &'static AssetSpec) -> PortResult<PriceTick> {
        let paths = self.inner.paths.lock();
        let path = paths
            .get(spec.symbol)
            .ok_or_else(|| PortError::NotFound(spec.symbol.to_string()))?;
        path.to_tick(&self.inner.source_id)
    }
}

impl SymbolPath {
    fn enter(&mut self, scenario: Scenario) {
        self.scenario = scenario;
        self.anchor = self.price;
        match scenario.opening_shock() {
            Some((per_step, steps)) => {
                self.shock_per_step = per_step;
                self.shock_steps_left = steps;
            }
            None => {
                self.shock_per_step = 1.0;
                self.shock_steps_left = 0;
            }
        }
    }

    fn advance_one(&mut self, dt: f64) {
        let sigma = self.spec.annual_volatility * self.scenario.volatility_multiplier();
        let mu = if self.scenario.is_mean_reverting() {
            0.0
        } else {
            self.spec.annual_drift + self.scenario.extra_annual_drift()
        };

        let z = standard_normal(&mut self.rng);
        // Itô correction: subtracting σ²/2 makes the *expected* return μ·dt
        // rather than μ·dt + σ²·dt/2. Without it a high-volatility scenario
        // would drift upwards purely as an artefact of the exponential.
        let exponent = (mu - 0.5 * sigma * sigma) * dt + sigma * dt.sqrt() * z;
        self.price *= exponent.exp();

        if self.shock_steps_left > 0 {
            self.price *= self.shock_per_step;
            self.shock_steps_left -= 1;
        }

        if self.scenario.is_mean_reverting() {
            self.price += (self.anchor - self.price) * CHOP_REVERSION_PER_STEP;
        }

        let floor = self.spec.start_price * PRICE_FLOOR_FRACTION;
        if self.price < floor {
            self.price = floor;
        }
    }

    /// Half-spread in price units, widening with the scenario's volatility.
    ///
    /// Market makers quote wider when they are more likely to be run over, so
    /// the spread scales with the volatility multiplier in force. It is
    /// floored at one tick, because a spread narrower than the minimum price
    /// increment cannot be quoted.
    fn half_spread(&self) -> f64 {
        let widening = self.scenario.volatility_multiplier().max(1.0);
        let bps = self.spec.base_spread_bps * widening;
        (self.price * bps / 10_000.0).max(tick_as_f64(self.spec))
    }

    fn to_tick(&self, source_id: &str) -> PortResult<PriceTick> {
        let dp = self.spec.price_decimals;
        let half = self.half_spread();

        let price = quantise(self.price, dp, self.spec.symbol, "price")?;
        let mut bid = quantise(self.price - half, dp, self.spec.symbol, "bid")?;
        let mut ask = quantise(self.price + half, dp, self.spec.symbol, "ask")?;

        // Rounding can collapse a sub-tick spread. A crossed or locked quote
        // would violate the book-integrity invariant the simulation asserts,
        // so push the sides apart by one tick rather than emit one.
        if bid >= ask {
            let tick = self.spec.tick();
            bid = price - tick;
            ask = price + tick;
        }

        Ok(PriceTick {
            symbol: self.spec.symbol.to_string(),
            price,
            bid: Some(bid),
            ask: Some(ask),
            at: self.observed_at,
            source: source_id.to_string(),
        })
    }
}

fn tick_as_f64(spec: &'static AssetSpec) -> f64 {
    10f64.powi(-(spec.price_decimals as i32))
}

/// Convert a simulated `f64` price to a quoted [`Decimal`].
///
/// A non-finite price means the model has blown up — an overflow or a NaN from
/// a bad parameter — and is reported rather than stored, because a NaN price
/// silently poisons every downstream margin calculation.
fn quantise(value: f64, decimals: u32, symbol: &str, what: &str) -> PortResult<Decimal> {
    if !value.is_finite() {
        return Err(PortError::Internal(format!(
            "simulated {what} for {symbol} is not finite ({value}); the price \
             model has diverged"
        )));
    }
    Decimal::from_f64_retain(value)
        .map(|d| d.round_dp(decimals))
        .ok_or_else(|| {
            PortError::Internal(format!(
                "simulated {what} for {symbol} ({value}) is outside the range of \
                 a decimal"
            ))
        })
}

/// One standard normal draw, by the Box–Muller transform.
///
/// Hand-rolled because the workspace carries `rand` but not `rand_distr`, and
/// a ten-line transform is a smaller thing to own than a dependency. The
/// second variate Box–Muller produces is discarded: caching it would make a
/// symbol's path depend on whether the previous step consumed one, which is
/// exactly the kind of hidden state that breaks reproducibility under
/// refactoring.
fn standard_normal(rng: &mut ChaCha8Rng) -> f64 {
    // `gen` yields [0, 1); the transform needs u1 > 0 for `ln`.
    let u1 = 1.0 - rng.gen::<f64>();
    let u2 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
}

/// Derive a symbol's RNG seed from the master seed and the symbol's name.
///
/// FNV-1a over the name, mixed through SplitMix64. Two properties are wanted:
/// the same (seed, symbol) always gives the same stream, and two symbols never
/// share one — which a simple `seed + index` scheme would violate the moment
/// the universe was reordered.
fn symbol_seed(master: u64, symbol: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in symbol.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    splitmix64(master ^ hash)
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[async_trait]
impl MarketDataFeed for SimulatedFeed {
    fn source_id(&self) -> &str {
        &self.inner.source_id
    }

    async fn latest(&self, symbol: &str) -> PortResult<PriceTick> {
        let spec = lookup(symbol)
            .ok_or_else(|| PortError::NotFound(format!("this source does not carry `{symbol}`")))?;
        self.tick_for(spec)
    }

    /// Unknown symbols are skipped rather than failing the batch, per the port
    /// contract: one stale entry in a caller's watchlist must not cost it every
    /// price it asked for.
    async fn latest_batch(&self, symbols: &[String]) -> PortResult<Vec<PriceTick>> {
        let mut out = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            if let Some(spec) = lookup(symbol) {
                if self.inner.paths.lock().contains_key(spec.symbol) {
                    out.push(self.tick_for(spec)?);
                }
            }
        }
        Ok(out)
    }

    async fn subscribe(&self, symbols: &[String]) -> PortResult<Box<dyn FeedSubscription>> {
        if symbols.is_empty() {
            return Err(PortError::InvalidArgument(
                "a feed subscription with no symbols would never yield anything".into(),
            ));
        }

        let carried: Vec<&'static AssetSpec> = symbols
            .iter()
            .filter_map(|s| lookup(s))
            .filter(|spec| self.inner.paths.lock().contains_key(spec.symbol))
            .collect();

        if carried.is_empty() {
            return Err(PortError::NotFound(format!(
                "this source carries none of {symbols:?}"
            )));
        }

        Ok(Box::new(SimSubscription {
            feed: Arc::clone(&self.inner),
            symbols: carried,
            generation: self.inner.generation.subscribe(),
            pending: Vec::new(),
        }))
    }

    async fn supported_symbols(&self) -> PortResult<Vec<String>> {
        Ok(self
            .inner
            .paths
            .lock()
            .keys()
            .map(|s| (*s).to_string())
            .collect())
    }

    /// Always healthy. A simulated source cannot be unreachable — which is
    /// precisely why [`crate::FaultyFeed`] exists: oracle-freshness logic needs
    /// a source that *can* fail, and it will not get one from here.
    async fn health_check(&self) -> PortResult<()> {
        Ok(())
    }
}

/// A stream of ticks, woken once per [`SimulatedFeed::advance`].
struct SimSubscription {
    feed: Arc<FeedState>,
    symbols: Vec<&'static AssetSpec>,
    generation: watch::Receiver<u64>,
    /// Ticks produced by one wake-up, drained one `next()` at a time.
    pending: Vec<PriceTick>,
}

#[async_trait]
impl FeedSubscription for SimSubscription {
    async fn next(&mut self) -> PortResult<Option<PriceTick>> {
        loop {
            if let Some(tick) = self.pending.pop() {
                return Ok(Some(tick));
            }

            if self.generation.changed().await.is_err() {
                // The feed was dropped; the stream is over rather than broken.
                return Ok(None);
            }

            let paths = self.feed.paths.lock();
            // Reversed, so `pop` from the end yields symbols in universe order.
            for spec in self.symbols.iter().rev() {
                if let Some(path) = paths.get(spec.symbol) {
                    self.pending.push(path.to_tick(&self.feed.source_id)?);
                }
            }
        }
    }
}

/// Whether a symbol trades continuously, for callers that want to know without
/// consulting reference data.
pub fn is_continuous(symbol: &str) -> bool {
    lookup(symbol).is_some_and(|s| s.asset_class == AssetClass::Crypto)
}
