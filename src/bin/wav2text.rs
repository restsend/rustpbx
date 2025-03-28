use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hound::WavReader;
use rustpbx::transcription::{AsrClient, AsrConfig, TencentCloudAsrClient};
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

    info!("Reading WAV file: {}", args.input_file.display());
    let file = File::open(&args.input_file)
        .with_context(|| format!("Failed to open input file: {}", args.input_file.display()))?;
    let reader = BufReader::new(file);

    let mut wav_reader = WavReader::new(reader)
        .with_context(|| format!("Failed to parse WAV file: {}", args.input_file.display()))?;
    let spec = wav_reader.spec();

    info!(
        "WAV file info: {} channels, {} Hz, {} bits per sample",
        spec.channels, spec.sample_rate, spec.bits_per_sample
    );

    // Read all samples
    let mut all_samples = Vec::new();

    match spec.sample_format {
        hound::SampleFormat::Int => match spec.bits_per_sample {
            16 => {
                for sample in wav_reader.samples::<i16>() {
                    all_samples.push(sample.unwrap_or(0));
                }
            }
            8 => {
                for sample in wav_reader.samples::<i8>() {
                    all_samples.push(sample.unwrap_or(0) as i16);
                }
            }
            24 | 32 => {
                for sample in wav_reader.samples::<i32>() {
                    all_samples.push((sample.unwrap_or(0) >> 16) as i16);
                }
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported bits per sample: {}",
                    spec.bits_per_sample
                ));
            }
        },
        hound::SampleFormat::Float => {
            for sample in wav_reader.samples::<f32>() {
                all_samples.push((sample.unwrap_or(0.0) * 32767.0) as i16);
            }
        }
    }

    debug!("Read {} raw samples from WAV file", all_samples.len());

    // If stereo, convert to mono by averaging channels
    if spec.channels == 2 {
        info!("Converting stereo to mono");
        let mono_samples: Vec<i16> = all_samples
            .chunks(2)
            .map(|chunk| ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16)
            .collect();
        all_samples = mono_samples;
        debug!("Converted to {} mono samples", all_samples.len());
    }

    // Resample to 16kHz if needed
    if spec.sample_rate != 16000 {
        info!("Resampling from {}Hz to 16000Hz", spec.sample_rate);
        let ratio = 16000.0 / spec.sample_rate as f64;
        let new_len = (all_samples.len() as f64 * ratio) as usize;
        let mut resampled = vec![0i16; new_len];

        for i in 0..new_len {
            let src_idx = (i as f64 / ratio) as usize;
            if src_idx < all_samples.len() {
                resampled[i] = all_samples[src_idx];
            }
        }

        all_samples = resampled;
        debug!("Resampled to {} samples at 16kHz", all_samples.len());
    }

    info!("Processing {} audio samples", all_samples.len());

    // Create ASR client
    let asr_client = Arc::new(TencentCloudAsrClient::new());

    // Create ASR config
    let asr_config = AsrConfig {
        enabled: true,
        model: None,
        language: Some(args.language),
        appid: Some(tencent_appid),
        secret_id: Some(tencent_secret_id),
        secret_key: Some(tencent_secret_key),
        engine_type: Some(args.engine),
    };

    // Send the samples to the ASR client
    info!("Sending samples to ASR client for transcription...");
    match asr_client
        .transcribe(&all_samples, 16000, &asr_config)
        .await
    {
        Ok(text) => {
            if let Some(output_path) = args.output {
                // Write to output file
                std::fs::write(&output_path, &text).with_context(|| {
                    format!("Failed to write to output file: {}", output_path.display())
                })?;
                info!("Transcription saved to: {}", output_path.display());
            } else {
                // Print to console
                println!("{}", text);
            }
            info!("ASR processing completed successfully");
        }
        Err(e) => {
            error!("ASR error: {}", e);
            return Err(anyhow!("ASR processing failed: {}", e));
        }
    }

    Ok(())
}
