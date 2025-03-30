use crate::{
    event::SessionEvent,
    handler::call::{
        ActiveCall, AsrConfig, AsrProcessor, CallEvent, CallHandlerState, CallResponse,
        SynthesisConfig, VadConfig, WsCommand,
    },
    media::{
        stream::MediaStreamBuilder,
        track::{tts::TtsTrack, Track},
    },
};
use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};
use uuid::Uuid;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

// Payload for WebRTC call request
#[derive(Debug, Deserialize)]
pub struct WebRtcCallRequest {
    pub sdp: String,
    pub asr: Option<AsrConfig>,
    pub vad: Option<VadConfig>,
    pub record: Option<bool>,
    pub llm: crate::llm::LlmConfig,
    pub tts: SynthesisConfig,
}

// Configure Router for WebRTC
pub fn router() -> Router<CallHandlerState> {
    Router::new().route("/call/webrtc", get(webrtc_ws_handler))
}

// WebSocket handler for WebRTC calls
async fn webrtc_ws_handler(
    State(state): State<CallHandlerState>,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| handle_webrtc_ws(socket, state))
}

// Handler function for WebRTC WebSocket connections
async fn handle_webrtc_ws(socket: WebSocket, state: CallHandlerState) {
    let (mut sender, mut receiver) = socket.split();

    // Wait for the initial WebRTC offer
    let offer = match receiver.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<WebRtcCallRequest>(&text) {
            Ok(request) => request,
            Err(e) => {
                let error_msg = format!(
                    "{{\"type\":\"error\",\"error\":\"Invalid WebRTC offer: {}\"}}",
                    e
                );
                let _ = sender.send(Message::Text(error_msg.into())).await;
                return;
            }
        },
        _ => {
            let error_msg = "{\"type\":\"error\",\"error\":\"Expected WebRTC offer\"}".to_string();
            let _ = sender.send(Message::Text(error_msg.into())).await;
            return;
        }
    };

    // Setup WebRTC connection
    match setup_webrtc_connection(state.clone(), offer).await {
        Ok((session_id, sdp, event_rx)) => {
            // Send the SDP answer
            let response = CallResponse {
                session_id: session_id.clone(),
                sdp,
            };
            let response_json = serde_json::to_string(&response).unwrap();
            let _ = sender.send(Message::Text(response_json.into())).await;

            // Handle the WebSocket session
            handle_ws_session(session_id, sender, receiver, state, event_rx).await;
        }
        Err(e) => {
            let error_msg = format!(
                "{{\"type\":\"error\",\"error\":\"Failed to setup WebRTC connection: {}\"}}",
                e
            );
            let _ = sender.send(Message::Text(error_msg.into())).await;
        }
    }
}

// Common WebSocket session handler for WebRTC calls
async fn handle_ws_session(
    session_id: String,
    mut sender: futures::stream::SplitSink<WebSocket, Message>,
    mut receiver: futures::stream::SplitStream<WebSocket>,
    state: CallHandlerState,
    mut event_rx: broadcast::Receiver<CallEvent>,
) {
    // Send connected event
    let connected_msg = serde_json::json!({
        "type": "connected",
        "timestamp": chrono::Utc::now().timestamp(),
        "session_id": session_id
    });
    let connected_json = serde_json::to_string(&connected_msg).unwrap();
    let _ = sender.send(Message::Text(connected_json.into())).await;

    // Clone state for command handler
    let state_for_commands = state.clone();
    let session_id_for_commands = session_id.clone();

    // Handle incoming commands
    let command_handler = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
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

    // Cleanup call if it still exists
    if let Some(call) = state.active_calls.lock().await.remove(&session_id) {
        info!("Cleaning up call {}", session_id);
        call.media_stream.stop();
        let _ = call.peer_connection.close().await;
    }
}

// Process WebSocket commands for WebRTC
pub async fn handle_ws_command(state: &CallHandlerState, session_id: &str, text: String) {
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
                }
                WsCommand::Mute { track_id } => {
                    let track = track_id.unwrap_or_else(|| "main".to_string());
                    info!("Received mute command for track: {}", track);

                    // Implement mute functionality here
                    // This would interact with the media stream to mute a specific track
                }
                WsCommand::Unmute { track_id } => {
                    let track = track_id.unwrap_or_else(|| "main".to_string());
                    info!("Received unmute command for track: {}", track);

                    // Implement unmute functionality here
                    // This would interact with the media stream to unmute a specific track
                }
            }
        }
        Err(e) => {
            error!("Failed to parse WebSocket command: {}", e);
        }
    }
}

