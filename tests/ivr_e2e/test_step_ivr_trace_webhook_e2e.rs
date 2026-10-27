//! Step-IVR `ivr_step_trace` contract over the REAL RWI webhook path.
//!
//! Locks the flow-end trace sequence against silent drops. The webhook
//! handler's former `(call_id, cached_at, event_type)` dedup collapsed the
//! back-to-back same-type burst the executor emits at flow end, so webhook
//! consumers saw the flow's start but never its end marker:
//!
//! - natural completion: `[session_start] … [audio_complete end=true] +
//!   [session_end end=true end_reason]` — consumers reported "只有一个
//!   audio_complete, 少了一个 end";
//! - barge-in on an interruptible prompt (`prompt_break`): `[dtmf] +
//!   [dtmf end=true] + [session_end end=true]` — consumers reported "prompt
//!   break 只有 start, 没有 end";
//! - caller hangup mid-prompt: `[user_hangup end=true] + [session_end
//!   end=true]`.
//!
//! Each scenario drives a REAL call (SIP + RTP + scripted step-provider
//! HTTP mock) and asserts on the webhook stream:
//!
//! 1. **完整事件集(不能少)** — `call_created → call_answered → N ×
//!    ivr_step_trace → call_hangup`, with the EXACT trace count per
//!    scenario and strict relative ordering;
//! 2. **格式(不能错)** — every envelope carries `rwi`/`event_id`/
//!    RFC3339 `timestamp`/`call_id`/`event_type` + a nested `event` object
//!    that never repeats `event_type`, and each event type's required
//!    fields are present and well-typed.
//!
//! The exact-count assertion is what catches event-dropping regressions
//! (dedup, queue lag, allow-list drift).

use anyhow::{Result, anyhow};
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::media::wav_reader::{SampleFormat, WavSpec, WavWriter};
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteRule};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig, create_test_sdp};
use crate::common::webhook_capture::WebhookCapture;

const IVR_NUMBER: &str = "9300";
const CALLER: &str = "trace_caller";
const SAMPLE_RATE: u32 = 8000;

/// Webhook allow-list: EVERY event type the step-IVR flow must produce.
/// The stream assertions below treat this as a closed set — any event type
/// appearing here must actually arrive, and nothing outside may appear.
const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_answered",
    "ivr_step_trace",
    "call_hangup",
];

// ── Step-provider HTTP mock ────────────────────────────────────────────────

/// Scripted step provider: every `POST /ivr/step` returns the next script
/// entry; lifecycle endpoints (`/start`, `/end`) acknowledge fire-and-forget.
struct StepProviderMock {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn start_step_provider(script: Vec<serde_json::Value>) -> StepProviderMock {
    let idx = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);

    let idx_step = idx.clone();
    let script_step = script.clone();
    let requests_step = requests.clone();
    let app = axum::Router::new()
        .route(
            "/ivr/step",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let idx = idx_step.clone();
                let script = script_step.clone();
                let requests = requests_step.clone();
                async move {
                    requests.lock().unwrap().push(body);
                    let i = idx.fetch_add(1, Ordering::SeqCst);
                    let action = script
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "hangup"}));
                    axum::Json(action)
                }
            }),
        )
        .route(
            "/ivr/step/start",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"status": "ok"}))
            }),
        )
        .route(
            "/ivr/step/end",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"status": "ok"}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    StepProviderMock {
        url: format!("http://127.0.0.1:{port}/ivr/step"),
        requests,
    }
}

impl StepProviderMock {
    /// The provider must have seen exactly one `session_start` followed by
    /// the given event types, in order (provider-side mirror of the flow).
    fn assert_event_sequence(&self, expected: &[&str]) {
        let requests = self.requests.lock().unwrap();
        let events: Vec<String> = requests
            .iter()
            .filter_map(|b| b["event"]["type"].as_str().map(|s| s.to_string()))
            .collect();
        let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            events, expected,
            "provider /step event sequence mismatch; requests: {requests:?}"
        );
    }
}

// ── Harness ────────────────────────────────────────────────────────────────

