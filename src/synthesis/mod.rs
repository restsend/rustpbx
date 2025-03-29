use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
// Common imports
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
// Add imports for event system
use crate::event::{EventSender, SessionEvent};
use async_trait::async_trait;

// Add provider modules
mod tencent_cloud;
pub use tencent_cloud::TencentCloudTtsClient;

mod google;
pub use google::GoogleTtsClient;

mod http;
pub use http::HttpTtsClient;

// Configuration for Text-to-Speech
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SynthesisConfig {
    pub url: String,
    pub voice: Option<String>,
    pub rate: Option<f32>,
    // Add TencentCloud specific configuration
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub volume: Option<i32>,
    pub speaker: Option<i32>,
    pub codec: Option<String>,
}

// Simplified synthesis client trait for pipeline usage
#[async_trait]
pub trait SynthesisClient: Send + Sync + std::fmt::Debug {
    async fn synthesize(&self, text: &str, config: &SynthesisConfig) -> Result<Vec<u8>>;
}

// TTS Events
#[derive(Debug, Clone, Serialize)]
pub struct TtsEvent {
    pub timestamp: u32,
    pub text: String,
}

// TTS Processor
pub struct TtsProcessor {
    config: SynthesisConfig,
    client: Arc<dyn SynthesisClient>,
    event_sender: broadcast::Sender<TtsEvent>,
    // Add Session Event Sender for sending metrics
    session_event_sender: Option<EventSender>,
}

impl TtsProcessor {
    pub fn new(
        config: SynthesisConfig,
        client: Arc<dyn SynthesisClient>,
        event_sender: broadcast::Sender<TtsEvent>,
    ) -> Self {
        Self {
            config,
            client,
            event_sender,
            session_event_sender: None,
        }
    }

    // Set session event sender for metrics
    pub fn with_session_event_sender(mut self, event_sender: EventSender) -> Self {
        self.session_event_sender = Some(event_sender);
        self
    }

    // Synthesize text to speech
    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        // Record start time for metrics
        let start_time = std::time::Instant::now();

        // Process with TTS client
        let audio_data = self.client.synthesize(text, &self.config).await?;

        // Calculate processing time and log metrics
        let processing_time = start_time.elapsed().as_millis() as u64;

        // Store metrics
        if let Some(sender) = &self.session_event_sender {
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;

            let _ = sender.send(SessionEvent::Metrics(
                timestamp,
                serde_json::json!({
                    "service": "tts",
                    "processing_time_ms": processing_time,
                    "text_length": text.len(),
                }),
            ));
        }

        // Send TTS event
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let _ = self.event_sender.send(TtsEvent {
            timestamp,
            text: text.to_string(),
        });

        Ok(audio_data)
    }
}

// Clone implementation for TTS Processor
impl Clone for TtsProcessor {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            event_sender: self.event_sender.clone(),
            session_event_sender: self.session_event_sender.clone(),
        }
    }
}

// Include tests in a separate module
#[cfg(test)]
mod tests;
