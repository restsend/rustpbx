// tests/helpers/ws_harness.rs
// Shared in-process RWI WebSocket test harness: starts a real Axum server on
// an OS-assigned port with the production `rwi_ws_handler`, and provides
// connect/send/recv helpers over tokio-tungstenite.

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

pub const TEST_TOKEN: &str = "rwi-test-token";

pub type TestWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Build a minimal `RwiAuth` that accepts a single hard-coded token.
pub fn make_auth() -> RwiAuthRef {
    let config = RwiConfig {
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
pub async fn start_test_server() -> (String, RwiGatewayRef, Arc<ActiveProxyCallRegistry>) {
    let auth = make_auth();
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new(RwiGateway::new()));
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
    rustpbx::utils::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("ws://127.0.0.1:{}/rwi/v1", port);
    (url, gateway, registry)
}

/// Connect a WebSocket client with the test token.
pub async fn connect(url: &str) -> TestWs {
    let full = format!("{}?token={}", url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&full))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

/// Send a JSON request and wait for the next text frame.
pub async fn send_recv(ws: &mut TestWs, json: &str) -> serde_json::Value {
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

/// Send a JSON request and wait for the next text frame that matches
/// `expected_action_id` (skipping interleaved events).
pub async fn send_recv_matching(
    ws: &mut TestWs,
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

/// Build a simple request JSON with a fresh action_id.
pub fn req(action: &str, params: serde_json::Value) -> (String, String) {
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
