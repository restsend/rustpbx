use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hound::WavReader;
use rustpbx::{
    media::track::file::read_wav_file,
    transcription::{TencentCloudAsrClientBuilder, TranscriptionClient, TranscriptionConfig},
};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info};

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

    // Create ASR client
    let transcription_client = TencentCloudAsrClientBuilder::new(TranscriptionConfig {
        language: Some(args.language),
        appid: Some(tencent_appid),
        secret_id: Some(tencent_secret_id),
        secret_key: Some(tencent_secret_key),
        engine_type: args.engine,
        ..Default::default()
    })
    .build()
    .await?;
    // Send the samples to the transcription client
    info!("Sending samples to transcription client for processing...");
    transcription_client.send_audio(&all_samples).await?;

    // Get the transcription result
    let result = transcription_client.next().await.expect("No result");
    println!("Transcription result: {}", result.text);

    Ok(())
}
