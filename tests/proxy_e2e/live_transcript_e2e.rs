//! E2E coverage for external quality text -> RWI/CC event tap and final webhooks.
//! Uses a real local WebSocket peer; no ASR credentials or provider registry.

use std::sync::Arc;
use std::time::Duration;

use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use rustpbx::call::domain::CallCommand;
use rustpbx::call::quality::QualityConfig;
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode};
use rustpbx::rwi::gateway::EventCacheEntry;
use rustpbx::rwi::{
    RwiEvent, RwiGateway, RwiGatewayRef, TranscriptFinal,
    webhook::{WEBHOOK_CHANNEL_SIZE, start_rwi_webhook_handler},
};
use futures::{SinkExt, StreamExt};

// Local protocol peer exercises the actual quality WebSocket client.
async fn quality_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Bound the listener lifetime even when a test aborts early.
        while let Ok(Ok((socket, _))) = tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let start = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let start: serde_json::Value = serde_json::from_str(&start).unwrap();
                assert_eq!(start["type"], "start");
                assert_eq!(start["version"], 1);
                assert!(start["sample_rate"].as_u64().is_some_and(|rate| rate > 0));
                assert!(start.get("encoding").is_none());
                // Results are emitted only after real call PCM reaches this connection.
                let audio = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
                assert!(audio.is_binary());
                assert_eq!(audio.into_data().len() as u64, start["sample_rate"].as_u64().unwrap() / 50 * 2); // 20 ms native-rate PCM16

                if matches!(start["side"].as_str(), Some("caller" | "callee")) {
                    for (text, partial, end_ms) in [("你好", true, 300), ("你好请问在吗", false, 800)] {
                        ws.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::json!({
                            "type":"text", "text":text, "partial":partial,
                            "start_ms":0, "end_ms":end_ms, "lang":"zh"
                        }).to_string().into())).await.unwrap();
                    }
                }
                while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_secs(15), ws.next()).await {
                    if message.is_close() { break; }
                }
            });
        }
    });
    format!("ws://{address}/v1/quality")
}

// ─── Server / call harness ───────────────────────────────────────────────────

struct TestEnv {
    port: u16,
    registry: Arc<rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry>,
    gateway: RwiGatewayRef,
    records: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<rustpbx::callrecord::CallRecord>>,
    cancel_token: tokio_util::sync::CancellationToken,
    _server_handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

async fn start_transcript_server(remote: QualityConfig) -> Result<TestEnv> {
    let port = portpicker::pick_unused_port().unwrap_or(15080);
    let mut proxy_config = test_helpers::test_proxy_config(port);
    // All: anchor media through the MediaBridge — transcription taps decoded
    // leg PCM and is rejected in bypass mode.
    proxy_config.media_proxy = MediaProxyMode::All;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.quality = Some(remote);

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));

    let config = Arc::new(proxy_config);
    let user_backend = rustpbx::proxy::user::MemoryUserBackend::new(None);
    for user in test_helpers::standard_test_users() {
        user_backend.create_user(user).await?;
    }
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let (record_tx, record_rx) = tokio::sync::mpsc::channel(16);
    let builder = rustpbx::proxy::server::SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(rustpbx::proxy::locator::MemoryLocator::new()))
        .with_cancel_token(cancel_token.clone())
        .with_rwi_gateway(gateway.clone())
        .with_callrecord_sender(Some(record_tx));
    let builder = test_helpers::register_standard_modules(builder);

    let server = Arc::new(builder.build().await?);
    let registry = server.get_inner().active_call_registry.clone();
    let handle = rustpbx::utils::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    Ok(TestEnv {
        port,
        registry,
        gateway,
        records: tokio::sync::Mutex::new(record_rx),
        cancel_token,
        _server_handle: handle,
    })
}

