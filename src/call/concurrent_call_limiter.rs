use parking_lot::Mutex;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrentCallLimitExceeded {
    pub limit: u32,
}

impl fmt::Display for ConcurrentCallLimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Concurrent call limit exceeded (limit: {})",
            self.limit
        )
    }
}

#[derive(Debug)]
pub struct ConcurrentCallPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Default)]
pub struct ConcurrentCallLease {
    permits: Mutex<Vec<ConcurrentCallPermit>>,
}

impl ConcurrentCallLease {
    pub fn push(&self, permit: ConcurrentCallPermit) {
        self.permits.lock().push(permit);
    }

    pub fn release_all(&self) {
        let permits = std::mem::take(&mut *self.permits.lock());
        drop(permits);
    }

    pub fn take(&self) -> Self {
        Self {
            permits: Mutex::new(std::mem::take(&mut *self.permits.lock())),
        }
    }

    pub fn len(&self) -> usize {
        self.permits.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.permits.lock().is_empty()
    }
}

impl fmt::Debug for ConcurrentCallLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConcurrentCallLease")
            .field("permit_count", &self.len())
            .finish()
    }
}

/// Concurrent-call capacity for one runtime configuration.
///
/// Each successful acquisition returns an owned permit. The slot is released
/// automatically when the permit is dropped.
#[derive(Debug)]
pub struct ConcurrentCallLimiter {
    limit: u32,
    semaphore: Arc<Semaphore>,
}

impl ConcurrentCallLimiter {
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            semaphore: Arc::new(Semaphore::new(limit as usize)),
        }
    }

    pub fn try_acquire(
        &self,
    ) -> Result<ConcurrentCallPermit, ConcurrentCallLimitExceeded> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map(|permit| ConcurrentCallPermit { _permit: permit })
            .map_err(|_| ConcurrentCallLimitExceeded { limit: self.limit })
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    pub fn current(&self) -> u32 {
        self.limit
            .saturating_sub(self.semaphore.available_permits() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permit_releases_on_drop() {
        let limiter = ConcurrentCallLimiter::new(1);
        let permit = limiter.try_acquire().unwrap();
        assert_eq!(limiter.current(), 1);
        assert_eq!(
            limiter.try_acquire().unwrap_err(),
            ConcurrentCallLimitExceeded { limit: 1 }
        );

        drop(permit);

        assert_eq!(limiter.current(), 0);
        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn zero_limit_always_rejects() {
        let limiter = ConcurrentCallLimiter::new(0);
        assert_eq!(
            limiter.try_acquire().unwrap_err(),
            ConcurrentCallLimitExceeded { limit: 0 }
        );
        assert_eq!(limiter.current(), 0);
    }

    #[test]
    fn lease_groups_and_releases_all_permits() {
        let first = ConcurrentCallLimiter::new(1);
        let second = ConcurrentCallLimiter::new(1);
        let lease = ConcurrentCallLease::default();
        lease.push(first.try_acquire().unwrap());
        lease.push(second.try_acquire().unwrap());
        assert_eq!(lease.len(), 2);
        assert_eq!(first.current(), 1);
        assert_eq!(second.current(), 1);

        lease.release_all();

        assert!(lease.is_empty());
        assert_eq!(first.current(), 0);
        assert_eq!(second.current(), 0);
    }

    #[test]
    fn taking_a_lease_transfers_its_permits() {
        let limiter = ConcurrentCallLimiter::new(1);
        let lease = ConcurrentCallLease::default();
        lease.push(limiter.try_acquire().unwrap());
        let taken = lease.take();

        assert!(lease.is_empty(), "the source lease must be empty");
        assert_eq!(taken.len(), 1);
        assert_eq!(limiter.current(), 1);
        taken.release_all();
        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn lease_drop_releases_permits_without_cleanup() {
        let limiter = ConcurrentCallLimiter::new(1);
        let lease = ConcurrentCallLease::default();
        lease.push(limiter.try_acquire().unwrap());
        assert_eq!(limiter.current(), 1);

        drop(lease);

        assert!(limiter.try_acquire().is_ok());
    }

    #[test]
    fn reload_generations_are_isolated() {
        let old = ConcurrentCallLimiter::new(1);
        let old_permit = old.try_acquire().unwrap();
        let new = ConcurrentCallLimiter::new(1);
        let new_permit = new.try_acquire().unwrap();

        drop(old_permit);

        assert_eq!(new.current(), 1);
        assert!(new.try_acquire().is_err());
        drop(new_permit);
    }
}
