use anyhow::Result;
use axum::{
    extract::{Json, State},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use uuid::Uuid;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
};

use crate::media::{
    processor::{AudioFrame, Processor, Samples},
    stream::{MediaStream, MediaStreamBuilder, MediaStreamEvent},
    track::{tts::TtsTrack, Track},
};

// Session state for active calls
#[derive(Clone)]
pub struct CallHandlerState {
    pub active_calls: Arc<Mutex<std::collections::HashMap<String, ActiveCall>>>,
}

// Active call information
#[derive(Clone)]
pub struct ActiveCall {
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub peer_connection: Arc<RTCPeerConnection>,
    pub events: tokio::sync::broadcast::Sender<CallEvent>,
}

// Configuration for ASR (Automatic Speech Recognition)
#[derive(Debug, Clone, Deserialize)]
pub struct AsrConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub language: Option<String>,
}

// Configuration for Language Model
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    pub model: String,
    pub prompt: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub tools: Option<Vec<serde_json::Value>>,
}

// Configuration for Text-to-Speech
#[derive(Debug, Clone, Deserialize)]
pub struct TtsConfig {
    pub url: String,
    pub voice: Option<String>,
    pub rate: Option<f32>,
}

// Configuration for Voice Activity Detection
#[derive(Debug, Clone, Deserialize)]
pub struct VadConfig {
    pub enabled: bool,
    pub mode: Option<String>,
    pub min_speech_duration_ms: Option<u32>,
    pub min_silence_duration_ms: Option<u32>,
}

// Payload for WebRTC call request
#[derive(Debug, Deserialize)]
pub struct WebRtcCallRequest {
    pub sdp: String,
    pub asr: Option<AsrConfig>,
    pub vad: Option<VadConfig>,
    pub record: Option<bool>,
    pub llm: LlmConfig,
    pub tts: TtsConfig,
}

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
    pub llm: Option<LlmConfig>,
    pub tts: Option<TtsConfig>,
    pub vad: Option<VadConfig>,
    pub record: Option<bool>,
}

// Response for WebRTC SDP answer
#[derive(Debug, Serialize)]
pub struct CallResponse {
    pub session_id: String,
    pub sdp: String,
}

// Call Events
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum CallEvent {
    #[serde(rename = "vad")]
    VadEvent {
        track_id: String,
        timestamp: u32,
        is_speech: bool,
    },
    #[serde(rename = "asr")]
    AsrEvent {
        track_id: String,
        timestamp: u32,
        text: String,
        is_final: bool,
    },
    #[serde(rename = "llm")]
    LlmEvent {
        timestamp: u32,
        text: String,
        is_final: bool,
    },
    #[serde(rename = "tts")]
    TtsEvent { timestamp: u32, text: String },
    #[serde(rename = "metrics")]
    MetricsEvent {
        timestamp: u32,
        metrics: serde_json::Value,
    },
    #[serde(rename = "ringing")]
    RingingEvent { timestamp: u32 },
    #[serde(rename = "hangup")]
    HangupEvent { timestamp: u32, reason: String },
    #[serde(rename = "refer")]
    ReferEvent { timestamp: u32, target: String },
    #[serde(rename = "error")]
    ErrorEvent { timestamp: u32, error: String },
}

// WebSocket Commands
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command")]
pub enum WsCommand {
    #[serde(rename = "play_tts")]
    PlayTts { text: String },
    #[serde(rename = "play_wav")]
    PlayWav { url: String },
    #[serde(rename = "hangup")]
    Hangup {},
    #[serde(rename = "refer")]
    Refer { target: String },
}

// ASR Processor that integrates with the media stream system
pub struct AsrProcessor {
    config: AsrConfig,
    event_sender: tokio::sync::broadcast::Sender<CallEvent>,
}

impl AsrProcessor {
    pub fn new(config: AsrConfig, event_sender: tokio::sync::broadcast::Sender<CallEvent>) -> Self {
        Self {
            config,
            event_sender,
        }
    }
}