// Helper function to setup WebRTC connection
pub async fn setup_webrtc_connection(
    state: CallHandlerState,
    request: WebRtcCallRequest,
) -> Result<(String, String, broadcast::Receiver<CallEvent>), String> {
    // Generate session ID
    let session_id = Uuid::new_v4().to_string();
    info!("Starting WebRTC call with session ID: {}", session_id);

    // Create MediaEngine
    let media_engine = MediaEngine::default();

    // Create API registry and build
    let registry = Registry::new();
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    // Configure ICE servers
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create peer connection
    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .map_err(|e| format!("Failed to create peer connection: {}", e))?,
    );

    // Parse the SDP offer
    let offer = RTCSessionDescription::offer(request.sdp)
        .map_err(|e| format!("Failed to parse SDP offer: {}", e))?;

    // Set the remote description
    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(|e| format!("Failed to set remote description: {}", e))?;

    // Create an answer
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(|e| format!("Failed to create answer: {}", e))?;

    // Create data channel for control messages
    let _data_channel = peer_connection
        .create_data_channel("control", None)
        .await
        .map_err(|e| format!("Failed to create data channel: {}", e))?;

    // Set up MediaStream
    let cancel_token = tokio_util::sync::CancellationToken::new();

    // Create event broadcaster
    let (event_sender, event_receiver) = broadcast::channel(100);

    // Build media stream
    let mut builder = MediaStreamBuilder::new()
        .id(format!("call-{}", session_id))
        .cancel_token(cancel_token.clone());

    // Add recording if requested
    if let Some(true) = request.record {
        builder = builder.recorder(format!("call-{}.wav", session_id));
    }

    let media_stream = Arc::new(builder.build());

    // Create processors
    if let Some(asr_config) = request.asr {
        if asr_config.enabled {
            let asr_processor = AsrProcessor::new(asr_config, event_sender.clone());

            // Create a new track and add processor
            let track_id = format!("asr-test-{}", Uuid::new_v4());
            let mut test_track = TtsTrack::new(track_id);
            test_track.with_processors(vec![Box::new(asr_processor)]);

            // Add track to media stream
            media_stream.update_track(Box::new(test_track)).await;
        }
    }

    // Create a converter for events
    let (session_event_sender, _) = broadcast::channel(100);
    let event_sender_clone = event_sender.clone();

    // Forward session events to call events
    let session_event_receiver = session_event_sender.subscribe();
    tokio::spawn(async move {
        let mut receiver = session_event_receiver;
        while let Ok(event) = receiver.recv().await {
            match event {
                SessionEvent::TranscriptionFinal(track_id, timestamp, text) => {
                    let _ = event_sender_clone.send(CallEvent::AsrEvent {
                        track_id,
                        timestamp,
                        text,
                        is_final: true,
                    });
                }
                SessionEvent::TranscriptionDelta(track_id, timestamp, text) => {
                    let _ = event_sender_clone.send(CallEvent::AsrEvent {
                        track_id,
                        timestamp,
                        text,
                        is_final: false,
                    });
                }
                SessionEvent::Error(timestamp, error) => {
                    let _ = event_sender_clone.send(CallEvent::ErrorEvent { timestamp, error });
                }
                _ => {
                    // Ignore other session events
                }
            }
        }
    });

    // Store active call
    let active_call = ActiveCall {
        session_id: session_id.clone(),
        media_stream: media_stream.clone(),
        peer_connection: peer_connection.clone(),
        events: event_sender.clone(),
    };

    state
        .active_calls
        .lock()
        .await
        .insert(session_id.clone(), active_call);

    // Handle peer connection state changes
    let pc_clone = peer_connection.clone();
    let session_id_clone = session_id.clone();
    let state_clone = state.clone();

    // Set up a callback for peer connection state changes
    let _ = peer_connection.on_peer_connection_state_change(Box::new(
        move |s: RTCPeerConnectionState| {
            let pc = pc_clone.clone();
            let session_id = session_id_clone.clone();
            let state = state_clone.clone();

            info!("Peer connection state changed to {}", s);
            if s == RTCPeerConnectionState::Disconnected
                || s == RTCPeerConnectionState::Failed
                || s == RTCPeerConnectionState::Closed
            {
                info!("Cleaning up call {}", session_id);

                tokio::spawn(async move {
                    if let Some(call) = state.active_calls.lock().await.remove(&session_id) {
                        call.media_stream.stop();
                        let _ = call.peer_connection.close().await;
                    }
                });
            }

            Box::pin(async {})
        },
    ));

    // Return the session ID, SDP, and event receiver
    Ok((session_id, answer.sdp, event_receiver))
}
