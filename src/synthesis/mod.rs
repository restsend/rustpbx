use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
// Add the new imports for TencentCloud client
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
// Add imports for event system
use crate::event::{EventSender, SessionEvent};

// Configuration for Text-to-Speech
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TtsConfig {
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

// TTS Events
#[derive(Debug, Clone, Serialize)]
pub struct TtsEvent {
    pub timestamp: u32,
    pub text: String,
}

// TTS client trait - to be implemented with actual TTS integration
pub trait TtsClient: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        config: &'a TtsConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

// Default HTTP TTS client implementation
pub struct HttpTtsClient {}

impl HttpTtsClient {
    pub fn new() -> Self {
        Self {}
    }
}

impl TtsClient for HttpTtsClient {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        _config: &'a TtsConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        // In a production implementation, this would call the TTS service
        // For demonstration purposes, we'll just simulate a response
        let text = text.to_string();
        Box::pin(async move {
            // Simulate TTS processing by returning dummy audio data
            // A real implementation would call an actual TTS API
            println!("Synthesizing text: {}", text);
            Ok(vec![0u8; 1024]) // Empty audio data for demonstration
        })
    }
}

// Google Cloud TTS Client
pub struct GoogleTtsClient {
    api_key: String,
}

impl GoogleTtsClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl TtsClient for GoogleTtsClient {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        config: &'a TtsConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        // In a production implementation, this would call Google Cloud Text-to-Speech API
        // For demonstration purposes, we'll just simulate a response
        let text = text.to_string();
        let voice = config
            .voice
            .clone()
            .unwrap_or_else(|| "en-US-Standard-A".to_string());
        Box::pin(async move {
            println!(
                "Synthesizing text with Google TTS, voice {}: {}",
                voice, text
            );
            Ok(vec![0u8; 1024]) // Empty audio data for demonstration
        })
    }
}

// TencentCloud TTS Client Implementation
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TencentCloudTtsResponse {
    #[serde(rename = "Response")]
    response: TencentCloudTtsResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TencentCloudTtsResult {
    #[serde(rename = "Audio")]
    audio: String,
    #[serde(rename = "SessionId")]
    session_id: String,
    #[serde(rename = "RequestId")]
    request_id: String,
}

pub struct TencentCloudTtsClient {
    http_client: HttpClient,
}

impl TencentCloudTtsClient {
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
        request_body: &str,
    ) -> Result<String> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Step 1: Build canonical request
        let canonical_headers = format!(
            "content-type:application/json\nhost:{}\nx-tc-action:texttovoice\n",
            host
        );
        let signed_headers = "content-type;host;x-tc-action";
        let hashed_request_payload =
            hex::encode(sha2::Sha256::digest(request_body.as_bytes()).as_slice());

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            "/",
            "", // canonical query string
            canonical_headers,
            signed_headers,
            hashed_request_payload
        );

        // Step 2: Build string to sign
        let credential_scope = format!("{}/tts/tc3_request", date);
        let hashed_canonical_request =
            hex::encode(sha2::Sha256::digest(canonical_request.as_bytes()).as_slice());

        let string_to_sign = format!(
            "TC3-HMAC-SHA256\n{}\n{}\n{}",
            timestamp, credential_scope, hashed_canonical_request
        );

        // Step 3: Calculate signature
        let tc3_secret = format!("TC3{}", secret_key);

        let mut mac = Hmac::<Sha256>::new_from_slice(tc3_secret.as_bytes())?;
        mac.update(date.as_bytes());
        let secret_date = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_date)?;
        mac.update(b"tts");
        let secret_service = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_service)?;
        mac.update(b"tc3_request");
        let secret_signing = mac.finalize().into_bytes();

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_signing)?;
        mac.update(string_to_sign.as_bytes());
        let signature = mac.finalize().into_bytes();

        Ok(hex::encode(&signature))
    }
}

impl TtsClient for TencentCloudTtsClient {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        config: &'a TtsConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        // Clone values that we need in the async block
        let secret_id = config.secret_id.clone().unwrap_or_default();
        let secret_key = config.secret_key.clone().unwrap_or_default();
        let appid = config
            .appid
            .clone()
            .unwrap_or_default()
            .parse::<i32>()
            .unwrap_or(0);
        let speaker = config.speaker.unwrap_or(1);
        let volume = config.volume.unwrap_or(0);
        let speed = config.rate.unwrap_or(0.0);
        let codec = config.codec.clone().unwrap_or_else(|| "mp3".to_string());
        let text = text.to_string();

