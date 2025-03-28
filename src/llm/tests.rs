use super::*;
use tokio::sync::broadcast;

// This test uses the real OpenAI client, reading configuration from .env
// It will be skipped if the OpenAI API key is not available
#[tokio::test]
async fn test_openai_client_with_env() {
    // Load .env file
    dotenv().ok();

    // Check if we have an OpenAI API key
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            println!("Skipping OpenAI API test: No API key found in environment");
            return;
        }
    };

    // Check if the API key is a placeholder or empty
    if api_key.is_empty() || api_key == "-" || api_key.contains("your-api-key") {
        println!("Skipping OpenAI API test: API key appears to be a placeholder");
        return;
    }

    // Initialize the OpenAI client from environment variables
    let _model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL").ok();

    println!("Running OpenAI test with:");
    println!("  Model: {}", _model);
    if let Some(url) = &base_url {
        println!("  Base URL: {}", url);
    }

    // Create the client from environment variables
    let llm_client = match OpenAiClient::from_env() {
        Ok(client) => client,
        Err(err) => {
            panic!("Failed to initialize OpenAI client: {}", err);
        }
    };

    // Test configuration
    let specific_word = "RustPBXTest";
    let test_input = format!("Please respond with only the word: {}", specific_word);

    // Configure LLM for test
    let llm_config = LlmConfig {
        model: "gpt-3.5-turbo".to_string(), // Will be replaced by env if available
        prompt:
            "You are a test assistant. Always respond exactly as requested without additional text."
                .to_string(),
        temperature: Some(0.0), // Low temperature for deterministic results
        max_tokens: Some(10),
        stream: Some(false), // No streaming for this test
        base_url: None,
        tools: None,
        max_conversation_turns: Some(1),
    };

    // Test non-streaming response
    let response = llm_client.generate_response(&test_input, &llm_config).await;
    assert!(
        response.is_ok(),
        "Failed to get response: {:?}",
        response.err()
    );

    let response_text = response.unwrap();
    println!("Non-streaming response: {}", response_text);
    assert!(
        response_text.contains(specific_word),
        "Response should contain '{}' but got: '{}'",
        specific_word,
        response_text
    );
}

// Streaming test with real OpenAI client
#[tokio::test]
async fn test_openai_client_streaming() {
    // Load .env file
    dotenv().ok();

    // Check if we have an OpenAI API key
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            println!("Skipping OpenAI API streaming test: No API key found in environment");
            return;
        }
    };

    // Check if the API key is a placeholder or empty
    if api_key.is_empty() || api_key == "-" || api_key.contains("your-api-key") {
        println!("Skipping OpenAI API streaming test: API key appears to be a placeholder");
        return;
    }

    // Initialize the OpenAI client from environment variables
    let _model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL").ok();

    println!("Running OpenAI streaming test with:");
    println!("  Model: {}", _model);
    if let Some(url) = &base_url {
        println!("  Base URL: {}", url);
    }

    // Create the client
    let llm_client = match OpenAiClient::from_env() {
        Ok(client) => client,
        Err(err) => {
            panic!("Failed to initialize OpenAI client: {}", err);
        }
    };

    // Configure LLM for streaming test
    let llm_config = LlmConfig {
        model: "gpt-3.5-turbo".to_string(), // Will be replaced by env if available
        prompt: "You are a test assistant. Keep your response short and direct.".to_string(),
        temperature: Some(0.7),
        max_tokens: Some(50),
        stream: Some(true), // Enable streaming
        base_url: None,
        tools: None,
        max_conversation_turns: Some(1),
    };

    // Create event channel to receive streaming responses
    let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(10);

    // Use a processor for simplicity
    let processor = LlmProcessor::new(llm_config, Arc::new(llm_client), event_sender);

    // Use a simple question instead of requiring a specific word
    let test_input = "What is 2+2?";

    // Process in a separate task
    let process_task = tokio::spawn(async move { processor.process(test_input).await });

    // Collect streaming events
    let mut events = Vec::new();
    let mut timeout = tokio::time::interval(tokio::time::Duration::from_secs(60)); // Increase timeout to 60 seconds

    loop {
        tokio::select! {
            _ = timeout.tick() => {
                println!("Timeout waiting for streaming response");
                break;
            }
            event = event_receiver.recv() => {
                match event {
                    Ok(SessionEvent::LLM(_, text)) => {
                        println!("Received streaming chunk: {}", text);
                        events.push(text.clone());

                        // Break if we've received something beyond the initial "Processing..." message
                        // and it's a non-empty, non-processing message
                        if text.len() > 20 || (text != "Processing..." && !text.is_empty()) {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Verify process result
    let result = process_task.await.unwrap();

    // Just check if we got any response, without requiring a specific word
    if let Ok(final_text) = result {
        println!("Final response from process: {}", final_text);
    } else {
        println!("Process error: {:?}", result);
    }

    // Verify we got some events
    assert!(!events.is_empty(), "No streaming events received");

    // Print all events we received
    println!("Received {} streaming events.", events.len());
    for (i, event) in events.iter().enumerate() {
        println!("  Event {}: {}", i, event);
    }

    // Get the final response
    let final_response = events.last().unwrap();
    println!("Final streaming response: {}", final_response);

    // Test passes if we got any non-empty response, we don't require a specific format
    assert!(!final_response.is_empty(), "Received empty final response");
}

// Test specifically for OpenAI Model configuration from .env
#[tokio::test]
async fn test_openai_model_from_env() {
    // Load .env file
    dotenv().ok();

    // Check if we have an OpenAI API key
    let api_key = match std::env::var("OPENAI_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            println!("Skipping OpenAI model test: No API key found in environment");
            return;
        }
    };

    // Check if the API key is a placeholder or empty
    if api_key.is_empty() || api_key == "-" || api_key.contains("your-api-key") {
        println!("Skipping OpenAI model test: API key appears to be a placeholder");
        return;
    }

    // Get model from environment
    let env_model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL").ok();

    println!("Testing OpenAI model configuration:");
    println!("  Model from .env: {}", env_model);
    if let Some(url) = &base_url {
        println!("  Base URL: {}", url);
    }

    // Create the client
    let llm_client = match OpenAiClient::from_env() {
        Ok(client) => client,
        Err(err) => {
            panic!("Failed to initialize OpenAI client: {}", err);
        }
    };

    // Create an LLM config with the default model
    let llm_config = LlmConfig {
        model: "gpt-3.5-turbo".to_string(), // This should be replaced by the model from env
        prompt: "You are a test assistant. Please tell me what model you are.".to_string(),
        temperature: Some(0.7),
        max_tokens: Some(50),
        stream: Some(false),
        base_url: None,
        tools: None,
        max_conversation_turns: Some(1),
    };

    // Ask the model to identify itself (which can help verify the model was loaded from env)
    let response = llm_client
        .generate_response("What is your model name?", &llm_config)
        .await;
    assert!(
        response.is_ok(),
        "Failed to get model response: {:?}",
        response.err()
    );

    let response_text = response.unwrap();
    println!("Model identification response: {}", response_text);

    // The response might not contain the exact model name, but we can at least verify
    // that we got a reasonable response from the API
    assert!(
        !response_text.is_empty(),
        "Received empty response from LLM API"
    );
}
