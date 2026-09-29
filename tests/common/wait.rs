//! Condition-based waits for tests — the replacement for fixed `sleep()`.
//!
//! Single source of truth, included from two places:
//!   * `tests/common/mod.rs` (`pub mod wait;`) — integration test binaries
//!   * `src/proxy/tests/mod.rs` (`#[path]` include, `cfg(any(test, feature = "e2e-testing"))`)
//!
//! Rules of thumb:
//!   * Never write `sleep(X); assert(cond)`. Write `wait_until(X, || cond)`.
//!   * The condition is polled quickly (default 10ms), so tests finish as soon
//!     as the event actually happens instead of always paying the fixed sleep.
//!   * On timeout the returned `Err` carries the elapsed duration; the caller
//!     should `panic!`/`expect` with context about what was being waited for.

use std::future::Future;
use std::time::Duration;

/// Default polling interval — fast enough that tests feel event-driven,
/// cheap enough to hammer from hundreds of parallel tests.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Poll `cond` every [`DEFAULT_POLL_INTERVAL`] until it returns `true` or
/// `timeout` elapses. `Ok(())` as soon as the condition holds; `Err(elapsed)`
/// on timeout.
///
/// ```rust,ignore
/// wait_until(Duration::from_secs(5), || async {
///     server.cdr_capture.get_all_records().await.len() >= 2
/// })
/// .await
/// .expect("two CDR records within 5s");
/// ```
pub async fn wait_until<F, Fut>(timeout: Duration, mut cond: F) -> Result<(), Duration>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    wait_until_interval(timeout, DEFAULT_POLL_INTERVAL, &mut cond).await
}

/// [`wait_until`] with an explicit poll interval.
pub async fn wait_until_interval<F, Fut>(
    timeout: Duration,
    interval: Duration,
    cond: &mut F,
) -> Result<(), Duration>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if cond().await {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(start.elapsed());
        }
        tokio::time::sleep(interval).await;
    }
}

/// Poll `f` until it returns `Some(value)`; returns that value, or `Err` on
/// timeout. Use when the wait should also *produce* the thing being waited on.
///
/// ```rust,ignore
/// let record = wait_for_value(Duration::from_secs(5), || async {
///     server
///         .cdr_capture
///         .find_by_call_id(&call_id)
///         .await
///         .filter(|r| r.status == "ANSWERED")
/// })
/// .await
/// .expect("answered CDR within 5s");
/// ```
pub async fn wait_for_value<T, F, Fut>(timeout: Duration, mut f: F) -> Result<T, ()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let start = tokio::time::Instant::now();
    loop {
        if let Some(v) = f().await {
            return Ok(v);
        }
        if start.elapsed() >= timeout {
            return Err(());
        }
        tokio::time::sleep(DEFAULT_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn wait_until_returns_once_condition_holds() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let started = std::time::Instant::now();
        wait_until(Duration::from_secs(2), move || {
            let h = h.clone();
            async move { h.fetch_add(1, Ordering::SeqCst) >= 2 }
        })
        .await
        .expect("condition should hold on 3rd poll");
        // 3 polls at 10ms ≈ 20ms, far below the fixed-sleep alternative.
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn wait_until_times_out_with_elapsed() {
        let started = std::time::Instant::now();
        let err = wait_until(Duration::from_millis(120), || async { false })
            .await
            .expect_err("must time out");
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(err >= Duration::from_millis(120));
    }

    #[tokio::test]
    async fn wait_for_value_returns_the_value() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let v = wait_for_value(Duration::from_secs(2), move || {
            let h = h.clone();
            async move {
                if h.fetch_add(1, Ordering::SeqCst) >= 1 {
                    Some("ready")
                } else {
                    None
                }
            }
        })
        .await
        .expect("value should appear on 2nd poll");
        assert_eq!(v, "ready");
    }
}
