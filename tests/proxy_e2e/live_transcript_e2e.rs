//! E2E tests: live transcription orchestration.
//!
//! Covers the pieces this repo wires together for live ASR:
//! - `transcript_final` reaches an `[rwi_webhook]` filtered on finals only
//!   (raw `transcript_segment` events stay out)
//! - REST/`CallCommand` `StartTranscription`/`StopTranscription` drive the
//!   reference-counted orchestration: events (`transcript_started` /
//!   `transcript_segment` / `transcript_final` / `transcript_ended`) flow to
//!   the gateway event tap (the same stream the CC SSE endpoint consumes)
//! - ref-count coexistence: a second `StopTranscription` after the first is a
//!   no-op (exactly one `transcript_ended`)
//! - unknown provider names fail loudly with `transcript_error`
//! - `[proxy.transcript.remote] auto_start = true` starts transcription at
//!   call answer without any command
//!
//! The provider is a registered mock factory (`test-mock`) that emits canned
//! segments on `create` — no network, no real ASR. This also exercises the
//! third-party provider registration path end-to-end.

use std::sync::Arc;
use std::time::Duration;

use crate::common::test_helpers;
use crate::common::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use anyhow::Result;
use async_trait::async_trait;
use rustpbx::call::domain::CallCommand;
use rustpbx::call::transcription::remote::RemoteTranscriptConfig;
use rustpbx::call::transcription::{
    SidePcmFrame, TranscriptSegment, TranscriptSide, TranscriptionEvent,
    TranscriptionProviderFactory, register_transcription_provider,
};
use rustpbx::config::{LocatorWebhookConfig, MediaProxyMode, TranscriptSection};
use rustpbx::rwi::gateway::EventCacheEntry;
use rustpbx::rwi::{
    RwiEvent, RwiGateway, RwiGatewayRef, TranscriptFinal,
    webhook::{WEBHOOK_CHANNEL_SIZE, start_rwi_webhook_handler},
};
use tokio::sync::mpsc;

// ─── Mock provider ───────────────────────────────────────────────────────────

/// Provider that ignores PCM — the factory spawns the event-emission task, so
/// this object has nothing to do beyond keeping `stop` a no-op.
struct SilentProvider;

#[async_trait]
impl rustpbx::call::transcription::TranscriptionProvider for SilentProvider {
    fn push_pcm(&self, _frame: SidePcmFrame) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) {}
}

/// Factory registered under `test-mock`. On `create` it spawns a task that
/// emits one partial then one final segment through the provider event
/// channel — simulating what a real cloud ASR would stream back.
struct MockEmitterFactory;

impl TranscriptionProviderFactory for MockEmitterFactory {
    fn name(&self) -> &str {
        "test-mock"
    }

    fn create(
        &self,
        _call: &rustpbx::call::transcription::TranscriptionCallInfo,
        _sides: &[TranscriptSide],
        events: mpsc::UnboundedSender<TranscriptionEvent>,
        _params: &serde_json::Value,
    ) -> anyhow::Result<Arc<dyn rustpbx::call::transcription::TranscriptionProvider>> {
        tokio::spawn(async move {
            let seg = |text: &str, partial: bool, end_ms: u64| TranscriptSegment {
                side: TranscriptSide::Caller,
                text: text.to_string(),
                partial,
                start_ms: 0,
                end_ms,
                lang: Some("zh".to_string()),
            };
            let _ = events.send(TranscriptionEvent::Segment(seg("你好", true, 300)));
            let _ = events.send(TranscriptionEvent::Segment(seg("你好请问在吗", false, 800)));
        });
        Ok(Arc::new(SilentProvider))
    }
}

fn register_mock_provider() {
    register_transcription_provider(Arc::new(MockEmitterFactory));
}

// ─── Server / call harness ───────────────────────────────────────────────────

struct TestEnv {
    port: u16,
    registry: Arc<rustpbx::proxy::active_call_registry::ActiveProxyCallRegistry>,
    gateway: RwiGatewayRef,
    cancel_token: tokio_util::sync::CancellationToken,
    _server_handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

async fn start_transcript_server(remote: RemoteTranscriptConfig) -> Result<TestEnv> {
    let port = portpicker::pick_unused_port().unwrap_or(15080);
    let mut proxy_config = test_helpers::test_proxy_config(port);
    // All: anchor media through the MediaBridge — transcription taps decoded
    // leg PCM and is rejected in bypass mode.
    proxy_config.media_proxy = MediaProxyMode::All;
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.transcript = Some(TranscriptSection {
        remote: Some(remote),
    });

    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));

    let config = Arc::new(proxy_config);
    let user_backend = rustpbx::proxy::user::MemoryUserBackend::new(None);
    for user in test_helpers::standard_test_users() {
        user_backend.create_user(user).await?;
    }
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let builder = rustpbx::proxy::server::SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(rustpbx::proxy::locator::MemoryLocator::new()))
        .with_cancel_token(cancel_token.clone())
        .with_rwi_gateway(gateway.clone());
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
/// finals receives — raw `transcript_segment` events (partial and final)
/// stay out.
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

    // Broadcast the exact event sequence a transcribed call produces:
    // partial segment → final segment → transcript_final twin. Segments go
    // through broadcast, the final through send_to_owner (the production
    // path in live_transcription).
    let segment = |partial: bool| RwiEvent {
        event_type: "transcript_segment",
        call_id: Some("call-1".to_string()),
        payload: serde_json::json!({
            "call_id": "call-1",
            "side": "caller",
            "text": if partial { "你好" } else { "你好请问在吗" },
            "partial": partial,
            "start_ms": 0u64,
            "end_ms": 800u64,
            "lang": "zh",
        }),
    };
    gateway.read().broadcast_event(&segment(true));
    gateway.read().broadcast_event(&segment(false));
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

