//! Transfer-related events for cross-module communication.
//!
//! This module defines lightweight event types used to wire SIP-layer
//! transfer notifications (e.g. REFER response/NOTIFY) up to the RWI
//! transfer controller without introducing circular dependencies.

use tokio::sync::mpsc;

/// Type alias for ReferNotify event sender.
pub type ReferNotifyTx = mpsc::UnboundedSender<ReferNotifyEvent>;
/// Type alias for ReferNotify event receiver.
pub type ReferNotifyRx = mpsc::UnboundedReceiver<ReferNotifyEvent>;

/// REFER progress and WebSocket media bridge setup results.
#[derive(Debug, Clone)]
pub enum ReferNotifyEventType {
    /// Synchronous response to the REFER request (e.g. 202 Accepted).
    ReferResponse,
    /// Asynchronous transfer progress/result, from SIP NOTIFY or local bridge setup.
    Notify,
}

/// Internal transfer notification emitted by `SipSession`.
#[derive(Debug, Clone)]
pub struct ReferNotifyEvent {
    pub call_id: String,
    pub sip_status: u16,
    pub reason: Option<String>,
    pub event_type: ReferNotifyEventType,
}
