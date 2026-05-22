use crate::config::LocatorWebhookConfig;
use crate::rwi::gateway::EventCacheEntry;
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use tokio::sync::broadcast;
use tracing::{debug, error, warn};

/// Buffer size for the broadcast channel between gateway and webhook handler.
const WEBHOOK_CHANNEL_SIZE: usize = 512;
/// Max number of recent event (call_id, sequence) pairs kept for dedup.
const DEDUP_CACHE_SIZE: usize = 4096;

/// Start the RWI webhook handler background task.
///
/// Returns a `broadcast::Sender` that the gateway can use to send events.
pub fn start_rwi_webhook_handler(
    config: LocatorWebhookConfig,
) -> broadcast::Sender<EventCacheEntry> {
    let (tx, rx) = broadcast::channel(WEBHOOK_CHANNEL_SIZE);
    crate::utils::spawn(run_rwi_webhook_handler(config, rx));
    tx
}

async fn run_rwi_webhook_handler(
    config: LocatorWebhookConfig,
    mut rx: broadcast::Receiver<EventCacheEntry>,
) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            config.timeout_ms.unwrap_or(5000),
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    debug!("RWI webhook handler started for {}", config.url);

    // Dedup cache: ring buffer of (call_id, sequence) to skip duplicates
    // when the same event is forwarded from multiple call owners.
    let mut dedup: VecDeque<(String, u64)> = VecDeque::with_capacity(DEDUP_CACHE_SIZE + 1);
    let mut seen: HashSet<(String, u64)> = HashSet::new();

    loop {
        let entry = match rx.recv().await {
            Ok(entry) => entry,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("RWI webhook lagged, missed {} events", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        };

        // Dedup: skip event if same (call_id, sequence) already sent.
        let dedup_key = (entry.call_id.clone(), entry.sequence);
        if seen.contains(&dedup_key) {
            debug!("RWI webhook: skipping duplicate event {}", entry.sequence);
            continue;
        }
        seen.insert(dedup_key.clone());
        dedup.push_back(dedup_key);
        while dedup.len() > DEDUP_CACHE_SIZE {
            if let Some(old) = dedup.pop_front() {
                seen.remove(&old);
            }
        }

        // Determine the RWI event type name from the enum variant.
        let event_type = event_type_name(&entry.event);
        let event_type = match event_type {
            Some(name) => name,
            None => {
                debug!("RWI webhook: skipping event without type name");
                continue;
            }
        };

        // Apply event type filter if configured.
        if !config.events.is_empty() && !config.events.contains(&event_type.to_string()) {
            continue;
        }

        let payload = json!({
            "rwi": "1.0",
            "sequence": entry.sequence,
            "timestamp": entry.timestamp,
            "call_id": entry.call_id,
            "event_type": event_type,
            "event": entry.event,
        });

        let mut request = client.post(&config.url);
        if let Some(headers) = &config.headers {
            for (k, v) in headers {
                request = request.header(k, v);
            }
        }

        match request.json(&payload).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(
                        "RWI webhook returned error status: {} for {} (event: {})",
                        resp.status(),
                        config.url,
                        event_type
                    );
                }
            }
            Err(e) => {
                error!(
                    "Failed to send RWI webhook to {}: {} (event: {})",
                    config.url, e, event_type
                );
            }
        }
    }
}

