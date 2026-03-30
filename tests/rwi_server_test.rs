// In-process integration tests for the RWI WebSocket interface.
//
// These tests spin up a real Axum HTTP server bound to a random local port,
// then connect to it with tokio-tungstenite.  No external process is needed.
//
// Coverage:
//   • WebSocket upgrade / auth rejection (no token → 401)
//   • Full round-trip: subscribe → list_calls → response shape
//   • action_id echo — response always carries back the sent action_id
//   • Error codes: unknown_action, missing_action, not_found, not_implemented
//   • media.stop command (new in this sprint)
//   • call.unbridge command (new in this sprint)
//   • Event fan-out: second client receives event pushed by gateway

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

// ─────────────────────────────────────────────────────────────────────────────
// Test server helpers
// ─────────────────────────────────────────────────────────────────────────────

const TEST_TOKEN: &str = "integ-test-token";

/// Build a minimal `RwiAuth` that accepts a single hard-coded token.
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

/// Start an in-process Axum server on an OS-assigned port.
/// Returns (base_url, gateway_ref, registry_ref).
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

/// Connect a WebSocket client with the test token.
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

/// Send a JSON request and wait for the next text frame that matches action_id.
async fn send_recv_matching(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    json: &str,
    expected_action_id: &str,
) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    loop {
        let msg = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
                // Check if this is the response we're looking for
                if v.get("action_id").and_then(|a| a.as_str()) == Some(expected_action_id) {
                    return v;
                }
                // Otherwise continue to next message (could be an event)
            }
            other => panic!("unexpected frame: {:?}", other),
        }
    }
}

/// Send a JSON request and wait for the next text frame.
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

/// Build a simple request JSON with a fresh action_id.
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

// ─────────────────────────────────────────────────────────────────────────────
// Auth / connection
// ─────────────────────────────────────────────────────────────────────────────

/// Connecting without a token must get 401, not a WebSocket upgrade.
#[tokio::test]
async fn test_auth_rejected_without_token() {
    let (url, _gw, _reg) = start_test_server().await;
    let result = timeout(
        Duration::from_secs(5),
        connect_async(&url), // no token
    )
    .await
    .expect("timeout");

    // tungstenite returns Err on a non-101 HTTP response
    assert!(
        result.is_err(),
        "connection without token should be rejected"
    );
}

