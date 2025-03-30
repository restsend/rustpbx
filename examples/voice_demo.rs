/// This is a simplified demo application which reads wav file
/// from command line and processes it with LLM.
///
/// ```bash
/// # Use OpenAI to process input
/// cargo run --example voice_demo -- --input fixtures/hello_book_course_zh_16k.wav --api_key YOUR_API_KEY
/// ```
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use futures::StreamExt;
use rustpbx::media::track::file::read_wav_file;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber;

use rustpbx::{
    event::SessionEvent,
    llm::{LlmClient, LlmConfig, LlmContent, OpenAiClientBuilder},
};

/// Audio Agent
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// OpenAI API Key
    #[arg(long)]
    api_key: Option<String>,

    /// Input file (wav)
    #[arg(short, long)]
    input: Option<String>,

    /// System prompt for LLM
    #[arg(
        short = 'p',
        long,
        default_value = "You are a helpful assistant who responds in Chinese."
    )]
    prompt: String,
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

    if api_key.is_empty() {
        error!("OpenAI API key is required. Please specify with --api_key or set OPENAI_API_KEY environment variable.");
        return Ok(());
    }

    // Create cancellation token
    let cancel_token = CancellationToken::new();

    // Create event sender for events
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(32);

    // Set up LLM client
    let llm_config = LlmConfig {
        model: "gpt-3.5-turbo".to_string(),
        prompt: args.prompt.clone(),
        temperature: Some(0.7),
        max_tokens: Some(1000),
        max_conversation_turns: Some(10),
        stream: Some(true),
        base_url: None,
        tools: None,
    };

    info!("Initializing OpenAI client...");
    let openai_client = OpenAiClientBuilder::new()
        .with_api_key(api_key)
        .build()?
        .with_config(llm_config);

    // Read input file if specified
    if let Some(input_path) = &args.input {
        info!("Reading input file: {}", input_path);
        let (samples, sample_rate) = read_wav_file(input_path)?;
        info!("Read {} samples at {} Hz", samples.len(), sample_rate);

        // For demo purposes, convert audio to text (mock transcription)
        let transcription = format!(
            "这是一个{}Hz的音频样本，包含{}个采样点。请你解释一下这段音频可能包含的内容。",
            sample_rate,
            samples.len()
        );

        info!("Transcription: {}", transcription);

        // Process with LLM
        info!("Sending to OpenAI...");
        let mut stream = openai_client.generate(&transcription).await?;

        let mut full_response = String::new();

        // Process LLM response
        while let Some(result) = stream.next().await {
            match result {
                Ok(LlmContent::Delta(delta)) => {
                    print!("{}", delta);
                    std::io::Write::flush(&mut std::io::stdout()).unwrap();
                    full_response.push_str(&delta);

                    // Send event
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as u32;

                    let _ = event_sender.send(SessionEvent::LLMDelta(timestamp, delta));
                }
                Ok(LlmContent::Final(final_text)) => {
                    // Send final event
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as u32;

                    let _ = event_sender.send(SessionEvent::LLMFinal(timestamp, final_text));
                    println!("\nFinal response received");
                }
                Err(e) => {
                    error!("Error from LLM: {}", e);
                    break;
                }
            }
        }

        println!("\nFull response: {}", full_response);
    } else {
        error!("Input file is required. Please specify with --input option.");
        return Ok(());
    }

    // Start a task to handle events (for demonstration purposes)
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_receiver.recv().await {
            match &event {
                SessionEvent::LLMDelta(timestamp, text) => {
                    info!("Received LLM delta at {}: {}", timestamp, text);
                }
                SessionEvent::LLMFinal(timestamp, text) => {
                    info!("Received LLM final at {}: {}", timestamp, text);
                }
                SessionEvent::Error(timestamp, message) => {
                    error!("Error event at {}: {}", timestamp, message);
                }
                _ => {}
            }
        }
    });

    // Allow time for processing to complete
    tokio::time::sleep(Duration::from_secs(2)).await;

    event_task.abort();
    info!("All processing complete");

    Ok(())
}
