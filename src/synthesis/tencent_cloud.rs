use super::{SynthesisClient, SynthesisConfig};
use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures::{stream, Stream};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::pin::Pin;
use tracing::debug;
use uuid;

/// TencentCloud TTS Response structure
/// https://cloud.tencent.com/document/api/1073/37995
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudTtsResponse {
    #[serde(rename = "Response")]
    pub response: TencentCloudTtsResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Subtitle {
    #[serde(rename = "Text")]
    pub text: String,
    #[serde(rename = "BeginTime")]
    pub begin_time: u32,
    #[serde(rename = "EndTime")]
    pub end_time: u32,
    #[serde(rename = "BeginIndex")]
    pub begin_index: u32,
    #[serde(rename = "EndIndex")]
    pub end_index: u32,
    #[serde(rename = "Phoneme")]
    pub phoneme: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudTtsResult {
    #[serde(rename = "Audio")]
    pub audio: Option<String>,
    #[serde(rename = "SessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "RequestId")]
    pub request_id: String,
    #[serde(rename = "Subtitle")]
    pub subtitles: Option<Vec<Subtitle>>,
}

#[derive(Debug)]
pub struct TencentCloudTtsClient {
    config: SynthesisConfig,
}

impl TencentCloudTtsClient {
    pub fn new(config: SynthesisConfig) -> Self {
        Self { config }
    }

    // Build with specific configuration
    pub fn with_config(mut self, config: SynthesisConfig) -> Self {
        self.config = config;
        self
    }
    // Generate authentication signature for TencentCloud API
    fn generate_signature(
        &self,
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

    // Internal function to synthesize text to audio
    async fn synthesize_text(&self, text: &str) -> Result<Vec<u8>> {
        let secret_id = self.config.secret_id.clone().unwrap_or_default();
        let secret_key = self.config.secret_key.clone().unwrap_or_default();

        let volume = self.config.volume.unwrap_or(0);
        let speed = self.config.rate.unwrap_or(0.0);
        let codec = self
            .config
            .codec
            .clone()
            .unwrap_or_else(|| "pcm".to_string());

        let timestamp = chrono::Utc::now().timestamp() as u64;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut request_data = serde_json::json!({
            "Text": text,
            "Volume": volume,
            "Speed": speed,
            "ProjectId": 0,
            "ModelType": 1,
            "PrimaryLanguage": 1,
            "SampleRate": 16000,
            "Codec": codec,
            "SessionId": uuid::Uuid::new_v4().to_string()
        });

        if let Some(ref s) = self.config.speaker {
            if let Ok(v) = s.parse::<u32>() {
                request_data["VoiceType"] = v.into();
            }
        }
        if let Some(ref v) = self.config.subtitle {
            request_data["Subtitle"] = v.clone().into();
        }
        if let Some(ref v) = self.config.emotion {
            request_data["Emotion"] = v.clone().into();
        }
        let host = "tts.tencentcloudapi.com";
        let signature = self.generate_signature(
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
        debug!("Sending TTS request with data: {:?}", request_data);

        // Send request to TencentCloud TTS API
        let response = HttpClient::new()
            .post(format!("https://{}", host))
            .header("Content-Type", "application/json")
            .header("Authorization", authorization)
            .header("Host", host)
            .header("X-TC-Action", "TextToVoice")
            .header("X-TC-Version", "2019-08-23")
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("X-TC-Region", "ap-guangzhou")
            .json(&request_data)
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await?;
        debug!(
            "TTS API Response status: {}, body: {}",
            status, response_text
        );

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "TTS API request failed with status {}: {}",
                status,
                response_text
            ));
        }

        let response: TencentCloudTtsResponse =
            serde_json::from_str(&response_text).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse response: {}. Response text: {}",
                    e,
                    response_text
                )
            })?;

        // Check if audio field exists and handle it safely
        let audio = response.response.audio.ok_or_else(|| {
            anyhow::anyhow!(
                "No audio data in response. Full response: {}",
                response_text
            )
        })?;

        let audio_bytes = BASE64_STANDARD.decode(audio)?;
        let duration = request_start_time.elapsed().as_millis();
        debug!(
            "TencentCloud TTS response: {} bytes in {}ms, text: {} ",
            audio_bytes.len(),
            duration,
            text,
        );
        Ok(audio_bytes)
    }
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>> {
        let audio = self.synthesize_text(text).await?;
        Ok(Box::pin(stream::once(async move { Ok(audio) })))
    }
}
