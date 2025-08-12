use crate::app::{AppState, AppStateBuilder};
use crate::call::{CallOption, Command};
use crate::callrecord::CallRecordManagerBuilder;
use crate::config::{Config, ProxyConfig, UseragentConfig};
use crate::event::SessionEvent;
use crate::media::codecs::g722::G722Encoder;
use crate::media::codecs::resample::resample_mono;
use crate::media::codecs::{CodecType, Encoder};
use crate::media::track::file::read_wav_file;
use crate::media::track::webrtc::WebrtcTrack;
use crate::synthesis::SynthesisType;
use crate::transcription::TranscriptionType;
use crate::{synthesis::SynthesisOption, transcription::TranscriptionOption};
use anyhow::Result;
use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use dotenv::dotenv;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio::{select, time};
use tokio_tungstenite::tungstenite;
use tokio_util::time::FutureExt;
use tracing::{error, info};
use uuid::Uuid;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::{
    api::APIBuilder, peer_connection::configuration::RTCConfiguration,
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

// Error handling middleware
async fn handle_error(
    State(_state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    error!("Error handling request: {:?}", request);
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[tokio::test]
async fn test_webrtc_audio_streaming() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    dotenv().ok();
    // Create a custom config with different ports to avoid conflicts
    let mut config = Config::default();
    // Use different static ports for this test to avoid conflicts
    let ua_port = 25062; // Different from default 25060 and ws_test 25061
    let proxy_port = 5062; // Different from default 5060 and ws_test 5061

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

    // Create a minimal state with call record manager
    let mut callrecord = CallRecordManagerBuilder::new()
        .with_config(config.callrecord.clone().unwrap_or_default())
        .build();
    let callrecord_sender = callrecord.sender.clone();

    tokio::spawn(async move {
        callrecord.serve().await;
    });
    let (state, _) = AppStateBuilder::new()
        .with_config(config)
        .with_callrecord_sender(callrecord_sender)
        .build()
        .await?;
    let app = Router::new()
        .route(
            "/ws",
            axum::routing::get(crate::handler::handler::webrtc_handler),
        )
        .fallback(handle_error)
        .with_state(state);

    // Start the server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

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

    let mut asr_config = TranscriptionOption::default();
    let mut tts_config = SynthesisOption::default();
    let tencent_appid = std::env::var("TENCENT_APPID").unwrap();
    let tencent_secret_id = std::env::var("TENCENT_SECRET_ID").unwrap();
    let tencent_secret_key = std::env::var("TENCENT_SECRET_KEY").unwrap();

    asr_config.provider = Some(TranscriptionType::TencentCloud);
    asr_config.app_id = Some(tencent_appid.clone());
    asr_config.secret_id = Some(tencent_secret_id.clone());
    asr_config.secret_key = Some(tencent_secret_key.clone());
    asr_config.model_type = Some("16k_zh".to_string());

    tts_config.provider = Some(SynthesisType::TencentCloud);
    tts_config.app_id = Some(tencent_appid);
    tts_config.secret_id = Some(tencent_secret_id);
    tts_config.secret_key = Some(tencent_secret_key);
    tts_config.speaker = Some("101001".to_string());

    // Create the invite command with proper options
    let option = CallOption {
        offer: Some(offer.sdp.clone()),
        vad: Some(crate::media::vad::VADOption::default()),
        asr: Some(asr_config),
        tts: Some(tts_config),
        ..Default::default()
    };

    let command = Command::Invite { option };
    // Send the invite command
    let command_str = serde_json::to_string_pretty(&command)?;
    ws_sender
        .send(tungstenite::Message::Text(command_str.into()))
        .await?;
    let has_answer = Arc::new(tokio::sync::Mutex::new(false));
    let has_answer_clone = Arc::clone(&has_answer);
    // Wait for transcription event
    let recv_event_loop = async move {
        loop {
            let msg = match ws_receiver.next().await {
                Some(Ok(msg)) => msg,
                Some(Err(e)) => {
                    error!("WebSocket error: {:?}", e);
                    assert!(false, "WebSocket error: {:?}", e);
                    break;
                }
                None => {
                    info!("WebSocket connection closed");
                    break;
                }
            };
            let event: SessionEvent = serde_json::from_str(&msg.to_string())?;
            match event {
                SessionEvent::Answer { sdp, .. } => {
                    info!("Received answer: {}", sdp);
                    *has_answer_clone.lock().await = true;
                    let offer = RTCSessionDescription::answer(sdp)?;
                    peer_connection.set_remote_description(offer).await?;
                }
                SessionEvent::Error { error, .. } => {
                    error!("Received error: {}", error);
                    assert!(false, "Received error: {}", error);
                    break;
                }
                _ => {
                    info!("Received unexpected event: {:?}", event);
                    continue;
                }
            }
        }
        info!("Event loop finished");
        Ok::<(), anyhow::Error>(())
    };
    let send_audio_loop = async move {
        // Read test audio file
        let (mut audio_samples, sample_rate) =
            read_wav_file("fixtures/hello_book_course_zh_16k.wav").unwrap();
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
        info!(
            "Audio samples length: {} duration: {}",
            audio_samples.len(),
            audio_samples.len() as f64 / sample_rate as f64
        );
        let start_time = SystemTime::now();
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
        info!(
            "Audio sent done, duration: {:?}",
            start_time.elapsed().unwrap()
        );
    };

    select! {
        _ = recv_event_loop => {
            info!("Transcription received");
        }
        _ = send_audio_loop => {
            info!("Audio sent successfully");
        }
        _ = time::sleep(std::time::Duration::from_secs(30)) => {
            error!("Transcription timeout");
        }
    }
    assert!(*has_answer.lock().await, "No answer received");

    Ok(())
}

#[tokio::test]
async fn test_tts_interrupt() -> Result<()> {
    dotenv().ok();
    let tencent_appid = std::env::var("TENCENT_APPID").unwrap();
    let tencent_secret_id = std::env::var("TENCENT_SECRET_ID").unwrap();
    let tencent_secret_key = std::env::var("TENCENT_SECRET_KEY").unwrap();
    let tecent_tts_config = SynthesisOption {
        provider: Some(SynthesisType::TencentCloud),
        app_id: Some(tencent_appid),
        secret_id: Some(tencent_secret_id),
        secret_key: Some(tencent_secret_key),
        speaker: Some("101001".to_string()),
        ..Default::default()
    };

    assert!(test_tts_interrupt_with_config(tecent_tts_config.clone(), "你好，我是吴彦祖，今天星期二，我想吃一碗拉面").await?);

    let dashscope_api_key = std::env::var("DASHSCOPE_API_KEY").unwrap();
    let aliyun_tts_config = SynthesisOption {
        provider: Some(SynthesisType::Aliyun),
        secret_key: Some(dashscope_api_key),
        speaker: Some("longyumi_v2".to_string()), // Default voice
        volume: Some(5),                          // Medium volume (0-10)
        speed: Some(1.0),                         // Normal speed
        codec: Some("pcm".to_string()),           // PCM format for easy verification
        samplerate: Some(16000),                  // 16kHz sample rate
        ..Default::default()
    };

    assert!(test_tts_interrupt_with_config(aliyun_tts_config, "你好，我是吴彦祖，今天星期三，我想吃麻辣烫").await?);
    Ok(())
}

async fn get_random_port() -> Result<u16> {
    // Bind to port 0 - OS will assign a free port
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener); // Release the port
    Ok(port)
}

async fn test_tts_interrupt_with_config(tts_config: SynthesisOption, text: &str) -> Result<bool> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    // Create a custom config with different ports to avoid conflicts
    let mut config = Config::default();
    // Use different static ports for this test to avoid conflicts
    let ua_port = get_random_port().await?;
    let proxy_port = get_random_port().await?;

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

    // Create a minimal state with call record manager
    let mut callrecord = CallRecordManagerBuilder::new()
        .with_config(config.callrecord.clone().unwrap_or_default())
        .build();
    let callrecord_sender = callrecord.sender.clone();

    tokio::spawn(async move {
        callrecord.serve().await;
    });

    let app = Router::new()
        .route(
            "/ws",
            axum::routing::get(crate::handler::handler::webrtc_handler),
        )
        .fallback(handle_error)
        .with_state(
            AppStateBuilder::new()
                .config(config)
                .with_callrecord_sender(callrecord_sender)
                .build()
                .await?,
        );

    // Start the server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

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

    let (track_tx, mut track_rx) = tokio::sync::mpsc::channel::<()>(1);
    peer_connection.on_track(Box::new(move |remote, _, _| {
        let remote_clone = Arc::clone(&remote);
        let mut track_tx = Some(track_tx.clone());

        Box::pin(async move {
            while let Ok((_packet, _)) = remote_clone.read_rtp().await {
                if let Some(track_tx) = track_tx.take() {
                    track_tx.send(()).await.ok();
                }
            }
        })
    }));

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

    // Create the invite command with proper options
    let option = CallOption {
        offer: Some(offer.sdp.clone()),
        vad: None,
        asr: None,
        tts: Some(tts_config.clone()),
        ..Default::default()
    };

    let invite_command = Command::Invite { option };
    let invite_command_str = serde_json::to_string_pretty(&invite_command)?;
    ws_sender
        .send(tungstenite::Message::Text(invite_command_str.into()))
        .await?;

    let tts_command = Command::Tts {
        text: text.to_string(),
        speaker: None,
        play_id: None,
        auto_hangup: Some(false),
        streaming: None,
        end_of_stream: None,
        option: None,
        wait_input_timeout: None,
    };

    let tts_command_str = serde_json::to_string_pretty(&tts_command)?;
    ws_sender
        .send(tungstenite::Message::Text(tts_command_str.into()))
        .await?;

    tokio::spawn(async move {
        let _ = track_rx.recv().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let interrupt_command = Command::Interrupt {};
        let interrupt_command_str = serde_json::to_string_pretty(&interrupt_command).unwrap();
        let _ = ws_sender
            .send(tungstenite::Message::Text(interrupt_command_str.into()))
            .await;
        info!("Interrupt command sent");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupted_clone = Arc::clone(&interrupted);
    let peer_connection_clone = Arc::clone(&peer_connection);
    let recv_event_loop = async move {
        while let Some(Ok(msg)) = ws_receiver.next().await
            && let Ok(event) = serde_json::from_str(&msg.to_string())
        {
            match event {
                SessionEvent::Answer { sdp, .. } => {
                    info!("Received answer: {}", sdp);
                    let offer = RTCSessionDescription::answer(sdp).unwrap();
                    peer_connection_clone
                        .set_remote_description(offer)
                        .await
                        .unwrap();
                }
                SessionEvent::OnInterrupt {
                    subtitle,
                    position,
                    total_duration,
                    current,
                } => {
                    interrupted_clone.store(true, Ordering::Relaxed);
                    info!(
                        "OnInterrupt: subtitle: {:?}, position: {:?}, total_duration: {:?}, current: {:?}",
                        subtitle, position, total_duration, current
                    );
                    break;
                }
                SessionEvent::Error { .. } => {
                    break;
                }
                SessionEvent::TrackEnd { .. } => {
                    break;
                }
                others @ _ => {
                    info!("Received unexpected event: {:?}", others);
                    continue;
                }
            }
        }
    };

    let _ = recv_event_loop
        .timeout(std::time::Duration::from_secs(30))
        .await
        .unwrap();

    Ok(interrupted.load(Ordering::Relaxed))
}
