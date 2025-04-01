use crate::event::SessionEvent;
use crate::media::codecs::g722::G722Encoder;
use crate::media::codecs::{convert_u8_to_s16, Encoder};
use crate::media::vad::VadType;
use crate::transcription::TranscriptionType;
use crate::{
    handler::{call::CallHandlerState, Command, StreamOptions},
    synthesis::{SynthesisConfig, SynthesisType},
    transcription::TranscriptionConfig,
};
use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use dotenv::dotenv;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::{fs::File, io::AsyncReadExt, net::TcpListener};
use tokio::{select, time};
use tokio_tungstenite::tungstenite;
use tracing::{error, info};
use uuid::Uuid;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    peer_connection::configuration::RTCConfiguration,
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

// Error handling middleware
async fn handle_error(
    State(_state): State<CallHandlerState>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    error!("Error handling request: {:?}", request);
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[tokio::test]
async fn test_webrtc_audio_streaming() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    dotenv().ok();

    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .try_init()
        .ok();

    let app = Router::new()
        .route(
            "/ws",
            axum::routing::get(crate::handler::webrtc::webrtc_handler),
        )
        .fallback(handle_error)
        .with_state(CallHandlerState::new());

    // Start the server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Create WebRTC client
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: "audio/G722".to_owned(),
                clock_rate: 8000,
                channels: 1,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 9,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    let api = APIBuilder::new().with_media_engine(media_engine).build();

    let config = RTCConfiguration {
        ..Default::default()
    };

    let peer_connection = Arc::new(api.new_peer_connection(config).await?);
    // Create an audio track
    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: "audio/G722".to_owned(),
            clock_rate: 8000,
            channels: 1,
            ..Default::default()
        },
        "audio".to_owned(),
        "webrtc-rs".to_owned(),
    ));

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
        .pending_local_description()
        .await
        .ok_or(anyhow::anyhow!("Failed to get local description"))?;

    // Connect to WebSocket
    let session_id = Uuid::new_v4().to_string();
    let url = format!("ws://127.0.0.1:{}/ws?id={}", server_addr.port(), session_id);
    info!("Connecting to WebSocket: {}", url);
    let ws_stream = match tokio_tungstenite::connect_async(url).await {
        Ok((ws_stream, resp)) => {
            info!("Connected to WebSocket: {}", resp.status());
            ws_stream
        }
        Err(e) => {
            match e {
                tungstenite::Error::Http(resp) => {
                    let body = resp.body();
                    let body_str = String::from_utf8_lossy(&body.as_ref().unwrap());
                    error!("Failed to connect to WebSocket: {}", body_str);
                }
                _ => {
                    error!("Failed to connect to WebSocket: {:?}", e);
                }
            }
            return Err(anyhow::anyhow!("Failed to connect to WebSocket"));
        }
    };
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let mut asr_config = TranscriptionConfig::default();
    let mut tts_config = SynthesisConfig::default();
    let tencent_appid = std::env::var("TENCENT_APPID").unwrap();
    let tencent_secret_id = std::env::var("TENCENT_SECRET_ID").unwrap();
    let tencent_secret_key = std::env::var("TENCENT_SECRET_KEY").unwrap();

    asr_config.appid = Some(tencent_appid.clone());
    asr_config.secret_id = Some(tencent_secret_id.clone());
    asr_config.secret_key = Some(tencent_secret_key.clone());

    tts_config.appid = Some(tencent_appid);
    tts_config.secret_id = Some(tencent_secret_id);
    tts_config.secret_key = Some(tencent_secret_key);

    // Create the invite command with proper options
    let options = StreamOptions {
        sdp: Some(offer.sdp.clone()),
        enable_recorder: Some(true),
        vad_type: Some(VadType::WebRTC),
        asr_type: Some(TranscriptionType::TencentCloud),
        asr_config: Some(asr_config),
        tts_type: Some(SynthesisType::TencentCloud),
        tts_config: Some(tts_config),
        ..Default::default()
    };

    let command = Command::Invite { options };
    // Send the invite command
    let command_str = serde_json::to_string_pretty(&command)?;
    ws_sender
        .send(tungstenite::Message::Text(command_str.into()))
        .await?;

    let msg = ws_receiver.next().await.unwrap().unwrap();
    let event: SessionEvent = serde_json::from_str(&msg.to_string())?;
    match event {
        SessionEvent::Answer {
            track_id,
            timestamp,
            sdp,
        } => {
            info!("Received answer: {}", sdp);
            let offer = RTCSessionDescription::answer(sdp)?;
            peer_connection.set_remote_description(offer).await?;
        }
        _ => {
            error!("Received unexpected event: {:?}", event);
            return Err(anyhow::anyhow!("Received unexpected event"));
        }
    }

    // Wait for transcription event
    let recv_event_loop = async move {
        let mut transcription_received = false;
        while !transcription_received {
            if let Some(Ok(msg)) = ws_receiver.next().await {
                if let tungstenite::Message::Text(text) = msg {
                    info!("Received message: {}", text);
                    let event: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if event["type"] == "transcription_final" {
                        assert_eq!(event["text"].as_str().unwrap(), "你好,请帮我预约课程");
                        transcription_received = true;
                    }
                }
            }
        }
    };
    let send_audio_loop = async move {
        // Read test audio file
        let mut file = File::open("fixtures/hello_book_course_zh_16k.wav")
            .await
            .unwrap();
        let mut audio_data = Vec::new();
        file.read_to_end(&mut audio_data)
            .await
            .expect("Failed to read audio file");
        let mut ticker = time::interval(std::time::Duration::from_millis(20));
        let mut encoder_g722 = G722Encoder::new();
        for chunk in audio_data.chunks(640) {
            let samples = convert_u8_to_s16(chunk);
            let g722_data = encoder_g722.encode(&samples).unwrap();
            let sample = webrtc::media::Sample {
                data: g722_data.into(),
                duration: std::time::Duration::from_millis(20),
                timestamp: SystemTime::now(),
                ..Default::default()
            };
            track.write_sample(&sample).await.unwrap();
            ticker.tick().await;
        }
    };

    select! {
        _ = recv_event_loop => {
            info!("Transcription received");
        }
        _ = send_audio_loop => {
            info!("Audio sent");
        }
        _ = time::sleep(std::time::Duration::from_secs(30)) => {
            error!("Transcription timeout");
        }
    }

    Ok(())
}
