use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::handler::call::{ActiveCall, CallEvent, CallHandlerState, WsCommand};

// Configure WebSocket routes
pub fn router() -> Router<CallHandlerState> {
    Router::new().route("/ws/{session_id}", get(ws_handler))
}

// WebSocket handler function
async fn ws_handler(
    State(state): State<CallHandlerState>,
    Path(session_id): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Check if session exists before upgrading
    let call_exists = state.active_calls.lock().await.contains_key(&session_id);
    if !call_exists {
        // We can't reject WebSocket upgrade at this point with a proper error message
        // so we'll return a normal upgrade but immediately send an error and close it
        return ws.on_upgrade(move |socket| handle_error_session(socket, session_id));
    }

    // Get active call
    let call = state
        .active_calls
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .unwrap();

    // Subscribe to events
    let event_rx = call.events.subscribe();

    // Handler for WebSocket connection
    ws.on_upgrade(move |socket| handle_session(socket, state, session_id, event_rx))
}

// Handle a non-existent session
async fn handle_error_session(socket: WebSocket, session_id: String) {
    let (mut sender, _) = socket.split();
    let error_msg = json!({
        "type": "error",
        "timestamp": chrono::Utc::now().timestamp(),
        "error": format!("Session {} not found", session_id)
    });

    // Send error and close
    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&error_msg)
                .unwrap_or_else(|_| {
                    r#"{"type":"error","error":"Failed to serialize error message"}"#.to_string()
                })
                .into(),
        ))
        .await;
}

// Handle a valid session
async fn handle_session(
    socket: WebSocket,
    state: CallHandlerState,
    session_id: String,
    mut event_rx: broadcast::Receiver<CallEvent>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Send initial connection message
    let connected_msg = json!({
        "type": "connected",
        "timestamp": chrono::Utc::now().timestamp(),
        "session_id": session_id
    });

    if let Err(e) = sender
        .send(Message::Text(
            serde_json::to_string(&connected_msg).unwrap().into(),
        ))
        .await
    {
        error!("Failed to send initial connection message: {}", e);
        return;
    }

    // Clone state for command handler
    let state_for_commands = state.clone();
    let session_id_for_commands = session_id.clone();

    // Handle incoming commands
    let command_handler = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // Convert from Utf8Bytes to String
                    let text_string = text.to_string();
                    handle_ws_command(&state_for_commands, &session_id_for_commands, text_string)
                        .await;
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket closed by client");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Send events
    let event_sender = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match serde_json::to_string(&event) {
                Ok(json) => {
                    if let Err(e) = sender.send(Message::Text(json.into())).await {
                        error!("Failed to send WebSocket message: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to serialize event: {}", e);
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = command_handler => {
            info!("Command handler finished");
        }
        _ = event_sender => {
            info!("Event sender finished");
        }
    }
}

// Process WebSocket commands
async fn handle_ws_command(state: &CallHandlerState, session_id: &str, text: String) {
    let command: Result<WsCommand, _> = serde_json::from_str(&text);

    match command {
        Ok(cmd) => {
            let call = match state.active_calls.lock().await.get(session_id).cloned() {
                Some(call) => call,
                None => {
                    warn!("Session {} not found for command", session_id);
                    return;
                }
            };

            match cmd {
                WsCommand::PlayTts { text } => {
                    info!("Received TTS command: {}", text);

                    // Send TTS event to indicate processing
                    let tts_event = CallEvent::TtsEvent {
                        timestamp: chrono::Utc::now().timestamp() as u32,
                        text: text.clone(),
                    };

                    let _ = call.events.send(tts_event);

                    // Here you would implement TTS processing
                    // For now we just acknowledge the command
                }
                WsCommand::PlayWav { url } => {
                    info!("Received play WAV command: {}", url);

                    // Here you would implement WAV playback
                    // For now we just acknowledge the command
                }
                WsCommand::Hangup {} => {
                    info!("Received hangup command for session {}", session_id);

                    // Send hangup event
                    let hangup_event = CallEvent::HangupEvent {
                        timestamp: chrono::Utc::now().timestamp() as u32,
                        reason: "User initiated hangup".to_string(),
                    };

                    let _ = call.events.send(hangup_event);

                    // Clean up resources
                    tokio::spawn(async move {
                        call.media_stream.stop();
                        let _ = call.peer_connection.close().await;
                    });

                    // Remove from active calls
                    state.active_calls.lock().await.remove(session_id);
                }
                WsCommand::Refer { target } => {
                    info!("Received refer command: {}", target);

                    // Send refer event
                    let refer_event = CallEvent::ReferEvent {
                        timestamp: chrono::Utc::now().timestamp() as u32,
                        target,
                    };

                    let _ = call.events.send(refer_event);

                    // Here you would implement call transfer/refer
                }
            }
        }
        Err(e) => {
            error!("Failed to parse WebSocket command: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::call::{ActiveCall, CallEvent, CallHandlerState};
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::sync::{broadcast, Mutex};

    // Mock RTCPeerConnection for testing
    #[derive(Clone)]
    struct MockRTCPeerConnection;

    impl MockRTCPeerConnection {
        pub async fn close(&self) -> Result<(), String> {
            Ok(())
        }
    }

    // Test helper to create a test call
    async fn create_test_call() -> (CallHandlerState, String, broadcast::Sender<CallEvent>) {
        let state = CallHandlerState {
            active_calls: Arc::new(Mutex::new(std::collections::HashMap::new())),
        };

        let session_id = "test-session".to_string();
        let (events_tx, _) = broadcast::channel(100);

        // Create empty media stream
        let media_stream = Arc::new(
            crate::media::stream::MediaStreamBuilder::new()
                .id("test-stream".to_string())
                .build(),
        );

        // For testing, we'll create a type that can be converted to Arc<RTCPeerConnection>
        use webrtc::peer_connection::RTCPeerConnection;

        struct MockPeerConnection;

        impl MockPeerConnection {
            pub async fn close(&self) -> Result<(), String> {
                Ok(())
            }
        }

        // Cast the mock to the right type for testing
        // In a real implementation we'd use a proper mock
        let peer_connection = Arc::new(MockPeerConnection) as Arc<dyn std::any::Any>;
        let peer_connection = peer_connection as Arc<_>;

        // Skip creating the actual ActiveCall for testing
        /*
        let active_call = ActiveCall {
            session_id: session_id.clone(),
            media_stream,
            peer_connection: Arc::new(MockRTCPeerConnection),
            events: events_tx.clone(),
        };

        state.active_calls.lock().await.insert(session_id.clone(), active_call);
        */

        (state, session_id, events_tx)
    }

    // Simple test for WsCommand serialization/deserialization
    #[test]
    fn test_ws_command_serialization() {
        let play_tts_cmd = WsCommand::PlayTts {
            text: "Hello, world!".to_string(),
        };

        let json = serde_json::to_string(&play_tts_cmd).unwrap();
        assert!(json.contains("play_tts"));
        assert!(json.contains("Hello, world!"));

        let hangup_cmd = WsCommand::Hangup {};
        let json = serde_json::to_string(&hangup_cmd).unwrap();
        assert!(json.contains("hangup"));
    }
}
