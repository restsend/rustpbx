use crate::{
    app::AppStateBuilder,
    config::{Config, ProxyConfig, UseragentConfig},
    handler::{
        call::{ActiveCallType, CallParams},
        middleware::clientaddr::ClientAddr,
    },
    media::engine::StreamEngine,
};
use axum::{
    extract::{Query, State, WebSocketUpgrade},
    response::Response,
};
use futures::{SinkExt, StreamExt};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tower_http::cors::CorsLayer;
use tracing::info;

async fn test_call_handler(
    client_ip: ClientAddr,
    call_type: ActiveCallType,
    ws: WebSocketUpgrade,
    state: crate::app::AppState,
    params: CallParams,
) -> Response {
    // Use the existing call_handler functionality
    crate::handler::handler::call_handler(client_ip, call_type, ws, state, params).await
}

#[tokio::test]
async fn test_call_record_creation() {
    tracing_subscriber::fmt::try_init().ok();

    // Create a test configuration with different ports to avoid conflicts
    let mut config = Config::default();
    // Use different static ports for this test to avoid conflicts
    let ua_port = 25063; // Different from other tests
    let proxy_port = 5063; // Different from other tests

    config.ua = Some(UseragentConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: ua_port,
        rtp_start_port: Some(12000),
        rtp_end_port: Some(42000),
        useragent: Some("rustpbx-test".to_string()),
        ..Default::default()
    });

    config.proxy = Some(ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(proxy_port),
        ..Default::default()
    });

    // Build app state
    let state = AppStateBuilder::new()
        .config(config)
        .with_stream_engine(Arc::new(StreamEngine::default()))
        .build()
        .await
        .expect("Failed to build app state");

    // Create router with our handler
    let app = axum::Router::new()
        .route(
            "/call",
            axum::routing::get(
                |ws: WebSocketUpgrade,
                 Query(params): Query<CallParams>,
                 State(state): State<crate::app::AppState>| async move {
                    test_call_handler(
                        ClientAddr::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 12345))),
                        ActiveCallType::WebSocket,
                        ws,
                        state,
                        params,
                    )
                    .await
                },
            ),
        )
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    // Start server
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );

    // Spawn server task
    let server_task = tokio::spawn(async move {
        server.await.expect("Server failed");
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create WebSocket client
    let ws_url = format!("ws://127.0.0.1:{}/call?id=test-call-123", addr.port());
    info!("Connecting to WebSocket: {}", ws_url);

    let (ws_stream, _) = connect_async(&ws_url).await.expect("Failed to connect");
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Send invite message
    let invite_msg = serde_json::json!({
        "command": "invite",
        "option": {
            "caller": "test-caller",
            "callee": "test-callee"
        }
    });

    ws_sender
        .send(WsMessage::Text(invite_msg.to_string().into()))
        .await
        .expect("Failed to send invite");

    // Wait for response
    let response = timeout(Duration::from_secs(5), ws_receiver.next())
        .await
        .expect("Timeout waiting for response")
        .expect("No response received")
        .expect("WebSocket error");

    info!("Received response: {:?}", response);

    // Send hangup command
    let hangup_msg = serde_json::json!({
        "command": "hangup",
        "reason": "test_completed",
        "initiator": "caller"
    });

    ws_sender
        .send(WsMessage::Text(hangup_msg.to_string().into()))
        .await
        .expect("Failed to send hangup");

    // Close WebSocket connection
    ws_sender.close().await.ok();

    // Wait a bit for call record to be generated
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The call should be removed from active_calls after completion
    let active_calls = state.active_calls.lock().await;
    assert!(
        active_calls.is_empty(),
        "Active calls should be empty after call completion"
    );

    info!("Call record test completed successfully");

    // Cleanup
    server_task.abort();
}