async fn create_ua(port: u16, username: &str, password: &str) -> Result<TestUa> {
    let config = TestUaConfig {
        webrtc: false,
        username: username.to_string(),
        password: password.to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(25000),
        proxy_addr: format!("127.0.0.1:{port}").parse()?,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    ua.register().await?;
    Ok(ua)
}

/// Establish alice→bob and wait for the session to appear in the registry.
async fn establish_call(
    alice: &Arc<TestUa>,
    bob: &TestUa,
    registry: &Arc<rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry>,
) -> Result<String> {
    let alice_sdp = test_helpers::pcmu_sdp("127.0.0.1", 21000);
    let bob_sdp = test_helpers::pcmu_sdp("127.0.0.1", 21010);

    let alice_clone = alice.clone();
    let caller_handle =
        rustpbx::utils::spawn(async move { alice_clone.make_call("bob", Some(alice_sdp)).await });

    let mut bob_answered = false;
    for _ in 0..50 {
        let events = bob.process_dialog_events().await?;
        for event in events {
            if let TestUaEvent::IncomingCall(id, _) = event {
                bob.answer_call(&id, Some(bob_sdp.clone())).await?;
                bob_answered = true;
                break;
            }
        }
        if bob_answered {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(bob_answered, "Bob should receive the INVITE");
    tokio::time::timeout(Duration::from_secs(5), caller_handle)
        .await
        .expect("make_call timed out")
        .expect("task panicked")
        .expect("make_call failed");

    // Wait for the registry entry (the session id is what commands target).
    let start = tokio::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Some(entry) = registry.list_recent(10).first() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Ok(entry.session_id.clone());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("call should appear in the active-call registry");
}

// ─── Event-tap helpers ───────────────────────────────────────────────────────

type Tap = tokio::sync::broadcast::Receiver<EventCacheEntry>;

/// Drain the tap until `pred` matches an event or the deadline passes.
async fn wait_for_event<F>(tap: &mut Tap, mut pred: F, timeout: Duration) -> Option<RwiEvent>
where
    F: FnMut(&RwiEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, tap.recv()).await {
            Ok(Ok(entry)) => {
                if pred(&entry.event) {
                    return Some(entry.event);
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => return None,
            Err(_) => return None,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// `transcript_final` is the only event type an `[rwi_webhook]` filtered on
/// finals receives — partial `transcript_segment` events stay out.
#[tokio::test]
async fn test_transcript_final_webhook_filtering() {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = crate::common::webhook_capture::WebhookCapture::start().await;
    let webhook_tx = start_rwi_webhook_handler(
        LocatorWebhookConfig {
            url: capture.url.clone(),
            events: vec!["transcript_final".to_string()],
            headers: None,
            timeout_ms: Some(5000),
            retries: None,
            track_queue_latency: None,
        },
        WEBHOOK_CHANNEL_SIZE,
    );
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(webhook_tx);
        gw
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Partial and final text each produce one event.
    gateway.read().broadcast_event(&RwiEvent {
        event_type: "transcript_segment",
        kind: Default::default(),
        meta: Default::default(),
        call_id: Some("call-1".to_string()),
        payload: serde_json::json!({
            "call_id": "call-1", "side": "caller", "text": "你好",
            "partial": true, "start_ms": 0u64, "end_ms": 800u64, "lang": "zh",
        }),
    });
    gateway.read().send_to_owner(&TranscriptFinal {
        call_id: "call-1".to_string(),
        side: "caller".to_string(),
        text: "你好请问在吗".to_string(),
        start_ms: 0,
        end_ms: 800,
        lang: Some("zh".to_string()),
        provider: Some("deepgram".to_string()),
    });

    tokio::time::sleep(Duration::from_millis(700)).await;

    let received = capture.received.lock().unwrap();
    let types: Vec<&str> = received
        .iter()
        .filter_map(|v| v["event_type"].as_str())
        .collect();
    assert_eq!(
        types,
        vec!["transcript_final"],
        "webhook filtered on finals must receive exactly one transcript_final, got {types:?}"
    );
    let payload = &received[0];
    assert_eq!(payload["call_id"].as_str(), Some("call-1"));
    assert_eq!(payload["event"]["text"].as_str(), Some("你好请问在吗"));
    assert_eq!(payload["event"]["side"].as_str(), Some("caller"));
    assert_eq!(payload["event"]["provider"].as_str(), Some("deepgram"));
    assert_eq!(payload["rwi"].as_str(), Some("1.0"));
}

/// REST/`CallCommand` start → local quality service streams segments → gateway events
/// (the stream the CC SSE endpoint and `[rwi_webhook]` consume); stop →
/// exactly one `transcript_ended`; second stop is an idempotent no-op.
#[tokio::test]
async fn test_start_stop_quality_command_flow() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let env = start_transcript_server(QualityConfig {
        url: quality_endpoint().await,
        enabled: true,
        ..Default::default()
    })
    .await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;

    // Subscribe BEFORE starting so early events are not missed (same as the
    // CC SSE endpoint does).
    let mut tap = env.gateway.read().subscribe_events();

    let session_id = establish_call(&alice, &bob, &env.registry).await?;

    env.registry
        .get_handle(&session_id)
        .expect("session handle must exist")
        .send_command(CallCommand::StartQualityCheck)
        .expect("start_quality_check dispatch");

    // started → partial → final, one event per text update.
    let started = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_started",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_started event");
    assert_eq!(started.call_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(
        started.payload["provider"].as_str(),
        None,
        "quality events must not invent an ASR provider"
    );

    let partial = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_segment",
        Duration::from_secs(5),
    )
    .await
    .expect("partial segment event");
    assert_eq!(partial.payload["partial"].as_bool(), Some(true));

    let final_event = wait_for_event(
        &mut tap,
        |e| matches!(e.event_type, "transcript_segment" | "transcript_final")
            && e.payload["side"] == partial.payload["side"],
        Duration::from_secs(5),
    )
    .await
    .expect("final text event");
    assert_eq!(final_event.event_type, "transcript_final", "final text must not be duplicated as a segment");
    assert_eq!(final_event.payload["text"].as_str(), Some("你好请问在吗"));
    assert!(final_event.payload["provider"].is_null());

    assert!(wait_for_event(&mut tap,
        |e| matches!(e.event_type, "transcript_segment" | "transcript_final")
            && e.payload["side"] == partial.payload["side"],
        Duration::from_millis(200)).await.is_none(), "no duplicate text event after final");

    // Stop → exactly one transcript_ended. A second stop is an idempotent
    // no-op and must not emit another one.
    env.registry
        .get_handle(&session_id)
        .expect("session handle")
        .send_command(CallCommand::StopQualityCheck)
        .expect("stop dispatch");
    let ended = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_ended",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_ended event");
    assert_eq!(ended.call_id.as_deref(), Some(session_id.as_str()));

    env.registry
        .get_handle(&session_id)
        .expect("session handle")
        .send_command(CallCommand::StopQualityCheck)
        .expect("second stop dispatch");
    let extra_ended = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_ended",
        Duration::from_secs(1),
    )
    .await;
    assert!(
        extra_ended.is_none(),
        "second StopQualityCheck must not emit another transcript_ended"
    );

    Ok(())
}

/// Invalid quality endpoint → `transcript_error` (never a silent no-op).
#[tokio::test]
async fn test_invalid_quality_endpoint_reports_error() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let env = start_transcript_server(QualityConfig {
        url: "http://invalid-quality-endpoint".into(),
        enabled: true,
        ..Default::default()
    })
    .await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let mut tap = env.gateway.read().subscribe_events();
    establish_call(&alice, &bob, &env.registry).await?;

    let error = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_error",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_error event");
    let msg = error.payload["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("quality URL must use ws or wss"),
        "error should name the invalid endpoint, got: {msg}"
    );

    Ok(())
}