fn write_sine_wav(path: &Path, freq_hz: f64, duration_ms: u64) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let num_samples = SAMPLE_RATE as u64 * duration_ms / 1000;
    let mut writer = WavWriter::create(path, spec)?;
    for i in 0..num_samples {
        let t = i as f64 / SAMPLE_RATE as f64;
        let sample = (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16;
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn step_ivr_toml(provider_url: &str) -> String {
    format!(
        r#"[ivr]
name = "step-trace-ivr"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 2
retry_delay_ms = 200
timeout_secs = 5
"#
    )
}

async fn start_harness(capture: &WebhookCapture, ivr_file: &Path) -> Result<E2eTestServer> {
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let mut proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.routes = Some(vec![RouteRule {
        name: "route_to_step_ivr_trace".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(IVR_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_file.to_string_lossy(),
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let users = vec![SipUser {
        id: 1,
        username: CALLER.to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        ..Default::default()
    }];

    E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            users,
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await
}

fn make_caller_ua(proxy_addr: std::net::SocketAddr) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: CALLER.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(29000),
        proxy_addr,
    })
}

/// Keep the caller UA's dialog state machine alive so incoming BYEs are
/// answered and `CallTerminated` surfaces.
fn spawn_caller_pump(ua: TestUa) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(_) => {}
                Err(_) => break,
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
}

/// Wait until the webhook capture has seen `event_type`; fails the test on
/// timeout with the types received so far.
async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if events
                .iter()
                .any(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let types: Vec<String> = {
                let events = capture.received.lock().unwrap();
                events
                    .iter()
                    .map(|v| v["event_type"].as_str().unwrap_or("?").to_string())
                    .collect()
            };
            return Err(anyhow!(
                "timed out waiting for webhook event '{event_type}'; got {types:?}"
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Collect the call's `ivr_step_trace` payloads in delivery order.
fn collect_step_traces(capture: &WebhookCapture) -> Vec<serde_json::Value> {
    let events = capture.received.lock().unwrap();
    events
        .iter()
        .filter(|v| v["event_type"].as_str() == Some("ivr_step_trace"))
        .map(|v| v["event"].clone())
        .collect()
}

/// Wait until no new step traces arrive for `quiet` ms, then snapshot the
/// whole capture.
async fn settle_stream(capture: &WebhookCapture, quiet_ms: u64) -> Vec<serde_json::Value> {
    let mut last_count = 0usize;
    let mut quiet_since: Option<std::time::Instant> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let count = collect_step_traces(capture).len();
        if count != last_count {
            last_count = count;
            quiet_since = Some(std::time::Instant::now());
        }
        if let Some(since) = quiet_since
            && since.elapsed() >= Duration::from_millis(quiet_ms)
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    capture.received.lock().unwrap().clone()
}

// ── Stream assertions: 必要事件不能少 + 格式不能错 ─────────────────────────

/// Expected shape of one `ivr_step_trace` in the delivered stream.
struct TraceExpect {
    trigger: &'static str,
    /// `None` = the `end` field must be ABSENT (flow continues);
    /// `Some(true)` = the event must carry `end: true`.
    end: Option<bool>,
    /// Expected `trigger.detail.digit` (barge-in steps).
    digit: Option<&'static str>,
    /// Exact `end_reason` — asserted only on `session_end` entries.
    end_reason: Option<&'static str>,
}

fn trace_expect(trigger: &'static str, end: Option<bool>) -> TraceExpect {
    TraceExpect {
        trigger,
        end,
        digit: None,
        end_reason: None,
    }
}

/// Validate the ENTIRE captured webhook stream of one call:
///
/// 1. envelope format of every event (`rwi`, `event_id`, RFC3339
///    `timestamp`, `call_id`, `event_type`, nested `event` without
///    `event_type`);
/// 2. one logical call — a single `call_id` across the stream;
/// 3. the necessary lifecycle events, in order, none missing:
///    `call_created` (first) → `call_answered` (before the first trace) →
///    `ivr_step_trace` × N (exact count + ordered trigger/end/digit/
///    end_reason contract) → `call_hangup` (exactly one session-scoped,
///    after `call_created`);
/// 4. per-type required fields, well-typed.
async fn assert_webhook_stream(
    capture: &WebhookCapture,
    expected_traces: &[TraceExpect],
    // Allowed `hangup_by` on the session-scoped call_hangup for this scenario.
    session_hangup_by: &[&str],
) -> Result<()> {
    let events = settle_stream(capture, 400).await;
    assert!(
        !events.is_empty(),
        "webhook stream must not be empty — the allow-list {WEBHOOK_EVENTS:?} \
         must produce at least one event"
    );

    // ── 1. envelope format: every event, no exceptions ──
    for (i, body) in events.iter().enumerate() {
        let ctx = |what: &str| format!("envelope[{i}] {what}: {body}");
        assert_eq!(
            body["rwi"].as_str(),
            Some("1.0"),
            "{}",
            ctx("must carry rwi=\"1.0\"")
        );
        assert!(
            body["event_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "{}",
            ctx("must carry a non-empty event_id idempotency key")
        );
        let ts = body["timestamp"].as_str().unwrap_or_else(|| {
            panic!("{}", ctx("must carry a string timestamp"))
        });
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("{}", ctx(&format!("timestamp must be RFC3339: {e}"))));
        assert!(
            body["call_id"].as_str().is_some_and(|s| !s.is_empty()),
            "{}",
            ctx("must carry a non-empty call_id")
        );
        assert!(
            body["event_type"].as_str().is_some_and(|s| !s.is_empty()),
            "{}",
            ctx("must carry a non-empty event_type")
        );
        let event = body["event"]
            .as_object()
            .unwrap_or_else(|| panic!("{}", ctx("must carry a nested event object")));
        assert!(
            !event.contains_key("event_type"),
            "{}",
            ctx("nested event must NOT repeat event_type")
        );
    }

    // ── 2. one logical call ──
    let call_ids: HashSet<&str> = events
        .iter()
        .filter_map(|b| b["call_id"].as_str())
        .collect();
    assert_eq!(
        call_ids.len(),
        1,
        "one scenario drives exactly one logical call — got call_ids {call_ids:?}"
    );
    let call_id = call_ids.into_iter().next().unwrap();

    // ── 3. necessary lifecycle events: none missing, ordered ──
    let types: Vec<&str> = events
        .iter()
        .map(|b| b["event_type"].as_str().unwrap_or("?"))
        .collect();
    let unknown: Vec<&str> = types
        .iter()
        .filter(|t| !WEBHOOK_EVENTS.contains(t))
        .copied()
        .collect();
    assert!(
        unknown.is_empty(),
        "unexpected event types in the stream (allow-list drift): {unknown:?}"
    );

    let first = types.first().copied().unwrap();
    assert_eq!(
        first, "call_created",
        "call_created must be the first event of the flow, got {types:?}"
    );
    let created_pos = 0;
    let answered_positions: Vec<usize> = types
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == "call_answered")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !answered_positions.is_empty(),
        "the IVR answers the caller leg — call_answered must not be missing: {types:?}"
    );
    let trace_positions: Vec<usize> = types
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == "ivr_step_trace")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        trace_positions.len(),
        expected_traces.len(),
        "expected exactly {} ivr_step_trace events (不能少), got {} — stream: {types:?}",
        expected_traces.len(),
        trace_positions.len()
    );
    assert!(
        answered_positions
            .iter()
            .all(|a| trace_positions.first().is_none_or(|first| a < first)),
        "call_answered must precede the first ivr_step_trace: {types:?}"
    );
    let hangup_positions: Vec<usize> = types
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == "call_hangup")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !hangup_positions.is_empty(),
        "call_hangup must not be missing: {types:?}"
    );
    assert!(
        hangup_positions.iter().all(|h| *h > created_pos),
        "call_hangup must follow call_created: {types:?}"
    );

    // ── 4a. call_created required fields ──
    for body in events.iter().filter(|b| b["event_type"] == "call_created") {
        let ev = &body["event"];
        assert_eq!(
            ev["call_id"].as_str(),
            Some(call_id),
            "call_created.call_id must match the envelope: {body}"
        );
        assert!(
            ev["caller"].as_str().is_some_and(|s| !s.is_empty()),
            "call_created.caller required: {body}"
        );
        assert!(
            ev["callee"].as_str().is_some_and(|s| !s.is_empty()),
            "call_created.callee required: {body}"
        );
        assert!(
            ev["context"].is_string(),
            "call_created.context required (string): {body}"
        );
        assert!(
            ev["sip_headers"].is_object(),
            "call_created.sip_headers required (object): {body}"
        );
    }

    // ── 4b. call_answered: the IVR answered the caller leg ──
    for body in events.iter().filter(|b| b["event_type"] == "call_answered") {
        let ev = &body["event"];
        assert_eq!(
            ev["call_id"].as_str(),
            Some(call_id),
            "call_answered.call_id must match the envelope: {body}"
        );
        assert_eq!(
            ev["leg_id"].as_str(),
            Some("caller"),
            "the auto-answer targets the caller leg: {body}"
        );
    }

    // ── 4c. ivr_step_trace: ordered trigger/end/digit/end_reason contract ──
    let traces: Vec<&serde_json::Value> = events
        .iter()
        .filter(|b| b["event_type"] == "ivr_step_trace")
        .map(|b| &b["event"])
        .collect();
    for (i, (trace, expect)) in traces.iter().zip(expected_traces).enumerate() {
        let ctx = |what: &str| format!("trace[{i}] {what}: {trace}");
        assert_eq!(
            trace["call_id"].as_str(),
            Some(call_id),
            "{}",
            ctx("call_id must match the envelope")
        );
        assert_eq!(
            trace["trigger"]["type"].as_str(),
            Some(expect.trigger),
            "{}",
            ctx("trigger.type mismatch (ordered sequence)")
        );
        match expect.end {
            None => assert!(
                trace.get("end").is_none(),
                "{}",
                ctx("flow continues here — the end field must be ABSENT")
            ),
            Some(want) => assert_eq!(
                trace["end"].as_bool(),
                Some(want),
                "{}",
                ctx("end marker mismatch")
            ),
        }
        if let Some(digit) = expect.digit {
            assert_eq!(
                trace["trigger"]["detail"]["digit"].as_str(),
                Some(digit),
                "{}",
                ctx("barge-in digit must surface in trigger.detail.digit")
            );
        }
        match expect.end_reason {
            Some(reason) => assert_eq!(
                trace["end_reason"].as_str(),
                Some(reason),
                "{}",
                ctx("end_reason mismatch")
            ),
            None => {}
        }
        // Completion marker: both stamps, end >= start, duration matches.
        let start = trace["step_start_time"]
            .as_str()
            .unwrap_or_else(|| panic!("{}", ctx("step_start_time required (string)")));
        let end = trace["step_end_time"]
            .as_str()
            .unwrap_or_else(|| panic!("{}", ctx("step_end_time required (completion marker)")));
        let start_t = chrono::DateTime::parse_from_rfc3339(start).unwrap_or_else(|e| {
            panic!(
                "{}",
                ctx(&format!("step_start_time must be RFC3339: {e}"))
            )
        });
        let end_t = chrono::DateTime::parse_from_rfc3339(end).unwrap_or_else(|e| {
            panic!("{}", ctx(&format!("step_end_time must be RFC3339: {e}")))
        });
        assert!(
            end_t >= start_t,
            "{}",
            ctx("step_end_time must not precede step_start_time")
        );
        let duration = trace["duration_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("{}", ctx("duration_ms required (u64)")));
        let stamp_ms = (end_t - start_t).num_milliseconds();
        assert!(
            (duration as i64 - stamp_ms).abs() <= 50,
            "{}",
            ctx(&format!(
                "duration_ms ({duration}) must match step_end_time - step_start_time ({stamp_ms}ms, ±50ms)"
            ))
        );
    }

    // ── 4d. call_hangup: exactly one session-scoped, well-typed ──
    let hangups: Vec<&serde_json::Value> = events
        .iter()
        .filter(|b| b["event_type"] == "call_hangup")
        .map(|b| &b["event"])
        .collect();
    let session_scoped: Vec<&&serde_json::Value> = hangups
        .iter()
        .filter(|h| h["scope"].as_str() == Some("call"))
        .collect();
    assert_eq!(
        session_scoped.len(),
        1,
        "exactly one session-scoped call_hangup expected, got: {hangups:?}"
    );
    for h in &hangups {
        assert!(
            matches!(h["scope"].as_str(), Some("call") | Some("leg")),
            "call_hangup.scope must be call|leg: {h}"
        );
        assert_eq!(
            h["call_id"].as_str(),
            Some(call_id),
            "call_hangup.call_id must match the envelope: {h}"
        );
        assert!(
            h["hangup_by"]
                .as_str()
                .is_none_or(|s| !s.is_empty()),
            "hangup_by, when present, must be a non-empty string: {h}"
        );
        assert!(
            h["sip_status"].is_null() || h["sip_status"].is_u64(),
            "sip_status must be null or a number: {h}"
        );
    }
    let session_hangup = *session_scoped[0];
    assert!(
        session_hangup_by.contains(&session_hangup["hangup_by"].as_str().unwrap_or("")),
        "session-scoped hangup_by expected in {session_hangup_by:?}, got: {session_hangup}"
    );
    assert!(
        session_hangup["duration_secs"].is_u64(),
        "session-scoped hangup of an answered call must carry duration_secs (u64): {session_hangup}"
    );

    Ok(())
}

