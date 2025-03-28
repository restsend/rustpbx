use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rustpbx::media::{
    codecs::CodecType,
    stream::{MediaStream, MediaStreamBuilder, MediaStreamEvent},
    track::{
        file::FileTrack,
        webrtc::{WebrtcTrack, WebrtcTrackConfig},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    env::current_dir,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tracing::{debug, error, info, Level};
use uuid::Uuid;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_G722, MIME_TYPE_PCMU},
        APIBuilder,
    },
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
};

// Application state
struct AppState {
    connections: Mutex<HashMap<String, ConnectionState>>,
    root_dir: PathBuf,
}

struct ConnectionState {
    peer_connection: Arc<RTCPeerConnection>,
    media_stream: Arc<MediaStream>,
    cancel_token: CancellationToken,
}

// WebRTC Session
#[derive(Debug, Serialize, Deserialize)]
struct WebRTCSessionDescription {
    #[serde(rename = "type")]
    type_field: String,
    sdp: String,
}

// WebRTC Offer request
#[derive(Debug, Deserialize)]
struct WebRTCOffer {
    sdp: WebRTCSessionDescription,
}

// WebRTC Answer response
#[derive(Debug, Serialize)]
struct WebRTCAnswer {
    sdp: WebRTCSessionDescription,
    #[serde(skip_serializing_if = "Vec::is_none")]
    ice_candidates: Vec<RTCIceCandidateInit>,
}

// Helper extension trait
trait VecExt<T> {
    fn is_none(&self) -> bool;
}

impl<T> VecExt<T> for Vec<T> {
    fn is_none(&self) -> bool {
        self.is_empty()
    }
}

// Index page handler
async fn index_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let html_html = std::fs::read_to_string(state.root_dir.join("static/index.html")).unwrap();
    Html(html_html)
}

// Handle WebRTC offer
async fn offer_handler(
    State(state): State<Arc<AppState>>,
    Json(offer): Json<WebRTCOffer>,
) -> impl IntoResponse {
    match process_offer(state, offer).await {
        Ok((id, answer)) => (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "answer": answer
            })),
        ),
        Err(e) => {
            error!("Failed to process offer: {}", e);
            let error_answer = WebRTCAnswer {
                sdp: WebRTCSessionDescription {
                    type_field: "error".to_string(),
                    sdp: e.to_string(),
                },
                ice_candidates: vec![],
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "id": Uuid::new_v4().to_string(),
                    "answer": error_answer
                })),
            )
        }
    }
}

