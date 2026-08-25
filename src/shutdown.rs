//! Graceful shutdown (drain) state.
//!
//! Once initiated, the node enters a *draining* state:
//! - it stops accepting new registrations / calls (SIP layer replies
//!   503, OPTIONS and HTTP health checks reply 500),
//! - existing calls are allowed to finish,
//! - the process exits after the last call ends (see the AMI
//!   `/shutdown` handler which polls for drained state then cancels
//!   the runtime token).
//!
//! The flag is process-global: a PBX process has a single lifecycle.

use std::sync::atomic::{AtomicBool, Ordering};

static DRAINING: AtomicBool = AtomicBool::new(false);

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