/// REST/`CallCommand` start → mock provider streams segments → gateway events
/// (the stream the CC SSE endpoint and `[rwi_webhook]` consume); stop →
/// exactly one `transcript_ended`; second stop is a ref-count no-op.
#[tokio::test]
async fn test_start_stop_transcription_command_flow() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    register_mock_provider();

    let env = start_transcript_server(RemoteTranscriptConfig {
        provider: Some("test-mock".to_string()),
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
        .send_command(CallCommand::StartTranscription { language: None })
        .expect("start_transcription dispatch");

    // started → partial → final (+transcript_final twin).
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
        Some("test-mock"),
        "started event must name the resolved provider"
    );

    let partial = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_segment",
        Duration::from_secs(5),
    )
    .await
    .expect("partial segment event");
    assert_eq!(partial.payload["partial"].as_bool(), Some(true));

    let final_seg = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_segment" && e.payload["partial"].as_bool() == Some(false),
        Duration::from_secs(5),
    )
    .await
    .expect("final segment event");
    assert_eq!(final_seg.payload["text"].as_str(), Some("你好请问在吗"));

    let final_event = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_final",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_final twin event");
    assert_eq!(final_event.payload["text"].as_str(), Some("你好请问在吗"));
    assert_eq!(final_event.payload["provider"].as_str(), Some("test-mock"));

    // Stop → exactly one transcript_ended. A second stop is a ref-count
    // no-op and must not emit another one.
    env.registry
        .get_handle(&session_id)
        .expect("session handle")
        .send_command(CallCommand::StopTranscription)
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
        .send_command(CallCommand::StopTranscription)
        .expect("second stop dispatch");
    let extra_ended = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_ended",
        Duration::from_secs(1),
    )
    .await;
    assert!(
        extra_ended.is_none(),
        "second StopTranscription must not emit another transcript_ended"
    );

    Ok(())
}

/// Unknown provider name → `transcript_error` (never a silent no-op).
#[tokio::test]
async fn test_unknown_provider_reports_error() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let env = start_transcript_server(RemoteTranscriptConfig {
        provider: Some("no-such-provider".to_string()),
        ..Default::default()
    })
    .await?;

    let alice = Arc::new(create_ua(env.port, "alice", "password123").await?);
    let bob = create_ua(env.port, "bob", "password456").await?;
    let mut tap = env.gateway.read().subscribe_events();
    let session_id = establish_call(&alice, &bob, &env.registry).await?;

    env.registry
        .get_handle(&session_id)
        .expect("session handle")
        .send_command(CallCommand::StartTranscription { language: None })
        .expect("dispatch");

    let error = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_error",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_error event");
    let msg = error.payload["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("unknown transcription provider"),
        "error should name the unknown provider, got: {msg}"
    );

    Ok(())
}

/// `[proxy.transcript.remote] auto_start = true` starts transcription at
/// answer with no command at all.
#[tokio::test]
async fn test_auto_start_at_answer() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    register_mock_provider();

    let env = start_transcript_server(RemoteTranscriptConfig {
        provider: Some("test-mock".to_string()),
        auto_start: Some(true),
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

    // The mock's canned segments flow without any command, and the final one
    // produces the transcript_final twin.
    let final_event = wait_for_event(
        &mut tap,
        |e| e.event_type == "transcript_final",
        Duration::from_secs(5),
    )
    .await
    .expect("transcript_final should flow from auto-started transcription");
    assert_eq!(final_event.payload["provider"].as_str(), Some("test-mock"));

    Ok(())
}

/// Re-answer paths (`CallCommand::Answer`, callee re-attach, queue playback)
/// re-enter `accept_call` — auto-start must never double-fire: the running
/// transcription is left untouched (no provider replacement, no second
/// `transcript_started`, reference count preserved).
#[tokio::test]
async fn test_auto_start_does_not_retrigger_on_reanswer() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    register_mock_provider();

    let env = start_transcript_server(RemoteTranscriptConfig {
        provider: Some("test-mock".to_string()),
        auto_start: Some(true),
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
