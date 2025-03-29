use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use super::SynthesisClient;
use super::SynthesisConfig;

// TencentCloud TTS Response structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudTtsResponse {
    #[serde(rename = "Response")]
    pub response: TencentCloudTtsResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudTtsResult {
    #[serde(rename = "Audio")]
    pub audio: String,
    #[serde(rename = "SessionId")]
    pub session_id: String,
    #[serde(rename = "RequestId")]
    pub request_id: String,
}

#[derive(Debug)]
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

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    async fn synthesize(&self, text: &str, config: &SynthesisConfig) -> Result<Vec<u8>> {
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
            .json(&request_data)
            .send()
            .await?;

        // Calculate TTFB
        let ttfb = request_start_time.elapsed().as_millis() as u64;

        // Store TTS metrics for monitoring
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        // Print the raw response for debugging
        let response_text = response.text().await?;
        // Parse the response
        let response: TencentCloudTtsResponse = serde_json::from_str(&response_text)?;

        // Decode base64 audio data
        let audio_bytes = BASE64_STANDARD.decode(response.response.audio)?;
        Ok(audio_bytes)
    }
}
