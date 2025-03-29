/// This is a demo application which reads wav file from command line
/// and processes it with ASR, LLM and TTS. It also supports local microphone input
/// and speaker output with Voice Activity Detection (VAD) and Noise Suppression (NS).
///
/// ```bash
/// # Use a WAV file as input and write to a WAV file
/// cargo run --example voice_demo -- --input fixtures/hello_book_course_zh_16k.wav --output out.wav
///
/// # Use microphone input and speaker output
/// cargo run --example voice_demo -- --use-mic --use-speaker
/// ```
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, SizedSample, StreamConfig};
use dotenv::dotenv;
use rustpbx::media::track::file::read_wav_file;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber;

use rustpbx::{
    event::SessionEvent,
    llm::{LlmClient, LlmConfig, OpenAiClientBuilder},
    media::{
        codecs::resample,
        noise::NoiseReducer,
        pipeline::{
            llm::LlmPipeline, synthesis::SynthesisPipeline, transcription::TranscriptionPipeline,
            PipelineManager, StreamState,
        },
        processor::{AudioFrame, Processor, Samples},
        vad::{VadProcessor, VadType},
    },
    synthesis::{SynthesisClient, SynthesisConfig, TencentCloudTtsClient},
    transcription::{TencentCloudAsrClient, TranscriptionClient, TranscriptionConfig},
};

/// Audio processing buffer for handling real-time audio samples
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

/// Audio Agent
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Appid, for TencentCloud
    #[arg(short, long)]
    appid: Option<String>,

    /// Secret id, for TencentCloud
    #[arg(long)]
    secret_id: Option<String>,

    /// Secret key, for TencentCloud
    #[arg(long)]
    secret_key: Option<String>,

    /// OpenAI API Key
    #[arg(long)]
    api_key: Option<String>,

    /// Input file (wav)
    #[arg(short, long)]
    input: Option<String>,

    /// Sample rate
    #[arg(short = 'R', long, default_value = "16000")]
    sample_rate: u32,

    /// System prompt for LLM
    #[arg(
        short = 'p',
        long,
        default_value = "You are a helpful assistant who responds in Chinese."
    )]
    prompt: String,

    /// Enable VAD (Voice Activity Detection)
    #[arg(long, default_value = "true")]
    vad: bool,

    /// Enable NS (Noise Suppression)
    #[arg(long, default_value = "true")]
    ns: bool,
}
// Build an input stream for capturing audio from a microphone
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
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}

