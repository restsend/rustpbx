use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rustpbx::media::{
    stream::MediaStreamBuilder,
    track::{file::FileTrack, webrtc::WebrtcTrack, TrackConfig},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    env::current_dir,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tracing::{error, info, warn, Level};
use uuid::Uuid;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use clap::Parser;

#[derive(Parser)]
struct Args {
    #[clap(short, long, default_value = "assets/sample.wav")]
    media_path: String,
}
// Application state
struct AppState {
    connections: Mutex<HashMap<String, ConnectionState>>,
    root_dir: PathBuf,
    media_path: PathBuf,
}

struct ConnectionState {
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
    ice_candidates: Vec<RTCIceCandidateInit>,
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
    let connection_id = Uuid::new_v4().to_string();
    let cancel_token = CancellationToken::new();
    let event_sender = rustpbx::event::create_event_sender();
    let media_stream_builder = MediaStreamBuilder::new(event_sender.clone());
    let media_stream = Arc::new(
        media_stream_builder
            .with_cancel_token(cancel_token.clone())
            .build(),
    );
    let track_id = format!("webrtc-{}", Uuid::new_v4());
    let mut webrtc_track = WebrtcTrack::new(
        cancel_token.child_token(),
        track_id.clone(),
        TrackConfig::default(),
    );
    let sample_path = state.media_path.to_string_lossy().to_string();
    // Create connection state
    let connection_state = ConnectionState {
        cancel_token: cancel_token.clone(),
    };
    let mut local_desc = None;
    match webrtc_track.setup_webrtc_track(offer.sdp.sdp, None).await {
        Ok(answer) => {
            info!("Webrtc track setup complete {}", answer.sdp);
            let file_track_id = format!("file-{}", Uuid::new_v4());
            let file_track = FileTrack::new(file_track_id.clone()).with_path(sample_path);
            local_desc = Some(answer);
            media_stream.update_track(Box::new(webrtc_track)).await;
            media_stream.update_track(Box::new(file_track)).await;
            tokio::spawn(async move {
                if let Err(e) = media_stream.serve().await {
                    error!("Media stream error: {}", e);
                }
            });
        }
        Err(e) => {
            warn!("Failed to setup webrtc track: {}", e);
        }
    }
    // Store connection in state
    {
        let mut connections = state.connections.lock().unwrap();
        connections.insert(connection_id.clone(), connection_state);
    }

    let answer_json = WebRTCAnswer {
        sdp: WebRTCSessionDescription {
            type_field: "answer".to_string(),
            sdp: local_desc.unwrap().sdp,
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
    let args = Args::parse();
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

    let media_path = root_dir.join(args.media_path);

    info!("Looking for Media file at path: {:?}", media_path);
    if !media_path.exists() {
        error!("Media file does not exist at path: {:?}", media_path);
        return Err(anyhow::anyhow!(
            "Media file does not exist at path: {:?}",
            media_path
        ));
    }

    // Create app state
    let state = Arc::new(AppState {
        connections: Mutex::new(HashMap::new()),
        root_dir,
        media_path,
    });

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
