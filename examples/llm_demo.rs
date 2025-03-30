use std::sync::Arc;

use rustpbx::{
    event::SessionEvent,
    llm::{LlmClient, LlmConfig, LlmContent, OpenAiClient, OpenAiClientBuilder},
};

use anyhow::Result;
use dotenv::dotenv;
use futures::StreamExt;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> Result<()> {
    // Set up logging
    tracing_subscriber::fmt::init();

    // Load environment variables from .env file
    dotenv().ok();

    println!("Initializing LLM client from environment variables...");

    // Display OpenAI configuration from environment variables
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL").ok();
    let max_turns = std::env::var("OPENAI_MAX_TURNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10);

    println!("Configuration:");
    println!("  Model: {}", model);
    if let Some(url) = &base_url {
        println!("  Base URL: {}", url);
    }
    println!("  Max conversation turns: {}", max_turns);

    // Initialize the OpenAI client from environment variables
    let llm_client = match OpenAiClientBuilder::from_env().build() {
        Ok(client) => client,
        Err(err) => {
            println!("Failed to initialize OpenAI client: {}", err);
            println!("Please set OPENAI_API_KEY in your .env file or environment variables.");
            return Ok(());
        }
    };

    // Create an event channel to receive LLM responses
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(10);

    // Use LlmConfig default which already reads from OPENAI_MODEL environment variable
    let llm_config = LlmConfig {
        model,
        prompt: "You are a helpful assistant. Keep your responses brief and to the point."
            .to_string(),
        temperature: Some(0.7),
        max_tokens: Some(500),
        stream: Some(true), // Make sure streaming is enabled
        base_url: None,
        tools: None,
        max_conversation_turns: Some(5),
        ..LlmConfig::default() // Get default values including model from env
    };

    // Configure client with our custom config
    let llm_client = llm_client.with_config(llm_config);

    // Start listening for events in a separate task
    let event_task = tokio::spawn(async move {
        println!("Listening for LLM events...");
        while let Ok(event) = event_receiver.recv().await {
            match event {
                SessionEvent::LLMDelta(timestamp, text) => {
                    // Clear the line and print the current response
                    print!("\r\x1b[K"); // Clear line
                    print!("[{}] Assistant: {}", timestamp, text);
                    std::io::Write::flush(&mut std::io::stdout()).unwrap();
                }
                SessionEvent::LLMFinal(timestamp, text) => {
                    // Add a newline after the complete response
                    print!("\r\x1b[K"); // Clear line
                    println!("[{}] Assistant: {}", timestamp, text);
                }
                _ => {} // Ignore other events
            }
        }
    });

    // Process user input
    let mut input = String::new();

    println!("\nEnter your message (or type 'exit' to quit):");
    loop {
        print!("> ");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();

        input.clear();
        std::io::stdin().read_line(&mut input)?;

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if input.to_lowercase() == "exit" {
            break;
        }

        println!("User: {}", input);
        println!("Assistant is thinking...");

        // Get LLM response stream
        match llm_client.generate(input).await {
            Ok(mut stream) => {
                // Process stream items
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(LlmContent::Delta(delta)) => {
                            // Send delta event
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
                            let _ =
                                event_sender.send(SessionEvent::LLMFinal(timestamp, final_text));
                            break;
                        }
                        Err(e) => {
                            println!("\nError: {}", e);
                            break;
                        }
                    }
                }
                println!(); // Add a newline after response is complete
            }
            Err(e) => println!("Error: {}", e),
        }
    }

    // Cancel the event listening task
    event_task.abort();

    println!("Goodbye!");

    Ok(())
}