// ── Scenarios ──────────────────────────────────────────────────────────────

/// Scenario A — natural completion: `[prompt, hangup]` script, caller listens.
///
/// Contract: exactly THREE traces — prompt step (`session_start`, flow
/// continues), hangup step (`audio_complete`, `end=true`), session close
/// (`session_end`, `end=true`, `end_reason=hangup`). The former webhook
/// dedup collapsed the tail burst and dropped the end markers.
#[tokio::test]
async fn test_step_ivr_trace_natural_completion_full_sequence() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let prompt_wav = std::env::temp_dir().join("rustpbx_e2e_step_trace_prompt_a.wav");
    write_sine_wav(&prompt_wav, 530.0, 800)?;

    let provider = start_step_provider(vec![
        serde_json::json!({
            "type": "prompt",
            "file": prompt_wav.to_string_lossy(),
            "interruptible": false,
        }),
        serde_json::json!({"type": "hangup"}),
    ])
    .await;

    let capture = WebhookCapture::start().await;
    let ivr_path: PathBuf = std::env::temp_dir().join(format!(
        "step-trace-ivr-a-{}.toml",
        portpicker::pick_unused_port().unwrap_or(43000)
    ));
    std::fs::write(&ivr_path, step_ivr_toml(&provider.url))?;

    let server = start_harness(&capture, &ivr_path).await?;
    let mut caller = make_caller_ua(server.proxy_addr);
    caller.start().await?;
    let pump = spawn_caller_pump(caller.clone());

    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(31000),
        false,
    );
    let _dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;

    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(20)).await?;
    assert_webhook_stream(
        &capture,
        &[
            trace_expect("session_start", None),
            trace_expect("audio_complete", Some(true)),
            TraceExpect {
                end_reason: Some("hangup"),
                ..trace_expect("session_end", Some(true))
            },
        ],
        // The IVR's hangup node issues the BYE locally → system.
        &["system"],
    )
    .await?;

    // Provider-side mirror: session_start then audio_complete, nothing else.
    provider.assert_event_sequence(&["session_start", "audio_complete"]);

    let _ = pump;
    Ok(())
}