/// A valid token must result in a successful WebSocket upgrade.
#[tokio::test]
async fn test_valid_token_connects() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;
    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// session.subscribe / session.list_calls
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_session_subscribe_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req(
        "session.subscribe",
        serde_json::json!({"contexts": ["default"]}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["action_id"], id, "action_id must be echoed");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_session_list_calls_empty_returns_array() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["action_id"], id);
    // data should be an array (possibly empty)
    assert!(
        v["data"].is_array(),
        "list_calls data must be array, got: {}",
        v
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// action_id round-trip
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_action_id_always_echoed_on_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    for _ in 0..3 {
        let (id, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["action_id"], id, "action_id must match");
    }

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_action_id_echoed_on_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req("call.answer", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert_eq!(v["action_id"], id, "action_id must be echoed even on error");

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Error codes
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_action_returns_unknown_action_code() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("totally.unknown", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("unknown_action")).unwrap_or(false), "error should contain 'unknown_action': {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_missing_action_field_returns_missing_action_code() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    ws.send(Message::Text(r#"{"rwi":"1.0","params":{}}"#.into()))
        .await
        .unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected: {:?}", other),
    };

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("missing_action")).unwrap_or(false), "error should contain 'missing_action': {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_invalid_json_returns_parse_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    ws.send(Message::Text("not json at all".into()))
        .await
        .unwrap();
    let msg = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected: {:?}", other),
    };

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("parse_error")).unwrap_or(false), "error should contain 'parse_error': {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_answer_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.answer",
        serde_json::json!({"call_id": "no-such-call"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("not found") || s.contains("Call not found")).unwrap_or(false), "error should contain not_found: {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_originate_no_sip_server_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.originate",
        serde_json::json!({
            "call_id": "new-call",
            "destination": "sip:test@local",
        }),
    );
    let v = send_recv(&mut ws, &json).await;

    // Without a SIP server, originate returns command_failed
    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("Command failed") || s.contains("command")).unwrap_or(false), "error should contain command_failed: {}", v);

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// New commands: media.stop, call.unbridge
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_media_stop_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("media.stop", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("not found") || s.contains("Call not found")).unwrap_or(false), "error should contain not_found: {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_unbridge_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("call.unbridge", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("not found") || s.contains("Call not found")).unwrap_or(false), "error should contain not_found: {}", v);

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_bridge_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "call.bridge",
        serde_json::json!({"leg_a": "ghost-a", "leg_b": "ghost-b"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "error");
    assert!(v["error"].as_str().map(|s| s.contains("not found") || s.contains("Call not found")).unwrap_or(false), "error should contain not_found: {}", v);

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Multiple operations without disconnect
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sequential_commands_on_single_connection() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let commands = vec![
        (
            "session.subscribe",
            serde_json::json!({"contexts": ["ctx1", "ctx2"]}),
        ),
        ("session.list_calls", serde_json::json!({})),
        ("call.answer", serde_json::json!({"call_id": "no-call"})),
        ("call.hangup", serde_json::json!({"call_id": "no-call"})),
        ("call.ring", serde_json::json!({"call_id": "no-call"})),
        ("media.stop", serde_json::json!({"call_id": "no-call"})),
        ("call.unbridge", serde_json::json!({"call_id": "no-call"})),
    ];

    for (action, params) in commands {
        let (id, json) = req(action, params);
        let v = send_recv(&mut ws, &json).await;
        // Every response must be valid JSON with action_id echoed
        assert!(
            v["status"] == "success" || v["status"] == "error",
            "unexpected response for {}: {}",
            action,
            v
        );
        assert_eq!(v["action_id"], id, "action_id mismatch for {}", action);
    }

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Event push: gateway fan-out reaches subscribed session
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_event_pushed_from_gateway_arrives_at_client() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Subscribe to "push-ctx"
    let (_, json) = req(
        "session.subscribe",
        serde_json::json!({"contexts": ["push-ctx"]}),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    // Small delay so the gateway receives the subscription before we push
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Push a DTMF event via the gateway directly
    let event = rustpbx::rwi::RwiEvent::Dtmf {
        call_id: "pushed-call".to_string(),
        digit: "7".to_string(),
    };
    {
        let gw = gateway.read().await;
        let call_id = "pushed-call".to_string();
        gw.fan_out_event_to_context("push-ctx", &event, &call_id);
    }

    // The client must receive it within 2 seconds
    let msg = timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for pushed event")
        .expect("stream ended")
        .expect("ws error");

    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected frame: {:?}", other),
    };

    let s = serde_json::to_string(&v).unwrap();
    assert!(
        s.contains("pushed-call"),
        "event should reference pushed-call: {s}"
    );
    assert!(
        s.contains("\"7\"") || s.contains("\"digit\""),
        "event should contain digit or field: {s}"
    );

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Reconnect: second connection after first closes works normally
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_reconnect_after_close() {
    let (url, _gw, _reg) = start_test_server().await;

    // First connection
    {
        let mut ws = connect(&url).await;
        let (_, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["status"], "success");
        ws.close(None).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second connection on the same server must also work
    {
        let mut ws = connect(&url).await;
        let (_, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["status"], "success");
        ws.close(None).await.unwrap();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Conference command tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_conference_create_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal",
            "max_members": 10
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["conf_id"], "room-1");
}

#[tokio::test]
async fn test_conference_create_duplicate_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create first conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to create duplicate
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("already exists")
    );
}

#[tokio::test]
async fn test_conference_create_external_requires_mcu_uri() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "external"
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("external backend requires mcu_uri")
    );
}

#[tokio::test]
async fn test_conference_destroy_returns_success() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Destroy conference
    let (action_id, json) = req(
        "conference.destroy",
        serde_json::json!({
            "conf_id": "room-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["conf_id"], "room-1");
}

#[tokio::test]
async fn test_conference_destroy_not_found_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req(
        "conference.destroy",
        serde_json::json!({
            "conf_id": "nonexistent"
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("not found")
    );
}

#[tokio::test]
async fn test_conference_add_not_found_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Try to add call to non-existent conference
    let (_, json) = req(
        "conference.add",
        serde_json::json!({
            "conf_id": "nonexistent",
            "call_id": "call-1"
        }),
    );
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("not found")
    );
}

#[tokio::test]
async fn test_conference_mute_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to mute call that's not in conference
    let (action_id, json) = req(
        "conference.mute",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("is not in conference")
    );
}

