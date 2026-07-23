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
use std::fmt;
use std::sync::Arc;

/// Forward-declare AppRuntime so the hook method can accept it without
/// creating a circular dependency at the module level.
use crate::call::runtime::AppRuntime;

/// Per-session typed extensions bag (like [`http::Extensions`]) wrapped in an
/// [`Arc`] so it can be cheaply cloned (all clones share the same bag).
/// Automatically cleaned up when the last reference is dropped (session ends).
///
/// Addons use this to pass typed data across [`CallSessionHook`] callbacks
/// without resorting to a global key-value store.
#[derive(Clone)]
pub struct SessionExtensions(pub Arc<parking_lot::RwLock<http::Extensions>>);

impl SessionExtensions {
    /// Create a new empty session extensions bag.
    pub fn new() -> Self {
        Self(Arc::new(parking_lot::RwLock::new(http::Extensions::new())))
    }
}

impl Default for SessionExtensions {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for SessionExtensions {
    type Target = Arc<parking_lot::RwLock<http::Extensions>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Lightweight context passed to every session-hook callback.
/// All fields are cheaply cloneable.
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
    /// Call direction: `"inbound"` or `"outbound"`.
    pub direction: String,
    /// ISO-8601 timestamp when the session was created (before ringing).
    pub started_at: Option<String>,
    /// Per-session typed extensions bag (session cookie). Any addon can
    /// insert/read typed data here (e.g. `ctx.extensions.write().insert(MyData)`).
    /// Clones share the same underlying bag. Automatically cleaned up when the
    /// session ends.
    ///
    /// Routing-layer metadata (X-CRM-* / X-CC-* headers) is pre-populated as
    /// `HashMap<String, String>` — read via
    /// `ctx.extensions.read().get::<HashMap<String, String>>()`.
    pub extensions: SessionExtensions,
}

impl Clone for CallSessionContext {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            caller: self.caller.clone(),
            callee: self.callee.clone(),
            connected_callee: self.connected_callee.clone(),
            queue_name: self.queue_name.clone(),
            direction: self.direction.clone(),
            started_at: self.started_at.clone(),
            extensions: self.extensions.clone(),
        }
    }
}

impl fmt::Debug for CallSessionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSessionContext")
            .field("session_id", &self.session_id)
            .field("caller", &self.caller)
            .field("callee", &self.callee)
            .field("connected_callee", &self.connected_callee)
            .field("queue_name", &self.queue_name)
            .field("direction", &self.direction)
            .field("started_at", &self.started_at)
            .field("extensions", &"<http::Extensions>")
            .finish()
    }
}

/// Observer trait for call-session lifecycle events.
///
/// Implementations must be `Send + Sync` so they can be stored in a shared
/// `Arc` and called from async tasks.
#[async_trait]
pub trait CallSessionHook: Send + Sync {
    /// Called when the callee starts ringing (180 Ringing sent to caller).
    async fn on_call_ringing(&self, _ctx: &CallSessionContext) {}

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

    /// Called when the connected callee (agent) terminates while the caller
    /// is still connected.  Return `true` to suppress normal hangup — the
    /// hook takes over the caller session (e.g. starting a survey app).
    ///
    /// Fires *before* [`on_call_ended`] so the hook can transition the
    /// session to a new app before final cleanup.
    async fn on_agent_disconnected(
        &self,
        _ctx: &CallSessionContext,
        _app_runtime: &dyn AppRuntime,
    ) -> bool {
        false
    }
}
