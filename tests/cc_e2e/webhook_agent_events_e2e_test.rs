//! E2E test: agent status changes → RWI event → gateway → webhook HTTP POST.
//!
//! Verifies that `agent_state_changed` and `recording_metadata_available`
//! events reach the configured `[rwi_webhook]` URL with `agent_id`/`agent_name`
//! populated.

use std::sync::Arc;
use std::time::Duration;

use crate::common::webhook_capture::WebhookCapture;
use rustpbx::addons::cc::{CcAddonState, agent::AgentStatus};
use rustpbx::config::LocatorWebhookConfig;
use rustpbx::rwi::{
    RwiGateway, RwiGatewayRef, proto::RecordingMetadata, webhook::start_rwi_webhook_handler,
};

/// Forward agent events from registry channel to gateway → webhook.
fn start_event_forwarder(
    gateway: RwiGatewayRef,
    rx: tokio::sync::mpsc::UnboundedReceiver<rustpbx::rwi::proto::RwiEvent>,
) {
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(event) = rx.recv().await {
            gateway.read().broadcast_event(&event);
        }
    });
}

#[tokio::test]
async fn test_agent_state_change_reaches_webhook() {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;

    let webhook_tx = start_rwi_webhook_handler(LocatorWebhookConfig {
        url: capture.url.clone(),
        events: vec![],
        headers: None,
        timeout_ms: Some(5000),
    });

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

    let cc_state = CcAddonState::default();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    cc_state.agent_registry.set_event_tx(event_tx);
    start_event_forwarder(gateway.clone(), event_rx);

    cc_state
        .agent_registry
        .register("agent-1001".to_string(), vec!["support".to_string()], 2)
        .await
        .expect("register agent");

    // Give the webhook handler task time to start polling
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Trigger offline → idle
    cc_state
        .agent_registry
        .update_status("agent-1001", AgentStatus::Idle)
        .await
        .expect("update to idle");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify agent_state_changed in webhook
    {
        let events = capture.received.lock().unwrap();
        let agent_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["event_type"].as_str() == Some("agent_state_changed"))
            .collect();

        assert!(
            !agent_events.is_empty(),
            "no agent_state_changed in webhook"
        );
        let payload = agent_events[0];
        assert_eq!(payload["event"]["agent_id"].as_str(), Some("agent-1001"));
        assert_eq!(payload["event"]["from_status"].as_str(), Some("offline"));
        assert_eq!(payload["event"]["to_status"].as_str(), Some("idle"));
    }

    // Trigger idle → busy
    cc_state
        .agent_registry
        .update_status(
            "agent-1001",
            AgentStatus::Busy {
                call_id: "call-001".into(),
                since: std::time::Instant::now(),
            },
        )
        .await
        .expect("update to busy");

    tokio::time::sleep(Duration::from_millis(500)).await;

    {
        let events = capture.received.lock().unwrap();
        let busy_events: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| {
                v["event_type"].as_str() == Some("agent_state_changed")
                    && v["event"]["to_status"].as_str() == Some("busy")
            })
            .collect();

        assert!(!busy_events.is_empty(), "no busy event in webhook");
        let payload = busy_events[0];
        assert_eq!(payload["event"]["from_status"].as_str(), Some("idle"));
        assert_eq!(payload["event"]["to_status"].as_str(), Some("busy"));
        assert_eq!(payload["event"]["call_id"].as_str(), Some("call-001"));
    }
}

