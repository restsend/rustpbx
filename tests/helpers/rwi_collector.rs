// tests/helpers/rwi_collector.rs
//
// RWI event stream collector for E2E tests.
//
// Connects to the PBX's RWI WebSocket, subscribes, and collects all
// server-pushed events for later verification.
//
// Event format (modern):
//   {"call_id": "...", "event_type": "call_ringing", ...}
//
// Event format (legacy, send_event_to_call_owner):
//   {"call_ringing": {"call_id": "...", ...}}
//
// We handle both.

use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tracing::{debug, warn};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type EventVec = Arc<RwLock<Vec<serde_json::Value>>>;
type WsWriter = futures::stream::SplitSink<WsStream, Message>;

fn is_rwi_event(v: &serde_json::Value, event_type: &str) -> bool {
    // Modern format: {"call_id": "...", "event_type": "call_ringing", ...}
    if v["event_type"].as_str() == Some(event_type) {
        return true;
    }
    // Legacy format: {"call_ringing": {"call_id": "...", ...}}
    if v.get(event_type).is_some() {
        return true;
    }
    // Response format: {"type": "command_completed", ...}
    if v["type"].as_str() == Some(event_type) {
        return true;
    }
    false
}

fn extract_rwi_event_type(v: &serde_json::Value) -> String {
    // Modern format: event_type field
    if let Some(et) = v["event_type"].as_str() {
        return et.to_string();
    }
    // Legacy format: top-level key matching known events
    let known_events = [
        "call_ringing",
        "call_answered",
        "call_early_media",
        "call_hangup",
        "call_busy",
        "call_no_answer",
        "call_incoming",
        "call_bridged",
        "call_unbridged",
        "call_transferred",
        "record_started",
        "record_stopped",
        "media_hold_started",
        "media_hold_stopped",
        "media_play_started",
        "media_play_finished",
        "dtmf",
        "dtmf_collected",
        "conference_created",
        "conference_destroyed",
        "queue_joined",
        "queue_left",
        "supervisor_listen_started",
        "supervisor_whisper_started",
        "supervisor_barge_started",
        "supervisor_takeover_started",
        "ivr_step_trace",
        "ivr_node_entered",
        "agent_state_changed",
        "dn_state_changed",
        "parallel_originate_started",
    ];
    if let Some(obj) = v.as_object() {
        for key in obj.keys() {
            if known_events.contains(&key.as_str()) {
                return key.clone();
            }
        }
    }
    // Response format: {"type": "command_completed|command_failed"}
    if let Some(t) = v["type"].as_str() {
        return format!("{}", t);
    }
    "unknown".to_string()
}

/// Collects RWI events from the WebSocket for verification.
pub struct RwiCollector {
    writer: WsWriter,
    events: EventVec,
    subscribe_handle: tokio::task::JoinHandle<()>,
}

impl RwiCollector {
    /// Connect to the RWI WebSocket and start collecting events.
    pub async fn connect(rwi_url: &str, token: &str) -> Self {
        let url = format!("{}?token={}", rwi_url, token);
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(&url),
        )
        .await
        .expect("RWI connect timeout")
        .expect("RWI connect error");

        let events: EventVec = Arc::new(RwLock::new(Vec::new()));
        let events_clone = events.clone();
        let (writer, read) = ws.split();

        // Spawn a background reader that collects ALL events
        let subscribe_handle = tokio::spawn(async move {
            let mut read = read;
            loop {
                tokio::select! {
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                    let event_type = extract_rwi_event_type(&val);
                                    debug!(event_type, text_len = text.len(), "RWI event collected");
                                    events_clone.write().await.push(val);
                                }
                            }
                            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                            Some(Ok(Message::Close(_))) => {
                                debug!("RWI WebSocket closed");
                                break;
                            }
                            Some(Err(e)) => {
                                warn!("RWI WebSocket error: {:?}", e);
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
            }
        });

        let mut collector = Self {
            writer,
            events,
            subscribe_handle,
        };

        // Send subscribe (matching existing test pattern)
        let subscribe = serde_json::json!({
            "rwi": "1.0",
            "action": "session.subscribe",
            "action_id": "rwi-collector-subscribe",
            "params": {
                "contexts": ["default"]
            }
        });
        collector
            .writer
            .send(Message::Text(subscribe.to_string().into()))
            .await
            .unwrap();

        collector
    }

    /// Send a JSON command through the WebSocket.
    pub async fn send(&mut self, json: &serde_json::Value) {
        self.writer
            .send(Message::Text(json.to_string().into()))
            .await
            .unwrap();
    }

    /// Wait for an event matching `predicate` with timeout.
    pub async fn wait_for(
        &self,
        timeout_secs: u64,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> Option<serde_json::Value> {
        let start = tokio::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        while start.elapsed() < timeout {
            let events = self.events.read().await;
            if let Some(ev) = events.iter().rev().find(|&v| predicate(v)) {
                return Some(ev.clone());
            }
            drop(events);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }

    /// Wait for a specific event_type with timeout.
    pub async fn wait_for_event_type(
        &self,
        event_type: &str,
        timeout_secs: u64,
    ) -> Option<serde_json::Value> {
        self.wait_for(timeout_secs, |v| is_rwi_event(v, event_type))
            .await
    }

    /// Get event types in order.
    pub async fn get_event_types(&self) -> Vec<String> {
        self.events
            .read()
            .await
            .iter()
            .map(|v| extract_rwi_event_type(v))
            .collect()
    }

    /// Assert that event types appear in the specified sequence (in order).
    pub async fn assert_event_sequence(&self, expected: &[&str]) {
        let event_types = self.get_event_types().await;
        let mut idx = 0;
        let mut errors = Vec::new();

        for expected_type in expected {
            let found = event_types[idx..].iter().position(|t| t == expected_type);
            match found {
                Some(pos) => {
                    idx += pos + 1;
                }
                None => {
                    errors.push(format!(
                        "expected event '{}' not found after position {}",
                        expected_type, idx
                    ));
                }
            }
        }

        assert!(
            errors.is_empty(),
            "Event sequence mismatch:\n  {}\n\nCollected events:\n  {}",
            errors.join("\n  "),
            event_types.join(" → ")
        );
    }
}

impl Drop for RwiCollector {
    fn drop(&mut self) {
        self.subscribe_handle.abort();
    }
}
