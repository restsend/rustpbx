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

/// Whether this event represents a synchronous REFER response or an
/// asynchronous REFER subscription NOTIFY.
#[derive(Debug, Clone)]
pub enum ReferNotifyEventType {
    /// Synchronous response to the REFER request (e.g. 202 Accepted).
    ReferResponse,
    /// Asynchronous NOTIFY carrying the result of the referred action.
    Notify,
}

/// Emitted by `SipSession` when it receives a result for a REFER request.
#[derive(Debug, Clone)]
pub struct ReferNotifyEvent {
    pub call_id: String,
    pub sip_status: u16,
    pub reason: Option<String>,
    pub event_type: ReferNotifyEventType,
}
