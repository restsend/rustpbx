use crate::media::processor::{AudioFrame, Processor};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
// Add the new imports we need for TencentCloud client
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

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

// Default ASR client implementation (mock)
pub struct DefaultAsrClient {}

impl DefaultAsrClient {
    pub fn new() -> Self {
        Self {}
    }
}

impl AsrClient for DefaultAsrClient {
    fn transcribe<'a>(
        &'a self,
        _audio_data: &'a [i16],
        _sample_rate: u32,
        _config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // In a production implementation, this would call the ASR service
        // For demonstration purposes, we'll just simulate a response
        Box::pin(async { Ok("Sample transcription".to_string()) })
    }
}

// WhisperAsrClient - for integration with OpenAI's Whisper model
pub struct WhisperAsrClient {
    api_key: String,
}

impl WhisperAsrClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl AsrClient for WhisperAsrClient {
    fn transcribe<'a>(
        &'a self,
        _audio_data: &'a [i16],
        _sample_rate: u32,
        config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // In a production implementation, this would use the OpenAI API client
        // to process the audio data with Whisper.
        // For now, we'll return a mock response.
        let model = config.model.as_deref().unwrap_or("whisper-1");
        Box::pin(async move { Ok(format!("Transcription using model: {}", model)) })
    }
}

// TencentCloud ASR Client Implementation
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TencentCloudAsrResponse {
    #[serde(rename = "Response")]
    response: TencentCloudAsrResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TencentCloudAsrResult {
    #[serde(rename = "Result")]
    result: String,
    #[serde(rename = "RequestId")]
    request_id: String,
}

pub struct TencentCloudAsrClient {
    http_client: HttpClient,
}

impl TencentCloudAsrClient {
    pub fn new() -> Self {
        Self {
            http_client: HttpClient::new(),
        }
    }

    // Generate authentication signature for TencentCloud API
    fn generate_signature(
        &self,
        secret_id: &str,
        secret_key: &str,
        host: &str,
        method: &str,
        timestamp: u64,
    ) -> Result<String> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let canonical_request = format!("{}\n/\n\nhost={}\n\nhost\n", method, host);
        let string_to_sign = format!(
            "TC3-HMAC-SHA256\n{}\n{}/asr/tc3_request\n{}",
            timestamp,
            date,
            hex::encode(sha2::Sha256::digest(canonical_request.as_bytes()).as_slice())
        );

        let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())?;
        mac.update(date.as_bytes());
        let date_key = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&date_key)?;
        mac.update(b"asr");
        let service_key = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&service_key)?;
        mac.update(b"tc3_request");
        let request_key = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&request_key)?;
        mac.update(string_to_sign.as_bytes());
        let signature = mac.finalize().into_bytes();

        Ok(hex::encode(&signature))
    }
}

impl AsrClient for TencentCloudAsrClient {
    fn transcribe<'a>(
        &'a self,
        audio_data: &'a [i16],
        sample_rate: u32,
        config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // Clone values that we need in the async block
        let secret_id = config.secret_id.clone().unwrap_or_default();
        let secret_key = config.secret_key.clone().unwrap_or_default();
        let appid = config.appid.clone().unwrap_or_default();
        let engine_type = config
            .engine_type
            .clone()
            .unwrap_or_else(|| "16k_zh".to_string());
        let language = config
            .language
            .clone()
            .unwrap_or_else(|| "zh-CN".to_string());

        // Convert i16 audio samples to bytes
        let audio_bytes: Vec<u8> = audio_data
            .iter()
            .flat_map(|&sample| sample.to_le_bytes())
            .collect();

        Box::pin(async move {
            if secret_id.is_empty() || secret_key.is_empty() || appid.is_empty() {
                return Err(anyhow::anyhow!("Missing TencentCloud credentials"));
            }

            // Base64 encode the audio data
            let base64_audio = BASE64_STANDARD.encode(&audio_bytes);

            // Create request parameters
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

            let host = "asr.tencentcloudapi.com";
            let signature =
                self.generate_signature(&secret_id, &secret_key, host, "POST", timestamp)?;

            // Create request body
            let request_data = serde_json::json!({
                "ProjectId": appid,
                "SubServiceType": 2, // Real-time recognition
                "EngineType": engine_type,
                "ChannelNum": 1,
                "ResTextFormat": 0,
                "SourceType": 1, // Raw audio
                "VoiceFormat": "pcm",
                "SampleRate": sample_rate,
                "Data": base64_audio,
                "FilterDirty": 0,
                "FilterModal": 0,
                "FilterPunc": 0,
                "ConvertNumMode": 1,
                "WordInfo": 0
            });

            // Create authorization header
            let authorization = format!(
                "TC3-HMAC-SHA256 Credential={}/{}/asr/tc3_request, SignedHeaders=content-type;host, Signature={}",
                secret_id,
                chrono::Utc::now().format("%Y-%m-%d").to_string(),
                signature
            );

            // Make API request
            let response = self
                .http_client
                .post(format!("https://{}", host))
                .header("Authorization", authorization)
                .header("Content-Type", "application/json")
                .header("Host", host)
                .header("X-TC-Action", "SentenceRecognition")
                .header("X-TC-Version", "2019-06-14")
                .header("X-TC-Timestamp", timestamp.to_string())
                .header("X-TC-Language", language)
                .json(&request_data)
                .send()
                .await?;

            // Parse response
            if response.status().is_success() {
                let asr_response: TencentCloudAsrResponse = response.json().await?;
                Ok(asr_response.response.result)
            } else {
                let error_text = response.text().await?;
                Err(anyhow::anyhow!("ASR request failed: {}", error_text))
            }
        })
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
        }
    }

    // Set buffer duration in milliseconds
    pub fn with_buffer_duration(mut self, duration_ms: u32) -> Self {
        self.buffer_duration_ms = duration_ms;
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
        }
    }
}

// Include advanced tests in a separate module
#[cfg(test)]
mod tests;