impl Processor for AsrProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        if self.config.enabled {
            // In a real implementation, this would process the audio data
            // and send it to an ASR service

            // For now, we'll just simulate ASR events occasionally
            if frame.timestamp % 3000 == 0 {
                let event = CallEvent::AsrEvent {
                    track_id: frame.track_id.clone(),
                    timestamp: frame.timestamp,
                    text: "Sample transcription".to_string(),
                    is_final: frame.timestamp % 6000 == 0,
                };
                let _ = self.event_sender.send(event);
            }
        }
        Ok(())
    }
}

// Adapt MediaStreamEvents to CallEvents
pub fn convert_media_event(event: MediaStreamEvent) -> Option<CallEvent> {
    match event {
        MediaStreamEvent::StartSpeaking(track_id, timestamp) => Some(CallEvent::VadEvent {
            track_id,
            timestamp,
            is_speech: true,
        }),
        MediaStreamEvent::Silence(track_id, timestamp) => Some(CallEvent::VadEvent {
            track_id,
            timestamp,
            is_speech: false,
        }),
        MediaStreamEvent::Transcription(track_id, timestamp, text) => Some(CallEvent::AsrEvent {
            track_id,
            timestamp,
            text,
            is_final: true,
        }),
        MediaStreamEvent::TranscriptionSegment(track_id, timestamp, text) => {
            Some(CallEvent::AsrEvent {
                track_id,
                timestamp,
                text,
                is_final: false,
            })
        }
        _ => None,
    }
}

// Configure Router
pub fn router() -> Router<CallHandlerState> {
    let state = CallHandlerState {
        active_calls: Arc::new(Mutex::new(std::collections::HashMap::new())),
    };

    Router::new()
        .route("/call/webrtc", get(webrtc_ws_handler))
        .route("/call/sip", get(sip_ws_handler))
        .with_state(state)
}

// WebSocket handler for WebRTC calls
async fn webrtc_ws_handler(
    State(state): State<CallHandlerState>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| handle_webrtc_ws(socket, state))
}

// WebSocket handler for SIP calls
async fn sip_ws_handler(
    State(state): State<CallHandlerState>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(|socket| handle_sip_ws(socket, state))
}