/// Scenario B — barge-in on an interruptible prompt (`prompt_break`
/// semantics): `[prompt(interruptible), hangup]` script, key press mid-play.
///
/// Contract: exactly THREE traces — barged prompt step (`dtmf` + digit,
/// flow continues), hangup step (`dtmf`, `end=true`), session close
/// (`session_end`, `end=true`). All three used to collapse into the dedup
/// key, leaving consumers with a single start event and no end.
#[tokio::test]
async fn test_step_ivr_trace_barge_in_full_sequence() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    // Long enough that the key press reliably barges in mid-playback.
    let prompt_wav = std::env::temp_dir().join("rustpbx_e2e_step_trace_prompt_b.wav");
    write_sine_wav(&prompt_wav, 700.0, 6000)?;

    let provider = start_step_provider(vec![
        serde_json::json!({
            "type": "prompt",
            "file": prompt_wav.to_string_lossy(),
            "interruptible": true,
        }),
        serde_json::json!({"type": "hangup"}),
    ])
    .await;

    let capture = WebhookCapture::start().await;
    let ivr_path: PathBuf = std::env::temp_dir().join(format!(
        "step-trace-ivr-b-{}.toml",
        portpicker::pick_unused_port().unwrap_or(44000)
    ));
    std::fs::write(&ivr_path, step_ivr_toml(&provider.url))?;

    let server = start_harness(&capture, &ivr_path).await?;
    let mut caller = make_caller_ua(server.proxy_addr);
    caller.start().await?;
    let pump = spawn_caller_pump(caller.clone());

    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(32000),
        false,
    );
    let dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;

    // Give the prompt time to start playing, then barge in.
    sleep(Duration::from_millis(800)).await;
    caller.send_dtmf_info(&dialog, "5").await?;

    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(20)).await?;
    assert_webhook_stream(
        &capture,
        &[
            TraceExpect {
                digit: Some("5"),
                ..trace_expect("dtmf", None)
            },
            TraceExpect {
                digit: Some("5"),
                ..trace_expect("dtmf", Some(true))
            },
            TraceExpect {
                end_reason: Some("hangup"),
                ..trace_expect("session_end", Some(true))
            },
        ],
        // The provider's hangup node issues the BYE locally → system.
        &["system"],
    )
    .await?;

    // Provider-side mirror: session_start, then the barge-in digit.
    provider.assert_event_sequence(&["session_start", "dtmf"]);

    let _ = pump;
    Ok(())
}

