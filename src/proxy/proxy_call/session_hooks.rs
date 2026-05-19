//! Call-session lifecycle hooks.
//!
//! Any component that needs to react to call events (connected, held, ended, …)
//! can implement [`CallSessionHook`] and register it via
//! [`crate::proxy::server::SipServerBuilder::with_session_hook`].
//!
//! All callback methods have default no-op implementations so implementors
//! only override what they care about.

use crate::callrecord::CallRecordHangupReason;
use async_trait::async_trait;

/// Lightweight context passed to every session-hook callback.
/// All fields are cheaply cloneable.
#[derive(Debug, Clone)]
pub struct CallSessionContext {
    /// Unique session identifier (UUID string).
    pub session_id: String,
    /// Original caller URI / number as it arrived.
    pub caller: String,
    /// Original callee URI / number as it arrived.
    pub callee: String,
    /// The callee URI that was actually connected (set after answer).
    pub connected_callee: Option<String>,
    /// Queue name if the call was routed through a queue.
    pub queue_name: Option<String>,
}

/// Observer trait for call-session lifecycle events.
///
/// Implementations must be `Send + Sync` so they can be stored in a shared
/// `Arc` and called from async tasks.
#[async_trait]
pub trait CallSessionHook: Send + Sync {
    /// Called once both legs are connected (200 OK acknowledged by caller).
    async fn on_call_connected(&self, _ctx: &CallSessionContext) {}

    /// Called when a leg is put on hold (re-INVITE with `sendonly`/`inactive`).
    async fn on_call_held(&self, _ctx: &CallSessionContext, _leg_id: &str) {}

    /// Called when a previously held leg is retrieved.
    async fn on_call_unheld(&self, _ctx: &CallSessionContext, _leg_id: &str) {}

    /// Called at the very end of a session (during cleanup), regardless of how
    /// the call ended.  `duration_secs` is 0 for unanswered calls.
    async fn on_call_ended(
        &self,
        _ctx: &CallSessionContext,
        _reason: Option<&CallRecordHangupReason>,
        _duration_secs: u64,
    ) {
    }
}