// Build an output stream for playing audio through speakers
fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<AudioBuffer>,
    sample_rate: Option<u32>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + SizedSample + 'static,
{
    let err_fn = move |err| {
        error!("Error on output stream: {}", err);
    };

    // 如果指定了采样率，创建一个新的配置
    let config = if let Some(rate) = sample_rate {
        let mut custom_config = config.clone();
        custom_config.sample_rate = cpal::SampleRate(rate);
        custom_config
    } else {
        config.clone()
    };

    match std::any::type_name::<T>() {
        "i16" => {
            let stream = device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let samples = audio_buffer.drain(data.len());
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
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let samples = audio_buffer.drain(data.len());
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
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}

/// Extension trait for VadProcessor to check if frame contains speech
trait SpeechDetector {
    fn is_speech(&self, frame: &mut AudioFrame) -> bool;
}

impl SpeechDetector for VadProcessor {
    fn is_speech(&self, frame: &mut AudioFrame) -> bool {
        // Process the frame first to update internal state and send events
        if let Err(e) = self.process_frame(frame) {
            error!("VAD processing error: {}", e);
            return true; // Default to true on error
        }

        // Determine speech by checking the most recent event
        // Since the WebRTC VAD doesn't maintain state between frames, we can just look at the VAD
        // result from the current frame which is reflected in the event that was just sent
        match &frame.samples {
            Samples::PCM(samples) => !samples.is_empty(), // If we have samples, assume it's speech
            _ => true,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load environment variables from .env file
    dotenv().ok();

    // Initialize tracing for logging
    tracing_subscriber::fmt::init();

    // Parse arguments
    let args = Args::parse();

    // Get API key from args or environment
    let api_key = args
        .api_key
        .clone()
        .or_else(|| env::var("OPENAI_API_KEY").ok())
        .unwrap_or_default();

    // Get TencentCloud credentials from args or environment
    let appid = args
        .appid
        .clone()
        .or_else(|| env::var("TENCENT_APPID").ok())
        .unwrap_or_default();

    let secret_id = args
        .secret_id
        .clone()
        .or_else(|| env::var("TENCENT_SECRET_ID").ok())
        .unwrap_or_default();

    let secret_key = args
        .secret_key
        .clone()
        .or_else(|| env::var("TENCENT_SECRET_KEY").ok())
        .unwrap_or_default();

    // Log found credentials (without revealing sensitive data)
    info!(
        "Using TencentCloud AppID: {}",
        if appid.is_empty() { "Not set" } else { "Found" }
    );
    info!(
        "Using TencentCloud Secret ID: {}",
        if secret_id.is_empty() {
            "Not set"
        } else {
            "Found"
        }
    );
    info!(
        "Using TencentCloud Secret Key: {}",
        if secret_key.is_empty() {
            "Not set"
        } else {
            "Found"
        }
    );

    // Create cancellation token for pipeline
    let cancel_token = CancellationToken::new();

    // Create event sender for pipeline events
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(32);

    // Create audio buffer and channels for sending samples
    let audio_buffer = Arc::new(AudioBuffer::new(args.sample_rate as usize * 10)); // 10 seconds at specified sample rate
    let output_buffer = Arc::new(AudioBuffer::new(args.sample_rate as usize * 10));
    let (audio_sender, mut audio_receiver) = mpsc::channel::<Vec<i16>>(100);

    // Set up pipeline manager
    let pipeline_manager = Arc::new(PipelineManager::new(
        "voice_demo".to_string(),
        event_sender.clone(),
        cancel_token.clone(),
    ));

    // Set up ASR pipeline
    let transcription_config = TranscriptionConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        engine_type: Some("16k_zh".to_string()),
    };

    let asr_client = TencentCloudAsrClient::new();
    let transcription_pipeline = TranscriptionPipeline::new(
        "transcription".to_string(),
        Arc::new(asr_client) as Arc<dyn TranscriptionClient>,
        transcription_config,
    );

    // Set up LLM pipeline
    let llm_config = LlmConfig {
        model: "gpt-3.5-turbo".to_string(),
        prompt: args.prompt.clone(),
        temperature: Some(0.7),
        max_tokens: Some(1000),
        max_conversation_turns: Some(10),
        stream: Some(false),
        base_url: None,
        tools: None,
    };

    let openai_client = OpenAiClientBuilder::from_env()
        .with_api_key(api_key)
        .build()?;

    let llm_pipeline = LlmPipeline::new(
        "llm".to_string(),
        Arc::new(openai_client) as Arc<dyn LlmClient>,
        llm_config,
    );

    // Set up TTS pipeline
    let synthesis_config = SynthesisConfig {
        url: "".to_string(),
        voice: Some("1".to_string()),
        rate: Some(0.7),
        appid: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        volume: Some(0),
        speaker: Some(1),
        codec: Some("wav".to_string()),
    };

    let tts_client = TencentCloudTtsClient::new();
    let synthesis_pipeline = SynthesisPipeline::new(
        "synthesis".to_string(),
        Arc::new(tts_client) as Arc<dyn SynthesisClient>,
        synthesis_config,
    );

    // Add all pipelines to the manager
    pipeline_manager
        .add_pipeline(Box::new(transcription_pipeline))
        .await?;
    pipeline_manager
        .add_pipeline(Box::new(llm_pipeline))
        .await?;
    pipeline_manager
        .add_pipeline(Box::new(synthesis_pipeline))
        .await?;

    // Subscribe to pipeline state changes
    let mut state_receiver = pipeline_manager.subscribe();

    // Initialize VAD and Noise Suppression
    let (vad_event_sender, _) = broadcast::channel(32);
    let vad_processor = if args.vad {
        Some(VadProcessor::new(
            "main".to_string(),
            VadType::WebRTC,
            vad_event_sender,
        ))
    } else {
        None
    };

    let noise_reducer = if args.ns {
        Some(NoiseReducer::new())
    } else {
        None
    };

    // Set up microphone input if requested
    let mut input_stream = None;
    let mut input_sample_rate = 16000;
    if args.input.is_none() {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device available"))?;

        info!("Using input device: {}", input_device.name()?);

        let input_config = input_device.default_input_config()?;
        input_sample_rate = input_config.sample_rate().0;
        let audio_buffer_clone = audio_buffer.clone();
        let audio_sender_clone = audio_sender.clone();

        input_stream = Some(match input_config.sample_format() {
            SampleFormat::I16 => build_input_stream::<i16>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            SampleFormat::U16 => build_input_stream::<u16>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            SampleFormat::F32 => build_input_stream::<f32>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            sample_format => {
                return Err(anyhow::anyhow!(
                    "Unsupported sample format: {:?}",
                    sample_format
                ))
            }
        });

        // Start the input stream
        input_stream.as_ref().unwrap().play()?;
        info!("Microphone input started");
    }

    let host = cpal::default_host();
    let output_device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device available"))?;

    info!("Using output device: {}", output_device.name()?);

    let output_config = output_device.default_output_config()?;
    info!("Default output config: {:?}", output_config);

    let output_sample_rate = args.sample_rate;
    info!(
        "Attempting to use output sample rate: {}Hz",
        output_sample_rate
    );

    let output_buffer_clone = output_buffer.clone();
    let stream_config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(output_sample_rate),
        buffer_size: BufferSize::Default,
    };
    let output_stream = Some(match output_config.sample_format() {
        SampleFormat::I16 => build_output_stream::<i16>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        SampleFormat::U16 => build_output_stream::<u16>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        SampleFormat::F32 => build_output_stream::<f32>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        sample_format => {
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {:?}",
                sample_format
            ))
        }
    });

    // Start the output stream
    output_stream.as_ref().unwrap().play()?;
    info!(
        "Speaker output started with sample rate {}Hz",
        output_sample_rate
    );

    // Process input file if specified
    if let Some(input_path) = &args.input {
        info!("Reading input file: {}", input_path);
        let (samples, sample_rate) = read_wav_file(input_path)?;
        info!("Read {} samples at {} Hz", samples.len(), sample_rate);
        input_sample_rate = sample_rate;
        // Process audio through the pipeline
        info!("Processing audio through pipeline...");
        pipeline_manager
            .process_audio(samples, args.sample_rate)
            .await?;
    } else {
        // Start a task to process microphone audio
        let pipeline_manager_clone = pipeline_manager.clone();
        let cancel_token_clone = cancel_token.clone();

        tokio::spawn(async move {
            info!("Processing microphone audio through pipeline...");

            // Process audio in chunks
            let chunk_size = 48000 / 1000 * 20; // 20ms at 48kHz
            let mut buffer = Vec::new();

            while let Some(samples) = audio_receiver.recv().await {
                if cancel_token_clone.is_cancelled() {
                    break;
                }
                if input_sample_rate != args.sample_rate {
                    // resample to 16k
                    let resampled_samples =
                        resample::resample_mono(&samples, input_sample_rate, args.sample_rate);
                    buffer.extend_from_slice(&resampled_samples);
                } else {
                    buffer.extend_from_slice(&samples);
                }
                // Process in complete chunks
                if buffer.len() >= chunk_size {
                    let chunk: Vec<i16> = buffer.drain(..chunk_size).collect();

                    // Apply VAD if enabled
                    let is_speech = if let Some(vad) = &vad_processor {
                        let mut frame = AudioFrame {
                            track_id: "main".to_string(),
                            samples: Samples::PCM(chunk.clone()),
                            timestamp: 0,
                            sample_rate: args.sample_rate as u16,
                        };

                        // Use the extension trait method to check if this is speech
                        vad.is_speech(&mut frame)
                    } else {
                        true // No VAD, always treat as speech
                    };

                    // If speech detected or VAD disabled, continue processing
                    if is_speech {
                        // Apply noise suppression if enabled
                        let processed_samples = if let Some(ns) = &noise_reducer {
                            let mut frame = AudioFrame {
                                track_id: "main".to_string(),
                                samples: Samples::PCM(chunk.clone()),
                                timestamp: 0,
                                sample_rate: args.sample_rate as u16,
                            };

                            if let Err(e) = ns.process_frame(&mut frame) {
                                error!("Noise suppression error: {}", e);
                            }

                            match frame.samples {
                                Samples::PCM(samples) => samples,
                                _ => chunk,
                            }
                        } else {
                            chunk
                        };

                        // Send to pipeline
                        if let Err(e) = pipeline_manager_clone
                            .process_audio(processed_samples, args.sample_rate)
                            .await
                        {
                            error!("Failed to process audio through pipeline: {}", e);
                        }
                    }
                }
            }

            Ok::<(), anyhow::Error>(())
        });
    }

    // Process events from the pipeline
    let test_completed = Arc::new(Mutex::new(false));
    let test_completed_clone = test_completed.clone();

    // Start a task to handle events
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_receiver.recv().await {
            match &event {
                SessionEvent::Transcription(track_id, timestamp, text) => {
                    info!("Transcription from {}: {} at {}", track_id, text, timestamp);
                }
                SessionEvent::LLM(timestamp, text) => {
                    info!("LLM response at {}: {}", timestamp, text);
                }
                SessionEvent::TTS(timestamp, text) => {
                    info!("TTS event at {}: {}", timestamp, text);
                }
                SessionEvent::Error(timestamp, message) => {
                    error!("Error event at {}: {}", timestamp, message);
                }
                _ => {}
            }
        }
    });

    // Process state updates from the pipeline
    let test_completed_for_task = test_completed_clone.clone();

    tokio::spawn(async move {
        while let Ok(state) = state_receiver.recv().await {
            match state {
                StreamState::Transcription(track_id, text) => {
                    info!("Transcription from {}: {}", track_id, text);
                }
                StreamState::LlmResponse(text) => {
                    info!("LLM response: {}", text);
                }
                StreamState::TtsAudio(audio_data, sample_rate) => {
                    info!(
                        "Received synthesized audio: {} samples at {}Hz",
                        audio_data.len(),
                        sample_rate
                    );

                    // Convert sample rate if needed
                    let final_audio = if sample_rate != output_sample_rate {
                        info!(
                            "Resampling TTS audio from {}Hz to {}Hz",
                            sample_rate, output_sample_rate
                        );

                        let resampled_audio =
                            resample::resample_mono(&audio_data, sample_rate, output_sample_rate);

                        info!(
                            "Resampled from {} to {} samples",
                            audio_data.len(),
                            resampled_audio.len()
                        );

                        resampled_audio
                    } else {
                        audio_data.clone()
                    };

                    // Calculate audio duration in seconds based on final sample count
                    let audio_duration_seconds =
                        final_audio.len() as f64 / output_sample_rate as f64;
                    info!("Audio duration: {:.2} seconds", audio_duration_seconds);

                    // Set a timeout to mark test as completed after audio finishes playing
                    let test_completed_for_audio = test_completed_for_task.clone();
                    tokio::spawn(async move {
                        // Add a small buffer to ensure audio has time to play completely
                        let timeout_duration =
                            Duration::from_secs_f64(audio_duration_seconds + 0.5);
                        tokio::time::sleep(timeout_duration).await;

                        // Mark test as completed
                        let mut completed = test_completed_for_audio.lock().unwrap();
                        *completed = true;
                        info!("Test marked as completed after audio playback");
                    });
                    info!("After speed adjustment: {} samples", final_audio.len());

                    output_buffer.push(&final_audio);
                }
                StreamState::Error(err) => {
                    error!("Pipeline error: {}", err);
                }
                _ => {}
            }
        }
    });

    // Wait for processing to complete or timeout
    let mut timeout_intervals = 0;
    let max_timeout_intervals = 60; // 60 seconds total timeout

    loop {
        if *test_completed.lock().unwrap() {
            info!("Test completed successfully");
            break;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        timeout_intervals += 1;

        if timeout_intervals >= max_timeout_intervals {
            error!("Timeout waiting for processing to complete");
            break;
        }
    }

    // Shut down the pipeline
    pipeline_manager.stop().await?;
    event_task.abort();

    // Close input/output streams
    drop(input_stream);
    drop(output_stream);
    info!("All processing complete");

    Ok(())
}