/// `[proxy.quality] enabled = true` starts transcription at
/// answer with no command at all.
#[tokio::test]
async fn test_auto_start_at_answer() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let env = start_transcript_server(QualityConfig {
        url: quality_endpoint().await,
        enabled: true,
        ..Default::default()
    })
    .await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;

    // Subscribe before the call so the answer-time auto start is captured.
    let mut tap = env.gateway.read().subscribe_events();

    let session_id = establish_call(&alice, &bob, &env.registry).await?;

    let started = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_started",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_started should fire automatically at answer");
    assert_eq!(started.call_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(started.payload["sides"], serde_json::json!(["caller", "callee"]),
        "auto-start must attach both negotiated audio legs");

    // The local service's text segments flow without any command, and the final one
    // produces one transcript_final event.
    let final_event = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_final",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_final should flow from auto-started transcription");
    assert!(final_event.payload["provider"].is_null());

    // Attaching/dropping an event subscriber must not change inspection ownership.
    let subtitle_subscription = env.gateway.read().subscribe_events();
    drop(subtitle_subscription);
    assert!(wait_for_event(&mut tap, |e| e.event_type == "transcript_ended", Duration::from_millis(400)).await.is_none(),
        "subtitle disconnect must not stop inspection");
    Ok(())
}

/// Re-answer paths (`CallCommand::Answer`, callee re-attach, queue playback)
/// re-enter `accept_call` — auto-start must never double-fire: the running
/// transcription is left untouched (no provider replacement, no second
/// `transcript_started`, inspection connection preserved).
#[tokio::test]
async fn test_auto_start_does_not_retrigger_on_reanswer() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let env = start_transcript_server(QualityConfig {
        url: quality_endpoint().await,
        enabled: true,
        ..Default::default()
    })
    .await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;

    let mut tap = env.gateway.read().subscribe_events();

    let session_id = establish_call(&alice, &bob, &env.registry).await?;

    // First (and only legitimate) auto-start at answer.
    wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_started",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_started at answer");

    // Re-enter the answer path the way the API Answer command does
    // (session.rs `CallCommand::Answer` -> `accept_call` -> auto-start hook).
    use rustpbx::call::domain::LegId;
    for _ in 0..2 {
        env.registry
            .get_handle(&session_id)
            .expect("session handle")
            .send_command(CallCommand::Answer {
                leg_id: LegId::new("caller"),
            })
            .expect("Answer dispatch");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Drain everything the tap still holds; no second transcript_started may
    // appear (the running transcription must be left untouched).
    let mut extra_starts = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), tap.recv()).await {
            Ok(Ok(entry)) => {
                if entry.event.event_type == "transcript_started" {
                    extra_starts += 1;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    assert_eq!(
        extra_starts, 0,
        "re-answer must not retrigger auto-start (provider would be replaced)"
    );

    Ok(())
}

/// Each selected leg gets an independent connection; returned text retains its leg identity.
#[tokio::test]
async fn test_quality_side_selection() -> Result<()> {
    use rustpbx::call::quality::QualitySides;
    for (selection, expected) in [(QualitySides::Caller, "caller"), (QualitySides::Callee, "callee")] {
        let env = start_transcript_server(QualityConfig {
            url: quality_endpoint().await, sides: selection, enabled: true,
            ..Default::default()
        }).await?;
        let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
        let bob = create_ua(env.port, "bob", "password456").await?;
        let mut tap = env.gateway.read().subscribe_events();
        establish_call(&alice, &bob, &env.registry).await?;
        let started = wait_for_event(&mut tap, |e| e.event_type == "transcript_started", Duration::from_secs(5)).await.unwrap();
        assert_eq!(started.payload["sides"], serde_json::json!([expected]));
        let text = wait_for_event(&mut tap, |e| e.event_type == "transcript_final", Duration::from_secs(5)).await.unwrap();
        assert_eq!(text.payload["side"], expected);
        assert!(wait_for_event(&mut tap, |e| e.event_type == "transcript_segment" && e.payload["side"] != expected, Duration::from_millis(200)).await.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn test_disabled_quality_does_not_start_even_with_an_endpoint() -> Result<()> {
    let env = start_transcript_server(QualityConfig { url: quality_endpoint().await, ..Default::default() }).await?;
    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let mut tap = env.gateway.read().subscribe_events();
    let session_id = establish_call(&alice, &bob, &env.registry).await?;
    assert!(wait_for_event(&mut tap, |e| e.event_type == "transcript_started", Duration::from_millis(300)).await.is_none());
    env.registry.get_handle(&session_id).unwrap().send_command(CallCommand::StartQualityCheck).unwrap();
    let error = wait_for_event(&mut tap, |e| e.event_type == "transcript_error", Duration::from_secs(3)).await.unwrap();
    assert!(error.payload["error"].as_str().unwrap().contains("quality disabled"));
    Ok(())
}

/// Hangup alone records service diagnostics once on End; observations remain optional.
#[tokio::test]
async fn test_quality_hangup_records_service_reason_on_end() -> Result<()> {
    use rustpbx::call::quality::QualitySides;
    use tokio_tungstenite::tungstenite::Message;
    for with_observations in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/v1/quality", listener.local_addr()?);
        let trigger = Arc::new(tokio::sync::Notify::new());
        let service_trigger = trigger.clone();
        let service = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let start: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(start["side"], "callee");
            assert!(start["direction"].is_string());
            assert!(start["sample_rate"].as_u64().is_some_and(|rate| rate > 0));
            service_trigger.notified().await;
            for message in [
                serde_json::json!({"type":"calltrace", "kind":"quality", "code":"quality.voicemail_detected", "severity":"info", "message":"Tone evidence", "ts":-999, "side":"caller", "detail":{"method":"beep","frequency_hz":1309}}),
                serde_json::json!({"type":"calltrace", "kind":"quality", "code":"quality.voicemail_detected", "severity":"info", "message":"Voicemail greeting", "detail":{"method":"asr","matched_text":"请在提示音后留言"}}),
                serde_json::json!({"type":"calltrace", "kind":"quality", "code":"quality.ai_assistant_detected", "severity":"info", "message":"AI assistant greeting"}),
                // A service-supplied side must not override the connection identity.
                serde_json::json!({"type":"hangup", "side":"caller", "reason":"quality.voicemail_detected", "detail":{"method":"beep","side":"caller"}}),
            ] {
                if !with_observations && message["type"] == "calltrace" { continue; }
                ws.send(Message::Text(message.to_string().into())).await.unwrap();
            }
            while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
                if message.is_close() { break; }
            }
        });
        let env = start_transcript_server(QualityConfig { url, enabled: true, sides: QualitySides::Callee, ..Default::default() }).await?;
        let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
        let bob = create_ua(env.port, "bob", "password456").await?;
        let session_id = establish_call(&alice, &bob, &env.registry).await?;
        trigger.notify_one();
        let record = tokio::time::timeout(Duration::from_secs(10), async { env.records.lock().await.recv().await }).await?.expect("terminal CDR");
        assert_eq!(record.call_id, session_id);
        assert!(matches!(record.hangup_reason, Some(rustpbx::callrecord::CallRecordHangupReason::Autohangup)));
        let metadata = record.details.metadata.unwrap();
        let trace = metadata["trace"].as_array().unwrap();
        let end_index = trace.iter().position(|e| e["kind"] == "end").expect("terminal event");
        assert_eq!(trace.iter().filter(|e| e["kind"] == "end").count(), 1);
        assert!(!trace.iter().any(|e| e["code"] == "quality.hangup"), "hangup must not create a second trace");
        if with_observations {
            let result_index = trace.iter().position(|e| e["code"] == "quality.voicemail_detected" && e["detail"]["method"] == "beep").expect("result trace");
            assert!(result_index < end_index);
            let observation = &trace[result_index];
            assert_eq!(observation["kind"], "quality");
            assert_eq!(observation["severity"], "info");
            assert_eq!(observation["side"], "callee");
            assert!(observation["ts"].as_i64().unwrap() >= 0, "PBX must stamp its own timestamp");
            assert_eq!(observation["detail"], serde_json::json!({"method":"beep","frequency_hz":1309}));
            assert!(trace.iter().any(|event| event["code"] == "quality.voicemail_detected"
                && event["detail"]["method"] == "asr"));
            for code in ["quality.voicemail_detected", "quality.ai_assistant_detected"] {
                assert!(trace.iter().any(|event| event["code"] == code && event["kind"] == "quality"));
            }
        } else {
            assert!(!trace.iter().any(|event| event["kind"] == "quality"));
        }
        assert_eq!(trace[end_index]["detail"], serde_json::json!({
            "source":"realtime_quality", "reason":"quality.voicemail_detected", "side":"callee",
            "detail":{"method":"beep","side":"caller"},
        }));
        assert!(metadata.get("hangup_trace_detail").is_none());
        tokio::time::timeout(Duration::from_secs(3), service).await??;
    }
    Ok(())
}

/// A service can end either stream without a result; the other stream stays usable.
#[tokio::test]
async fn test_quality_service_close_is_per_side() -> Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    for abrupt in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let caller_done = Arc::new(tokio::sync::Notify::new());
        let finish = Arc::new(tokio::sync::Notify::new());
        let service_finish = finish.clone();
        let service = tokio::spawn(async move {
            let mut peers = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let caller_done = caller_done.clone();
                let finish = service_finish.clone();
                peers.spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let start: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
                    if start["side"] == "caller" {
                        if abrupt {
                            drop(ws);
                        } else {
                            ws.close(None).await.unwrap();
                            loop {
                                let message = ws.next().await.expect("Close acknowledgement").unwrap();
                                if message.is_close() { let _ = ws.flush().await; break; }
                            }
                        }
                        caller_done.notify_one();
                    } else {
                        caller_done.notified().await;
                        // Reads must drive automatic Pong replies even with no outgoing audio.
                        ws.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
                        tokio::time::timeout(Duration::from_secs(2), async {
                            loop {
                                if let Message::Pong(payload) = ws.next().await.unwrap().unwrap() {
                                    assert_eq!(payload.as_ref(), &[1, 2, 3]);
                                    break;
                                }
                            }
                        }).await.expect("automatic Pong from split reader");
                        ws.send(Message::text(r#"{"type":"unknown"}"#)).await.unwrap();
                        ws.send(Message::Text(serde_json::json!({"type":"text","text":"still inspecting","start_ms":0,"end_ms":20}).to_string().into())).await.unwrap();
                        finish.notified().await;
                        ws.close(None).await.unwrap();
                        loop {
                            let message = ws.next().await.expect("Close acknowledgement").unwrap();
                            if message.is_close() { let _ = ws.flush().await; break; }
                        }
                    }
                });
            }
            while let Some(result) = peers.join_next().await { result.unwrap(); }
        });
        let env = start_transcript_server(QualityConfig { url, enabled: true, ..Default::default() }).await?;
        let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
        let bob = create_ua(env.port, "bob", "password456").await?;
        let mut tap = env.gateway.read().subscribe_events();
        let session_id = establish_call(&alice, &bob, &env.registry).await?;
        let text = wait_for_event(&mut tap, |e| e.event_type == "transcript_final", Duration::from_secs(5)).await.expect("other side remains active");
        assert_eq!(text.payload["side"], "callee");
        assert_eq!(text.payload["text"], "still inspecting");
        finish.notify_one();
        tokio::time::timeout(Duration::from_secs(3), service).await??;
        assert!(env.registry.get_handle(&session_id).is_some(), "service closure must not hang up the call");
    }
    Ok(())
}

