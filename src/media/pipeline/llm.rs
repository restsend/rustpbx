use anyhow::Result;
use async_trait::async_trait;
use serde_json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::event::EventSender;
use crate::llm::{LlmClient, LlmConfig};
use crate::media::processor::AudioFrame;

use super::{Pipeline, StreamState, StreamStateReceiver, StreamStateSender};

/// Pipeline processor that handles LLM processing
#[derive(Debug)]
pub struct LlmPipeline {
    id: String,
    client: Arc<dyn LlmClient>,
    config: LlmConfig,
    token: Option<CancellationToken>,
}

impl LlmPipeline {
    pub fn new(id: String, client: Arc<dyn LlmClient>, config: LlmConfig) -> Self {
        Self {
            id,
            client,
            config,
            token: None,
        }
    }

    // Create a LlmPipeline from a configuration
    pub fn from_config(id: String, config_json: Option<serde_json::Value>) -> Self {
        // Create a default LlmConfig
        let mut config = LlmConfig::default();

        // If we have a config JSON, parse it
        if let Some(json) = config_json {
            // Update LlmConfig fields based on the JSON
            if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                config.model = model.to_string();
            }

            if let Some(prompt) = json.get("prompt").and_then(|v| v.as_str()) {
                config.prompt = prompt.to_string();
            }

            if let Some(temperature) = json.get("temperature").and_then(|v| v.as_f64()) {
                config.temperature = Some(temperature as f32);
            }

            if let Some(max_tokens) = json.get("max_tokens").and_then(|v| v.as_i64()) {
                config.max_tokens = Some(max_tokens as u32);
            }
        }

        // Create a simple mock LLM client
        #[derive(Debug)]
        struct MockLlmClient;

        #[async_trait]
        impl LlmClient for MockLlmClient {
            fn generate_response<'a>(
                &'a self,
                text: &'a str,
                _config: &'a LlmConfig,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
            {
                Box::pin(async move {
                    // This is a mock that just echoes the input with a prefix
                    Ok(format!("LLM response to: {}", text))
                })
            }

            fn generate_stream<'a>(
                &'a self,
                text: &'a str,
                _config: &'a LlmConfig,
                _event_sender: EventSender,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
            {
                Box::pin(async move {
                    // Simply echo back the input with a prefix
                    Ok(format!("LLM streaming response to: {}", text))
                })
            }
        }

        let client: Arc<dyn LlmClient> = Arc::new(MockLlmClient);

        Self {
            id,
            client,
            config,
            token: None,
        }
    }

    async fn process_with_llm(&self, text: &str) -> Result<String> {
        info!("Processing with LLM: {}", text);

        // Set a timeout for LLM processing
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.client.generate_response(text, &self.config),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("LLM processing timed out")),
        }
    }
}

#[async_trait]
impl Pipeline for LlmPipeline {
    fn id(&self) -> String {
        self.id.clone()
    }

    async fn start(
        &mut self,
        token: CancellationToken,
        state_sender: StreamStateSender,
        state_receiver: StreamStateReceiver,
        _event_sender: EventSender,
    ) -> Result<()> {
        info!("Starting LLM pipeline {}", self.id);
        self.token = Some(token.clone());

        let client = Arc::clone(&self.client);
        let id = self.id.clone();
        let config = self.config.clone();

        let state_sender_clone = state_sender.clone();
        let mut receiver = state_receiver;

        // Start the pipeline processing in a separate task
        tokio::spawn(async move {
            info!("LLM pipeline {} started", id);

            // Process incoming stream states
            while let Ok(state) = receiver.recv().await {
                if token.is_cancelled() {
                    break;
                }

                match state {
                    StreamState::Transcription(track_id, text) => {
                        info!(
                            "LLM pipeline received transcription from {}: {}",
                            track_id, text
                        );

                        // Create a reference to self for the async block
                        let pipeline = LlmPipeline {
                            id: id.clone(),
                            client: client.clone(),
                            config: config.clone(), // Use the passed config
                            token: Some(token.clone()),
                        };

                        // Process through LLM
                        match pipeline.process_with_llm(&text).await {
                            Ok(llm_response) => {
                                info!("LLM response: {}", llm_response);

                                // Send the LLM response to the pipeline
                                if let Err(e) =
                                    state_sender_clone.send(StreamState::LlmResponse(llm_response))
                                {
                                    error!("Failed to send LLM response to pipeline: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("LLM processing failed: {}", e);
                                if let Err(e) = state_sender_clone
                                    .send(StreamState::Error(format!("LLM error: {}", e)))
                                {
                                    error!("Failed to send error to pipeline: {}", e);
                                }
                            }
                        }
                    }
                    _ => {
                        // Ignore other state types
                    }
                }
            }

            info!("LLM pipeline {} stopped", id);
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = &self.token {
            token.cancel();
        }
        Ok(())
    }
}