#[tokio::test]
async fn test_conference_unmute_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to unmute call that's not in conference
    let (action_id, json) = req(
        "conference.unmute",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("is not in conference")
    );
}

#[tokio::test]
async fn test_conference_remove_not_in_conference_returns_error() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Create conference
    let (action_id, json) = req(
        "conference.create",
        serde_json::json!({
            "conf_id": "room-1",
            "backend": "internal"
        }),
    );
    let _v = send_recv_matching(&mut ws, &json, &action_id).await;

    // Try to remove call that's not in conference
    let (action_id, json) = req(
        "conference.remove",
        serde_json::json!({
            "conf_id": "room-1",
            "call_id": "call-1"
        }),
    );
    let v = send_recv_matching(&mut ws, &json, &action_id).await;
    assert_eq!(v["status"], "error");
    assert!(
        v["error"].as_str().unwrap_or("").contains("is not in conference")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Session Resume & Event Replay
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_session_resume_returns_events_and_sequence() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Cache some events in the gateway
    {
        let gw = gateway.read().await;
        let event1 = rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "test-call-1".to_string(),
        };
        let event2 = rustpbx::rwi::RwiEvent::CallAnswered {
            call_id: "test-call-1".to_string(),
        };
        gw.cache_event(&"test-call-1".to_string(), &event1);
        gw.cache_event(&"test-call-1".to_string(), &event2);
    }

    // Request session resume without last_sequence (should return all events)
    let (_, json) = req("session.resume", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    // Debug: print response if error
    if v["status"] == "error" {
        eprintln!("Session resume error: {:?}", v);
    }

    assert_eq!(v["status"], "success");
    assert!(v["data"]["events"].is_array(), "events should be an array");
    assert!(v["data"]["current_sequence"].is_u64(), "current_sequence should be a number");
    assert!(v["data"]["replayed_count"].is_u64(), "replayed_count should be a number");
    
    // Should have cached events
    let replayed = v["data"]["replayed_count"].as_u64().unwrap();
    assert!(replayed >= 2, "should have at least 2 cached events");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_session_resume_with_sequence_returns_partial_events() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Cache events
    {
        let gw = gateway.read().await;
        let event1 = rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "seq-call".to_string(),
        };
        let event2 = rustpbx::rwi::RwiEvent::CallAnswered {
            call_id: "seq-call".to_string(),
        };
        let event3 = rustpbx::rwi::RwiEvent::CallBridged {
            leg_a: "seq-call".to_string(),
            leg_b: "other".to_string(),
        };
        gw.cache_event(&"seq-call".to_string(), &event1);
        gw.cache_event(&"seq-call".to_string(), &event2);
        gw.cache_event(&"seq-call".to_string(), &event3);
    }

    // Get initial sequence
    let (_, json) = req("session.resume", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    let total_events = v["data"]["replayed_count"].as_u64().unwrap();
    
    // Resume with a sequence number (should return events after that sequence)
    let (_, json) = req("session.resume", serde_json::json!({"last_sequence": 1}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    let replayed = v["data"]["replayed_count"].as_u64().unwrap();
    assert!(replayed < total_events, "should return fewer events when using last_sequence");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_resume_returns_call_specific_events() {
    let (url, gateway, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Cache events for different calls
    {
        let gw = gateway.read().await;
        let event1 = rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "call-a".to_string(),
        };
        let event2 = rustpbx::rwi::RwiEvent::CallRinging {
            call_id: "call-b".to_string(),
        };
        gw.cache_event(&"call-a".to_string(), &event1);
        gw.cache_event(&"call-b".to_string(), &event2);
    }

    // Request call resume for specific call
    let (_, json) = req(
        "call.resume",
        serde_json::json!({"call_id": "call-a"}),
    );
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["status"], "success");
    assert_eq!(v["data"]["call_id"], "call-a");
    assert!(v["data"]["events"].is_array());
    
    // Should only have events for call-a
    let events = v["data"]["events"].as_array().unwrap();
    for event in events {
        assert_eq!(event["call_id"], "call-a", "should only have events for call-a");
    }

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Binary PCM WebSocket
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_binary_pcm_frame_rejected_without_ownership() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Build a PCM binary frame
    // Header: 8 bytes call_id + 4 bytes timestamp + 2 bytes sample_rate + 2 bytes flags
    let mut frame = vec![0u8; 16];
    
    // Call ID: "unowned" (padded)
    frame[0..7].copy_from_slice(b"unowned");
    
    // Timestamp (big-endian)
    let timestamp: u32 = 12345;
    frame[8..12].copy_from_slice(&timestamp.to_be_bytes());
    
    // Sample rate: 8000 Hz
    let sample_rate: u16 = 8000;
    frame[12..14].copy_from_slice(&sample_rate.to_be_bytes());
    
    // Flags: 0
    frame[14..16].copy_from_slice(&[0u8, 0u8]);
    
    // Add some PCM data (16-bit samples)
    frame.extend_from_slice(&[0x00, 0x01, 0x00, 0x02]); // 2 samples

    // Send binary frame - should not panic, but will be dropped (session doesn't own the call)
    ws.send(Message::Binary(frame.into())).await.unwrap();
    
    // Send a ping to ensure connection is still alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    if v["status"] == "error" {
        eprintln!("Error response: {:?}", v);
    }
    assert_eq!(v["status"], "success", "connection should remain alive after binary frame");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_binary_pcm_frame_empty_call_id_rejected() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Build a PCM binary frame with empty call_id
    let mut frame = vec![0u8; 16]; // All zeros = empty call_id
    
    // Timestamp
    let timestamp: u32 = 12345;
    frame[8..12].copy_from_slice(&timestamp.to_be_bytes());
    
    // Sample rate
    let sample_rate: u16 = 8000;
    frame[12..14].copy_from_slice(&sample_rate.to_be_bytes());
    
    // Flags
    frame[14..16].copy_from_slice(&[0u8, 0u8]);

    // Send binary frame - should be silently dropped
    ws.send(Message::Binary(frame.into())).await.unwrap();
    
    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_binary_pcm_frame_too_small_rejected() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    // Send a binary frame that's too small (less than 16 bytes header)
    let small_frame = vec![0u8; 10];
    ws.send(Message::Binary(small_frame.into())).await.unwrap();
    
    // Connection should remain alive
    let (_, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;
    assert_eq!(v["status"], "success");

    ws.close(None).await.unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Leg Timeline CDR Enhancement Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_leg_timeline_serialization() {
    use rustpbx::callrecord::{LegTimeline, LegTimelineEventType};
    
    let mut timeline = LegTimeline::new();
    
    // Add some events
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Added,
        None,
        Some(serde_json::json!({"source": "originate"})),
    );
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Bridged,
        Some("leg-2".to_string()),
        None,
    );
    
    timeline.add_event(
        "leg-1".to_string(),
        LegTimelineEventType::Removed,
        None,
        Some(serde_json::json!({"reason": "hangup"})),
    );
    
    // Serialize to JSON
    let json = serde_json::to_value(&timeline).unwrap();
    
    assert!(json["events"].is_array());
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    
    // Verify event structure (using camelCase due to #[serde(rename_all = "camelCase")])
    assert_eq!(events[0]["legId"], "leg-1");
    assert_eq!(events[0]["eventType"], "added");
    assert_eq!(events[1]["eventType"], "bridged");
    assert_eq!(events[1]["peerLegId"], "leg-2");
}

#[tokio::test]
async fn test_leg_timeline_is_empty() {
    use rustpbx::callrecord::LegTimeline;
    
    let timeline = LegTimeline::new();
    assert!(timeline.is_empty());
    
    let mut timeline_with_events = LegTimeline::new();
    timeline_with_events.add_event(
        "leg-1".to_string(),
        rustpbx::callrecord::LegTimelineEventType::Added,
        None,
        None,
    );
    assert!(!timeline_with_events.is_empty());
}