/// Both explicit stop and parent/session cancellation complete a WebSocket handshake.
#[tokio::test]
async fn test_quality_pbx_close_survives_session_cancellation() -> Result<()> {
    use rustpbx::call::{domain::HangupCommand, quality::QualitySides};
    for hangup in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let service = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap().is_text());
            ready_tx.send(()).unwrap();
            loop {
                let message = ws.next().await.expect("PBX must send Close, not just drop TCP").unwrap();
                if message.is_close() {
                    // The reply arrives after the cancellation signal, not before it.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ws.flush().await.unwrap();
                    break;
                }
            }
        });
        let env = start_transcript_server(QualityConfig { url, enabled: true, sides: QualitySides::Callee, ..Default::default() }).await?;
        let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
        let bob = create_ua(env.port, "bob", "password456").await?;
        let mut tap = env.gateway.read().subscribe_events();
        let session_id = establish_call(&alice, &bob, &env.registry).await?;
        tokio::time::timeout(Duration::from_secs(3), ready_rx).await??;
        let command = if hangup { CallCommand::Hangup(HangupCommand::local("test", None, None)) } else { CallCommand::StopQualityCheck };
        env.registry.get_handle(&session_id).unwrap().send_command(command)?;
        tokio::time::timeout(Duration::from_secs(4), service).await??;
        if !hangup {
            wait_for_event(&mut tap, |e| e.event_type == "transcript_ended", Duration::from_secs(3)).await.expect("stop completed");
            assert!(env.registry.get_handle(&session_id).is_some());
        }
    }
    Ok(())
}

