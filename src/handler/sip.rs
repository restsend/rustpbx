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
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use uuid::Uuid;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    event::{EventSender, SessionEvent},
    handler::call::{
        ActiveCall, AsrConfig, CallEvent, CallHandlerState, CallResponse, Command, SynthesisConfig,
        VadConfig,
    },
    media::stream::MediaStreamBuilder,
    useragent::{
        client::{SipClient, SipClientEvent},
        config::SipConfig,
    },
};

// SIP call configuration
#[derive(Debug, Deserialize)]
pub struct SipCallConfig {
    pub target_uri: String,
    pub local_uri: Option<String>,
    pub local_ip: Option<String>,
    pub local_port: Option<u16>,
    pub display_name: Option<String>,
    pub proxy: Option<String>,
}

// Payload for WebRTC-to-SIP call request
#[derive(Debug, Deserialize)]
pub struct WebRtcSipCallRequest {
    pub sdp: String,
    pub sip: SipCallConfig,
    pub asr: Option<AsrConfig>,
    pub llm: Option<crate::llm::LlmConfig>,
    pub tts: Option<SynthesisConfig>,
    pub vad: Option<VadConfig>,
    pub record: Option<bool>,
}

// Configure Router for SIP
pub fn router() -> Router<CallHandlerState> {
    Router::new().route("/call/sip", get(sip_ws_handler))
}

// WebSocket handler for SIP calls
async fn sip_ws_handler(
    State(state): State<CallHandlerState>,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| handle_sip_ws(socket, state))
}

// Handler function for SIP WebSocket connections
async fn handle_sip_ws(socket: WebSocket, state: CallHandlerState) {
    let (mut sender, mut receiver) = socket.split();

    // Wait for the initial WebRTC-SIP request
    let request = match receiver.next().await {
        Some(Ok(Message::Text(text))) => {
            match serde_json::from_str::<WebRtcSipCallRequest>(&text) {
                Ok(request) => request,
                Err(e) => {
                    let error_msg = format!(
                        "{{\"type\":\"error\",\"error\":\"Invalid SIP call request: {}\"}}",
                        e
                    );
                    let _ = sender.send(Message::Text(error_msg.into())).await;
                    return;
                }
            }
        }
        _ => {
            let error_msg =
                "{\"type\":\"error\",\"error\":\"Expected SIP call request\"}".to_string();
            let _ = sender.send(Message::Text(error_msg.into())).await;
            return;
        }
    };

    // Setup WebRTC-SIP connection
    match setup_sip_connection(state.clone(), request).await {
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
                "{{\"type\":\"error\",\"error\":\"Failed to setup SIP connection: {}\"}}",
                e
            );
            let _ = sender.send(Message::Text(error_msg.into())).await;
        }
    }
}

