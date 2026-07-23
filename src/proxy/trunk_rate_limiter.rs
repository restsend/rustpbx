use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reason a trunk rate-limit check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrunkLimitReason {
    /// `max_concurrent` exceeded. `current` is the in-flight count at reject time.
    Concurrent { current: u32, limit: u32 },
    /// `max_cps` exceeded. `current` is the 1-second-window count at reject time.
    Cps { current: u32, limit: u32 },
}

impl TrunkLimitReason {
    /// HTTP/SIP status code that should be returned to the caller.
    pub fn status_code(&self) -> u16 {
        match self {
            // Busy Here — gateway has no free line
            TrunkLimitReason::Concurrent { .. } => 486,
            // Service Unavailable / Too Many Requests equivalent in SIP
            TrunkLimitReason::Cps { .. } => 503,
        }
    }

    pub fn reason_phrase(&self) -> String {
        match self {
            TrunkLimitReason::Concurrent { current, limit } => {
                format!("Concurrent call limit exceeded ({}/{})", current, limit)
            }
            TrunkLimitReason::Cps { current, limit } => {
                format!("CPS limit exceeded ({}/{})", current, limit)
            }
        }
    }
}

/// In-memory per-trunk rate limiter.
///
/// Tracks two independent budgets keyed by trunk **name**:
/// * `concurrent` — number of in-flight calls. A slot is acquired during routing
///   and must be released by the session cleanup path (`release_concurrent`).
/// * `cps` — 1-second sliding window of INVITE attempts. Self-expiring; no
///   explicit release is needed.
///
/// The limiter is intentionally lock-based (`std::sync::Mutex`) to match the
/// existing wholesale rate limiter pattern; the critical sections are tiny.
#[derive(Debug, Default)]
pub struct TrunkRateLimiter {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// trunk_name -> active call count
    concurrent: HashMap<String, u32>,
    /// trunk_name -> timestamps of CPS acquisitions within the last second
    cps_windows: HashMap<String, VecDeque<Instant>>,
}