/// Explicit stop is bounded even when the service never reads Close.
#[tokio::test]
async fn test_quality_unresponsive_service_is_bounded() -> Result<()> {
    use rustpbx::call::quality::QualitySides;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}", listener.local_addr()?);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_text());
        ready_tx.send(()).unwrap();
        // Do not poll the socket: no automatic Pong or Close response.
        let _ = release_rx.await;
        drop(ws);
    });
    let env = start_transcript_server(QualityConfig { url, enabled: true, sides: QualitySides::Callee, ..Default::default() }).await?;
    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let mut tap = env.gateway.read().subscribe_events();
    let session_id = establish_call(&alice, &bob, &env.registry).await?;
    tokio::time::timeout(Duration::from_secs(3), ready_rx).await??;
    env.registry.get_handle(&session_id).unwrap().send_command(CallCommand::StopQualityCheck)?;
    wait_for_event(&mut tap, |e| e.event_type == "transcript_ended", Duration::from_secs(3)).await.expect("bounded connection cleanup");
    assert!(env.registry.get_handle(&session_id).is_some());
    let _ = release_tx.send(());
    service.await?;
    Ok(())
}

/// Receive silence closes inspection at the configured timeout without hanging up the call.
#[tokio::test]
async fn test_quality_receive_timeout_keeps_call_running() -> Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    use rustpbx::call::quality::QualitySides;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}", listener.local_addr()?);
    let service = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_text());
        let started = tokio::time::Instant::now();
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(_) => {},
                Message::Close(_) => {
                    assert!(started.elapsed() >= Duration::from_secs(1));
                    let _ = ws.flush().await;
                    break;
                },
                other => panic!("unexpected proactive PBX frame: {other:?}"),
            }
        }
    });
    let env = start_transcript_server(QualityConfig { url, enabled: true, sides: QualitySides::Callee, receive_timeout_secs: 2, ..Default::default() }).await?;
    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let session_id = establish_call(&alice, &bob, &env.registry).await?;
    tokio::time::timeout(Duration::from_secs(5), service).await??;
    assert!(env.registry.get_handle(&session_id).is_some(), "receive timeout must not hang up the call");
    Ok(())
}

