use anyhow::{anyhow, Context, Result};
use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, SizedSample,
};
use futures::{pin_mut, SinkExt, StreamExt};
use rustpbx::media::processor::Samples;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{stdout, AsyncWriteExt},
    sync::mpsc,
    time::Duration,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, error, info};
use url::Url;
use uuid::Uuid;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS},
        APIBuilder,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::configuration::RTCConfiguration,
    peer_connection::peer_connection_state::RTCPeerConnectionState,
    peer_connection::sdp::session_description::RTCSessionDescription,
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The rustpbx server URL
    #[clap(short, long, default_value = "ws://localhost:3000/call/webrtc")]
    url: String,

    /// The ASR model name (if any)
    #[clap(long, default_value = "whisper-large-v3")]
    asr_model: String,

    /// The LLM model name
    #[clap(long, default_value = "claude-3-5-sonnet")]
    llm_model: String,

    /// The TTS URL (if any)
    #[clap(long, default_value = "http://localhost:8080/synthesize")]
    tts_url: String,

    /// The TTS voice (if any)
    #[clap(long, default_value = "alloy")]
    voice: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebRtcCallRequest {
    call_id: String,
    offer: String,
    asr: Option<AsrConfig>,
    vad: Option<VadConfig>,
    llm: Option<LlmConfig>,
    tts: Option<TtsConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AsrConfig {
    model: String,
    language: Option<String>,
    interim_results: Option<bool>,
    single_utterance: Option<bool>,
    vad_mode: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct VadConfig {
    mode: String,
    enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LlmConfig {
    model: String,
    system_prompt: Option<String>,
    context: Option<Vec<ChatMessage>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TtsConfig {
    url: String,
    voice: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WebRtcCallResponse {
    call_id: String,
    answer: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct WsCommand {
    command: String,
    call_id: String,
    data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CallEvent {
    event: String,
    call_id: String,
    timestamp: String,
    data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
enum CallState {
    NEW,
    CONNECTING,
    CONNECTED,
    COMPLETED,
    FAILED,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Store audio samples in a ringbuffer
#[derive(Debug)]
struct AudioBuffer {
    buffer: Mutex<VecDeque<i16>>,
    max_size: usize,
}

impl AudioBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(max_size)),
            max_size,
        }
    }

    fn push(&self, samples: &[i16]) {
        let mut buffer = self.buffer.lock().unwrap();
        for &sample in samples {
            buffer.push_back(sample);
            if buffer.len() > self.max_size {
                buffer.pop_front();
            }
        }
    }

    fn drain(&self, max_samples: usize) -> Vec<i16> {
        let mut buffer = self.buffer.lock().unwrap();
        let count = std::cmp::min(max_samples, buffer.len());
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(sample) = buffer.pop_front() {
                result.push(sample);
            }
        }
        result
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    debug!("Args: {:?}", args);

    let call_id = Uuid::new_v4().to_string();
    info!("Call ID: {}", call_id);

    // Set up the WebRTC peer connection
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a MediaEngine and register the audio codec
    let mut m = MediaEngine::default();
    m.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // Create an audio track
    let audio_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_string(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "".to_string(),
            rtcp_feedback: vec![],
        },
        format!("audio-{}", uuid::Uuid::new_v4()),
        format!("webrtc-call-{}", uuid::Uuid::new_v4()),
    ));

    // Add the track to the peer connection
    let rtp_sender = peer_connection.add_track(audio_track.clone()).await?;

    // Create a task to read RTP packets from the track
    let _pc_clone = peer_connection.clone();
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    // Set up the WebSocket connection to the RustPBX server
    let url = Url::parse(&args.url)?;

    // Create a websocket connection compatible with tungstenite 0.21.0
    let (ws_stream, _) = connect_async(url.to_string()).await?;
    info!("WebSocket connection established");

    // Create a channel for sending WebSocket messages
    let (ws_tx, mut ws_rx) = mpsc::channel::<String>(100);

    // Split the WebSocket stream
    let (mut ws_write, ws_read) = ws_stream.split();

    // Task to handle WebSocket writes
    let ws_write_task = tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if let Err(e) = ws_write.send(Message::Text(msg.into())).await {
                error!("Failed to send WebSocket message: {}", e);
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    // Set up input device for capturing audio
    let host = cpal::default_host();
    let input_device = host
        .default_input_device()
        .context("Failed to get default input device")?;

    info!("Using input device: {}", input_device.name()?);

    let output_device = host
        .default_output_device()
        .context("Failed to get default output device")?;

    info!("Using output device: {}", output_device.name()?);

    // Set up the audio buffer for storing audio samples
    let audio_buffer = Arc::new(AudioBuffer::new(48000 * 10)); // 10 seconds at 48kHz

    // Set up channels for sending audio to the WebRTC track
    let (audio_sender, mut audio_receiver) = mpsc::channel::<Vec<i16>>(100);

    // Build the SDP offer
    let offer = peer_connection.create_offer(None).await?;
    peer_connection.set_local_description(offer.clone()).await?;

    // Wait for ICE gathering to complete
    let pc_clone = peer_connection.clone();
    let (gather_complete_tx, gather_complete_rx) = tokio::sync::oneshot::channel::<()>();

    let gather_complete_tx = Arc::new(Mutex::new(Some(gather_complete_tx)));
    peer_connection.on_ice_gathering_state_change(Box::new(move |state| {
        if state.to_string() == "complete" {
            let mut tx = gather_complete_tx.lock().unwrap();
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        }
        Box::pin(async {})
    }));

    // Wait for gathering to complete
    let _ = gather_complete_rx.await;
    let local_description = pc_clone.local_description().await.unwrap();

    // Send the WebRTC call request to the server
    let request = WebRtcCallRequest {
        call_id: call_id.clone(),
        offer: local_description.sdp,
        asr: Some(AsrConfig {
            model: args.asr_model.clone(),
            language: Some("en".to_string()),
            interim_results: Some(true),
            single_utterance: Some(false),
            vad_mode: Some("VERY_AGGRESSIVE".to_string()),
        }),
        vad: Some(VadConfig {
            mode: "VERY_AGGRESSIVE".to_string(),
            enabled: true,
        }),
        llm: Some(LlmConfig {
            model: args.llm_model.clone(),
            system_prompt: Some("You are a helpful assistant.".to_string()),
            context: None,
        }),
        tts: Some(TtsConfig {
            url: args.tts_url.clone(),
            voice: args.voice.clone(),
        }),
    };

    let request_json = serde_json::to_string(&request)?;

    // Send the WebRTC call request
    ws_tx.send(request_json).await?;

    // Handle the WebSocket response
    let audio_track_clone = audio_track.clone();
    let pc_clone = pc_clone.clone();
    let ws_read_task = tokio::spawn(async move {
        pin_mut!(ws_read);

        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(response) = serde_json::from_str::<WebRtcCallResponse>(&text) {
                        info!("Received WebRTC call response: {}", response.call_id);

                        // Set the remote description (SDP answer)
                        let answer = RTCSessionDescription::answer(response.answer)?;
                        if let Err(e) = pc_clone.set_remote_description(answer).await {
                            error!("Failed to set remote description: {}", e);
                            break;
                        }
                    } else if let Ok(event) = serde_json::from_str::<CallEvent>(&text) {
                        match event.event.as_str() {
                            "asr.transcript" => {
                                if let Some(data) = event.data {
                                    if let Some(text) = data.get("text") {
                                        info!("Transcript: {}", text);
                                    }
                                }
                            }
                            "llm.response" => {
                                if let Some(data) = event.data {
                                    if let Some(text) = data.get("text") {
                                        info!("LLM: {}", text);
                                    }
                                }
                            }
                            "tts.audio" => {
                                info!("Received TTS audio");
                                // We would handle incoming audio here
                            }
                            "call.state" => {
                                if let Some(data) = event.data {
                                    if let Some(state) = data.get("state") {
                                        info!("Call state: {}", state);
                                    }
                                }
                            }
                            _ => {
                                debug!("Received event: {}", event.event);
                            }
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    debug!("Received binary message: {} bytes", data.len());
                    // Handle binary data like audio from TTS
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    // Set up a task to handle user input
    let ws_tx_clone = ws_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let mut buffer = String::new();
        let mut stdout = stdout();

        loop {
            // Print prompt
            stdout.write_all(b"> ").await?;
            stdout.flush().await?;

            // Read a line of input
            match tokio::io::AsyncBufReadExt::read_line(
                &mut tokio::io::BufReader::new(tokio::io::stdin()),
                &mut buffer,
            )
            .await
            {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let input = buffer.trim();
                    if input == "quit" || input == "exit" {
                        break;
                    }

                    // Send a command to the server
                    let cmd = WsCommand {
                        command: "user.input".to_string(),
                        call_id: call_id.clone(),
                        data: Some(serde_json::json!({
                            "text": input
                        })),
                    };

                    let cmd_json = serde_json::to_string(&cmd)?;
                    ws_tx_clone.send(cmd_json).await?;

                    buffer.clear();
                }
                Err(e) => {
                    eprintln!("Error reading input: {}", e);
                    break;
                }
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    // Set up a task to send audio samples to the WebRTC track
    let audio_task = tokio::spawn(async move {
        // Setup to read from audio buffer and send to WebRTC track
        while let Some(samples) = audio_receiver.recv().await {
            // Convert samples to PCM bytes
            let pcm_data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

            // Write samples to the WebRTC track
            if let Err(e) = audio_track_clone
                .write_sample(&webrtc::media::Sample {
                    data: pcm_data.into(),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await
            {
                error!("Failed to write audio sample: {}", e);
            }
        }

        Ok::<(), anyhow::Error>(())
    });

    // Set up the input audio stream
    let audio_buffer_clone = audio_buffer.clone();
    let input_config = input_device.default_input_config()?;
    info!("Input config: {:?}", input_config);

    let input_stream = match input_config.sample_format() {
        SampleFormat::I16 => build_input_stream::<i16>(
            &input_device,
            &input_config.into(),
            audio_buffer_clone.clone(),
            audio_sender.clone(),
        )?,
        SampleFormat::U16 => build_input_stream::<u16>(
            &input_device,
            &input_config.into(),
            audio_buffer_clone.clone(),
            audio_sender.clone(),
        )?,
        SampleFormat::F32 => build_input_stream::<f32>(
            &input_device,
            &input_config.into(),
            audio_buffer_clone.clone(),
            audio_sender.clone(),
        )?,
        sample_format => return Err(anyhow!("Unsupported sample format: {:?}", sample_format)),
    };

    input_stream.play()?;
    info!("Input stream started");

    // Set up the output stream
    let output_config = output_device.default_output_config()?;
    info!("Output config: {:?}", output_config);

    /*
    // Output stream currently not used
    let output_stream = match output_config.sample_format() {
        SampleFormat::I16 => build_output_stream::<i16>(&output_device, &output_config.into(), audio_buffer_clone)?,
        SampleFormat::U16 => build_output_stream::<u16>(&output_device, &output_config.into(), audio_buffer_clone)?,
        SampleFormat::F32 => build_output_stream::<f32>(&output_device, &output_config.into(), audio_buffer_clone)?,
        sample_format => return Err(anyhow!("Unsupported sample format: {:?}", sample_format)),
    };

    output_stream.play()?;
    info!("Output stream started");
    */

    // Handle changes in the peer connection state
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("Peer connection state has changed: {}", s);
        if s == RTCPeerConnectionState::Failed {
            // The PeerConnection has gone to failed, this is a terminal state
            info!("Peer connection has failed, stopping...");
        }
        Box::pin(async {})
    }));

    // Wait for tasks to complete
    let _ = tokio::join!(ws_read_task, ws_write_task, stdin_task, audio_task);

    Ok(())
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<AudioBuffer>,
    audio_sender: mpsc::Sender<Vec<i16>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + SizedSample + 'static,
{
    let err_fn = move |err| {
        error!("Error on input stream: {}", err);
    };

    match std::any::type_name::<T>() {
        "i16" => {
            let stream = device.build_input_stream(
                config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    // Already i16, just need to clone
                    let samples_i16 = data.to_vec();
                    audio_buffer.push(&samples_i16);
                    let _ = audio_sender.try_send(samples_i16);
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        "f32" => {
            let stream = device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Convert f32 to i16
                    let samples_i16: Vec<i16> = data
                        .iter()
                        .map(|&sample| (sample * 32767.0).round() as i16)
                        .collect();
                    audio_buffer.push(&samples_i16);
                    let _ = audio_sender.try_send(samples_i16);
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        _ => {
            return Err(anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<AudioBuffer>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + SizedSample + 'static,
{
    let err_fn = move |err| {
        error!("Error on output stream: {}", err);
    };

    match std::any::type_name::<T>() {
        "i16" => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    // Get samples from the audio buffer
                    let samples = audio_buffer.drain(data.len());
                    // Copy samples directly
                    for (i, out) in data.iter_mut().enumerate() {
                        *out = if i < samples.len() { samples[i] } else { 0 };
                    }
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        "f32" => {
            let stream = device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Get samples from the audio buffer
                    let samples = audio_buffer.drain(data.len());
                    // Convert i16 to f32
                    for (i, out) in data.iter_mut().enumerate() {
                        let value = if i < samples.len() { samples[i] } else { 0 };
                        *out = value as f32 / 32767.0;
                    }
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        _ => {
            return Err(anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}