fn event_type_name(event: &crate::rwi::proto::RwiEvent) -> Option<&'static str> {
    use crate::rwi::proto::RwiEvent::*;
    Some(match event {
        CallIncoming(_) => "call_incoming",
        CallRinging { .. } => "call_ringing",
        CallEarlyMedia { .. } => "call_early_media",
        CallAnswered { .. } => "call_answered",
        CallBridged { .. } => "call_bridged",
        CallUnbridged { .. } => "call_unbridged",
        CallTransferred { .. } => "call_transferred",
        CallTransferAccepted { .. } => "call_transfer_accepted",
        CallTransferFailed { .. } => "call_transfer_failed",
        CallHangup { .. } => "call_hangup",
        CallNoAnswer { .. } => "call_no_answer",
        CallBusy { .. } => "call_busy",
        MediaHoldStarted { .. } => "media_hold_started",
        MediaHoldStopped { .. } => "media_hold_stopped",
        MediaRingbackPassthroughStarted { .. } => "media_ringback_passthrough_started",
        MediaRingbackPassthroughStopped { .. } => "media_ringback_passthrough_stopped",
        MediaPlayStarted { .. } => "media_play_started",
        MediaPlayFinished { .. } => "media_play_finished",
        MediaStreamStarted { .. } => "media_stream_started",
        MediaStreamStopped { .. } => "media_stream_stopped",
        RecordStarted { .. } => "record_started",
        RecordPaused { .. } => "record_paused",
        RecordResumed { .. } => "record_resumed",
        RecordStopped { .. } => "record_stopped",
        RecordFailed { .. } => "record_failed",
        QueueJoined { .. } => "queue_joined",
        QueuePositionChanged { .. } => "queue_position_changed",
        QueueAgentOffered { .. } => "queue_agent_offered",
        QueueAgentConnected { .. } => "queue_agent_connected",
        QueueLeft { .. } => "queue_left",
        QueueWaitTimeout { .. } => "queue_wait_timeout",
        QueueOverflowed { .. } => "queue_overflowed",
        QueueVoicemailRedirected { .. } => "queue_voicemail_redirected",
        SupervisorListenStarted { .. } => "supervisor_listen_started",
        SupervisorWhisperStarted { .. } => "supervisor_whisper_started",
        SupervisorBargeStarted { .. } => "supervisor_barge_started",
        SupervisorTakeoverStarted { .. } => "supervisor_takeover_started",
        SupervisorModeStopped { .. } => "supervisor_mode_stopped",
        SipMessageReceived { .. } => "sip_message_received",
        SipNotifyReceived { .. } => "sip_notify_received",
        Dtmf { .. } => "dtmf",
        DtmfCollected { .. } => "dtmf_collected",
        DtmfCollectionTimeout { .. } => "dtmf_collection_timeout",
        ConferenceCreated { .. } => "conference_created",
        ConferenceMemberJoined { .. } => "conference_member_joined",
        ConferenceMemberLeft { .. } => "conference_member_left",
        ConferenceMemberMuted { .. } => "conference_member_muted",
        ConferenceMemberUnmuted { .. } => "conference_member_unmuted",
        ConferenceDestroyed { .. } => "conference_destroyed",
        ConferenceError { .. } => "conference_error",
        ConferenceConsultDialing { .. } => "conference_consult_dialing",
        ConferenceConsultConnected { .. } => "conference_consult_connected",
        ConferenceMergeRequested { .. } => "conference_merge_requested",
        ConferenceMerged { .. } => "conference_merged",
        ConferenceMergeFailed { .. } => "conference_merge_failed",
        AgentStateChanged { .. } => "agent_state_changed",
        QueueCandidatesFound { .. } => "queue_candidates_found",
        QueueAgentRinging { .. } => "queue_agent_ringing",
        QueueAgentNoAnswer { .. } => "queue_agent_no_answer",
        QueueAgentRejected { .. } => "queue_agent_rejected",
        QueueFallbackExecuted { .. } => "queue_fallback_executed",
        QueueAlert { .. } => "queue_alert",
        ConferenceSeatReplaceStarted { .. } => "conference_seat_replace_started",
        ConferenceSeatReplaceSucceeded { .. } => "conference_seat_replace_succeeded",
        ConferenceSeatReplaceFailed { .. } => "conference_seat_replace_failed",
        ConferenceSeatReplaceRollbackFailed { .. } => "conference_seat_replace_rollback_failed",
        CallOwnershipChanged { .. } => "call_ownership_changed",
        SessionResumed { .. } => "session_resumed",
        ParallelOriginateStarted { .. } => "parallel_originate_started",
        ParallelOriginateLegRinging { .. } => "parallel_originate_leg_ringing",
        ParallelOriginateWinner { .. } => "parallel_originate_winner",
        ParallelOriginateLegCancelled { .. } => "parallel_originate_leg_cancelled",
        ParallelOriginateCompleted { .. } => "parallel_originate_completed",
        ParallelOriginateFailed { .. } => "parallel_originate_failed",
        // New events from rwi_event.md
        RecordingMetadataAvailable { .. } => "recording_metadata_available",
        IvrNodeEntered { .. } => "ivr_node_entered",
        IvrNodeExited { .. } => "ivr_node_exited",
        IvrFlowTransitioned { .. } => "ivr_flow_transitioned",
        IvrFlowCompleted { .. } => "ivr_flow_completed",
        DnStateChanged { .. } => "dn_state_changed",
        DnRegistered { .. } => "dn_registered",
        DnUnregistered { .. } => "dn_unregistered",
        CallMetadataUpdated { .. } => "call_metadata_updated",
        IvrStepTrace { .. } => "ivr_step_trace",
    })
}

