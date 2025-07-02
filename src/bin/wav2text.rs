use anyhow::{anyhow, Result};
use clap::Parser;
use rustpbx::{
    event::SessionEvent,
    media::track::file::read_wav_file,
    transcription::{TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionOption},
};
use std::{path::PathBuf, time::Duration};
use tracing::{debug, info};

/// Convert WAV audio files to text using speech recognition
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to input WAV file
    #[arg(value_name = "INPUT")]
    input_file: PathBuf,

    /// Language code (default: zh-CN)
    #[arg(short, long, default_value = "zh-CN")]
    language: String,

    /// Engine type (default: 16k_zh for Chinese)
    #[arg(short, long, default_value = "16k_zh")]
    engine: String,

    /// Output text to a file instead of console
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    // Set up logging
    tracing_subscriber::fmt::init();

    // Parse command line arguments
    let args = Args::parse();

    // Set log level based on verbose flag
    if args.verbose {
        std::env::set_var("RUST_LOG", "debug");
    } else {
        std::env::set_var("RUST_LOG", "info");
    }

    // Load .env file if it exists
    dotenv::dotenv().ok();

    // Get credentials from environment variables
    let tencent_secret_id = match std::env::var("TENCENT_SECRET_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_SECRET_ID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow!("Missing TENCENT_SECRET_ID"));
        }
    };

    let tencent_secret_key = match std::env::var("TENCENT_SECRET_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("Error: TENCENT_SECRET_KEY environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow!("Missing TENCENT_SECRET_KEY"));
        }
    };

    let tencent_appid = match std::env::var("TENCENT_APPID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_APPID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow!("Missing TENCENT_APPID"));
        }
    };

    // Read the WAV file
    if !args.input_file.exists() {
        return Err(anyhow!(
            "Input file does not exist: {}",
            args.input_file.display()
        ));
    }

    let (all_samples, sample_rate) = read_wav_file(args.input_file.to_str().unwrap())?;
    debug!(
        "Read {} samples, {}Hz from WAV file",
        all_samples.len(),
        sample_rate
    );
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(16);
    // Create ASR client
    let transcription_client = TencentCloudAsrClientBuilder::new(
        TranscriptionOption {
            language: Some(args.language),
            app_id: Some(tencent_appid),
            secret_id: Some(tencent_secret_id),
            secret_key: Some(tencent_secret_key),
            model_type: Some(args.engine),
            ..Default::default()
        },
        event_sender,
    )
    .build()
    .await?;
    // Send the samples to the transcription client
    transcription_client.send_audio(&all_samples)?;
    transcription_client.send_audio(&[])?;
    let duration = Duration::from_secs(all_samples.len() as u64 / sample_rate as u64);
    info!(
        "Sending samples to transcription client for processing {} seconds",
        duration.as_secs()
    );
    let retch_result = async move {
        let mut result = String::new();
        while let Ok(event) = event_receiver.recv().await {
            match event {
                SessionEvent::AsrFinal { text, end_time, .. } => {
                    result.push_str(&text);
                    if end_time.unwrap_or(0) > duration.as_millis() as u64 {
                        break;
                    }
                }
                _ => {}
            }
        }
        result
    };

    let result = tokio::time::timeout(duration + Duration::from_secs(1), retch_result).await?;
    println!("Transcription result: {}", result);

    Ok(())
}
