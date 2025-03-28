use anyhow::{anyhow, Result};
use clap::Parser;
use rustpbx::event::{EventSender, SessionEvent};
use rustpbx::llm::{LlmConfig, OpenAiClientBuilder};
use rustpbx::media::pipeline::{
    llm::LlmPipeline, synthesis::SynthesisPipeline, transcription::TranscriptionPipeline,
    PipelineManager,
};
use rustpbx::synthesis::{SynthesisClient, SynthesisConfig, TencentCloudTtsClient};
use rustpbx::transcription::{TencentCloudAsrClient, TranscriptionClient, TranscriptionConfig};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::broadcast, time};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to WAV file to use as input
    #[arg(short, long)]
    input: Option<PathBuf>,
}

async fn process_wav_file(
    path: &PathBuf,
    event_sender: EventSender,
    cancel_token: &CancellationToken,
) -> Result<()> {
    let session_id = Uuid::new_v4().to_string();
    info!("Starting pipeline demo with session ID: {}", session_id);

    // Create a pipeline manager
    let pipeline_manager = Arc::new(PipelineManager::new(
        format!("pipeline:{}", session_id),
        event_sender.clone(),
        cancel_token.clone(),
    ));

    // Open the WAV file
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    info!(
        "WAV file properties: {} channels, {}Hz, {} bits per sample",
        spec.channels, spec.sample_rate, spec.bits_per_sample
    );

    // Read the samples
    let samples: Vec<i16> = reader.samples().collect::<Result<Vec<i16>, _>>()?;
    info!("Read {} samples from WAV file", samples.len());

    // Configure LLM
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "qwen-turbo".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL").ok();

    info!("Using LLM model: {}", model);
    if let Some(url) = &base_url {
        info!("Using custom base URL: {}", url);
    }

    // Initialize OpenAI client
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let api_key_masked = if api_key.len() > 5 {
        format!("{}...", &api_key[..5])
    } else {
        "[none]".to_string()
    };
    info!(
        "Initializing OpenAI client with API key: {}",
        api_key_masked
    );

    let llm_config = LlmConfig {
        model,
        prompt: "你是一位专业的课程顾问，帮助学生预约课程。请简短回复。".to_string(),
        temperature: Some(0.7),
        max_tokens: Some(200),
        stream: Some(false),
        base_url: base_url.clone(),
        max_conversation_turns: Some(5),
        ..LlmConfig::default()
    };

    let openai_client = match OpenAiClientBuilder::new()
        .with_api_key(api_key)
        .with_base_url(base_url.unwrap_or_default())
        .build()
    {
        Ok(client) => {
            info!("OpenAI client initialized successfully");
            client
        }
        Err(e) => {
            error!("Failed to initialize OpenAI client: {}", e);
            return Err(anyhow!("Failed to initialize OpenAI client"));
        }
    };

    // Configure ASR
    let transcription_config = TranscriptionConfig {
        enabled: true,
        model: None,
        language: Some("zh-CN".to_string()),
        appid: Some(std::env::var("TENCENT_APPID").unwrap_or_default()),
        secret_id: Some(std::env::var("TENCENT_SECRET_ID").unwrap_or_default()),
        secret_key: Some(std::env::var("TENCENT_SECRET_KEY").unwrap_or_default()),
        engine_type: Some("16k_zh".to_string()),
    };

    let asr_client = TencentCloudAsrClient::new();

    // Configure TTS
    let tts_config = SynthesisConfig {
        url: "".to_string(),
        voice: Some("1".to_string()), // 标准女声
        rate: Some(1.0),
        appid: Some(std::env::var("TENCENT_APPID").unwrap_or_default()),
        secret_id: Some(std::env::var("TENCENT_SECRET_ID").unwrap_or_default()),
        secret_key: Some(std::env::var("TENCENT_SECRET_KEY").unwrap_or_default()),
        volume: Some(5),
        speaker: Some(101001), // Female voice
        codec: Some("wav".to_string()),
    };

    let tts_client = TencentCloudTtsClient::new();

    // Create and initialize pipeline components
    // ASR pipeline
    let transcription_pipeline = TranscriptionPipeline::new(
        "transcription".to_string(),
        Arc::new(asr_client) as Arc<dyn TranscriptionClient>,
        transcription_config,
    );

    // LLM pipeline
    let llm_pipeline = LlmPipeline::new(
        "llm".to_string(),
        Arc::new(openai_client) as Arc<dyn rustpbx::llm::LlmClient>,
        llm_config,
    );

    // TTS pipeline
    let synthesis_pipeline = SynthesisPipeline::new(
        "synthesis".to_string(),
        Arc::new(tts_client) as Arc<dyn SynthesisClient>,
        tts_config,
    );

    // Register all pipeline components
    pipeline_manager
        .add_pipeline(Box::new(transcription_pipeline))
        .await?;
    pipeline_manager
        .add_pipeline(Box::new(llm_pipeline))
        .await?;
    pipeline_manager
        .add_pipeline(Box::new(synthesis_pipeline))
        .await?;

    // Process the audio
    info!("Processing audio through pipeline...");
    pipeline_manager
        .process_audio(samples, spec.sample_rate)
        .await?;

    // Wait for all processing to complete (in a real app, we would wait for specific events)
    info!("Waiting for pipeline processing to complete...");
    time::sleep(Duration::from_secs(30)).await;

    // Shutdown the pipeline
    pipeline_manager.stop().await?;

    info!("Pipeline processing complete");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing for better logging
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let cancel_token = CancellationToken::new();

    // Create a channel for event distribution
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(32);

    // Start a task to handle events
    let event_handler = tokio::spawn(async move {
        while let Ok(event) = event_receiver.recv().await {
            match &event {
                SessionEvent::Transcription(track_id, timestamp, text) => {
                    info!(
                        "Transcription event from {}: {} at {}",
                        track_id, text, timestamp
                    );
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
                _ => {
                    // Ignore other events
                }
            }
        }
    });

    // Handle input based on command line arguments
    if let Some(input_path) = args.input {
        info!("Processing WAV file: {:?}", input_path);

        if let Err(e) = process_wav_file(&input_path, event_sender, &cancel_token).await {
            error!("Failed to process WAV file: {}", e);
            return Err(e);
        }
    } else {
        // For this example, we'll skip the microphone setup and just exit
        info!("No input file specified. Run with --input PATH to process a WAV file.");
    }

    // Cleanup
    info!("Pipeline demo complete");
    cancel_token.cancel();

    // Wait for event handler to finish
    event_handler.abort();

    // Give tasks a moment to clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(())
}
