use anyhow::{anyhow, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, SizedSample,
};
use rustpbx::{
    event::{EventSender, SessionEvent},
    llm::{LlmClient, LlmConfig, OpenAiClient, OpenAiClientBuilder},
    media::{
        noise::NoiseReducer,
        processor::{AudioFrame, Processor, Samples},
        stream::{MediaStream, MediaStreamBuilder, MediaStreamEvent},
        track::{tts::TtsTrack, Track, TrackId},
        vad::{VadProcessor, VadType},
    },
    synthesis::{TencentCloudTtsClient, TtsClient, TtsConfig, TtsEvent, TtsProcessor},
    transcription::{AsrConfig, AsrEvent, AsrProcessor, TencentCloudAsrClient},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{broadcast, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Audio buffer to handle microphone input
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
        }
        // Trim to max size if needed
        while buffer.len() > self.max_size {
            buffer.pop_front();
        }
    }

    fn drain(&self, max_samples: usize) -> Vec<i16> {
        let mut buffer = self.buffer.lock().unwrap();
        let count = max_samples.min(buffer.len());
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(sample) = buffer.pop_front() {
                result.push(sample);
            }
        }
        result
    }
}

// Custom processor to handle microphone input
struct MicrophoneProcessor {
    audio_sender: mpsc::Sender<Vec<i16>>,
}

impl MicrophoneProcessor {
    fn new(audio_sender: mpsc::Sender<Vec<i16>>) -> Self {
        Self { audio_sender }
    }
}

impl Processor for MicrophoneProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        if let Samples::PCM(samples) = &frame.samples {
            let _ = self.audio_sender.try_send(samples.clone());
        }
        Ok(())
    }
}

