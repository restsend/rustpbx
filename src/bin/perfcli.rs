use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use futures::{SinkExt, StreamExt};
use rustpbx::{
    event::SessionEvent,
    handler::{CallOption, Command},
    media::{
        codecs::{g722::G722Encoder, resample::resample_mono, CodecType, Encoder},
        track::{file::read_wav_file, webrtc::WebrtcTrack},
        vad::VADOption,
    },
    version,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{select, time};
use tokio_tungstenite::tungstenite;
use tracing::{error, info, level_filters::LevelFilter, warn};
use uuid::Uuid;
use webrtc::{
    api::APIBuilder,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

#[derive(Parser, Debug, Clone)]
#[command(
    author,
    version = version::get_short_version(),
    about = "A WebRTC performance test client",
    long_about = version::get_version_info()
)]
struct Cli {
    /// Number of concurrent connections
    #[clap(long, help = "Number of concurrent connections", default_value = "10")]
    clients: u32,

    /// Endpoint of the server
    #[clap(
        long,
        help = "Endpoint of the server",
        default_value = "ws://localhost:8080/call/webrtc"
    )]
    endpoint: String,

    /// Path to the file to play
    #[clap(long, help = "Url to the file to play(Server side)")]
    play_file: String,

    /// Path to the file to play
    #[clap(long, help = "Path to the file to play(Client side)")]
    input_file: String,

    /// Verbose
    #[clap(long, help = "Verbose")]
    verbose: bool,
}

async fn serve_client(cli: Cli, id: u32) -> Result<()> {
    // Create WebRTC client
    let media_engine = WebrtcTrack::get_media_engine(None)?;
    let api = APIBuilder::new().with_media_engine(media_engine).build();
    let config = RTCConfiguration {
        ..Default::default()
    };

    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    // Create an audio track
    let track = WebrtcTrack::create_audio_track(CodecType::G722, None);
    // Add the track to the peer connection
    let _rtp_sender = peer_connection
        .add_track(Arc::clone(&track) as Arc<TrackLocalStaticSample>)
        .await?;

    // Create a channel to collect ICE candidates
    let (ice_candidates_tx, mut ice_candidates_rx) = tokio::sync::mpsc::channel(1);
    let ice_candidates_tx = Arc::new(tokio::sync::Mutex::new(ice_candidates_tx));

    // Set up ICE candidate handler
    let ice_candidates_tx_clone = Arc::clone(&ice_candidates_tx);
    peer_connection.on_ice_candidate(Box::new(
        move |candidate: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
            info!("ICE candidate received: {:?}", candidate);
            let ice_candidates_tx_clone = ice_candidates_tx_clone.clone();
            Box::pin(async move {
                if candidate.is_none() {
                    ice_candidates_tx_clone.lock().await.send(()).await.ok();
                }
            })
        },
    ));

    // Create offer
    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    select! {
        _ = ice_candidates_rx.recv() => {
            info!("ICE gathering ok");
        }
        _ = time::sleep(std::time::Duration::from_secs(3)) => {
            error!("ICE gathering timeout");
        }
    }

    let offer = peer_connection
        .local_description()
        .await
        .ok_or(anyhow::anyhow!("Failed to get local description"))?;

    // Connect to WebSocket
    let session_id = Uuid::new_v4().to_string();
    let url = format!("{}?id=prefcli_{}_{}", cli.endpoint, session_id, id);
    info!(id, "Connecting to WebSocket: {}", url);
    let ws_stream = match tokio_tungstenite::connect_async(url).await {
        Ok((ws_stream, resp)) => {
            info!(id, "Connected to WebSocket: {}", resp.status());
            ws_stream
        }
        Err(e) => {
            match e {
                tungstenite::Error::Http(resp) => {
                    let body = resp.body();
                    let body_str = String::from_utf8_lossy(&body.as_ref().unwrap());
                    error!(id, "Failed to connect to WebSocket: {}", body_str);
                }
                _ => {
                    error!(id, "Failed to connect to WebSocket: {:?}", e);
                }
            }
            return Err(anyhow::anyhow!("Failed to connect to WebSocket"));
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Create the invite command with proper options
    let option = CallOption {
        offer: Some(offer.sdp.clone()),
        vad: Some(VADOption::default()),
        ..Default::default()
    };

    let command = Command::Invite { option };
    // Send the invite command
    let command_str = serde_json::to_string_pretty(&command)?;
    ws_sender
        .send(tungstenite::Message::Text(command_str.into()))
        .await?;

    // Wait for transcription event
    let recv_event_loop = async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            let event: SessionEvent = serde_json::from_str(&msg.to_string())?;
            match event {
                SessionEvent::Answer { sdp, .. } => {
                    info!(id, "Received answer: {}", sdp);
                    let offer = RTCSessionDescription::answer(sdp)?;
                    peer_connection.set_remote_description(offer).await?;
                }
                SessionEvent::Error { error, .. } => {
                    error!(id, "Received error: {}", error);
                    break;
                }
                _ => {
                    info!(id, "Received unexpected event: {:?}", event);
                    continue;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let send_audio_loop = async move {
        // Read test audio file
        let (mut audio_samples, sample_rate) =
            read_wav_file(&cli.input_file).expect("Failed to read input file");
        let mut ticker = time::interval(Duration::from_millis(20));
        let mut encoder = G722Encoder::new();
        let mut packet_timestamp = 0;
        let chunk_size = if encoder.sample_rate() == 16000 {
            320
        } else {
            160
        };
        if sample_rate != encoder.sample_rate() {
            audio_samples = resample_mono(&audio_samples, sample_rate, encoder.sample_rate());
        }
        let start = SystemTime::now();
        for chunk in audio_samples.chunks(chunk_size) {
            let encoded = encoder.encode(&chunk);
            let sample = webrtc::media::Sample {
                data: encoded.into(),
                duration: Duration::from_millis(20),
                timestamp: SystemTime::now(),
                packet_timestamp,
                ..Default::default()
            };
            track.write_sample(&sample).await.unwrap();
            packet_timestamp += chunk_size as u32;
            ticker.tick().await;
        }
        let duration = start.elapsed().unwrap();
        warn!(id, "Audio sent done, duration: {:?}", duration);
    };

    select! {
        _ = recv_event_loop => {
            info!(id, "Transcription received");
        }
        _ = send_audio_loop => {
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    sqlx::any::install_default_drivers();

    let cli = Cli::parse();

    // Set up logging
    tracing_subscriber::fmt()
        .with_max_level(if cli.verbose {
            LevelFilter::DEBUG
        } else {
            LevelFilter::WARN
        })
        .with_file(true)
        .with_line_number(true)
        .try_init()
        .ok();
    dotenv().ok();

    let mut handles = Vec::new();
    for id in 0..cli.clients {
        let cli = cli.clone();
        handles.push(tokio::spawn(async move {
            loop {
                serve_client(cli.clone(), id)
                    .await
                    .expect("Failed to serve client")
            }
        }));
    }

    info!("Waiting for {} clients to connect...", cli.clients);
    for handle in handles {
        handle.await.expect("Failed to join client");
    }
    Ok(())
}