/// Both service Ping and business messages renew each leg's receive deadline.
#[tokio::test]
async fn test_quality_service_messages_keep_both_sides_alive() -> Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}", listener.local_addr()?);
    let (alive_tx, mut alive_rx) = tokio::sync::mpsc::channel(2);
    let service = tokio::spawn(async move {
        let mut peers = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let alive_tx = alive_tx.clone();
            peers.spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let start: serde_json::Value = serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
                let ping = start["side"] == "caller";
                let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(10), Duration::from_secs(10));
                let mut sent = 0;
                loop {
                    tokio::select! {
                        _ = ticks.tick(), if sent < 4 => {
                            let message = if ping { Message::Ping(vec![1].into()) } else {
                                Message::text(r#"{"type":"text","text":"still inspecting","start_ms":0,"end_ms":20}"#)
                            };
                            ws.send(message).await.unwrap();
                            sent += 1;
                            if sent == 4 { alive_tx.send(()).await.unwrap(); }
                        },
                        message = ws.next() => match message.unwrap().unwrap() {
                            Message::Binary(_) | Message::Pong(_) => {},
                            Message::Close(_) => {
                                assert_eq!(sent, 4, "service activity must prevent receive timeout");
                                let _ = ws.flush().await;
                                break;
                            },
                            other => panic!("unexpected proactive PBX frame: {other:?}"),
                        }
                    }
                }
            });
        }
        while let Some(result) = peers.join_next().await { result.unwrap(); }
    });
    let env = start_transcript_server(QualityConfig { url, enabled: true, ..Default::default() }).await?;
    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let mut tap = env.gateway.read().subscribe_events();
    let session_id = establish_call(&alice, &bob, &env.registry).await?;
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(43), alive_rx.recv()).await?.expect("quality side stayed connected");
    }
    env.registry.get_handle(&session_id).unwrap().send_command(CallCommand::StopQualityCheck)?;
    wait_for_event(&mut tap, |e| e.event_type == "transcript_ended", Duration::from_secs(3)).await.expect("explicit stop");
    tokio::time::timeout(Duration::from_secs(3), service).await??;
    Ok(())
}
