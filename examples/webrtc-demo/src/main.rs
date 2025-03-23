use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use hound::{SampleFormat, WavReader, WavSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
};
use tower_http::services::ServeDir;
use tracing::{error, info, Level};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS},
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
    connections: Mutex<HashMap<String, Arc<RTCPeerConnection>>>,
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
async fn index_handler() -> impl IntoResponse {
    Html(include_str!("../static/index.html"))
}

// Handle WebRTC offer
async fn offer_handler(
    State(state): State<Arc<AppState>>,
    Json(offer): Json<WebRTCOffer>,
) -> impl IntoResponse {
    match process_offer(state, offer).await {
        Ok(answer) => (StatusCode::OK, Json(answer)),
        Err(e) => {
            error!("Failed to process offer: {}", e);
            let error_answer = WebRTCAnswer {
                sdp: WebRTCSessionDescription {
                    type_field: "error".to_string(),
                    sdp: e.to_string(),
                },
                ice_candidates: vec![],
            };
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_answer))
        }
    }
}

// Process WebRTC offer and create answer
async fn process_offer(state: Arc<AppState>, offer: WebRTCOffer) -> Result<WebRTCAnswer> {
    // Create media engine
    let mut media_engine = MediaEngine::default();

    // Register opus codec
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
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
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create peer connection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Create a static sample track
    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_string(),
            ..Default::default()
        },
        "audio".to_string(),
        "wav-player".to_string(),
    ));

    // Add track to peer connection
    let _rtp_sender = peer_connection
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Handle connection state changes
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("Peer connection state changed: {}", s);

        if s == RTCPeerConnectionState::Connected {
            // In a real implementation, this is where we would start
            // playing the WAV file to the WebRTC track
            info!("Connection established - would start playing audio");
        }

        Box::pin(async {})
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
    let connection_id = uuid::Uuid::new_v4().to_string();
    {
        let mut connections = state.connections.lock().unwrap();
        connections.insert(connection_id, Arc::clone(&peer_connection));
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

    Ok(answer_json)
}

// Simple handler for ICE candidates and closing connections
async fn simple_handler() -> impl IntoResponse {
    StatusCode::OK
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_max_level(Level::INFO)
        .init();

    // Create sample WAV file if it doesn't exist
    let wav_path = Path::new("examples/webrtc-demo/assets/sample.wav");
    if !wav_path.exists() {
        info!("Creating sample WAV file");
        create_sample_wav_file(wav_path)?;
    }

    // Create app state
    let state = Arc::new(AppState {
        connections: Mutex::new(HashMap::new()),
    });

    // Build router
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/webrtc/offer", post(offer_handler))
        .route("/webrtc/signal", post(simple_handler))
        .route("/webrtc/close", post(simple_handler))
        .nest_service("/static", ServeDir::new("examples/webrtc-demo/static"))
        .with_state(state);

    // Bind to address
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Listening on http://{}", addr);

    // Start server
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// Create a sample WAV file for testing
fn create_sample_wav_file(path: &Path) -> Result<()> {
    // Create WAV spec - 48kHz, mono, 16-bit PCM
    let spec = WavSpec {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    // Create WAV writer
    let mut writer = hound::WavWriter::create(path, spec)?;

    // Generate a 3-second sine wave at 440Hz
    let sample_rate = spec.sample_rate as f32;
    let frequency = 440.0; // A4 note
    let duration = 3.0; // 3 seconds

    for t in 0..(sample_rate as usize * duration as usize) {
        let sample = (t as f32 * frequency * 2.0 * std::f32::consts::PI / sample_rate).sin();
        let amplitude = 0.5;
        let sample_i16 = (sample * amplitude * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    info!("Created sample WAV file at: {}", path.display());

    Ok(())
}

// Unit tests
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;

    #[tokio::test]
    async fn test_create_sample_wav_file() -> Result<()> {
        // Create a temporary directory for test files
        let temp_dir = env::temp_dir().join("webrtc_demo_test");
        fs::create_dir_all(&temp_dir)?;

        // Create a test WAV file path
        let test_wav_path = temp_dir.join("test_sample.wav");

        // Remove the file if it already exists
        if test_wav_path.exists() {
            fs::remove_file(&test_wav_path)?;
        }

        // Create the sample WAV file
        create_sample_wav_file(&test_wav_path)?;

        // Check if the file exists
        assert!(test_wav_path.exists());

        // Open the WAV file to check its properties
        let reader = WavReader::open(&test_wav_path)?;
        let spec = reader.spec();

        // Verify the WAV file properties
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, SampleFormat::Int);

        // Clean up
        fs::remove_file(&test_wav_path)?;

        Ok(())
    }

    // Note: We cannot easily test the WebRTC functionality in a unit test
    // as it requires a full browser environment. Integration tests would be
    // more appropriate for that part of the code.
}
