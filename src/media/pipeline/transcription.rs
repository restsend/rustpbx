use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::event::EventSender;
use crate::media::processor::AudioFrame;
use crate::transcription::{TranscriptionClient, TranscriptionConfig};

use super::{Pipeline, StreamState, StreamStateReceiver, StreamStateSender};

/// Pipeline processor that handles speech-to-text transcription
#[derive(Debug)]
pub struct TranscriptionPipeline {
    id: String,
    client: Arc<dyn TranscriptionClient>,
    config: TranscriptionConfig,
    token: Option<CancellationToken>,
}

impl TranscriptionPipeline {
    pub fn new(
        id: String,
        client: Arc<dyn TranscriptionClient>,
        config: TranscriptionConfig,
    ) -> Self {
        Self {
            id,
            client,
            config,
            token: None,
        }
    }

    /// Create a new TranscriptionPipeline from a JSON configuration
    pub fn from_config(id: String, config_json: Option<serde_json::Value>) -> Self {
        // Default configuration
        let mut config = TranscriptionConfig::default();

        // Update config from JSON if provided
        if let Some(json) = config_json {
            if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
                config.model = Some(model.to_string());
            }

            if let Some(language) = json.get("language").and_then(|v| v.as_str()) {
                config.language = Some(language.to_string());
            }

            // No URL field in TranscriptionConfig, so we'll skip this
            // if let Some(url) = json.get("url").and_then(|v| v.as_str()) {
            //     config.url = Some(url.to_string());
            // }
        }

        // Create a simple mock transcription client
        #[derive(Debug)]
        struct MockTranscriptionClient;

        #[async_trait]
        impl TranscriptionClient for MockTranscriptionClient {
            async fn transcribe(
                &self,
                _samples: &[i16],
                _sample_rate: u32,
                _config: &TranscriptionConfig,
            ) -> Result<String> {
                // This is a mock that just returns a fixed response
                Ok("This is a mock transcription".to_string())
            }
        }

        let client: Arc<dyn TranscriptionClient> = Arc::new(MockTranscriptionClient);

        Self {
            id,
            client,
            config,
            token: None,
        }
    }
}

#[async_trait]
impl Pipeline for TranscriptionPipeline {
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
        info!("Starting transcription pipeline {}", self.id);
        self.token = Some(token.clone());

        let config = self.config.clone();
        let client = self.client.clone();
        let id = self.id.clone();

        let state_sender_clone = state_sender.clone();
        let mut receiver = state_receiver;

        // Start the pipeline processing in a separate task
        tokio::spawn(async move {
            info!("Transcription pipeline {} started", id);

            // Process incoming stream states
            while let Ok(state) = receiver.recv().await {
                if token.is_cancelled() {
                    break;
                }

                match state {
                    StreamState::AudioInput(samples, sample_rate) => {
                        // Process audio through ASR
                        match client.transcribe(&samples, sample_rate, &config).await {
                            Ok(transcription) => {
                                info!("Transcription result: {}", transcription);

                                // Send the transcription to the pipeline
                                if let Err(e) = state_sender_clone
                                    .send(StreamState::Transcription(id.clone(), transcription))
                                {
                                    error!("Failed to send transcription to pipeline: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Transcription failed: {}", e);
                                if let Err(e) = state_sender_clone
                                    .send(StreamState::Error(format!("Transcription error: {}", e)))
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

            info!("Transcription pipeline {} stopped", id);
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = &self.token {
            token.cancel();
        }
        Ok(())
    }

    async fn process_frame(&self, _frame: &AudioFrame) -> Result<()> {
        // This is handled via the StreamState
        Ok(())
    }
}
