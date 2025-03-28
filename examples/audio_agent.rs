/// This is a demo application which reads wav file from command line
/// and processes it with ASR, LLM and TTS.
///
/// ```bash
/// cargo run --example audio_agent -- --input data/audio/sample.wav --output out.wav
/// ```
use std::env;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber;

use rustpbx::{
    event::SessionEvent,
    llm::{LlmClient, LlmConfig, OpenAiClientBuilder},
    media::pipeline::{
        llm::LlmPipeline, synthesis::SynthesisPipeline, transcription::TranscriptionPipeline,
        PipelineManager, StreamState,
    },
    synthesis::{SynthesisClient, SynthesisConfig, TencentCloudTtsClient},
    transcription::{TencentCloudAsrClient, TranscriptionClient, TranscriptionConfig},
};

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
    input: String,

    /// Output file path
    #[arg(short, long)]
    output: String,

    /// Sample rate
    #[arg(short = 'R', long, default_value = "16000")]
    sample_rate: u32,

    /// System prompt for LLM
    #[arg(short = 'p', long, default_value = "You are a helpful assistant.")]
    prompt: String,
}

// Helper function to read samples from a wav file
fn read_wav_file(path: &str) -> Result<(Vec<i16>, u32)> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let samples: Vec<i16> = reader.samples().filter_map(Result::ok).collect();
    Ok((samples, spec.sample_rate))
}

// Helper to write wav file
struct WavWriter {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl WavWriter {
    fn new(path: &str, sample_rate: usize, channels: u16) -> Result<Self> {
        let spec = hound::WavSpec {
            channels,
            sample_rate: sample_rate as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(path, spec)?;
        Ok(Self { writer })
    }

    fn write_samples(&mut self, samples: &[i16]) -> Result<()> {
        for &sample in samples {
            self.writer.write_sample(sample)?;
        }
        Ok(())
    }

    fn close(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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

    // Create cancellation token for pipeline
    let cancel_token = CancellationToken::new();

    // Create event sender for pipeline events
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(32);

    // Set up pipeline manager
    let pipeline_manager = PipelineManager::new(
        "audio_agent".to_string(),
        event_sender.clone(),
        cancel_token.clone(),
    );

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
        rate: Some(1.0),
        appid: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        volume: Some(100),
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

    // Read the input WAV file
    info!("Reading input file: {}", args.input);
    let (samples, sample_rate) = read_wav_file(&args.input)?;
    info!("Read {} samples at {} Hz", samples.len(), sample_rate);

    // Create output WAV writer
    let mut wav_writer = WavWriter::new(&args.output, args.sample_rate as usize, 1)?;

    // Process audio through the pipeline
    info!("Processing audio through pipeline...");
    pipeline_manager
        .process_audio(samples, args.sample_rate)
        .await?;

    // Process events from the pipeline
    let mut final_audio: Option<Vec<i16>> = None;

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
    while let Ok(state) = state_receiver.recv().await {
        match state {
            StreamState::Transcription(track_id, text) => {
                info!("Transcription from {}: {}", track_id, text);
            }
            StreamState::LlmResponse(text) => {
                info!("LLM response: {}", text);
            }
            StreamState::TtsAudio(audio_data, _) => {
                info!("Received synthesized audio: {} samples", audio_data.len());
                final_audio = Some(audio_data);
            }
            StreamState::Error(err) => {
                error!("Pipeline error: {}", err);
            }
            _ => {}
        }
    }

    // Wait for processing to complete
    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

    // Shut down the pipeline
    pipeline_manager.stop().await?;
    event_task.abort();

    // Write the final audio to the output file
    if let Some(audio) = final_audio {
        wav_writer.write_samples(&audio)?;
        info!("Wrote {} audio samples to {}", audio.len(), args.output);
    } else {
        error!("No audio was generated");
    }

    // Close the wav writer
    wav_writer.close()?;

    info!("All processing complete");

    Ok(())
}