/// Scenario C — caller hangs up mid-prompt: `[prompt]` (long) and the caller
/// BYEs while it plays.
///
/// Contract: exactly TWO traces — the in-flight prompt step whose trigger is
/// rewritten to `user_hangup` (`end=true`), and the `session_end` entry
/// (`end=true`, `end_reason=user_hangup`). Both are emitted back-to-back in
/// `on_exit` with no await between them — precisely the burst shape the old
/// dedup collapsed.
#[tokio::test]
async fn test_step_ivr_trace_caller_hangup_mid_prompt() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    let prompt_wav = std::env::temp_dir().join("rustpbx_e2e_step_trace_prompt_c.wav");
    write_sine_wav(&prompt_wav, 850.0, 6000)?;

    let provider = start_step_provider(vec![serde_json::json!({
        "type": "prompt",
        "file": prompt_wav.to_string_lossy(),
        "interruptible": false,
    })])
    .await;

    let capture = WebhookCapture::start().await;
    let ivr_path: PathBuf = std::env::temp_dir().join(format!(
        "step-trace-ivr-c-{}.toml",
        portpicker::pick_unused_port().unwrap_or(45000)
    ));
    std::fs::write(&ivr_path, step_ivr_toml(&provider.url))?;

    let server = start_harness(&capture, &ivr_path).await?;
    let mut caller = make_caller_ua(server.proxy_addr);
    caller.start().await?;
    let pump = spawn_caller_pump(caller.clone());

    let offer = create_test_sdp(
        "127.0.0.1",
        portpicker::pick_unused_port().unwrap_or(33000),
        false,
    );
    let dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;

    // Hang up while the 6s prompt is still playing.
    sleep(Duration::from_millis(600)).await;
    caller.hangup(&dialog).await?;

    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(20)).await?;
    assert_webhook_stream(
        &capture,
        &[
            trace_expect("user_hangup", Some(true)),
            TraceExpect {
                end_reason: Some("user_hangup"),
                ..trace_expect("session_end", Some(true))
            },
        ],
        // Inbound call, the caller leg's remote sent the BYE. Documented
        // normalization maps the initiator to "caller"; the legacy non-CC
        // fallback is "callee" — both are contract-legal.
        &["caller", "callee"],
    )
    .await?;

    // Provider-side mirror: only session_start — the caller hung up before
    // any completion event; the /end notification is fire-and-forget.
    provider.assert_event_sequence(&["session_start"]);

    let _ = pump;
    Ok(())
}
