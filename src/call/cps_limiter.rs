use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpsLimitExceeded {
    pub limit: u32,
}

impl fmt::Display for CpsLimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CPS limit exceeded (limit: {})", self.limit)
    }
}

/// Lock-free calls-per-second limiter using the generic cell rate algorithm.
///
/// A newly-created limiter allows a burst of `limit_per_second` calls. After
/// that burst is consumed, capacity returns continuously at the configured
/// rate. Rejected attempts do not consume capacity.
#[derive(Debug)]
pub struct CpsLimiter {
    limit: NonZeroU32,
    origin: Instant,
    interval_ns: u64,
    burst_tolerance_ns: u64,
    tat_ns: AtomicU64,
}

impl CpsLimiter {
    pub fn new(limit_per_second: NonZeroU32) -> Self {
        let limit = u64::from(limit_per_second.get());
        let interval_ns = 1_000_000_000_u64.div_ceil(limit);
        let burst_tolerance_ns = (limit - 1) * interval_ns;
        Self {
            limit: limit_per_second,
            origin: Instant::now(),
            interval_ns,
            burst_tolerance_ns,
            tat_ns: AtomicU64::new(0),
        }
    }

    /// Consume one call from the current CPS budget.
    ///
    /// `Ok` means the call was accepted, while `Err` means the burst budget
    /// was already full.
    pub fn try_acquire(&self) -> Result<(), CpsLimitExceeded> {
        let mut now = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let mut tat = self.tat_ns.load(Ordering::Acquire);

        loop {
            let allow_at = tat.saturating_sub(self.burst_tolerance_ns);
            if now < allow_at {
                return Err(CpsLimitExceeded {
                    limit: self.limit.get(),
                });
            }

            let new_tat = tat.max(now).saturating_add(self.interval_ns);
            match self.tat_ns.compare_exchange_weak(
                tat,
                new_tat,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => {
                    tat = actual;
                    now =
                        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
                }
            }
        }
    }

    pub fn limit(&self) -> u32 {
        self.limit.get()
    }

    /// Return the current GCRA pressure after accounting for elapsed time.
    pub fn current_count(&self) -> u32 {
        let now = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.count_at(self.tat_ns.load(Ordering::Acquire), now)
    }

    fn count_at(&self, tat: u64, now: u64) -> u32 {
        let pressure = tat.saturating_sub(now);
        if pressure == 0 {
            return 0;
        }
        let count = pressure.div_ceil(self.interval_ns);
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn allows_initial_burst_and_rejects_next_call() {
        let limiter = CpsLimiter::new(NonZeroU32::new(3).unwrap());

        assert_eq!(limiter.try_acquire(), Ok(()));
        assert_eq!(limiter.try_acquire(), Ok(()));
        assert_eq!(limiter.try_acquire(), Ok(()));
        assert_eq!(
            limiter.try_acquire(),
            Err(CpsLimitExceeded { limit: 3 })
        );
    }

    #[test]
    fn refills_continuously_at_configured_rate() {
        let limiter = CpsLimiter::new(NonZeroU32::new(20).unwrap());
        for _ in 0..20 {
            limiter.try_acquire().unwrap();
        }
        assert_eq!(
            limiter.try_acquire(),
            Err(CpsLimitExceeded { limit: 20 })
        );

        std::thread::sleep(Duration::from_millis(60));

        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn concurrent_acquires_do_not_spuriously_reject_burst() {
        const LIMIT: u32 = 128;
        const ATTEMPTS: usize = LIMIT as usize;
        let limiter = Arc::new(CpsLimiter::new(NonZeroU32::new(LIMIT).unwrap()));
        let barrier = Arc::new(Barrier::new(ATTEMPTS + 1));
        let mut threads = Vec::new();

        for _ in 0..ATTEMPTS {
            let limiter = Arc::clone(&limiter);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                limiter.try_acquire().is_ok()
            }));
        }

        barrier.wait();
        let accepted = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|accepted| *accepted)
            .count();

        assert_eq!(accepted, LIMIT as usize);
    }
}