// Convert MediaStream events to user-friendly status updates
fn handle_media_event(event: MediaStreamEvent) -> Option<String> {
    match event {
        MediaStreamEvent::StartSpeaking(track_id, timestamp) => Some(format!(
            "Started speaking on track {} at {}",
            track_id, timestamp
        )),
        MediaStreamEvent::Silence(track_id, timestamp) => Some(format!(
            "Silence detected on track {} at {}",
            track_id, timestamp
        )),
        MediaStreamEvent::Transcription(track_id, timestamp, text) => {
            Some(format!("Transcription on track {}: {}", track_id, text))
        }
        MediaStreamEvent::TranscriptionSegment(track_id, timestamp, text) => Some(format!(
            "Interim transcription on track {}: {}",
            track_id, text
        )),
        MediaStreamEvent::DTMF(track_id, timestamp, digit) => Some(format!(
            "DTMF digit {} detected on track {}",
            digit, track_id
        )),
        MediaStreamEvent::TrackStart(track_id) => Some(format!("Track {} started", track_id)),
        MediaStreamEvent::TrackStop(track_id) => Some(format!("Track {} stopped", track_id)),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Set up logging
    tracing_subscriber::fmt::init();

    // Load .env file if it exists
    dotenv::dotenv().ok();

    // Get credentials from environment variables
    let tencent_secret_id = match std::env::var("TENCENT_SECRET_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            println!("TENCENT_SECRET_ID env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    let tencent_secret_key = match std::env::var("TENCENT_SECRET_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            println!("TENCENT_SECRET_KEY env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    let tencent_appid = match std::env::var("TENCENT_APPID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            println!("TENCENT_APPID env var not set or empty. Please set it in .env file");
            return Ok(());
        }
    };

    // Create a cancellation token for clean shutdown
    let cancel_token = CancellationToken::new();
    let cancel_token_clone = cancel_token.clone();

    // Setup for media processing
    let session_id = Uuid::new_v4().to_string();
    info!("Starting audio agent with session ID: {}", session_id);

    // Create the media stream
    let media_stream = Arc::new(
        MediaStreamBuilder::new()
            .id(format!("audio-agent-{}", session_id))
            .cancel_token(cancel_token.clone())
            .build(),
    );

    // Set up microphone input
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

    // Set up channels for audio processing
    let (audio_sender, mut audio_receiver) = mpsc::channel::<Vec<i16>>(100);

    // Create the ASR configuration
    let asr_config = AsrConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(tencent_appid.clone()),
        secret_id: Some(tencent_secret_id.clone()),
        secret_key: Some(tencent_secret_key.clone()),
        engine_type: Some("16k_zh".to_string()), // Chinese 16kHz model
    };

    // Create the TTS configuration
    let tts_config = TtsConfig {
        url: "".to_string(),          // Not used with TencentCloud client
        voice: Some("1".to_string()), // Voice type
        rate: Some(1.0),              // Normal speed
        appid: Some(tencent_appid),
        secret_id: Some(tencent_secret_id.clone()),
        secret_key: Some(tencent_secret_key.clone()),
        volume: Some(5),                // Volume level (0-10)
        speaker: Some(1),               // Speaker type
        codec: Some("pcm".to_string()), // PCM format
    };

    // Create LLM configuration and client
    let llm_config = LlmConfig {
        prompt: "You are a helpful assistant. Keep your responses concise and to the point."
            .to_string(),
        temperature: Some(0.7),
        max_tokens: Some(100),
        max_conversation_turns: Some(10),
        stream: Some(true),
        base_url: None,
        tools: None,
        ..LlmConfig::default() // Get default values including model from env
    };

    // Setup event channels
    let (asr_sender, mut asr_receiver) = broadcast::channel::<AsrEvent>(10);
    let (tts_sender, mut tts_receiver) = broadcast::channel::<TtsEvent>(10);
    let (event_sender, _) = broadcast::channel::<SessionEvent>(10);
    let (vad_sender, _) = broadcast::channel::<MediaStreamEvent>(10);

    // Create the ASR client and processor
    let asr_client = Arc::new(TencentCloudAsrClient::new());
    let asr_processor = AsrProcessor::new(asr_config, asr_client, asr_sender.clone())
        .with_session_event_sender(event_sender.clone());

    // Create the TTS client and processor
    let tts_client = Arc::new(TencentCloudTtsClient::new());
    let tts_processor = TtsProcessor::new(tts_config, tts_client, tts_sender.clone())
        .with_session_event_sender(event_sender.clone());

    // Create the LLM client
    let llm_client = match OpenAiClientBuilder::from_env().build() {
        Ok(client) => Arc::new(client),
        Err(e) => {
            error!("Failed to create OpenAI client: {}", e);
            return Err(anyhow!("Failed to create OpenAI client: {}", e));
        }
    };

    // Create microphone track
    let mic_track_id = format!("mic-{}", Uuid::new_v4());
    let mut mic_track = TtsTrack::new(mic_track_id.clone());

    // Create microphone processor
    let mic_processor = MicrophoneProcessor::new(audio_sender.clone());

    // Create noise reducer processor
    let noise_reducer = NoiseReducer::new();

    // Create VAD processor
    let vad_processor = VadProcessor::new(mic_track_id.clone(), VadType::WebRTC, vad_sender);

    // Add processors to the microphone track
    mic_track.with_processors(vec![
        Box::new(mic_processor),
        Box::new(noise_reducer),
        Box::new(vad_processor),
        Box::new(asr_processor),
    ]);

    // Add track to media stream
    media_stream.update_track(Box::new(mic_track)).await;

    // Subscribe to media stream events for logging
    let mut media_events = media_stream.subscribe();

    // Task to forward microphone audio to the media stream
    let mic_track_id_clone = mic_track_id.clone();
    let media_stream_clone = media_stream.clone();
    tokio::spawn(async move {
        let mut timestamp = 0u32;
        while let Some(samples) = audio_receiver.recv().await {
            // Create audio frame from microphone samples
            let frame = AudioFrame {
                track_id: mic_track_id_clone.clone(),
                samples: Samples::PCM(samples),
                timestamp,
                sample_rate: 16000,
            };

            // Send the frame through the track's packet sender
            if let Err(e) = media_stream_clone.packet_sender.send(frame) {
                error!("Failed to send audio frame: {}", e);
            }

            timestamp += 20; // Assuming 20ms frames
        }
    });

    // Task to listen for media stream events and log them
    tokio::spawn(async move {
        while let Ok(event) = media_events.recv().await {
            if let Some(msg) = handle_media_event(event) {
                info!("{}", msg);
            }
        }
    });

    // Process ASR events and forward to LLM
    tokio::spawn(async move {
        while let Ok(event) = asr_receiver.recv().await {
            if event.is_final {
                info!("Final ASR result: {}", event.text);

                // Send to LLM for processing
                match llm_client.generate_response(&event.text, &llm_config).await {
                    Ok(response) => {
                        info!("LLM response: {}", response);

                        // Send to TTS for synthesis
                        match tts_processor.synthesize(&response).await {
                            Ok(audio_data) => {
                                info!("TTS generated {} bytes of audio", audio_data.len());

                                // Convert bytes to i16 samples
                                let samples: Vec<i16> = audio_data
                                    .chunks_exact(2)
                                    .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                                    .collect();

                                // Feed samples to output device
                                if let Err(e) = play_audio(&output_device, &samples) {
                                    error!("Failed to play audio: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("TTS synthesis failed: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM processing failed: {}", e);
                    }
                }
            }
        }
    });

    // Start the media stream
    tokio::spawn(async move {
        if let Err(e) = media_stream.serve().await {
            error!("Media stream error: {}", e);
        }
    });

    // Setup the input audio stream
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

    // Start the audio input stream
    input_stream.play()?;
    info!("Input stream started");

    // Run until user presses Enter
    println!("Audio agent is running. Press Enter to stop...");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    // Cleanup
    info!("Stopping audio agent...");
    cancel_token_clone.cancel();

    // Give tasks a moment to clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(())
}

// Helper function to build input stream for microphone
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

// Helper function to play audio through output device
fn play_audio(device: &cpal::Device, samples: &[i16]) -> Result<()> {
    let output_config = device.default_output_config()?;
    let config: cpal::StreamConfig = output_config.clone().into();

    let samples = samples.to_vec();
    let sample_mutex = Arc::new(Mutex::new((samples, 0)));

    let stream = match output_config.sample_format() {
        SampleFormat::I16 => {
            let sample_mutex_clone = sample_mutex.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut guard = sample_mutex_clone.lock().unwrap();
                    let (samples, position) = &mut *guard;

                    for out_sample in data.iter_mut() {
                        if *position < samples.len() {
                            *out_sample = samples[*position];
                            *position += 1;
                        } else {
                            *out_sample = 0;
                        }
                    }
                },
                |err| error!("Output error: {}", err),
                None,
            )?
        }
        SampleFormat::F32 => {
            let sample_mutex_clone = sample_mutex.clone();
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut guard = sample_mutex_clone.lock().unwrap();
                    let (samples, position) = &mut *guard;

                    for out_sample in data.iter_mut() {
                        if *position < samples.len() {
                            *out_sample = samples[*position] as f32 / 32767.0;
                            *position += 1;
                        } else {
                            *out_sample = 0.0;
                        }
                    }
                },
                |err| error!("Output error: {}", err),
                None,
            )?
        }
        _ => return Err(anyhow!("Unsupported sample format")),
    };

    stream.play()?;

    // Wait for all samples to be played
    let mut done = false;
    while !done {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let guard = sample_mutex.lock().unwrap();
        done = guard.1 >= guard.0.len();
    }

    Ok(())
}
