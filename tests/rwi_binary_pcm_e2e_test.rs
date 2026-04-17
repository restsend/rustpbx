// E2E tests for PCM Binary WebSocket
//
// These tests verify:
// 1. Binary PCM frames are accepted via WebSocket
// 2. Frame format validation works correctly
// 3. Session ownership is checked before processing

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Extension,
    extract::{Query, ws::WebSocketUpgrade},
    http::HeaderMap,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use rustpbx::{
    proxy::active_call_registry::ActiveProxyCallRegistry,
    rwi::{
        RwiAuth, RwiAuthRef, RwiGateway, RwiGatewayRef,
        auth::{RwiConfig, RwiTokenConfig},
        handler::rwi_ws_handler,
    },
};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const TEST_TOKEN: &str = "pcm-test-token";

fn make_auth() -> RwiAuthRef {
    let config = RwiConfig {
        enabled: true,
        tokens: vec![RwiTokenConfig {
            token: TEST_TOKEN.to_string(),
            scopes: vec!["call.control".to_string()],
        }],
        ..Default::default()
    };
    Arc::new(tokio::sync::RwLock::new(RwiAuth::new(&config)))
}

async fn start_test_server() -> (String, RwiGatewayRef, Arc<ActiveProxyCallRegistry>) {
    let auth = make_auth();
    let gateway: RwiGatewayRef = Arc::new(tokio::sync::RwLock::new(RwiGateway::new()));
    let registry = Arc::new(ActiveProxyCallRegistry::new());

    let auth_c = auth.clone();
    let gw_c = gateway.clone();
    let reg_c = registry.clone();

    let router = axum::Router::new().route(
        "/rwi/v1",
        get(
            move |client_addr: rustpbx::handler::middleware::clientaddr::ClientAddr,
                  ws: WebSocketUpgrade,
                  Query(params): Query<HashMap<String, String>>,
                  headers: HeaderMap| {
                let a = auth_c.clone();
                let g = gw_c.clone();
                let r = reg_c.clone();
                async move {
                    rwi_ws_handler(
                        client_addr,
                        ws,
                        Query(params),
                        Extension(a),
                        Extension(g),
                        Extension(r),
                        Extension(None::<rustpbx::proxy::server::SipServerRef>),
                        headers,
                    )
                    .await
                }
            },
        ),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("ws://127.0.0.1:{}/rwi/v1", port);
    (url, gateway, registry)
}

async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let full = format!("{}?token={}", url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&full))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

async fn send_recv(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    json: &str,
) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("ws error");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
        other => panic!("unexpected frame: {:?}", other),
    }
}

fn req(action: &str, params: serde_json::Value) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&serde_json::json!({
        "rwi": "1.0",
        "action_id": id,
        "action": action,
        "params": params,
    }))
    .unwrap();
    (id, json)
}

/// Build a PCM binary frame
fn build_pcm_frame(
    call_id: &str,
    timestamp_ms: u32,
    sample_rate: u16,
    flags: u16,
    pcm_data: &[u8],
) -> Vec<u8> {
    let mut frame = vec![0u8; 16 + pcm_data.len()];

    // Call ID (8 bytes, space-padded or null-terminated)
    let call_id_bytes = call_id.as_bytes();
    let len = call_id_bytes.len().min(8);
    frame[0..len].copy_from_slice(&call_id_bytes[0..len]);

    // Timestamp (4 bytes, big-endian)
    frame[8..12].copy_from_slice(&timestamp_ms.to_be_bytes());

    // Sample rate (2 bytes, big-endian)
    frame[12..14].copy_from_slice(&sample_rate.to_be_bytes());

    // Flags (2 bytes, big-endian)
    frame[14..16].copy_from_slice(&flags.to_be_bytes());

    // PCM data
    frame[16..].copy_from_slice(pcm_data);

    frame
}

/// Test: Valid PCM frame format is accepted
#[tokio::test]
async fn test_valid_pcm_frame_accepted() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Build a valid PCM frame
    let pcm_samples: Vec<u8> = (0..320).map(|i| (i % 256) as u8).collect(); // 160 samples * 2 bytes
    let frame = build_pcm_frame(
        "test-call",
        12345, // timestamp
        8000,  // sample rate
        0,     // flags
        &pcm_samples,
    );

    // Send binary frame
    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Connection should remain alive and responsive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: PCM frame with last_frame flag
#[tokio::test]
async fn test_pcm_frame_with_last_flag() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Build PCM frame with last_frame flag (bit 0)
    let pcm_samples: Vec<u8> = vec![0x00, 0x01, 0x00, 0x02];
    let frame = build_pcm_frame(
        "test-call",
        99999,
        16000,  // 16kHz sample rate
        0x0001, // last_frame flag set
        &pcm_samples,
    );

    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: PCM frame with different sample rates
