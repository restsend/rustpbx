//! SSE encoding helpers for the outbound dial interface.
//!
//! The SSE stream is a pure RWI event passthrough — zero custom event types.
//! These helpers encode RWI gateway events as `(event_name, data_json)` pairs
//! that the SSE wrapper converts to axum `Event`s.

use crate::rwi::gateway::EventCacheEntry;

/// A single SSE event: the event name (RWI event_type) and the JSON data
/// (RWI payload serialized). The SSE wrapper converts this to
/// `axum::response::sse::Event`.
#[derive(Debug, Clone)]
pub struct SseEntry {
    pub event: String,
    pub data: String,
}

/// RWI event types that indicate call failure when received before
/// `call_answered`. Used by the SSE pump to decide stream closure.
pub fn is_call_failure_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "call_busy" | "call_no_answer" | "call_hangup"
    )
}

/// Encode a gateway event as an `SseEntry`, preserving the RWI event type
/// name and payload verbatim (no translation, no wrapping).
pub fn encode_rwi_event(entry: &EventCacheEntry) -> SseEntry {
    SseEntry {
        event: entry.event.event_type.to_string(),
        data: serde_json::to_string(&entry.event.payload).unwrap_or_default(),
    }
}
