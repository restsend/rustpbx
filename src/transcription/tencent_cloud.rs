use crate::media::processor::{AudioFrame, Processor};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use super::AsrClient;
use super::AsrConfig;

// TencentCloud ASR Response structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrResponse {
    #[serde(rename = "Response")]
    pub response: TencentCloudAsrResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrResult {
    #[serde(rename = "Result")]
    pub result: String,
    #[serde(rename = "RequestId")]
    pub request_id: String,
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
        request_body: &str,
    ) -> Result<String> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Step 1: Build canonical request
        let canonical_headers = format!(
            "content-type:application/json\nhost:{}\nx-tc-action:sentencerecognition\n",
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
        let credential_scope = format!("{}/asr/tc3_request", date);
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
        mac.update(b"asr");
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

            // Create session ID
            let session_id = Uuid::new_v4().to_string();

            // Create request parameters
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

            // Create request body
            let request_data = json!({
                "ProjectId": 0,
                "SubServiceType": 2,
                "EngSerViceType": config.engine_type,
                "VoiceFormat": "pcm",
                "UsrAudioKey": "test",
                "Data": base64::encode(&audio_bytes),
                "DataLen": audio_bytes.len(),
                "SourceType": 1,
                "FilterDirty": 0,
                "FilterModal": 0,
                "FilterPunc": 0,
                "ConvertNumMode": 1
            });

            let host = "asr.tencentcloudapi.com";
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
                "TC3-HMAC-SHA256 Credential={}/{}/asr/tc3_request, SignedHeaders=content-type;host;x-tc-action, Signature={}",
                secret_id,
                date,
                signature
            );

            // Record request start time for TTFB measurement
            let request_start_time = std::time::Instant::now();

            // Send request to TencentCloud ASR API
            let response = self
                .http_client
                .post(format!("https://{}", host))
                .header("Content-Type", "application/json")
                .header("Authorization", authorization)
                .header("Host", host)
                .header("X-TC-Action", "SentenceRecognition")
                .header("X-TC-Version", "2019-06-14")
                .header("X-TC-Timestamp", timestamp.to_string())
                .header("X-TC-Region", "ap-guangzhou")
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

            crate::transcription::ASR_METRICS.with(|m| {
                *m.borrow_mut() = Some((
                    timestamp,
                    serde_json::json!({
                        "service": "asr",
                        "engine_type": engine_type,
                        "ttfb_ms": ttfb,
                    }),
                ));
            });

            // Print the raw response for debugging
            let response_text = response.text().await?;
            println!("ASR API Response: {}", response_text);

            // Parse the response
            let response: TencentCloudAsrResponse = serde_json::from_str(&response_text)?;
            Ok(response.response.result)
        })
    }
}
