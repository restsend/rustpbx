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

    assert_eq!(v["response"], "success");
    assert_eq!(v["action_id"], id, "action_id must be echoed");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_session_list_calls_empty_returns_array() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (id, json) = req("session.list_calls", serde_json::json!({}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["response"], "success");
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

    assert_eq!(v["response"], "error");
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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "unknown_action");

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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "missing_action");

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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "parse_error");

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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "not_found");

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
    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "command_failed");

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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "not_found");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn test_call_unbridge_not_found_returns_not_found() {
    let (url, _gw, _reg) = start_test_server().await;
    let mut ws = connect(&url).await;

    let (_, json) = req("call.unbridge", serde_json::json!({"call_id": "ghost"}));
    let v = send_recv(&mut ws, &json).await;

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "not_found");

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

    assert_eq!(v["response"], "error");
    assert_eq!(v["error"]["code"], "not_found");

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
            v["response"] == "success" || v["response"] == "error",
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
    assert_eq!(v["response"], "success");

    // Small delay so the gateway receives the subscription before we push
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Push a DTMF event via the gateway directly
    let event = rustpbx::rwi::RwiEvent::Dtmf {
        call_id: "pushed-call".to_string(),
        digit: "7".to_string(),
    };
    {
        let gw = gateway.read().await;
        gw.fan_out_event_to_context("push-ctx", &event);
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
        assert_eq!(v["response"], "success");
        ws.close(None).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second connection on the same server must also work
    {
        let mut ws = connect(&url).await;
        let (_, json) = req("session.list_calls", serde_json::json!({}));
        let v = send_recv(&mut ws, &json).await;
        assert_eq!(v["response"], "success");
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
    assert_eq!(v["response"], "success");
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("external backend requires mcu_uri")
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
    assert_eq!(v["response"], "success");
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not found")
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not in conference")
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not in conference")
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
    assert_eq!(v["response"], "error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("is not in conference")
    );
}