// Handler function for WebRTC WebSocket connections
async fn handle_webrtc_ws(socket: axum::extract::ws::WebSocket, state: CallHandlerState) {
    use axum::extract::ws::{Message, WebSocket};
    use futures::{SinkExt, StreamExt};

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

// Handler function for SIP WebSocket connections
async fn handle_sip_ws(socket: axum::extract::ws::WebSocket, state: CallHandlerState) {
    use axum::extract::ws::{Message, WebSocket};
    use futures::{SinkExt, StreamExt};

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

// Common WebSocket session handler for both WebRTC and SIP calls
async fn handle_ws_session(
    session_id: String,
    mut sender: futures::stream::SplitSink<
        axum::extract::ws::WebSocket,
        axum::extract::ws::Message,
    >,
    mut receiver: futures::stream::SplitStream<axum::extract::ws::WebSocket>,
    state: CallHandlerState,
    mut event_rx: tokio::sync::broadcast::Receiver<CallEvent>,
) {
    use axum::extract::ws::Message;
    use futures::{SinkExt, StreamExt};

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

// Helper function to setup WebRTC connection
async fn setup_webrtc_connection(
    state: CallHandlerState,
    request: WebRtcCallRequest,
) -> Result<(String, String, tokio::sync::broadcast::Receiver<CallEvent>), String> {
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
    let (event_sender, event_receiver) = tokio::sync::broadcast::channel(100);

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

    // Subscribe to MediaStream events and forward them to CallEvents
    let ms_events = media_stream.subscribe();
    let events_sender = event_sender.clone();

    tokio::spawn(async move {
        let mut ms_events = ms_events;
        while let Ok(event) = ms_events.recv().await {
            if let Some(call_event) = convert_media_event(event) {
                let _ = events_sender.send(call_event);
            }
        }
    });

    // Set local description (answer)
    peer_connection
        .set_local_description(answer.clone())
        .await
        .map_err(|e| format!("Failed to set local description: {}", e))?;

    // Start serving the MediaStream
    let media_stream_clone = media_stream.clone();
    tokio::spawn(async move {
        if let Err(e) = media_stream_clone.serve().await {
            error!("MediaStream serving error: {}", e);
        }
    });

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

// Helper function to setup SIP connection
async fn setup_sip_connection(
    state: CallHandlerState,
    request: WebRtcSipCallRequest,
) -> Result<(String, String, tokio::sync::broadcast::Receiver<CallEvent>), String> {
    use crate::useragent::client::SipClient;
    use crate::useragent::config::SipConfig;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::time::sleep;

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
    let (event_sender, event_receiver) = tokio::sync::broadcast::channel(100);

    // Create media stream for the connection
    let stream_id = format!("webrtc-sip-{}", session_id);
    let cancel_token = tokio_util::sync::CancellationToken::new();

    let media_stream = Arc::new(
        MediaStreamBuilder::new()
            .id(stream_id.clone())
            .cancel_token(cancel_token.clone())
            .build(),
    );

    // Set up data channel for events
    let _data_channel = peer_connection
        .create_data_channel("events", None)
        .await
        .map_err(|e| format!("Failed to create data channel: {}", e))?;

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
                crate::useragent::client::SipClientEvent::CallEstablished(dialog_id) => {
                    info!("SIP call established: {}", dialog_id);
                    call_established_clone.store(true, Ordering::SeqCst);

                    let _ = event_sender_clone.send(CallEvent::LlmEvent {
                        timestamp: chrono::Utc::now().timestamp() as u32,
                        text: "Call established".to_string(),
                        is_final: true,
                    });
                }
                crate::useragent::client::SipClientEvent::MessageReceived(_, text) => {
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
    use axum::extract::ws::{Message, WebSocket};
    use futures::{stream::StreamExt, SinkExt};
    use tokio::sync::broadcast;

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

        let session_id = Uuid::new_v4().to_string();
        let (event_sender, _) = broadcast::channel(10);

        // Create a real RTCPeerConnection
        let config = RTCConfiguration {
            ice_servers: vec![],
            ..Default::default()
        };

        let api = APIBuilder::new()
            .with_media_engine(MediaEngine::default())
            .with_interceptor_registry(Registry::new())
            .build();

        let peer_connection = Arc::new(api.new_peer_connection(config).await.unwrap());

        let media_stream = Arc::new(
            MediaStreamBuilder::new()
                .id(format!("test-{}", session_id))
                .build(),
        );

        let active_call = ActiveCall {
            session_id: session_id.clone(),
            media_stream,
            peer_connection,
            events: event_sender.clone(),
        };

        state
            .active_calls
            .lock()
            .await
            .insert(session_id.clone(), active_call);

        (state, session_id, event_sender)
    }

    // Mock PeerConnection is no longer used
    // We keep it for reference
    #[derive(Clone)]
    struct MockPeerConnection;

    impl MockPeerConnection {
        pub async fn close(&self) -> Result<(), String> {
            Ok(())
        }
    }

    // Test WsCommand serialization
    #[test]
    fn test_ws_command_serialization() {
        // Test PlayTts command
        let cmd = WsCommand::PlayTts {
            text: "Hello, world!".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("play_tts"));
        assert!(json.contains("Hello, world!"));

        // Test PlayWav command
        let cmd = WsCommand::PlayWav {
            url: "https://example.com/audio.wav".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("play_wav"));
        assert!(json.contains("https://example.com/audio.wav"));

        // Test Hangup command
        let cmd = WsCommand::Hangup {};
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("hangup"));

        // Test deserialization
        let json = r#"{"command":"play_tts","text":"Test TTS"}"#;
        let cmd: WsCommand = serde_json::from_str(json).unwrap();
        match cmd {
            WsCommand::PlayTts { text } => {
                assert_eq!(text, "Test TTS");
            }
            _ => panic!("Unexpected command type"),
        }
    }

    // Test CallEvent serialization
    #[test]
    fn test_call_event_serialization() {
        let event = CallEvent::AsrEvent {
            track_id: "test-track".to_string(),
            timestamp: 12345,
            text: "Test ASR".to_string(),
            is_final: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("asr"));
        assert!(json.contains("test-track"));
        assert!(json.contains("Test ASR"));
        assert!(json.contains("12345"));
        assert!(json.contains("true"));
    }

    // Test media event conversion
    #[test]
    fn test_convert_media_event() {
        // Test StartSpeaking event conversion
        let track_id = "test-track".to_string();
        let timestamp = 12345u32;
        let event = MediaStreamEvent::StartSpeaking(track_id.clone(), timestamp);

        let converted = convert_media_event(event);
        assert!(converted.is_some());

        if let Some(CallEvent::VadEvent {
            track_id: tid,
            timestamp: ts,
            is_speech,
        }) = converted
        {
            assert_eq!(tid, track_id);
            assert_eq!(ts, timestamp);
            assert!(is_speech);
        } else {
            panic!("Converted event is not the expected type");
        }

        // Test Silence event conversion
        let event = MediaStreamEvent::Silence(track_id.clone(), timestamp);
        let converted = convert_media_event(event);
        assert!(converted.is_some());

        if let Some(CallEvent::VadEvent {
            track_id: tid,
            timestamp: ts,
            is_speech,
        }) = converted
        {
            assert_eq!(tid, track_id);
            assert_eq!(ts, timestamp);
            assert!(!is_speech);
        } else {
            panic!("Converted event is not the expected type");
        }
    }

    // Test ASR processor
    #[test]
    fn test_asr_processor() {
        let (sender, _) = broadcast::channel(10);
        let config = AsrConfig {
            enabled: true,
            model: Some("test".to_string()),
            language: Some("en".to_string()),
        };

        let processor = AsrProcessor::new(config, sender.clone());

        // Create test audio frame
        let mut frame = AudioFrame {
            track_id: "test".to_string(),
            samples: Samples::PCM(vec![0; 160]),
            timestamp: 3000, // Choose a timestamp divisible by 3000
            sample_rate: 16000,
        };

        // Test processor processing audio frame
        let result = processor.process_frame(&mut frame);
        assert!(result.is_ok());
    }

    // Test handling web socket command
    #[tokio::test]
    async fn test_handle_ws_command() {
        let (state, session_id, event_sender) = create_test_call().await;

        // Test PlayTts command
        let cmd = WsCommand::PlayTts {
            text: "Test TTS".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();

        // Create a receiver to check events
        let mut receiver = event_sender.subscribe();

        // Handle the command
        handle_ws_command(&state, &session_id, json).await;

        // Check if TTS event was sent
        if let Ok(event) = receiver.try_recv() {
            match event {
                CallEvent::TtsEvent { text, .. } => {
                    assert_eq!(text, "Test TTS");
                }
                _ => panic!("Expected TTS event"),
            }
        } else {
            panic!("No event received");
        }
    }

    // Test CallResponse structure
    #[test]
    fn test_call_response_structure() {
        // Create a call response
        let response = CallResponse {
            session_id: "test-session-id".to_string(),
            sdp: "test-sdp-content".to_string(),
        };

        // Serialize response to JSON
        let json = serde_json::to_string(&response).unwrap();

        // Test JSON structure contains expected fields
        assert!(json.contains("session_id"));
        assert!(json.contains("test-session-id"));
        assert!(json.contains("sdp"));
        assert!(json.contains("test-sdp-content"));
    }
}