        Box::pin(async move {
            if secret_id.is_empty() || secret_key.is_empty() {
                return Err(anyhow::anyhow!("Missing TencentCloud credentials"));
            }

            // Create session ID
            let session_id = Uuid::new_v4().to_string();

            // Create request parameters
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

            // Create request body
            let request_data = serde_json::json!({
                "Text": text,
                "SessionId": session_id,
                "Volume": volume,
                "Speed": speed,
                "ProjectId": 0,
                "ModelType": 1,
                "VoiceType": speaker,
                "PrimaryLanguage": 1,
                "SampleRate": 16000,
                "Codec": codec
            });

            let host = "tts.tencentcloudapi.com";
            let signature = self.generate_signature(
                &secret_id,
                &secret_key,
                host,
                "POST",
                timestamp,
                &request_data.to_string(),
            )?;

            // Create authorization header
            let authorization = format!(
                "TC3-HMAC-SHA256 Credential={}/{}/tts/tc3_request, SignedHeaders=content-type;host;x-tc-action, Signature={}",
                secret_id,
                date,
                signature
            );

            // Record request start time for TTFB measurement
            let request_start_time = std::time::Instant::now();

            // Send request to TencentCloud TTS API
            let response = self
                .http_client
                .post(format!("https://{}", host))
                .header("Content-Type", "application/json")
                .header("Authorization", authorization)
                .header("Host", host)
                .header("X-TC-Action", "TextToVoice")
                .header("X-TC-Version", "2019-08-23")
                .header("X-TC-Timestamp", timestamp.to_string())
                .header("X-TC-Region", "ap-guangzhou")
                .header("X-TC-AppId", appid.to_string())
                .json(&request_data)
                .send()
                .await?;

            // Calculate TTFB
            let ttfb = request_start_time.elapsed().as_millis() as u64;

            // Store TTFB metrics for later use
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;

            TTS_METRICS.with(|m| {
                *m.borrow_mut() = Some((
                    timestamp,
                    serde_json::json!({
                        "service": "tts",
                        "speaker": speaker,
                        "ttfb_ms": ttfb,
                    }),
                ));
            });

            // Print the status before consuming the response
            println!("TTS API Status: {}", response.status());

            // Parse the response
            let response_text = response.text().await?;

            // Print the full response for debugging
            println!("TTS API Raw Response: {}", response_text);

            let response: TencentCloudTtsResponse = match serde_json::from_str(&response_text) {
                Ok(resp) => resp,
                Err(e) => {
                    println!("Failed to parse TTS response: {}", e);
                    return Err(anyhow::anyhow!("Failed to parse TTS response: {}", e));
                }
            };

            // Decode the base64 audio data
            let audio_bytes = BASE64_STANDARD.decode(response.response.audio)?;

            Ok(audio_bytes)
        })
    }
}

// Thread local storage for TTS metrics
thread_local! {
    static TTS_METRICS: std::cell::RefCell<Option<(u32, serde_json::Value)>> = std::cell::RefCell::new(None);
}

// TTS Processor to integrate with the system
pub struct TtsProcessor {
    config: TtsConfig,
    client: Arc<dyn TtsClient>,
    event_sender: broadcast::Sender<TtsEvent>,
    // Add Session Event Sender for sending metrics
    session_event_sender: Option<EventSender>,
}

impl TtsProcessor {
    pub fn new(
        config: TtsConfig,
        client: Arc<dyn TtsClient>,
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

    // Process text input and generate speech
    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        // Generate audio using the TTS client
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        // Send initial processing event
        let _ = self.event_sender.send(TtsEvent {
            timestamp,
            text: text.to_string(),
        });

        // Generate audio
        let audio_data = self.client.synthesize(text, &self.config).await?;

        // Check if there are TTFB metrics to send
        TTS_METRICS.with(|metrics| {
            if let Some((ts, metrics_data)) = metrics.borrow_mut().take() {
                if let Some(sender) = &self.session_event_sender {
                    let _ = sender.send(SessionEvent::Metrics(ts, metrics_data.clone()));
                } else {
                    // Log if no sender
                    println!("TTS TTFB metrics: {:?}", metrics_data);
                }
            }
        });

        Ok(audio_data)
    }
}

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

// Include additional tests using mockall
#[cfg(test)]
mod tests;
