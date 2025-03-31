use crate::{
    event::{EventReceiver, EventSender, SessionEvent},
    handler::call::CallHandlerState,
    media::stream::MediaStreamBuilder,
};
use anyhow::Result;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use tokio::{fs::File, io::AsyncReadExt, net::TcpListener};
use tokio_tungstenite::tungstenite;
use webrtc::{
    api::{media_engine::MediaEngine, APIBuilder},
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

#[tokio::test]
async fn test_webrtc_audio_streaming() -> Result<()> {
    let app = Router::new()
        .route(
            "/call/webrtc",
            axum::routing::get(crate::handler::webrtc::webrtc_handler),
        )
        .with_state(CallHandlerState::new());

    // Start the server
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Create WebRTC client
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let api = APIBuilder::new().with_media_engine(media_engine).build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Create an audio track
    let track = Arc::new(TrackLocalStaticSample::new(
        webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
            mime_type: "audio/opus".to_owned(),
            ..Default::default()
        },
        "audio".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add the track to the peer connection
    let _rtp_sender = peer_connection
        .add_track(Arc::clone(&track) as Arc<TrackLocalStaticSample>)
        .await?;

    // Read test audio file
    let mut file = File::open("fixtures/hello_book_course_zh_16k.wav").await?;
    let mut audio_data = Vec::new();
    file.read_to_end(&mut audio_data).await?;

    // Connect to WebSocket
    let url = format!("ws://127.0.0.1:{}/call/webrtc", server_addr.port());
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Create offer
    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    // Send offer via WebSocket
    let offer_msg = json!({
        "sdp": offer.sdp,
    });
    let offer_str = offer_msg.to_string();
    ws_sender
        .send(tungstenite::Message::Text(offer_str.into()))
        .await?;

    // Wait for answer
    if let Some(Ok(msg)) = ws_receiver.next().await {
        if let tungstenite::Message::Text(text) = msg {
            let answer: serde_json::Value = serde_json::from_str(&text)?;
            let answer_sdp = answer["sdp"].as_str().unwrap();
            let answer = RTCSessionDescription::answer(answer_sdp.to_string())?;
            peer_connection.set_remote_description(answer).await?;
        }
    }

    // Create event channels
    let (event_tx, mut event_rx): (EventSender, EventReceiver) =
        tokio::sync::broadcast::channel(100);

    // Start event handling task
    let event_handle = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                SessionEvent::TranscriptionFinal { text, .. } => {
                    println!("Transcription: {}", text);
                }
                SessionEvent::StartSpeaking { .. } => {
                    println!("Speech detected");
                }
                SessionEvent::Silence { .. } => {
                    println!("Silence detected");
                }
                _ => {}
            }
        }
    });

    // Wait for ICE connection
    let mut ice_connected = false;
    peer_connection.on_ice_connection_state_change(Box::new(move |state| {
        println!("ICE connection state changed: {:?}", state);
        if state == RTCIceConnectionState::Connected {
            ice_connected = true;
        }
        Box::pin(async {})
    }));

    // Wait for ICE connection
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Send audio data
    let sample = webrtc::media::Sample {
        data: bytes::Bytes::from(audio_data),
        duration: std::time::Duration::from_secs(1),
        ..Default::default()
    };
    track.write_sample(&sample).await?;

    // Wait for events
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Cleanup
    peer_connection.close().await?;
    event_handle.abort();
    server_handle.abort();

    Ok(())
}
