//! E2E test for the RWI gateway event-tap mechanism.
//!
//! The outbound dial SSE surface (call_initiated / call_ringing / call_answered
//! / call_busy / call_no_answer) is covered by the Python suite
//! (`e2e/tests/test_outbound_dial.py`), so only the in-process gateway event
//! tap that cannot be exercised externally is kept here.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use rustpbx::rwi::CallInitiated;
use rustpbx::rwi::RwiGateway;
use tokio::time::timeout;

#[tokio::test]
async fn test_gateway_event_tap_delivers_events() {
    let _ = tracing_subscriber::fmt::try_init();

    let gw = Arc::new(RwLock::new(RwiGateway::new()));
    let mut rx = gw.read().subscribe_events();

    let call_id = "tap-test-call".to_string();
    gw.read().send_to_owner(&CallInitiated {
        call_id: call_id.clone(),
        destination: "sip:test@127.0.0.1".to_string(),
    });

    let entry = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("tap recv timeout")
        .expect("tap channel closed");

    assert_eq!(entry.call_id, call_id);
    assert_eq!(entry.event.event_type, "call_initiated");
}