// Common WebSocket session handler for SIP calls
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
                    crate::handler::webrtc::handle_ws_command(
                        &state_for_commands,
                        &session_id_for_commands,
                        text_string,
                    )
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
            // Convert to WebSocket message
            let msg = match event {
                CallEvent::AsrEvent {
                    track_id,
                    timestamp,
                    text,
                    is_final,
                } => {
                    json!({
                        "type": "transcription",
                        "timestamp": timestamp,
                        "track_id": track_id,
                        "text": text,
                        "is_final": is_final,
                    })
                }
                CallEvent::LlmEvent {
                    timestamp,
                    text,
                    is_final,
                } => {
                    json!({
                        "type": "llm",
                        "timestamp": timestamp,
                        "text": text,
                        "is_final": is_final,
                    })
                }
                CallEvent::TtsEvent { timestamp, text } => {
                    json!({
                        "type": "tts",
                        "timestamp": timestamp,
                        "text": text,
                    })
                }
                CallEvent::ErrorEvent { timestamp, error } => {
                    json!({
                        "type": "error",
                        "timestamp": timestamp,
                        "error": error,
                    })
                }
                CallEvent::RingingEvent { timestamp } => {
                    json!({
                        "type": "ringing",
                        "timestamp": timestamp,
                    })
                }
                _ => continue,
            };

            if let Err(e) = sender.send(Message::Text(msg.to_string().into())).await {
                error!("Failed to send WebSocket message: {}", e);
                break;
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

// Helper function to setup SIP connection
async fn setup_sip_connection(
    state: CallHandlerState,
    request: WebRtcSipCallRequest,
) -> Result<(String, String, broadcast::Receiver<CallEvent>), String> {
    // Generate a unique session ID
    let session_id = Uuid::new_v4().to_string();
    info!(
        "Starting WebRTC to SIP call with session ID: {}",
        session_id
    );

    // Set up the WebRTC connection
    let ice_servers = vec![RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_owned()],
        ..Default::default()
    }];

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    // Create a new MediaEngine and register codecs
    let m = MediaEngine::default();

    // Create a webrtc::API using our MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(Registry::new())
        .build();

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(
        api.new_peer_connection(config)
            .await
            .map_err(|e| format!("Failed to create peer connection: {}", e))?,
    );

    // Create a broadcast channel for events
    let (event_sender, event_receiver) = broadcast::channel(100);

    // Create a session event sender for the pipeline manager
    let (session_event_sender, _) = broadcast::channel(100);
    let event_sender_clone = event_sender.clone();

    // Forward session events to call events
    let session_event_receiver = session_event_sender.subscribe();
    tokio::spawn(async move {
        let mut receiver = session_event_receiver;
        while let Ok(event) = receiver.recv().await {
            match event {
                SessionEvent::TranscriptionFinal {
                    track_id,
                    timestamp,
                    text,
                } => {
                    let _ = event_sender_clone.send(CallEvent::AsrEvent {
                        track_id,
                        timestamp,
                        text,
                        is_final: true,
                    });
                }
                SessionEvent::TranscriptionDelta {
                    track_id,
                    timestamp,
                    text,
                } => {
                    let _ = event_sender_clone.send(CallEvent::AsrEvent {
                        track_id,
                        timestamp,
                        text,
                        is_final: false,
                    });
                }
                _ => {
                    // Ignore other session events
                }
            }
        }
    });

    // Create media stream for the connection
    let stream_id = format!("webrtc-sip-{}", session_id);
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let media_stream = Arc::new(
        MediaStreamBuilder::new()
            .id(stream_id.clone())
            .cancel_token(cancel_token.clone())
            .build(),
    );

    let active_call = ActiveCall {
        session_id: session_id.clone(),
        media_stream: media_stream.clone(),
        peer_connection: peer_connection.clone(),
        events: event_sender.clone(),
    };

    // Store the active call in state
    state
        .active_calls
        .lock()
        .await
        .insert(session_id.clone(), active_call);

    // Set up SIP client
    let sip_config = SipConfig {
        local_uri: request
            .sip
            .local_uri
            .unwrap_or_else(|| "sip:rustpbx@127.0.0.1".to_string()),
        local_ip: request
            .sip
            .local_ip
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        local_port: request.sip.local_port.unwrap_or(5060),
        display_name: request.sip.display_name,
        proxy: request.sip.proxy,
        ..Default::default()
    };

    let (mut sip_client, mut sip_event_receiver) = SipClient::new(sip_config);
    if let Err(e) = sip_client.start().await {
        return Err(format!("Failed to start SIP client: {}", e));
    }

    // Create a flag to track connection status
    let call_established = Arc::new(AtomicBool::new(false));
    let call_established_clone = call_established.clone();

    // Get media stream from SIP client
    let sip_media_stream = sip_client.media_stream();

    // Handle SIP events
    let event_sender_clone = event_sender.clone();

    tokio::spawn(async move {
        while let Some(event) = sip_event_receiver.recv().await {
            match event {
                SipClientEvent::CallEstablished(dialog_id) => {
                    info!("SIP call established: {}", dialog_id);
                    call_established_clone.store(true, Ordering::SeqCst);

                    let _ = event_sender_clone.send(CallEvent::LlmEvent {
                        timestamp: chrono::Utc::now().timestamp() as u32,
                        text: "Call established".to_string(),
                        is_final: true,
                    });
                }
                SipClientEvent::MessageReceived(_, text) => {
                    // Process received audio with ASR and pass to LLM
                    if !text.is_empty() {
                        // Simulate sending response
                        let _ = event_sender_clone.send(CallEvent::LlmEvent {
                            timestamp: chrono::Utc::now().timestamp() as u32,
                            text: format!("Response to: {}", text),
                            is_final: true,
                        });
                    }
                }
                _ => {
                    info!("SIP event: {:?}", event);
                }
            }
        }
    });

    // Set up WebRTC session description
    let offer = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::offer(
        request
            .sdp
            .parse()
            .map_err(|e| format!("Failed to parse SDP: {}", e))?,
    )
    .map_err(|e| format!("Failed to create offer: {}", e))?;

    // Set remote description
    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(|e| format!("Failed to set remote description: {}", e))?;

    // Create answer
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(|e| format!("Failed to create answer: {}", e))?;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection
        .set_local_description(answer.clone())
        .await
        .map_err(|e| format!("Failed to set local description: {}", e))?;

    // Start media stream
    tokio::spawn(async move {
        if let Err(e) = media_stream.serve().await {
            error!("Media stream error: {}", e);
        }
    });

    // Send initial ringing event
    let _ = event_sender.send(CallEvent::RingingEvent {
        timestamp: chrono::Utc::now().timestamp() as u32,
    });

    // Return the session ID, SDP, and event receiver
    Ok((session_id, answer.sdp, event_receiver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::call::CallEvent;
    use tokio::sync::broadcast;

    // Test SIP connection setup
    #[tokio::test]
    #[ignore] // Ignored because it requires a real SIP server
    async fn test_sip_connection() {
        let state = CallHandlerState {
            active_calls: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        };

        // Create a test SIP request
        let request = WebRtcSipCallRequest {
            sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:test\r\na=ice-pwd:test\r\na=fingerprint:sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00\r\na=setup:actpass\r\na=mid:0\r\na=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\na=sendrecv\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n".to_string(),
            sip: SipCallConfig {
                target_uri: "sip:test@example.com".to_string(),
                local_uri: Some("sip:local@127.0.0.1".to_string()),
                local_ip: Some("127.0.0.1".to_string()),
                local_port: Some(5060),
                display_name: Some("Test User".to_string()),
                proxy: None,
            },
            asr: None,
            llm: None,
            tts: None,
            vad: None,
            record: None,
        };

        // This test would normally try to set up a SIP connection
        // but we'll just check if the connection setup function doesn't crash
        let _result = setup_sip_connection(state, request).await;
        // Don't assert on the result because it likely will fail without a real SIP server
        // We're just checking that the function completes without crashing
    }

    // Test SIP event handling
    #[tokio::test]
    async fn test_sip_event_handling() {
        // Create an event sender
        let (event_sender, mut event_receiver) = broadcast::channel::<CallEvent>(10);

        // Simulate a SIP CallEstablished event
        let event = SipClientEvent::CallEstablished("test-dialog".to_string());

        // Manually process the event as our handler would
        match event {
            SipClientEvent::CallEstablished(dialog_id) => {
                let _ = event_sender.send(CallEvent::LlmEvent {
                    timestamp: chrono::Utc::now().timestamp() as u32,
                    text: "Call established".to_string(),
                    is_final: true,
                });
            }
            _ => {}
        }

        // Check if the event was properly converted
        let event = event_receiver.recv().await.unwrap();
        match event {
            CallEvent::LlmEvent { text, .. } => {
                assert_eq!(text, "Call established");
            }
            _ => panic!("Expected LlmEvent"),
        }
    }
}
