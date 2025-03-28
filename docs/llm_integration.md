# LLM Integration

This document provides instructions on how to use the LLM (Large Language Model) integration in RustPBX.

## Overview

RustPBX integrates with OpenAI's API to provide language model capabilities. The integration supports:

- Conversation history management with configurable context length
- Streaming responses for better user experience
- Environment variable configuration
- Customizable model parameters

## Prerequisites

Before using the LLM integration, you need to:

1. Obtain an OpenAI API key from [platform.openai.com](https://platform.openai.com/api-keys)
2. Set up environment variables or use a `.env` file

## Configuration

### Environment Variables

The LLM client can be configured using the following environment variables:

```sh
# Required
OPENAI_API_KEY=your_openai_api_key

# Optional
OPENAI_BASE_URL=https://api.openai.com/v1  # Alternative API endpoint
OPENAI_ORG_ID=your_org_id                  # Organization ID
OPENAI_MAX_TURNS=10                        # Max conversation turns to keep
```

You can create a `.env` file in your project root with these variables. See `.env.example` for a template.

### LLM Configuration

The `LlmConfig` struct allows you to customize the behavior of the language model:

```rust
// Default configuration reads model from OPENAI_MODEL environment variable
let config = LlmConfig::default();

// Or customize while keeping the model from environment
let config = LlmConfig {
    prompt: "You are a helpful assistant".to_string(), // System prompt
    temperature: Some(0.7),                            // Creativity (0.0-1.0)
    max_tokens: Some(500),                             // Max response length
    max_conversation_turns: Some(5),                   // Conversation context length
    stream: Some(true),                                // Enable streaming responses
    base_url: None,                                    // Custom API endpoint
    tools: None,                                       // Function calling (advanced)
    ..LlmConfig::default()                             // Get model from env
};

// Or specify the model explicitly (overrides environment variable)
let config = LlmConfig {
    model: "gpt-4o".to_string(),                       // Explicitly set model
    prompt: "You are a helpful assistant".to_string(), // System prompt
    // ... other settings
};
```

## Creating an LLM Client

### From Environment Variables

The simplest way to create an LLM client is from environment variables:

```rust
use rustpbx::llm::OpenAiClient;

// Load environment variables from .env file
dotenv::dotenv().ok();

// Create the client from environment variables
let llm_client = OpenAiClient::from_env()?;
```

### Using the Builder Pattern

For more control, you can use the builder pattern:

```rust
use rustpbx::llm::OpenAiClientBuilder;

let llm_client = OpenAiClientBuilder::new()
    .with_api_key("your_api_key")
    .with_base_url("https://api.openai.com/v1")
    .with_org_id("your_org_id")
    .with_max_conversation_turns(10)
    .build()?;
```

## Using the LLM Processor

The `LlmProcessor` integrates the LLM client with RustPBX's event system:

```rust
use std::sync::Arc;
use rustpbx::{
    event::{EventSender, SessionEvent},
    llm::{LlmConfig, LlmProcessor, OpenAiClient},
};
use tokio::sync::broadcast;

// Create event channel
let (event_sender, mut event_receiver) = broadcast::channel::<SessionEvent>(10);

// Create LLM client
let llm_client = OpenAiClient::from_env()?;

// Create processor
let processor = LlmProcessor::new(
    LlmConfig::default(),
    Arc::new(llm_client),
    event_sender,
);

// Listen for LLM responses
tokio::spawn(async move {
    while let Ok(event) = event_receiver.recv().await {
        if let SessionEvent::LLM(timestamp, text) = event {
            println!("[{}] AI: {}", timestamp, text);
        }
    }
});

// Process a user message
let response = processor.process("Hello, how are you?").await?;
```

## Streaming vs. Non-Streaming

By default, the LLM client uses streaming mode for better user experience. This can be configured in the `LlmConfig`:

```rust
// Enable streaming (default)
config.stream = Some(true);

// Disable streaming (wait for complete response)
config.stream = Some(false);
```

In streaming mode, the processor will emit multiple `SessionEvent::LLM` events as the response is generated, with each event containing the full response up to that point.

## Conversation History

The LLM client maintains a conversation history to provide context for subsequent messages. The history is limited by the `max_conversation_turns` parameter to control token usage.

## Example

See the full working example in `examples/llm_example.rs`:

```bash
# Set up your .env file first with your OpenAI API key
cargo run --example llm-example
```

## Integration with Media Stream

The LLM processor can be integrated with the media stream to create voice-based AI assistants:

1. Use ASR (Automatic Speech Recognition) to convert audio to text
2. Process the text with the LLM processor
3. Use TTS (Text-to-Speech) to convert the response to audio

See the WebRTC handler implementation for a complete example of this integration. 