/// Send a test RWI event to a webhook URL.
pub async fn send_test_event(
    url: &str,
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Result<reqwest::Response, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut request = client.post(url);

    if let Some(headers) = headers {
        for (k, v) in headers {
            request = request.header(k, v);
        }
    }

    let test_payload = json!({
        "rwi": "1.0",
        "sequence": 0,
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "call_id": "test-call-id",
        "event_type": "test",
        "event": {
            "test": {
                "message": "RustPBX RWI webhook test"
            }
        }
    });

    request.json(&test_payload).send().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LocatorWebhookConfig;
    use crate::rwi::gateway::{EventCacheEntry, RwiGateway};
    use crate::rwi::proto::RwiEvent;
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    struct TestHttpServer {
        port: u16,
        received: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl TestHttpServer {
        async fn start() -> Self {
            let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
            let received_clone = received.clone();

            let app = Router::new().route(
                "/hook",
                post(move |Json(body): Json<serde_json::Value>| {
                    received_clone.lock().unwrap().push(body);
                    async { Json(serde_json::json!({"status": "ok"})) }
                }),
            );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("failed to bind test server");
            let port = listener.local_addr().unwrap().port();

            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });

            Self { port, received }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/hook", self.port)
        }
    }

    async fn wait_for_events(
        received: &Arc<Mutex<Vec<serde_json::Value>>>,
        min_count: usize,
        timeout_ms: u64,
    ) {
        let start = std::time::Instant::now();
        loop {
            let count = received.lock().unwrap().len();
            if count >= min_count {
                return;
            }
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                panic!(
                    "timeout waiting for {min_count} events, got {count} after {}ms",
                    start.elapsed().as_millis()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn test_webhook_receives_call_ringing_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 1,
            timestamp: 1000000,
            call_id: "test-call-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "test-call-1".into(),
                context: Default::default(),
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "expected exactly one event");
        let body = &received[0];
        assert_eq!(body["rwi"], "1.0");
        assert_eq!(body["event_type"], "call_ringing");
        assert_eq!(body["call_id"], "test-call-1");
        assert_eq!(body["sequence"], 1);
        assert!(body["event"]["call_ringing"]["call_id"].is_string());
    }

    #[tokio::test]
    async fn test_webhook_receives_call_hangup_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 10,
            timestamp: 2000000,
            call_id: "test-call-2".into(),
            event: RwiEvent::CallHangup {
                call_id: "test-call-2".into(),
                reason: Some("normal_clearing".into()),
                sip_status: Some(200),
                context: Default::default(),
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        let body = &received[0];
        assert_eq!(body["event_type"], "call_hangup");
        assert_eq!(body["call_id"], "test-call-2");
        assert_eq!(body["event"]["call_hangup"]["reason"], "normal_clearing");
        assert_eq!(body["event"]["call_hangup"]["sip_status"], 200);
    }

    #[tokio::test]
    async fn test_webhook_receives_dn_state_changed_event() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let entry = EventCacheEntry {
            sequence: 99,
            timestamp: 3000000,
            call_id: "call-dn-1".into(),
            event: RwiEvent::DnStateChanged {
                dn: "80001".into(),
                event_code: 64,
                event_name: "ESTABLISHED".into(),
                system_time: "2026-05-14T17:54:49.003Z".into(),
                call_id: Some("call-dn-1".into()),
                kz_conn_id: Some("kc-12345".into()),
                agent_id: Some("10001".into()),
                other_dn: None,
                ani: Some("19534519769".into()),
                dnis: Some("39989".into()),
                reason_code: None,
                agent_work_mode: None,
                releasing_party: None,
                third_party_dn: None,
                vq_name: None,
                routing_target: None,
                skill_group: None,
                target_dn: None,
            },
        };
        tx.send(entry).ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        let body = &received[0];
        assert_eq!(body["event_type"], "dn_state_changed");
        assert_eq!(body["event"]["dn_state_changed"]["event_code"], 64);
        assert_eq!(
            body["event"]["dn_state_changed"]["event_name"],
            "ESTABLISHED"
        );
        assert_eq!(body["event"]["dn_state_changed"]["kz_conn_id"], "kc-12345");
    }

    #[tokio::test]
    async fn test_webhook_event_filtering() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec!["call_ringing".into()],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send hangup first (should be filtered OUT)
        tx.send(EventCacheEntry {
            sequence: 1,
            timestamp: 100,
            call_id: "c-1".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-1".into(),
                reason: None,
                sip_status: None,
                context: Default::default(),
            },
        })
        .ok();

        // Send ringing (should be delivered)
        tx.send(EventCacheEntry {
            sequence: 2,
            timestamp: 200,
            call_id: "c-2".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-2".into(),
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "only call_ringing should pass filter");
        assert_eq!(received[0]["event_type"], "call_ringing");
    }

    #[tokio::test]
    async fn test_webhook_multiple_events_ordering() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.send(EventCacheEntry {
            sequence: 1,
            timestamp: 100,
            call_id: "c-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-1".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 2,
            timestamp: 200,
            call_id: "c-2".into(),
            event: RwiEvent::CallAnswered {
                call_id: "c-2".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 3,
            timestamp: 300,
            call_id: "c-3".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-3".into(),
                reason: Some("normal".into()),
                sip_status: Some(200),
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 3, 3000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 3);
        assert_eq!(received[0]["sequence"], 1);
        assert_eq!(received[0]["event_type"], "call_ringing");
        assert_eq!(received[1]["sequence"], 2);
        assert_eq!(received[1]["event_type"], "call_answered");
        assert_eq!(received[2]["sequence"], 3);
        assert_eq!(received[2]["event_type"], "call_hangup");
    }

    #[tokio::test]
    async fn test_webhook_receives_from_gateway_broadcast() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut gateway = RwiGateway::new();
        gateway.set_webhook_tx(tx);

        gateway.broadcast_event(&RwiEvent::CallRinging {
            call_id: "gw-test-call".into(),
            context: Default::default(),
        });

        wait_for_events(&server.received, 1, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 1, "expected one event from gateway");
        assert_eq!(received[0]["event_type"], "call_ringing");
        assert_eq!(received[0]["call_id"], ""); // broadcast uses empty call_id
    }

    #[tokio::test]
    async fn test_webhook_dedup_same_event_twice() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send the same event twice with identical (call_id, sequence).
        let entry = EventCacheEntry {
            sequence: 42,
            timestamp: 100,
            call_id: "dedup-test".into(),
            event: RwiEvent::CallRinging {
                call_id: "dedup-test".into(),
                context: Default::default(),
            },
        };
        tx.send(entry.clone()).ok();
        tx.send(entry).ok();

        // Only one copy should arrive.
        wait_for_events(&server.received, 1, 2000).await;
        tokio::time::sleep(Duration::from_millis(200)).await; // give time for any spurious duplicate

        let received = server.received.lock().unwrap();
        assert_eq!(
            received.len(),
            1,
            "duplicate event should be suppressed, got {}",
            received.len()
        );
        assert_eq!(received[0]["sequence"], 42);
    }

    #[tokio::test]
    async fn test_webhook_dedup_does_not_swallow_unique_events() {
        let server = TestHttpServer::start().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = LocatorWebhookConfig {
            url: server.url(),
            events: vec![],
            headers: None,
            timeout_ms: Some(5000),
        };

        let tx = start_rwi_webhook_handler(config);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Two different events with different (call_id, sequence) — both must arrive.
        tx.send(EventCacheEntry {
            sequence: 1,
            timestamp: 100,
            call_id: "c-1".into(),
            event: RwiEvent::CallRinging {
                call_id: "c-1".into(),
                context: Default::default(),
            },
        })
        .ok();
        tx.send(EventCacheEntry {
            sequence: 2,
            timestamp: 200,
            call_id: "c-2".into(),
            event: RwiEvent::CallHangup {
                call_id: "c-2".into(),
                reason: None,
                sip_status: None,
                context: Default::default(),
            },
        })
        .ok();

        wait_for_events(&server.received, 2, 2000).await;

        let received = server.received.lock().unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["sequence"], 1);
        assert_eq!(received[1]["sequence"], 2);
    }
}