// Process WebRTC offer and create answer
async fn process_offer(state: Arc<AppState>, offer: WebRTCOffer) -> Result<(String, WebRTCAnswer)> {
    // Create media engine
    let mut media_engine = MediaEngine::default();

    // Register G722 codec - use G722 instead of Opus as required
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_G722.to_string(),
                clock_rate: 8000, // G722 uses 8kHz clock rate (16kHz samples)
                channels: 1,      // Mono
                sdp_fmtp_line: "".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 9, // Standard payload type for G722
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_PCMU.to_string(),
                clock_rate: 8000, // G722 uses 8kHz clock rate (16kHz samples)
                channels: 1,      // Mono
                sdp_fmtp_line: "".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 0, // Standard payload type for G722
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    // Create registry and API
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    // Create configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create peer connection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    let stream_samplerate = 16000;
    let mime_type = MIME_TYPE_G722.to_string();
    // Create audio track
    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type,
            clock_rate: 8000,
            channels: 1,
            ..Default::default()
        },
        "audio".to_string(),
        "wav-player".to_string(),
    ));

    // Add track to peer connection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Create a cancellation token
    let cancel_token = CancellationToken::new();

    // Create MediaStream
    let media_stream = Arc::new(
        MediaStreamBuilder::new()
            .id(format!("demo-stream-{}", Uuid::new_v4()))
            .cancel_token(cancel_token.child_token())
            .build(),
    );

    // Create connection state
    let connection_state = ConnectionState {
        peer_connection: Arc::clone(&peer_connection),
        media_stream: Arc::clone(&media_stream),
        cancel_token: cancel_token.clone(),
    };

    // Handle connection state changes
    let connection_id = Uuid::new_v4().to_string();
    let state_clone = Arc::clone(&state);
    let connection_id_clone = connection_id.clone();
    let track_clone = Arc::clone(&track);
    let sample_path = state
        .root_dir
        .join("assets/sample.wav")
        .canonicalize()?
        .to_string_lossy()
        .to_string();

    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        let connection_id = connection_id_clone.clone();
        let state = Arc::clone(&state_clone);
        let track = Arc::clone(&track_clone);
        let sample_path = sample_path.clone();

        Box::pin(async move {
            info!("Peer connection state changed: {}", s);

            if s == RTCPeerConnectionState::Connected {
                info!("Connection established - starting MediaStream");

                // Get the MediaStream from the connection state
                let mut media_stream_to_serve = None;
                {
                    let connections = state.connections.lock().unwrap();
                    if let Some(conn_state) = connections.get(&connection_id) {
                        media_stream_to_serve = Some(Arc::clone(&conn_state.media_stream));
                    }
                }

                if let Some(media_stream) = media_stream_to_serve {
                    // Start the media stream in a separate task
                    tokio::spawn(async move {
                        // Create WebRTC track
                        let webrtc_track_id = format!("webrtc-{}", Uuid::new_v4());
                        let webrtc_track = WebrtcTrack::new(webrtc_track_id.clone())
                            .with_sample_rate(stream_samplerate) // G722 uses 16kHz sample rate
                            .with_webrtc_track(WebrtcTrackConfig {
                                track,
                                payload_type: 9, // G722 payload type
                            });

                        // Create file track
                        let file_track_id = format!("file-{}", Uuid::new_v4());
                        let file_track = FileTrack::new(file_track_id.clone())
                            .with_sample_rate(stream_samplerate) // G722 uses 16kHz sample rate
                            .with_path(sample_path);

                        // Add tracks to the media stream
                        media_stream.update_track(Box::new(webrtc_track)).await;
                        media_stream.update_track(Box::new(file_track)).await;

                        // Serve the media stream
                        if let Err(e) = media_stream.serve().await {
                            error!("Media stream error: {}", e);
                        }
                    });
                }
            } else if s == RTCPeerConnectionState::Disconnected
                || s == RTCPeerConnectionState::Failed
                || s == RTCPeerConnectionState::Closed
            {
                // Clean up connection
                let mut connections = state.connections.lock().unwrap();
                if let Some(conn_state) = connections.remove(&connection_id) {
                    // Cancel the media stream
                    conn_state.cancel_token.cancel();
                }
            }
        })
    }));

    // Set remote description from offer
    let remote_desc = RTCSessionDescription::offer(offer.sdp.sdp)?;
    peer_connection.set_remote_description(remote_desc).await?;

    // Create answer
    let answer = peer_connection.create_answer(None).await?;
    peer_connection
        .set_local_description(answer.clone())
        .await?;

    // Store connection in state
    {
        let mut connections = state.connections.lock().unwrap();
        connections.insert(connection_id.clone(), connection_state);
    }

    // Return answer to client
    let local_desc = peer_connection.local_description().await.unwrap();
    let answer_json = WebRTCAnswer {
        sdp: WebRTCSessionDescription {
            type_field: "answer".to_string(),
            sdp: local_desc.sdp,
        },
        ice_candidates: vec![],
    };

    Ok((connection_id, answer_json))
}

// Simple handler for ICE candidates and closing connections
async fn close_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
        let mut connections = state.connections.lock().unwrap();
        if let Some(conn_state) = connections.remove(id) {
            // Cancel the media stream
            conn_state.cancel_token.cancel();
        }
    }
    StatusCode::OK
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::INFO)
        .init();

    let root_dir = vec![
        current_dir().unwrap(),
        Path::new("..").to_path_buf(),
        Path::new("examples/webrtc-demo").to_path_buf(),
    ]
    .iter()
    .find(|p| p.join("assets").exists())
    .unwrap()
    .to_path_buf();
    // Create app state
    let state = Arc::new(AppState {
        connections: Mutex::new(HashMap::new()),
        root_dir,
    });
    // // Create sample WAV file if it doesn't exist
    let wav_path = state.root_dir.join("assets/sample.wav");
    info!("Looking for WAV file at path: {:?}", wav_path);
    if !wav_path.exists() {
        error!("WAV file does not exist at path: {:?}", wav_path);
        return Err(anyhow::anyhow!(
            "WAV file does not exist at path: {:?}",
            wav_path
        ));
    }

    // Build router
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/webrtc/offer", post(offer_handler))
        .route("/webrtc/close", post(close_handler))
        .nest_service("/static", ServeDir::new(state.root_dir.join("static")))
        .with_state(state);

    // Bind to address
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Listening on http://{}", addr);

    // Start server
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
