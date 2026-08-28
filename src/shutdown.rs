//! Graceful shutdown (drain) state.
//!
//! Once initiated, the node enters a *draining* state:
//! - it stops accepting new registrations / calls (SIP layer replies
//!   503, OPTIONS and HTTP health checks reply 500),
//! - existing calls are allowed to finish,
//! - the process exits after the last call ends.
//!
//! The exit-wait itself lives in [`wait_until_drained`], shared by the
//! AMI `/shutdown` handler and the SIGTERM/SIGINT path in `main` —
//! `docker stop`, `systemctl stop` and the HTTP endpoint behave
//! identically.
//!
//! The flag is process-global: a PBX process has a single lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

static DRAINING: AtomicBool = AtomicBool::new(false);

/// Poll interval of [`wait_until_drained`].
const DRAIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Quiet period after `drained()` first reports true, so in-flight
/// teardown (BYE/ACK exchanges) completes before the exit.
const DRAIN_SETTLE: Duration = Duration::from_secs(2);

/// Enter draining mode. Returns `true` if this call performed the
/// transition (i.e. the node was not already draining), `false`
/// otherwise (idempotent repeat calls / already draining).
pub fn initiate() -> bool {
    !DRAINING.swap(true, Ordering::SeqCst)
}

/// Whether the node is currently draining (graceful shutdown in progress).
pub fn is_draining() -> bool {
    DRAINING.load(Ordering::SeqCst)
}

/// Outcome of [`wait_until_drained`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// All calls/dialogs ended (confirmed after the settle window).
    Drained,
    /// The timeout elapsed with activity still remaining.
    Timeout,
}

/// Wait until the node is drained (no active calls and no dialogs),
/// polling `drained` once per second.
///
/// When `drained()` first turns true the wait sleeps an additional
/// settle window and re-checks, absorbing last-moment BYE/ACK races.
/// `timeout` of `None` waits indefinitely.
///
/// Does NOT cancel any token or exit the process — callers decide what
/// "done" means (AMI handler cancels the runtime token; tests assert).
#[must_use = "the outcome determines whether the exit is clean or forced"]
pub async fn wait_until_drained(
    mut drained: impl FnMut() -> bool,
    timeout: Option<Duration>,
) -> DrainOutcome {
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    loop {
        if drained() {
            tokio::time::sleep(DRAIN_SETTLE).await;
            if drained() {
                return DrainOutcome::Drained;
            }
        }
        if let Some(dl) = deadline
            && tokio::time::Instant::now() >= dl
        {
            return DrainOutcome::Timeout;
        }
        tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
    }
}

/// Log helper for the shared drain loop: a one-line summary at the end
/// of a drain, matching the messages the AMI handler used to emit.
pub fn log_drain_outcome(outcome: DrainOutcome) {
    match outcome {
        DrainOutcome::Drained => {
            info!("Drain complete: no active calls/dialogs; exiting");
        }
        DrainOutcome::Timeout => {
            warn!("Drain timeout reached; force exiting with active calls remaining");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_initiate_and_is_draining() {
        // Reset for test isolation.
        DRAINING.store(false, Ordering::SeqCst);
        assert!(!is_draining());
        assert!(initiate());
        assert!(is_draining());
        // Idempotent: second call returns false.
        assert!(!initiate());
        assert!(is_draining());
        DRAINING.store(false, Ordering::SeqCst);
        assert!(!is_draining());
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_until_drained_immediate() {
        let n = AtomicUsize::new(0);
        let outcome = wait_until_drained(|| n.load(Ordering::Relaxed) == 0, None).await;
        assert_eq!(outcome, DrainOutcome::Drained);
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_until_drained_settle_recheck() {
        // "Empty" once, then busy again during the settle window — must
        // keep waiting instead of exiting on the first true.
        let checks = AtomicUsize::new(0);
        let probe = || {
            let c = checks.fetch_add(1, Ordering::Relaxed);
            // Polls: busy, busy, empty → settle re-check: busy again,
            // then busy, empty → settle re-check: empty → Drained.
            !matches!(c, 0 | 1 | 3 | 4)
        };
        let outcome = wait_until_drained(probe, None).await;
        assert_eq!(outcome, DrainOutcome::Drained);
        assert!(checks.load(Ordering::Relaxed) >= 6);
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_until_drained_timeout() {
        let outcome = wait_until_drained(|| false, Some(Duration::from_secs(10))).await;
        assert_eq!(outcome, DrainOutcome::Timeout);
    }

    #[test]
    fn test_graceful_config_defaults() {
        let cfg = crate::config::GracefulShutdownConfig::default();
        assert_eq!(cfg.drain_timeout_secs, 300);
        assert!(!cfg.enabled_at_startup);
        assert_eq!(
            cfg.effective_timeout(),
            Some(Duration::from_secs(300)),
            "default must be a finite 300s timeout"
        );
        let mut infinite = cfg.clone();
        infinite.drain_timeout_secs = 0;
        assert_eq!(infinite.effective_timeout(), None, "0 means wait forever");
    }
}
