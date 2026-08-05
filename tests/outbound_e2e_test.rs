//! E2E tests for the outbound dial SSE interface.
//!
//! The SSE stream is pure RWI event passthrough — tests assert on RWI event
//! type names (call_initiated, call_ringing, call_answered, etc.).

#![cfg(test)]

mod helpers;

use helpers::test_server::TestPbx;
use portpicker::pick_unused_port;
use rsipstack::{
    EndpointBuilder,
    sip::{Method, StatusCode, headers::Header},
    transport::{TransportLayer, udp::UdpConnection},
};
use rustpbx::config::{OutboundConfig, ProxyConfig};
use rustpbx::outbound::{OutboundContext, request::OnAnswer, api::execute_dial_core};
use rustpbx::outbound::events::SseEntry;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

// ── Mock SIP UAS ────────────────────────────────────────────────────────────

struct MockUas {
    _cancel: CancellationToken,
    port: u16,
}

impl MockUas {
    async fn start(reply_code: u16) -> Self {
        let port = pick_unused_port().unwrap();
        let cancel = CancellationToken::new();

        let tl = TransportLayer::new(cancel.child_token());
        let udp = UdpConnection::create_connection(
            format!("127.0.0.1:{port}").parse().unwrap(),
            None,
            Some(cancel.child_token()),
        )
        .await
        .unwrap();
        tl.add_transport(udp.into());

        let mut builder = EndpointBuilder::new();
        builder.with_user_agent("mock-uas/1.0");
        builder.with_transport_layer(tl);
        builder.with_cancel_token(cancel.child_token());
        builder.with_timer_interval(Duration::from_millis(50));
        let endpoint = builder.build();

        let ep_inner = endpoint.inner.clone();
        let ct = cancel.clone();
        rustpbx::utils::spawn(async move {
            tokio::select! {
                _ = ct.cancelled() => {}
                r = ep_inner.serve() => {
                    if let Err(e) = r { warn!("mock-uas serve: {e}"); }
                }
            }
        });

        let mut rx = endpoint.incoming_transactions().unwrap();
        let ct2 = cancel.clone();
        let uas_port = port;
        rustpbx::utils::spawn(async move {
            loop {
                tokio::select! {
                    _ = ct2.cancelled() => break,
                    tx_opt = rx.recv() => {
                        if let Some(mut tx) = tx_opt {
                            if tx.original.method == Method::Invite {
                                tx.reply(StatusCode::Trying).await.ok();
                                tokio::time::sleep(Duration::from_millis(50)).await;

                                if reply_code == 200 {
                                    tx.reply(StatusCode::Ringing).await.ok();
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    let contact = Header::from(
                                        rsipstack::sip::headers::typed::Contact::parse(
                                            &format!("sip:uas@127.0.0.1:{}", uas_port),
                                        ).unwrap(),
                                    );
                                    let sdp = format!(
                                        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\n\
                                         c=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                                         m=audio 5004 RTP/AVP 0\r\n\
                                         a=rtpmap:0 PCMU/8000\r\n"
                                    );
                                    tx.reply_with(
                                        StatusCode::OK,
                                        vec![contact],
                                        Some(sdp.into_bytes()),
                                    ).await.ok();
                                } else if reply_code == 486 {
                                    tx.reply(StatusCode::BusyHere).await.ok();
                                } else if reply_code == 408 {
                                    // no reply
                                } else {
                                    tx.reply(StatusCode::Other(reply_code, "Error".into()))
                                        .await.ok();
                                }
                            }
                        }
                    }
                }
            }
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        Self { _cancel: cancel, port }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

// ── Test helpers ────────────────────────────────────────────────────────────

async fn setup_pbx() -> TestPbx {
    let sip_port = pick_unused_port().unwrap();
    let pbx = TestPbx::start_with_inject(
        sip_port,
        helpers::test_server::TestPbxInject {
            proxy_config: Some(ProxyConfig {
                addr: "127.0.0.1".to_string(),
                udp_port: Some(sip_port),
                realms: Some(vec!["127.0.0.1".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    pbx
}

fn make_ctx(pbx: &TestPbx) -> OutboundContext {
    OutboundContext {
        sip_server: pbx.sip_server.clone().expect("sip_server"),
        gateway: pbx.gateway.clone(),
        call_registry: pbx.registry.clone(),
        conference_manager: pbx.conference_manager.clone().expect("conference_manager"),
        http_client: reqwest::Client::new(),
        config: OutboundConfig {
            default_ring_timeout: 10,
            default_answer_timeout: 15,
            default_webhook_timeout: 3,
            ..Default::default()
        },
    }
}

/// Drain SseEntry events until terminal or timeout.
async fn collect_sse(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SseEntry>,
    deadline: Duration,
) -> Vec<SseEntry> {
    let mut events = Vec::new();
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let remaining = end.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(entry)) => {
                let is_terminal = matches!(
                    entry.event.as_str(),
                    "call_answered" | "call_busy" | "call_no_answer" | "call_hangup"
                );
                events.push(entry);
                if is_terminal {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    events
}

fn event_names(events: &[SseEntry]) -> Vec<&str> {
    events.iter().map(|e| e.event.as_str()).collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sip_originate_answer_success() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(200).await;
    let ctx = make_ctx(&pbx);

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-sip-ok".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(10),
        on_answer: OnAnswer::ExecuteFlow,
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(15)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);

    assert!(names.contains(&"call_initiated"), "expected call_initiated, got: {:?}", names);
    assert!(names.contains(&"call_ringing"), "expected call_ringing, got: {:?}", names);
    assert!(names.contains(&"call_answered"), "expected call_answered, got: {:?}", names);
    pbx.stop();
}

#[tokio::test]
async fn test_sip_originate_busy_failure() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(486).await;
    let ctx = make_ctx(&pbx);

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-sip-busy".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(10),
        on_answer: OnAnswer::ExecuteFlow,
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(15)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);

    assert!(names.contains(&"call_initiated"), "expected call_initiated");
    assert!(names.contains(&"call_busy"), "expected call_busy, got: {:?}", names);
    pbx.stop();
}

#[tokio::test]
async fn test_sip_originate_timeout_failure() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(408).await;
    let ctx = make_ctx(&pbx);

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-sip-timeout".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(3),
        on_answer: OnAnswer::ExecuteFlow,
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(20)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);

    assert!(names.contains(&"call_initiated"), "expected call_initiated");
    assert!(
        names.contains(&"call_no_answer") || names.contains(&"call_hangup"),
        "expected failure event (timeout), got: {:?}", names
    );
    pbx.stop();
}

#[tokio::test]
async fn test_execute_flow_after_answer() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(200).await;
    let ctx = make_ctx(&pbx);

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-flow".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(10),
        on_answer: OnAnswer::ExecuteFlow,
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(15)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);
    assert!(names.contains(&"call_answered"), "expected call_answered");
    pbx.stop();
}

#[tokio::test]
async fn test_app_after_answer() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(200).await;
    let ctx = make_ctx(&pbx);

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-app".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(10),
        on_answer: OnAnswer::App {
            app_name: "voicemail".to_string(),
            app_params: Default::default(),
        },
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(15)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);
    assert!(names.contains(&"call_answered"), "expected call_answered");
    pbx.stop();
}

#[tokio::test]
async fn test_webhook_instruction() {
    let _ = tracing_subscriber::fmt::try_init();
    let pbx = setup_pbx().await;
    let uas = MockUas::start(200).await;

    let webhook_port = pick_unused_port().unwrap();
    let webhook_app = axum::Router::new().route(
        "/handle",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({"action": "hangup"}))
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", webhook_port))
        .await
        .unwrap();
    rustpbx::utils::spawn(async move {
        axum::serve(listener, webhook_app).await.ok();
    });

    let mut ctx = make_ctx(&pbx);
    ctx.config.default_webhook_timeout = 5;

    let req = rustpbx::outbound::DialRequest {
        call_id: Some("e2e-webhook".to_string()),
        caller_id: Some(format!("sip:test@{}", pbx.sip_host())),
        destination: format!("sip:callee@{}", uas.addr()),
        trunk: None,
        extra_headers: Default::default(),
        ring_timeout: Some(10),
        on_answer: OnAnswer::Webhook(rustpbx::outbound::request::WebhookAction {
            url: format!("http://127.0.0.1:{}/handle", webhook_port),
            headers: Default::default(),
            timeout_secs: Some(3),
            fallback: rustpbx::outbound::request::FallbackAction::Hangup,
        }),
        on_failure: None,
        metadata: Default::default(),
    };

    let rx = execute_dial_core(ctx, req).await.expect("originate ok");
    let events = collect_sse(rx, Duration::from_secs(15)).await;
    let names = event_names(&events);

    info!("events: {:?}", names);
    assert!(names.contains(&"call_answered"), "expected call_answered");
    pbx.stop();
}

#[tokio::test]
async fn test_gateway_event_tap_delivers_events() {
    use rustpbx::rwi::{RwiGateway, event::to_legacy_event};

    let _ = tracing_subscriber::fmt::try_init();

    let gw = std::sync::Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
    let mut rx = gw.read().subscribe_events();

    let call_id = "tap-test-call".to_string();
    gw.read().send_event_to_call_owner(
        &call_id,
        &to_legacy_event(
            &rustpbx::rwi::CallInitiated {
                call_id: call_id.clone(),
                destination: "sip:test@127.0.0.1".to_string(),
            },
            None,
        ),
    );

    let entry = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("tap recv timeout")
        .expect("tap channel closed");

    assert_eq!(entry.call_id, call_id);
    assert_eq!(entry.event.event_type, "call_initiated");
}
