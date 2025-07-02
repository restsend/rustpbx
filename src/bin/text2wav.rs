use anyhow::{Context, Result};
use clap::Parser;
use dotenv::dotenv;
use futures::StreamExt;
use hound::{SampleFormat, WavSpec, WavWriter};
use regex::Regex;
use rustpbx::PcmBuf;
use std::path::PathBuf;
use tracing::{debug, error, info};

use rustpbx::media::codecs::bytes_to_samples;
use rustpbx::synthesis::{SynthesisClient, SynthesisOption, SynthesisType, TencentCloudTtsClient};

const SAMPLE_RATE: u32 = 16000;

/// Text segment with optional pause
struct TextSegment {
    text: String,
    pause_ms: u32,
}

/// Convert text to WAV audio files using text-to-speech
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input text to convert to speech
    ///
    /// Format: Normal text or with pauses using the format ":N text" where N is pause duration in seconds
    /// Example: "Hello,:2 how are you?" (2 second pause between "Hello," and "how are you?")
    #[arg(value_name = "TEXT")]
    input_text: String,

    /// Path to output WAV file
    #[arg(value_name = "OUTPUT")]
    output_file: PathBuf,

    /// Speaker ID (default: 1)
    #[arg(short, long, default_value = "1005")]
    speaker: String,

    /// Speech rate (0.5-2.0, default: 1.0)
    #[arg(short, long, default_value = "1.0")]
    rate: f32,

    /// Volume level (0-10, default: 5)
    #[arg(short = 'l', long, default_value = "5")]
    volume: i32,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

/// Parse input text into segments with optional pauses
///
/// Format: Normal text or with pauses using the format ":N text" where N is pause duration in seconds
/// Example: "Hello,:2 how are you?" (2 second pause between "Hello," and "how are you?")
fn parse_input(input: &str) -> Vec<TextSegment> {
    let mut segments = Vec::new();
    let re = Regex::new(r":(\d+)([^:]+)").unwrap();

    let mut last_pos = 0;
    for cap in re.captures_iter(input) {
        let pause_ms = cap[1].parse::<u32>().unwrap_or(0) * 1000; // Convert seconds to ms
        let text = cap[2].trim().to_string();

        let full_match = cap.get(0).unwrap();
        if full_match.start() > last_pos {
            // Add text before pattern as a segment with no pause
            let prefix_text = input[last_pos..full_match.start()].trim();
            if !prefix_text.is_empty() {
                segments.push(TextSegment {
                    text: prefix_text.to_string(),
                    pause_ms: 0,
                });
            }
        }

        segments.push(TextSegment { text, pause_ms });
        last_pos = full_match.end();
    }

    // Add any remaining text
    if last_pos < input.len() {
        let remaining = input[last_pos..].trim();
        if !remaining.is_empty() {
            segments.push(TextSegment {
                text: remaining.to_string(),
                pause_ms: 0,
            });
        }
    }

    segments
}

/// Generate silence samples for a specified duration
fn generate_silence(duration_ms: u32) -> PcmBuf {
    let num_samples = (SAMPLE_RATE * duration_ms) / 1000;
    vec![0; num_samples as usize]
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    // Set up logging
    tracing_subscriber::fmt::init();
    dotenv().ok();
    // Parse command line arguments
    let args = Args::parse();

    // Set log level based on verbose flag
    if args.verbose {
        std::env::set_var("RUST_LOG", "debug");
    } else {
        std::env::set_var("RUST_LOG", "info");
    }

    // Display command information
    info!("Converting text to speech: '{}'", args.input_text);
    info!("Output file: {}", args.output_file.display());

    // Load .env file if it exists
    let env_result = dotenv().ok();
    if env_result.is_some() {
        debug!("Loaded environment variables from .env file");
    } else {
        debug!("No .env file found, using system environment variables");
    }

    // Get credentials from environment variables
    let secret_id = match std::env::var("TENCENT_SECRET_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_SECRET_ID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_SECRET_ID"));
        }
    };

    let secret_key = match std::env::var("TENCENT_SECRET_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("Error: TENCENT_SECRET_KEY environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_SECRET_KEY"));
        }
    };

    let appid = match std::env::var("TENCENT_APPID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_APPID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_APPID"));
        }
    };
    info!("Found TENCENT_APPID in environment");

    // Create TTS client and config
    let synthesis_config = SynthesisOption {
        provider: Some(SynthesisType::TencentCloud),
        speed: Some(args.rate), // Speech rate
        app_id: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        volume: Some(args.volume),   // Volume level (0-10)
        speaker: Some(args.speaker), // Speaker type
        ..Default::default()
    };
    info!("Created TTS configuration {:?}", synthesis_config);
    let tts_client = TencentCloudTtsClient::new(synthesis_config);

    // Parse the input text into segments
    let segments = parse_input(&args.input_text);
    debug!("Parsed input into {} segments", segments.len());

    // Prepare WAV file
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(&args.output_file, spec).with_context(|| {
        format!(
            "Failed to create output WAV file: {}",
            args.output_file.display()
        )
    })?;
    debug!(
        "Created WAV file writer with spec: channels={}, sample_rate={}, bits_per_sample={}",
        spec.channels, spec.sample_rate, spec.bits_per_sample
    );

    info!("Generating {} segments of audio...", segments.len());

    // Process each segment
    for (i, segment) in segments.iter().enumerate() {
        info!(
            "Segment {}: pause={}ms, text=\"{}\"",
            i + 1,
            segment.pause_ms,
            segment.text
        );

        // Add silence if specified
        if segment.pause_ms > 0 {
            let silence = generate_silence(segment.pause_ms);
            for &sample in &silence {
                writer.write_sample(sample)?;
            }
            debug!(
                "Added {}ms of silence ({} samples)",
                segment.pause_ms,
                silence.len()
            );
        }

        // Generate speech for the segment text
        if !segment.text.is_empty() {
            info!("Synthesizing text: {}", segment.text);

            match tts_client.synthesize(&segment.text, None).await {
                Ok(mut audio_stream) => {
                    let mut total_bytes = 0;
                    while let Some(chunk_result) = audio_stream.next().await {
                        match chunk_result {
                            Ok(chunk) => {
                                debug!("Received chunk of {} bytes", chunk.len());
                                total_bytes += chunk.len();
                                let samples: PcmBuf = bytes_to_samples(&chunk);
                                for &sample in &samples {
                                    writer.write_sample(sample)?;
                                }
                            }
                            Err(e) => {
                                error!("Error in audio stream chunk: {:?}", e);
                                return Err(anyhow::anyhow!(
                                    "Failed to process audio chunk: {}",
                                    e
                                ));
                            }
                        }
                    }
                    debug!("Received total of {} bytes of audio data", total_bytes);
                }
                Err(e) => {
                    error!("Failed to synthesize text: {}", e);
                    return Err(anyhow::anyhow!("Failed to synthesize text: {}", e));
                }
            }
        }
    }

    // Finalize the WAV file
    writer.finalize()?;
    info!(
        "Successfully created WAV file: {}",
        args.output_file.display()
    );

    Ok(())
}
