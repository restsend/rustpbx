use crate::app::{AppState, AppStateBuilder};
use crate::callrecord::CallRecordManagerBuilder;
use crate::config::{Config, ProxyConfig, UseragentConfig};
use crate::event::SessionEvent;
use crate::handler::{CallOption, Command};
use crate::media::codecs::samples_to_bytes;
use crate::media::track::file::read_wav_file;
use crate::synthesis::{SynthesisOption, SynthesisType};
use crate::transcription::{TranscriptionOption, TranscriptionType};
use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use dotenv::dotenv;
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::{select, time};
use tokio_tungstenite::tungstenite;
use tracing::{error, info};
use uuid::Uuid;

// Error handling middleware
async fn handle_error(
    State(_state): State<AppState>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    error!("Error handling request: {:?}", request);
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[tokio::test]
async fn test_websocket_pcm_streaming() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    dotenv().ok();
    // Create a custom config with different ports to avoid conflicts
    let mut config = Config::default();
    // Use different static ports for this test to avoid conflicts
    let ua_port = 25061; // Different from default 25060
    let proxy_port = 5061; // Different from default 5060

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

    // Create a minimal state with call record manager but without proxy/useragent
    let mut callrecord = CallRecordManagerBuilder::new()
        .with_config(config.callrecord.clone().unwrap_or_default())
        .build();

    let callrecord_sender = callrecord.sender.clone();
    tokio::spawn(async move {
        callrecord.serve().await;
    });

    let app = Router::new()
        .route(
            "/call",
            axum::routing::get(crate::handler::handler::ws_handler),
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
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to 127.0.0.1:0: {}", e);
            return Ok(());
        }
    };
    let server_addr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Connect to WebSocket
    let session_id = Uuid::new_v4().to_string();
    let url = format!(
        "ws://127.0.0.1:{}/call?id={}",
        server_addr.port(),
        session_id
    );
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
        // Explicitly specify we're using PCM codec
        codec: Some("pcm".to_string()),
        // Add a dummy SDP offer (this is required by the call handler)
        offer: None,
        asr: Some(TranscriptionOption {
            provider: Some(TranscriptionType::TencentCloud),
            ..Default::default()
        }),
        tts: Some(SynthesisOption {
            provider: Some(SynthesisType::TencentCloud), // Using VoiceApi as it's simpler
            ..Default::default()
        }),
        ..Default::default()
    };

    let command = Command::Invite { option };
    // Send the invite command
    let command_str = serde_json::to_string_pretty(&command)?;
    ws_sender
        .send(tungstenite::Message::Text(command_str.into()))
        .await?;

    // Wait for events (answer, etc.)
    let recv_event_flag = tokio::sync::watch::channel(false);
    let recv_event_sender = recv_event_flag.0;
    let mut recv_event_receiver = recv_event_flag.1;

    // Start a task to receive events from the server
    let recv_event_task = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                tungstenite::Message::Text(text) => {
                    let event: SessionEvent = match serde_json::from_str(&text) {
                        Ok(event) => event,
                        Err(e) => {
                            error!("Failed to parse event: {} - {}", e, text);
                            continue;
                        }
                    };
                    info!("Received event: {:?}", event);
                    events.push(event);

                    // Signal we've received at least one event
                    recv_event_sender.send(true).ok();
                }
                tungstenite::Message::Binary(_data) => {}
                _ => {}
            }
        }
        Ok::<Vec<SessionEvent>, anyhow::Error>(events)
    });

    // Read test audio file
    let (audio_samples, _sample_rate) = read_wav_file("fixtures/hello_book_course_zh_16k.wav")?;

    // Wait for at least one event before sending audio
    time::sleep(Duration::from_millis(500)).await;

    // Send audio as binary messages
    let send_audio_task = tokio::spawn(async move {
        let chunk_size = 320; // 20ms at 16kHz
        let mut ticker = time::interval(Duration::from_millis(20));

        for chunk in audio_samples.chunks(chunk_size) {
            // Convert PCM samples to bytes
            let bytes_data = samples_to_bytes(chunk);

            // Send as binary message
            match ws_sender
                .send(tungstenite::Message::Binary(bytes_data.into()))
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to send audio data: {}", e);
                    break;
                }
            }

            // Wait for the next tick to simulate real-time streaming
            ticker.tick().await;
        }

        info!("Audio sending completed");

        // Let's also test sending a TTS command
        let tts_command = Command::Tts {
            text: "Hello, this is a test message".to_string(),
            speaker: None,
            play_id: Some("test-play-id".to_string()),
            auto_hangup: Some(true),
            streaming: None,
            end_of_stream: None,
            option: None,
        };

        let tts_cmd_str = serde_json::to_string(&tts_command)?;
        ws_sender
            .send(tungstenite::Message::Text(tts_cmd_str.into()))
            .await?;

        info!("TTS command sent");

        // Keep connection open to receive server responses
        time::sleep(Duration::from_secs(2)).await;

        Ok::<(), anyhow::Error>(())
    });

    // Wait for events or timeout
    select! {
        _ = async {
            while !*recv_event_receiver.borrow() {
                if recv_event_receiver.changed().await.is_err() {
                    break;
                }
            }
        } => {
            info!("Received at least one event from the server");
        }
        _ = time::sleep(Duration::from_secs(5)) => {
            error!("Timed out waiting for server response");
        }
    }

    // Wait for both tasks to complete or timeout
    select! {
        _ = recv_event_task => {
            info!("Event reception task completed");
        }
        _ = send_audio_task => {
            info!("Audio sending task completed");
        }
        _ = time::sleep(Duration::from_secs(10)) => {
            error!("Test timed out");
        }
    }

    Ok(())
}
