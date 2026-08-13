use crate::call::{Dialplan, TransactionCookie};
use crate::callrecord::{CallRecordHangupMessage, CallRecordHangupReason};
use parking_lot::RwLock;
use rsipstack::dialog::DialogId;
use rsipstack::sip::StatusCode;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Snapshot of call session state for CDR/reporting
pub struct CallSessionRecordSnapshot {
    pub ring_time: Option<Instant>,
    pub answer_time: Option<Instant>,
    pub last_error: Option<(StatusCode, Option<String>)>,
    /// The INVITE transaction's final status, locked at call setup. Later
    /// signaling must not change the CDR/CallEnded status.
    pub invite_final_status: Option<u16>,
    pub hangup_reason: Option<CallRecordHangupReason>,
    pub hangup_messages: Vec<CallRecordHangupMessage>,
    pub original_caller: Option<String>,
    pub original_callee: Option<String>,
    pub routed_caller: Option<String>,
    pub routed_callee: Option<String>,
    pub connected_callee: Option<String>,
    pub routed_contact: Option<String>,
    pub routed_destination: Option<String>,
    pub last_queue_name: Option<String>,
    pub callee_call_ids: Vec<String>,
    pub server_dialog_id: DialogId,
    /// Merged session + routing metadata (opaque HashMap pass-through).
    /// Populated by `record_snapshot` from the session extensions bag and
    /// consumed directly by the reporter — no longer hidden inside `extensions`.
    /// Values are JSON so structured entries (e.g. the `trace` array) persist
    /// cleanly into the call-record `metadata` column.
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    /// Per-leg media quality (packets, RTCP jitter/RTT/loss) captured from the
    /// MediaBridge at call end. Persisted into the call-record `metadata` under
    /// `media_quality`.
    pub media_quality: Option<serde_json::Value>,
    pub extensions: http::Extensions,
}

/// Bookkeeping for an in-flight `media.play`, used to emit `Play` trace events
/// with duration and interruption.
#[derive(Clone, Debug)]
pub struct ActivePlay {
    /// What was being played (file path or description).
    pub source: String,
    /// When playback started (relative to session start, ms).
    pub started_at: std::time::Instant,
}

/// Session hangup message
#[derive(Clone, Debug)]
pub struct SessionHangupMessage {
    pub code: u16,
    pub reason: Option<String>,
    pub target: Option<String>,
}

impl From<&SessionHangupMessage> for CallRecordHangupMessage {
    fn from(message: &SessionHangupMessage) -> Self {
        Self {
            code: message.code,
            reason: message.reason.clone(),
            target: message.target.clone(),
        }
    }
}

/// Context carried throughout the lifetime of a call.
#[derive(Clone)]
pub struct CallContext {
    pub session_id: String,
    pub dialplan: Arc<Dialplan>,
    pub cookie: TransactionCookie,
    pub start_time: Instant,
    pub original_caller: String,
    pub original_callee: String,
    pub max_forwards: u32,
    /// ISO-8601 timestamp when the session was created.
    pub created_at: String,
    /// Application metadata injected by routing (e.g. X-CRM-* / X-CC-* headers).
    pub metadata: Option<std::collections::HashMap<String, String>>,
}


/// Delivers [`crate::call::app::ControllerEvent`]s to the running `CallApp`
/// event loop. Cloneable/shareable: the sender slot has interior mutability.
#[derive(Clone)]
pub struct AppEventBridge {
    app_event_tx: Arc<RwLock<Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>>>,
}

impl AppEventBridge {
    pub fn new() -> Self {
        Self {
            app_event_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Set (or clear) the app-event sender used by [`send_app_event`].
    ///
    /// Called by `run_application` at the start and end of a call app.
    pub fn set_app_event_sender(
        &self,
        sender: Option<mpsc::UnboundedSender<crate::call::app::ControllerEvent>>,
    ) {
        *self.app_event_tx.write() = sender;
    }

    /// Send a [`crate::call::app::ControllerEvent`] directly to the running
    /// `CallApp` event loop.
    ///
    /// Returns `true` if the event was delivered (i.e. an app is currently running
    /// on this call and the channel is open).
    pub fn send_app_event(&self, event: crate::call::app::ControllerEvent) -> bool {
        let slot = self.app_event_tx.read();
        if let Some(tx) = slot.as_ref() {
            return tx.send(event).is_ok();
        }
        false
    }
}

impl Default for AppEventBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    /// Test registry cleanup when handle is dropped
    #[test]
    fn test_registry_cleanup() {
        use crate::call::runtime::SessionId;
        use crate::proxy::active_call_registry::{ActiveProxyCallEntry, ActiveProxyCallRegistry, ActiveProxyCallStatus};
        use std::sync::Arc;
        use chrono::Utc;

        let registry = Arc::new(ActiveProxyCallRegistry::new());

        // Create a SipSessionHandle for the registry
        let id = SessionId::from("registry-test");
        let (handle, _cmd_rx) = crate::proxy::proxy_call::sip_session::SipSession::with_handle(id);

        // Register the handle
        let entry = ActiveProxyCallEntry {
            session_id: "registry-test".to_string(),
            caller: Some("caller".to_string()),
            callee: Some("callee".to_string()),
            direction: "inbound".to_string(),
            started_at: Utc::now(),
            answered_at: None,
            status: ActiveProxyCallStatus::Ringing,
        };

        registry.upsert(entry, handle.clone());
        assert_eq!(registry.count(), 1);

        // Drop handle and receiver
        drop(handle);
        drop(_cmd_rx);

        // Remove from registry (cleanup)
        registry.remove("registry-test");
        assert_eq!(registry.count(), 0);
    }
}