#[tokio::test]
async fn test_recording_metadata_webhook_carries_agent_context() {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let webhook_tx = start_rwi_webhook_handler(LocatorWebhookConfig {
        url: capture.url.clone(),
        events: vec![],
        headers: None,
        timeout_ms: Some(5000),
    });

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

    let cc_state = CcAddonState::default();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    cc_state.agent_registry.set_event_tx(event_tx);
    start_event_forwarder(gateway.clone(), event_rx);

    cc_state
        .agent_registry
        .register("agent-42".to_string(), vec![], 1)
        .await
        .unwrap();
    cc_state
        .agent_registry
        .update_status("agent-42", AgentStatus::Idle)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Emit recording_metadata_available through gateway
    gateway
        .read()
        .send_to_owner(&rustpbx::rwi::RecordingMetadataAvailable {
            call_id: "call-200".to_string(),
            metadata: RecordingMetadata {
                filename: "rec-200.wav".to_string(),
                file_size: 8192,
                download_url: Some("https://cdn.example.com/rec-200.wav".to_string()),
                caller_name: Some("1001".to_string()),
                callee_name: Some("agent-42".to_string()),
                call_type: "inbound".to_string(),
                call_start_time: Some("2026-01-01T00:00:00Z".to_string()),
                call_end_time: Some("2026-01-01T00:01:00Z".to_string()),
                upload_time: Some("2026-01-01T00:02:00Z".to_string()),
                // Addon-contributed keys are forwarded verbatim in the generic bag.
                extra: Some(std::collections::HashMap::from([
                    ("agent_id".to_string(), "agent-42".to_string()),
                    ("agent_name".to_string(), "Agent 42".to_string()),
                ])),
            },
        });

    tokio::time::sleep(Duration::from_millis(800)).await;

    // Verify recording_metadata_available in webhook
    let events = capture.received.lock().unwrap();
    let rec_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| v["event_type"].as_str() == Some("recording_metadata_available"))
        .collect();

    assert!(
        !rec_events.is_empty(),
        "no recording_metadata_available in webhook"
    );
    let metadata = &rec_events[0]["event"]["metadata"];
    // addon-contributed keys are flattened directly into metadata
    assert_eq!(metadata["agent_id"].as_str(), Some("agent-42"));
    assert_eq!(metadata["agent_name"].as_str(), Some("Agent 42"));
    assert_eq!(rec_events[0]["event"]["call_id"].as_str(), Some("call-200"));
    assert_eq!(
        metadata["download_url"].as_str(),
        Some("https://cdn.example.com/rec-200.wav")
    );
}

/// A CC call-lifecycle event delivered through the `[rwi_webhook]` must carry
/// the primary call's flat context (caller/callee/names/direction), the `root`
/// block, and the agent's `agent_id`/`agent_name`.
#[tokio::test]
async fn test_cc_call_event_webhook_carries_context() {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let webhook_tx = start_rwi_webhook_handler(LocatorWebhookConfig {
        url: capture.url.clone(),
        events: vec![],
        headers: None,
        timeout_ms: Some(5000),
    });

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));

    // Simulate the session's CallMetaStore entry (what sip_session writes).
    gateway.read().meta_store.insert(
        "call-cc-1".to_string(),
        rustpbx::rwi::CallMeta {
            caller: Some("sip:alice@localhost".to_string()),
            callee: Some("sip:4000@localhost".to_string()),
            caller_name: Some("alice".to_string()),
            callee_name: Some("4000".to_string()),
            direction: Some("inbound".to_string()),
            root: Some(rustpbx::rwi::RootCallInfo {
                caller: Some("sip:alice@localhost".to_string()),
                caller_name: Some("alice".to_string()),
                callee: Some("sip:4000@localhost".to_string()),
                callee_name: Some("4000".to_string()),
                call_id: Some("call-cc-1".to_string()),
                start_time: Some("2026-01-01T00:00:00Z".to_string()),
            }),
            ..Default::default()
        },
    );

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Broadcast the cc_answered event (the path CC events take).
    gateway.read().broadcast_event(&rustpbx::rwi::event::to_legacy_event(
        &rustpbx::addons::cc::cc_events::CcCallAnswered {
            call_id: "call-cc-1".to_string(),
            agent_id: "1001".to_string(),
            agent_name: Some("Agent 1001".to_string()),
            queue_id: Some("support".to_string()),
        },
        None,
    ));

    tokio::time::sleep(Duration::from_millis(800)).await;

    let events = capture.received.lock().unwrap();
    let cc_events: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| v["event_type"].as_str() == Some("cc_answered"))
        .collect();
    assert!(!cc_events.is_empty(), "no cc_answered in webhook");

    let ev = cc_events[0];
    let p = &ev["event"];
    // Own fields.
    assert_eq!(p["agent_id"].as_str(), Some("1001"));
    assert_eq!(p["agent_name"].as_str(), Some("Agent 1001"));
    assert_eq!(p["queue_id"].as_str(), Some("support"));
    // Primary call flat context via gateway enrichment.
    assert_eq!(p["caller"].as_str(), Some("sip:alice@localhost"));
    assert_eq!(p["callee"].as_str(), Some("sip:4000@localhost"));
    assert_eq!(p["caller_name"].as_str(), Some("alice"));
    assert_eq!(p["direction"].as_str(), Some("inbound"));
    // root block.
    assert_eq!(p["root"]["call_id"].as_str(), Some("call-cc-1"));
    assert_eq!(p["root"]["caller"].as_str(), Some("sip:alice@localhost"));
    assert_eq!(p["root"]["callee_name"].as_str(), Some("4000"));
    assert_eq!(p["root"]["start_time"].as_str(), Some("2026-01-01T00:00:00Z"));
}
