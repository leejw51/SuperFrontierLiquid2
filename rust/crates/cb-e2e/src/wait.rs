//! Waiting for a condition, never for a duration.
//!
//! A `sleep(500ms)` encodes a guess about a machine that is not the machine the
//! test will eventually run on. When the guess is too small the test is flaky;
//! when it is too large the suite is slow, which makes people stop running it.
//! Both failure modes end with the test being deleted, so this module offers a
//! predicate and a deadline instead, and reports what it was still waiting for
//! when the deadline passed.

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

/// How long a poll waited, and for what.
#[derive(Debug)]
pub struct WaitError {
    /// What the caller said it was waiting for.
    pub what: String,
    /// How long it waited.
    pub waited: Duration,
    /// The last observation, when the caller supplied one.
    pub last: Option<String>,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "timed out after {:.1}s waiting for {}",
            self.waited.as_secs_f64(),
            self.what
        )?;
        if let Some(last) = &self.last {
            write!(f, "; last seen: {last}")?;
        }
        Ok(())
    }
}

impl std::error::Error for WaitError {}

/// The interval between polls. Short enough that a test is not waiting on the
/// poller, long enough that the poller is not the load.
const TICK: Duration = Duration::from_millis(50);

/// Polls `probe` until it returns `true`, or the deadline passes.
///
/// `what` is a sentence fragment naming the condition — it ends up verbatim in
/// the failure message, so write it as the thing that did not happen
/// ("the gateway to answer /chain/status").
pub async fn poll_until<F, Fut>(
    what: &str,
    timeout: Duration,
    mut probe: F,
) -> Result<(), WaitError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    loop {
        if probe().await {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(WaitError {
                what: what.to_string(),
                waited: started.elapsed(),
                last: None,
            });
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Polls `probe` until it yields a value, or the deadline passes.
///
/// The `Err` arm of the probe is kept as the last observation, so a timeout can
/// say *why* the condition never held rather than only that it did not.
pub async fn poll_until_some<T, F, Fut>(
    what: &str,
    timeout: Duration,
    mut probe: F,
) -> Result<T, WaitError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let started = Instant::now();
    let mut last: Option<String>;
    loop {
        match probe().await {
            Ok(value) => return Ok(value),
            Err(why) => last = Some(why),
        }
        if started.elapsed() >= timeout {
            return Err(WaitError {
                what: what.to_string(),
                waited: started.elapsed(),
                last,
            });
        }
        tokio::time::sleep(TICK).await;
    }
}

/// [`poll_until_some`], panicking with the timeout message.
///
/// Tests use this rather than `?` so that the failure names the condition and
/// the last observation instead of an opaque error type.
pub async fn expect_within<T, F, Fut>(what: &str, timeout: Duration, probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    match poll_until_some(what, timeout, probe).await {
        Ok(value) => value,
        Err(err) => panic!("{err}"),
    }
}