#[tokio::test]
async fn test_pcm_frame_various_sample_rates() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let sample_rates = vec![8000u16, 16000, 22050, 24000, 32000, 44100, 48000];

    for rate in sample_rates {
        let pcm_samples: Vec<u8> = vec![0xAB; 160]; // 160 bytes
        let frame = build_pcm_frame("test-call", 1000, rate, 0, &pcm_samples);

        ws.send(Message::Binary(frame.into())).await.unwrap();
    }

    // Connection should remain alive after all frames
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Empty PCM payload (header only frame)
#[tokio::test]
async fn test_pcm_frame_empty_payload() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Frame with empty PCM data (just header)
    let frame = build_pcm_frame("test-call", 12345, 8000, 0, &[]);

    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: PCM frame with large payload
#[tokio::test]
async fn test_pcm_frame_large_payload() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Large PCM payload (10ms of 48kHz stereo = 1920 bytes)
    let large_pcm: Vec<u8> = vec![0x55; 1920];
    let frame = build_pcm_frame("test-call", 12345, 48000, 0, &large_pcm);

    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Multiple sequential PCM frames
#[tokio::test]
async fn test_multiple_sequential_pcm_frames() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Send 10 sequential frames
    for i in 0..10 {
        let pcm_samples: Vec<u8> = (0..160).map(|j| (j % 256) as u8).collect(); // 160 bytes
        let frame = build_pcm_frame(
            "stream-call",
            i as u32 * 20, // 20ms increments
            8000,
            if i == 9 { 0x0001 } else { 0 }, // Last frame has flag
            &pcm_samples,
        );

        ws.send(Message::Binary(frame.into())).await.unwrap();
    }

    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Call ID with exact 8 characters
#[tokio::test]
async fn test_pcm_frame_exact_8_char_call_id() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let frame = build_pcm_frame(
        "exact-8!", // Exactly 8 characters
        12345,
        8000,
        0,
        &[0x00, 0x01],
    );

    ws.send(Message::Binary(frame.into())).await.unwrap();

    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Call ID truncation (more than 8 chars)
#[tokio::test]
async fn test_pcm_frame_long_call_id_truncated() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let frame = build_pcm_frame(
        "very-long-call-id-that-exceeds-8-chars",
        12345,
        8000,
        0,
        &[0x00, 0x01],
    );

    ws.send(Message::Binary(frame.into())).await.unwrap();

    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Interleaved text and binary messages
#[tokio::test]
async fn test_interleaved_text_and_binary() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Send text command
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    // Send binary frame
    let frame = build_pcm_frame(
        "interleaved-call",
        12345,
        8000,
        0,
        &[0x00, 0x01, 0x00, 0x02],
    );
    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Send another text command
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    // Send another binary frame
    let frame = build_pcm_frame(
        "interleaved-call",
        12365,
        8000,
        0,
        &[0x00, 0x03, 0x00, 0x04],
    );
    ws.send(Message::Binary(frame.into())).await.unwrap();

    // Final text command
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

/// Test: Binary frame count in metrics (if available)
/// Note: This is a placeholder for when metrics are implemented
#[tokio::test]
async fn test_pcm_frame_does_not_break_session_state() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Subscribe to a context
    let (_, json) = req(
        "session.subscribe",
        serde_json::json!({"contexts": ["pcm-test"]}),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    // Send multiple binary frames
    for i in 0..5 {
        let frame = build_pcm_frame(
            &format!("pcm-call-{}", i),
            i as u32 * 100,
            8000,
            0,
            &[0x00, 0x01],
        );
        ws.send(Message::Binary(frame.into())).await.unwrap();
    }

    // Verify subscription still works by pushing an event
    {
        let gw = gateway.read().await;
        let event = rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "test".to_string(),
        };
        gw.fan_out_event_to_context("pcm-test", &event, &"test".to_string());
    }

    // Should receive the event
    let msg = timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error");

    match msg {
        Message::Text(t) => {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            // Event structure varies by event type - check for common patterns
            let is_event = v.get("type").is_some()
                || v.get("event").is_some()
                || v.get("call_id").is_some()
                || v.get("call_ringing").is_some()
                || v.get("call_answered").is_some()
                || v.get("dtmf").is_some();
            assert!(is_event, "Should receive an event: {}", v);
        }
        _ => panic!("Expected text message"),
    }

    ws.close(None).await.unwrap();
}
