//! End-to-end coverage for the unified `call_error` pipeline:
//!
//! * a pre-session failure (callee offline at routing time) emits a
//!   `call_error` RWI event with `stage = "routing"` and writes a
//!   `TraceKind::Error` entry into the CDR `metadata["trace"]`;
//! * an in-session failure (TTS synthesis for an IVR greeting) emits a
//!   `call_error` RWI event with `stage = "tts"` through the session's
//!   `ReportCallError` path and writes the matching CDR trace entry.
//!
//! Both are driven through a real SIP stack + RWI webhook, so they exercise
//! the whole chain: routing/session → log → `call_error` event → CDR trace.

use anyhow::Result;
use axum::{Router, http::StatusCode, routing::post};
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteRule};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::webhook_capture::WebhookCapture;

/// Event types the webhook must forward for these tests.
const WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_error",
    "call_hangup",
    "ivr_node_entered",
];

fn gateway_with_webhook(capture: &WebhookCapture) -> RwiGatewayRef {
    Arc::new(parking_lot::RwLock::new({
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
    }))
}

fn base_proxy_config() -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ensure_user: Some(false),
        enable_latching: false,
        ..Default::default()
    }
}

async fn wait_webhook_event_matching<F>(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
    predicate: F,
) -> Option<serde_json::Value>
where
    F: Fn(&serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if let Some(ev) = events
                .iter()
                .find(|v| v["event_type"].as_str() == Some(event_type) && predicate(v))
            {
                return Some(ev.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Assert a CDR `metadata["trace"]` error entry with the given registry code
/// exists (the unified `TraceKind::Error` entry written by the reporter /
/// `ReportCallError`).
fn assert_cdr_error_trace(record: &rustpbx::callrecord::CallRecord, expected_code: &str) {
    let metadata = record
        .details
        .metadata
        .as_ref()
        .expect("CDR metadata present");
    let trace = metadata
        .get("trace")
        .and_then(|v| v.as_array())
        .expect("CDR metadata['trace'] is an array");
    let error_entry = trace
        .iter()
        .find(|ev| ev["kind"] == "error")
        .unwrap_or_else(|| panic!("no TraceKind::Error entry in trace: {trace:?}"));
    assert_eq!(
        error_entry["code"].as_str(),
        Some(expected_code),
        "error trace entry code mismatch: {error_entry}"
    );
    // Severity + a human message must be present so the console trace tab
    // renders the entry (error/warn/info chip + message).
    assert!(
        error_entry["severity"].is_string(),
        "error trace entry missing severity: {error_entry}"
    );
    assert!(
        error_entry["message"].is_string(),
        "error trace entry missing message: {error_entry}"
    );
}

/// Inbound INVITE to an unregistered extension → routing rejects with 480
/// (`proxy.callee_offline`); the unified pipeline must emit `call_error`
/// (stage=routing) and persist an error trace in the CDR.
#[tokio::test]
async fn test_routing_failure_emits_call_error_and_cdr_trace() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let gateway = gateway_with_webhook(&capture);

    let server = E2eTestServer::start_with_inject(
        base_proxy_config(),
        E2eTestServerInject {
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await?;

    let alice = Arc::new(server.create_ua("alice").await?);
    sleep(Duration::from_millis(200)).await;

    // Unregistered extension: user backend miss + empty locator on the same
    // realm ⇒ CalleeOfflineMarker ⇒ 480. `report_failure` runs before any
    // SipSession exists, i.e. this is the pre-session path.
    let call = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call("9999", None).await }
    });

    let event =
        wait_webhook_event_matching(&capture, "call_error", Duration::from_secs(10), |ev| {
            ev["event"]["stage"].as_str() == Some("routing")
        })
        .await
        .expect("webhook must receive a routing call_error for an offline callee");
    let code = event["event"]["code"]
        .as_str()
        .expect("call_error must carry a registry code")
        .to_string();
    assert_eq!(
        code, "proxy.callee_offline",
        "routing failure code must be the offline catalog entry: {event}"
    );
    assert!(
        event["event"]["severity"].is_string(),
        "call_error severity missing: {event}"
    );
    let call_id = event["event"]["call_id"]
        .as_str()
        .expect("call_error must carry call_id")
        .to_string();

    let record = server
        .cdr_capture
        .wait_for_record(&call_id, Duration::from_secs(10))
        .await
        .expect("CDR record for the rejected call");
    assert_cdr_error_trace(&record, &code);

    call.abort();
    server.stop();
    Ok(())
}

/// Tree IVR whose greeting uses TTS (`greeting_text`) with no TTS service
/// configured: the prompt resolution fails and the session must emit a
/// `call_error` (stage=tts) and persist the matching CDR error trace.
#[tokio::test]
async fn test_tts_failure_emits_call_error_and_cdr_trace() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let gateway = gateway_with_webhook(&capture);

    let ivr_number = "9201";
    let ivr_file = std::env::temp_dir().join(format!(
        "call-error-tts-ivr-{}.toml",
        portpicker::pick_unused_port().unwrap_or(42000)
    ));
    std::fs::write(
        &ivr_file,
        r#"
[ivr]
name = "call-error-tts-ivr"

[ivr.root]
greeting_text = "hello from tts"
timeout_ms = 3000
max_retries = 1

[[ivr.root.entries]]
key = "9"
action = { type = "hangup" }
"#,
    )?;

    let mut config = base_proxy_config();
    config.routes = Some(vec![RouteRule {
        name: "route_to_tts_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(ivr_number.to_string()),
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

    let server = E2eTestServer::start_with_inject(
        config,
        E2eTestServerInject {
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await?;

    let alice = Arc::new(server.create_ua("alice").await?);
    sleep(Duration::from_millis(200)).await;

    let call = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call(ivr_number, None).await }
    });

    let event =
        wait_webhook_event_matching(&capture, "call_error", Duration::from_secs(15), |ev| {
            ev["event"]["stage"].as_str() == Some("tts")
        })
        .await
        .expect("webhook must receive a tts call_error for the TTS greeting");
    let code = event["event"]["code"]
        .as_str()
        .expect("call_error must carry a registry code")
        .to_string();
    assert!(
        code.starts_with("tts."),
        "TTS failure code must be a tts.* catalog entry, got {code}"
    );
    let call_id = event["event"]["call_id"]
        .as_str()
        .expect("call_error must carry call_id")
        .to_string();

    // The IVR has no audio and a short timeout, so it terminates the call on
    // its own and the CDR is emitted; assert the error trace it carries.
    let record = server
        .cdr_capture
        .wait_for_record(&call_id, Duration::from_secs(20))
        .await
        .expect("CDR record for the TTS-failed IVR call");
    assert_cdr_error_trace(&record, &code);

    call.abort();

    let _ = std::fs::remove_file(&ivr_file);
    server.stop();
    Ok(())
}

/// Inline step-mode IVR whose provider `/step` (and `/fail`) endpoint returns
/// 500: the session must emit a `call_error` with `stage = "ivr_step"` and
/// persist the matching CDR error trace.
#[tokio::test]
async fn test_step_ivr_next_failure_emits_call_error_and_cdr_trace() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Provider mock: every protocol endpoint fails with 500.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/step", post(mock_provider_error))
        .route("/fail", post(mock_provider_error))
        .route("/start", post(mock_provider_error))
        .route("/end", post(mock_provider_error));
    let provider = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let capture = WebhookCapture::start().await;
    let gateway = gateway_with_webhook(&capture);

    let ivr_number = "9202";
    let mut config = base_proxy_config();
    config.routes = Some(vec![RouteRule {
        name: "route_to_step_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(ivr_number.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "mode": "step",
                "url": format!("http://{addr}/step"),
                "name": "call-error-step-ivr",
                // No `fallback`: a provider failure must surface as an error
                // (not be masked by a fallback ActionNode).
                "retry": {
                    "max_retries": 1,
                    "timeout_ms": 500,
                    "delay_ms": 0,
                    "fallback": null
                }
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let server = E2eTestServer::start_with_inject(
        config,
        E2eTestServerInject {
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await?;

    let alice = Arc::new(server.create_ua("alice").await?);
    sleep(Duration::from_millis(200)).await;

    let call = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call(ivr_number, None).await }
    });

    let event =
        wait_webhook_event_matching(&capture, "call_error", Duration::from_secs(15), |ev| {
            ev["event"]["stage"].as_str() == Some("ivr_step")
        })
        .await
        .expect("webhook must receive an ivr_step call_error when the provider /step fails");
    let code = event["event"]["code"]
        .as_str()
        .expect("call_error must carry a registry code")
        .to_string();
    assert_eq!(
        code, "ivr.step_next_failed",
        "first provider failure must be ivr.step_next_failed: {event}"
    );
    let call_id = event["event"]["call_id"]
        .as_str()
        .expect("call_error must carry call_id")
        .to_string();

    let record = server
        .cdr_capture
        .wait_for_record(&call_id, Duration::from_secs(20))
        .await
        .expect("CDR record for the step-IVR-failed call");
    assert_cdr_error_trace(&record, &code);

    call.abort();
    server.stop();
    provider.abort();
    Ok(())
}

async fn mock_provider_error() -> (StatusCode, &'static str) {
    (StatusCode::INTERNAL_SERVER_ERROR, "provider boom")
}

/// Sequential queue whose only static target is an offline local extension:
/// every dial attempt fails, the queue app exhausts its agents and emits a
/// `call_error` with `stage = "queue"` + the matching CDR error trace.
#[tokio::test]
async fn test_queue_no_agents_emits_call_error_and_cdr_trace() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let capture = WebhookCapture::start().await;
    let gateway = gateway_with_webhook(&capture);

    let queue_name = "call_error_q";
    let queue_number = "9203";
    let mut config = base_proxy_config();
    config.queues.insert(
        queue_name.to_string(),
        rustpbx::proxy::routing::RouteQueueConfig {
            name: Some(queue_name.to_string()),
            strategy: rustpbx::proxy::routing::RouteQueueStrategyConfig {
                targets: vec![rustpbx::proxy::routing::RouteQueueTargetConfig {
                    // 1000 is not a provisioned user → every INVITE is
                    // rejected (480) so the queue never reaches an agent.
                    uri: "sip:1000@127.0.0.1".to_string(),
                    label: Some("Offline Agent".to_string()),
                }],
                wait_timeout_secs: Some(2),
                ..Default::default()
            },
            accept_immediately: true,
            ..Default::default()
        },
    );
    config.routes = Some(vec![RouteRule {
        name: "route_to_call_error_q".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(queue_number.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some(queue_name.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let server = E2eTestServer::start_with_inject(
        config,
        E2eTestServerInject {
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await?;

    let alice = Arc::new(server.create_ua("alice").await?);
    sleep(Duration::from_millis(200)).await;

    let call = tokio::spawn({
        let a = alice.clone();
        async move { a.make_call(queue_number, None).await }
    });

    let event =
        wait_webhook_event_matching(&capture, "call_error", Duration::from_secs(20), |ev| {
            ev["event"]["stage"].as_str() == Some("queue")
        })
        .await
        .expect("webhook must receive a queue call_error when no agent can be reached");
    let code = event["event"]["code"]
        .as_str()
        .expect("call_error must carry a registry code")
        .to_string();
    assert!(
        code.starts_with("queue."),
        "queue failure code must be a queue.* catalog entry, got {code}"
    );
    let call_id = event["event"]["call_id"]
        .as_str()
        .expect("call_error must carry call_id")
        .to_string();

    let record = server
        .cdr_capture
        .wait_for_record(&call_id, Duration::from_secs(20))
        .await
        .expect("CDR record for the queue-failed call");
    assert_cdr_error_trace(&record, &code);

    call.abort();
    server.stop();
    Ok(())
}
