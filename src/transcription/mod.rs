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
pub use whisper::WhisperAsrClient;

// Configuration for ASR (Automatic Speech Recognition)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AsrConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub language: Option<String>,
    // Add TencentCloud specific configuration
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub engine_type: Option<String>,
}

// ASR Events
#[derive(Debug, Clone, Serialize)]
pub struct AsrEvent {
    pub track_id: String,
    pub timestamp: u32,
    pub text: String,
    pub is_final: bool,
}

// ASR client trait - to be implemented with actual ASR integration
pub trait AsrClient: Send + Sync {
    fn transcribe<'a>(
        &'a self,
        audio_data: &'a [i16],
        sample_rate: u32,
        config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;
}

// Simplified transcription client trait for pipeline usage
#[async_trait]
pub trait TranscriptionClient: Send + Sync + std::fmt::Debug {
    async fn transcribe(
        &self,
        audio_data: &[i16],
        sample_rate: u32,
        config: &TranscriptionConfig,
    ) -> Result<String>;
}

// Pipeline-friendly configuration
#[derive(Debug, Clone, Default)]
pub struct TranscriptionConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub language: Option<String>,
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub engine_type: Option<String>,
}

// Implement TranscriptionClient for TencentCloudAsrClient
#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    async fn transcribe(
        &self,
        audio_data: &[i16],
        sample_rate: u32,
        config: &TranscriptionConfig,
    ) -> Result<String> {
        // Convert TranscriptionConfig to AsrConfig
        let asr_config = AsrConfig {
            enabled: config.enabled,
            model: config.model.clone(),
            language: config.language.clone(),
            appid: config.appid.clone(),
            secret_id: config.secret_id.clone(),
            secret_key: config.secret_key.clone(),
            engine_type: config.engine_type.clone(),
        };

        // Use the AsrClient implementation
        let future = <Self as AsrClient>::transcribe(self, audio_data, sample_rate, &asr_config);
        future.await
    }
}

// ASR Processor that integrates with the media stream system
pub struct AsrProcessor {
    config: AsrConfig,
    client: Arc<dyn AsrClient>,
    event_sender: broadcast::Sender<AsrEvent>,
    // Buffer for accumulating audio between processing
    buffer: Vec<i16>,
    // Buffer size in milliseconds
    buffer_duration_ms: u32,
    last_process_time: u64,
    // Add Session Event Sender for sending metrics
    session_event_sender: Option<EventSender>,
}

impl AsrProcessor {
    pub fn new(
        config: AsrConfig,
        client: Arc<dyn AsrClient>,
        event_sender: broadcast::Sender<AsrEvent>,
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

        // Process with ASR client
        let transcription = self
            .client
            .transcribe(&buffer_for_processing, sample_rate, &self.config)
            .await?;

        // Check for ASR metrics and send if we have a session event sender
        ASR_METRICS.with(|metrics| {
            if let Some((ts, metrics_data)) = metrics.borrow_mut().take() {
                if let Some(sender) = &self.session_event_sender {
                    let _ = sender.send(SessionEvent::Metrics(ts, metrics_data.clone()));
                } else {
                    // Log if no sender
                    println!("ASR TTFB metrics: {:?}", metrics_data);
                }
            }
        });

        // Send ASR event
        let _ = self.event_sender.send(AsrEvent {
            track_id: track_id.to_string(),
            timestamp,
            text: transcription,
            is_final: true,
        });

        Ok(())
    }
}

impl Processor for AsrProcessor {
    fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
        // If ASR is not enabled, do nothing
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
                    tracing::error!("ASR processing error: {}", e);
                }
                processor.last_process_time = timestamp as u64;
            }
        });

        Ok(())
    }
}

// Clone implementation for ASR Processor
impl Clone for AsrProcessor {
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

// Thread local storage for ASR metrics
thread_local! {
    pub(crate) static ASR_METRICS: std::cell::RefCell<Option<(u32, serde_json::Value)>> = std::cell::RefCell::new(None);
}