impl TrunkRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to acquire a concurrent slot AND record a CPS hit for `trunk`.
    ///
    /// Semantics:
    /// * `max_concurrent == None` — no concurrent limit, slot still counted.
    /// * `max_cps == None` — no CPS limit, hit still recorded.
    /// * On **CPS** failure, nothing is acquired (no slot, no cps record).
    /// * On **concurrent** failure, the CPS hit has already been recorded
    ///   (mirrors the wholesale limiter behaviour) but no concurrent slot is
    ///   taken, so the caller has nothing to release.
    ///
    /// On success the caller owns one concurrent slot that MUST later be
    /// released via [`release_concurrent`].
    pub fn try_acquire(
        &self,
        trunk: &str,
        max_concurrent: Option<u32>,
        max_cps: Option<u32>,
    ) -> Result<(), TrunkLimitReason> {
        let now = Instant::now();
        let window = Duration::from_secs(1);
        let mut guard = self.inner.lock().expect("trunk rate limiter poisoned");

        // 1. CPS check (sliding 1s window)
        let deque = guard.cps_windows.entry(trunk.to_string()).or_default();
        while let Some(&front) = deque.front() {
            if now.duration_since(front) >= window {
                deque.pop_front();
            } else {
                break;
            }
        }
        if let Some(limit) = max_cps {
            let current = deque.len() as u32;
            if current >= limit {
                return Err(TrunkLimitReason::Cps { current, limit });
            }
        }
        deque.push_back(now);

        // 2. Concurrent check
        let entry = guard.concurrent.entry(trunk.to_string()).or_insert(0);
        if let Some(limit) = max_concurrent
            && *entry >= limit
        {
            let current = *entry;
            return Err(TrunkLimitReason::Concurrent { current, limit });
        }
        *entry += 1;

        Ok(())
    }

    /// Release one concurrent slot for `trunk`. Idempotent — extra releases are
    /// silently clamped at zero, matching the wholesale limiter convention.
    pub fn release_concurrent(&self, trunk: &str) {
        let mut guard = self.inner.lock().expect("trunk rate limiter poisoned");
        if let Some(entry) = guard.concurrent.get_mut(trunk) {
            if *entry > 0 {
                *entry -= 1;
            }
            if *entry == 0 {
                guard.concurrent.remove(trunk);
            }
        }
    }

    /// Current in-flight call count for `trunk` (best-effort snapshot, mainly
    /// for diagnostics / metrics).
    pub fn concurrent_count(&self, trunk: &str) -> u32 {
        self.inner
            .lock()
            .expect("trunk rate limiter poisoned")
            .concurrent
            .get(trunk)
            .copied()
            .unwrap_or(0)
    }

    /// Current 1-second-window CPS for `trunk` (best-effort snapshot).
    pub fn cps_count(&self, trunk: &str) -> u32 {
        let now = Instant::now();
        let window = Duration::from_secs(1);
        let guard = self.inner.lock().expect("trunk rate limiter poisoned");
        guard
            .cps_windows
            .get(trunk)
            .map(|deque| {
                deque
                    .iter()
                    .filter(|&&t| now.duration_since(t) < window)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Reset all counters (used on config reload to avoid stale state).
    pub fn clear(&self) {
        let mut guard = self.inner.lock().expect("trunk rate limiter poisoned");
        guard.concurrent.clear();
        guard.cps_windows.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limits_always_acquires() {
        let l = TrunkRateLimiter::new();
        for _ in 0..1000 {
            assert!(l.try_acquire("t1", None, None).is_ok());
        }
        assert_eq!(l.concurrent_count("t1"), 1000);
    }

    #[test]
    fn cps_limit_rejects_within_window() {
        let l = TrunkRateLimiter::new();
        assert!(l.try_acquire("t", None, Some(2)).is_ok());
        assert!(l.try_acquire("t", None, Some(2)).is_ok());
        let err = l.try_acquire("t", None, Some(2)).unwrap_err();
        assert_eq!(
            err,
            TrunkLimitReason::Cps {
                current: 2,
                limit: 2
            }
        );
        // CPS rejection must NOT acquire a concurrent slot.
        assert_eq!(l.concurrent_count("t"), 2);
    }

    #[test]
    fn concurrent_limit_rejects() {
        let l = TrunkRateLimiter::new();
        assert!(l.try_acquire("t", Some(1), None).is_ok());
        let err = l.try_acquire("t", Some(1), None).unwrap_err();
        assert_eq!(
            err,
            TrunkLimitReason::Concurrent {
                current: 1,
                limit: 1
            }
        );
    }

    #[test]
    fn release_concurrent_frees_slot() {
        let l = TrunkRateLimiter::new();
        assert!(l.try_acquire("t", Some(1), None).is_ok());
        l.release_concurrent("t");
        assert!(l.try_acquire("t", Some(1), None).is_ok());
    }

    #[test]
    fn release_is_idempotent() {
        let l = TrunkRateLimiter::new();
        l.try_acquire("t", None, None).unwrap();
        l.release_concurrent("t");
        l.release_concurrent("t"); // no-op, no panic
        l.release_concurrent("missing"); // no-op
    }

    #[test]
    fn trunks_are_isolated() {
        let l = TrunkRateLimiter::new();
        assert!(l.try_acquire("a", Some(1), None).is_ok());
        // Different trunk, still has budget
        assert!(l.try_acquire("b", Some(1), None).is_ok());
        // 'a' is full
        assert!(l.try_acquire("a", Some(1), None).is_err());
    }

    #[test]
    fn cps_window_expires() {
        let l = TrunkRateLimiter::new();
        assert!(l.try_acquire("t", None, Some(1)).is_ok());
        assert!(l.try_acquire("t", None, Some(1)).is_err());
        std::thread::sleep(Duration::from_millis(1100));
        assert!(l.try_acquire("t", None, Some(1)).is_ok());
    }

    #[test]
    fn status_code_mapping() {
        assert_eq!(
            TrunkLimitReason::Concurrent {
                current: 5,
                limit: 5
            }
            .status_code(),
            486
        );
        assert_eq!(
            TrunkLimitReason::Cps {
                current: 5,
                limit: 5
            }
            .status_code(),
            503
        );
    }

    #[test]
    fn clear_resets_all_state() {
        let l = TrunkRateLimiter::new();
        l.try_acquire("t", None, None).unwrap();
        assert_eq!(l.concurrent_count("t"), 1);
        l.clear();
        assert_eq!(l.concurrent_count("t"), 0);
    }
}
