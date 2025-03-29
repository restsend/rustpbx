use crate::media::processor::{AudioFrame, Processor};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
// Add the common imports
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
// Add imports for event system
use crate::event::{EventSender, SessionEvent};
use async_trait::async_trait;

// Add provider modules
mod default;
pub use default::DefaultAsrClient;

mod tencent_cloud;
pub use tencent_cloud::TencentCloudAsrClient;

mod whisper;
pub use whisper::WhisperClient;

// Configuration for Transcription services (formerly ASR)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptionConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub language: Option<String>,
    // TencentCloud specific configuration
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub engine_type: Option<String>,
}

// Default config for backward compatibility
impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            language: None,
            appid: None,
            secret_id: None,
            secret_key: None,
            engine_type: None,
        }
    }
}

// Transcription Events
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptionEvent {
    pub track_id: String,
    pub timestamp: u32,
    pub text: String,
    pub is_final: bool,
}

// Unified transcription client trait with async_trait support
#[async_trait]
pub trait TranscriptionClient: Send + Sync + std::fmt::Debug {
    async fn transcribe(
        &self,
        audio_data: &[i16],
        sample_rate: u32,
        config: &TranscriptionConfig,
    ) -> Result<Option<String>>;
}

// Transcription Processor that integrates with the media stream system
pub struct TranscriptionProcessor {
    config: TranscriptionConfig,
    client: Arc<dyn TranscriptionClient>,
    event_sender: broadcast::Sender<TranscriptionEvent>,
    // Buffer for accumulating audio between processing
    buffer: Vec<i16>,
    // Buffer size in milliseconds
    buffer_duration_ms: u32,
    last_process_time: u64,
    // Session Event Sender for sending metrics
    session_event_sender: Option<EventSender>,
}

impl TranscriptionProcessor {
    pub fn new(
        config: TranscriptionConfig,
        client: Arc<dyn TranscriptionClient>,
        event_sender: broadcast::Sender<TranscriptionEvent>,
    ) -> Self {
        Self {
            config,
            client,
            event_sender,
            buffer: Vec::new(),
            buffer_duration_ms: 500, // Default 500ms buffer
            last_process_time: 0,
            session_event_sender: None,
        }
    }

    // Set buffer duration in milliseconds
    pub fn with_buffer_duration(mut self, duration_ms: u32) -> Self {
        self.buffer_duration_ms = duration_ms;
        self
    }

    // Set session event sender for metrics
    pub fn with_session_event_sender(mut self, event_sender: EventSender) -> Self {
        self.session_event_sender = Some(event_sender);
        self
    }

    // Process audio buffer and generate transcription
    async fn process_buffer(
        &mut self,
        track_id: &str,
        timestamp: u32,
        sample_rate: u32,
    ) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        // Make a copy of the buffer for processing and clear original
        let buffer_for_processing = std::mem::take(&mut self.buffer);

        // Process with transcription client
        let transcription = self
            .client
            .transcribe(&buffer_for_processing, sample_rate, &self.config)
            .await?;

        // Check for transcription metrics and send if we have a session event sender
        ASR_METRICS.with(|metrics| {
            if let Some((ts, metrics_data)) = metrics.borrow_mut().take() {
                if let Some(sender) = &self.session_event_sender {
                    let _ = sender.send(SessionEvent::Metrics(ts, metrics_data.clone()));
                } else {
                    // Log if no sender
                    println!("Transcription TTFB metrics: {:?}", metrics_data);
                }
            }
        });

        // Send transcription event if we have a result
        if let Some(text) = transcription {
            let _ = self.event_sender.send(TranscriptionEvent {
                track_id: track_id.to_string(),
                timestamp,
                text,
                is_final: true,
            });
        }

        Ok(())
    }
}

impl Processor for TranscriptionProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // If transcription is not enabled, do nothing
        if !self.config.enabled {
            return Ok(());
        }

        // Clone self to be able to modify in async context
        let mut processor = self.clone();

        // Extract PCM samples
        let samples = match &frame.samples {
            crate::media::processor::Samples::PCM(samples) => samples.clone(),
            _ => return Ok(()), // Skip non-PCM formats for simplicity
        };

        // Capture frame info
        let track_id = frame.track_id.clone();
        let timestamp = frame.timestamp;
        let sample_rate = frame.sample_rate;

        // Spawn processing task
        tokio::spawn(async move {
            // Add samples to buffer
            processor.buffer.extend_from_slice(&samples);

            // Calculate duration of audio in buffer in milliseconds
            let buffer_duration = (processor.buffer.len() as u64 * 1000) / (sample_rate as u64);

            // Process buffer if it's large enough or if enough time has passed
            if buffer_duration >= processor.buffer_duration_ms as u64 {
                if let Err(e) = processor
                    .process_buffer(&track_id, timestamp, sample_rate.into())
                    .await
                {
                    tracing::error!("Transcription processing error: {}", e);
                }
                processor.last_process_time = timestamp as u64;
            }
        });

        Ok(())
    }
}

// Clone implementation for Transcription Processor
impl Clone for TranscriptionProcessor {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            client: self.client.clone(),
            event_sender: self.event_sender.clone(),
            buffer: self.buffer.clone(),
            buffer_duration_ms: self.buffer_duration_ms,
            last_process_time: self.last_process_time,
            session_event_sender: self.session_event_sender.clone(),
        }
    }
}

// Include advanced tests in a separate module
#[cfg(test)]
mod tests;

// Thread local storage for Transcription metrics (renamed from ASR_METRICS but kept same name for backward compatibility)
thread_local! {
    pub(crate) static ASR_METRICS: std::cell::RefCell<Option<(u32, serde_json::Value)>> = std::cell::RefCell::new(None);
}
