use crate::media::processor::{AudioFrame, Processor};
use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::TranscriptionClient;
use super::TranscriptionConfig;

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

/// Tencent Cloud ASR (Automatic Speech Recognition) Client
///
/// This implementation handles sending audio data to Tencent Cloud's ASR service
/// and processing the results.
#[derive(Debug)]
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

#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    async fn transcribe(
        &self,
        audio_data: &[i16],
        sample_rate: u32,
        config: &TranscriptionConfig,
    ) -> Result<Option<String>> {
        // Clone values that we need in the async block
        let secret_id = config.secret_id.clone().unwrap_or_default();
        let secret_key = config.secret_key.clone().unwrap_or_default();
        let appid = config.appid.clone().unwrap_or_default();
        let engine_type = config
            .engine_type
            .clone()
            .unwrap_or_else(|| "16k_zh".to_string());
        // We don't use language parameter since it's not recognized by Tencent Cloud API
        // Using engine_type parameter is sufficient

        // Log input
        debug!(
            "ASR transcribe called with {} audio samples, sample rate: {}",
            audio_data.len(),
            sample_rate
        );

        // Convert i16 audio samples to bytes
        let audio_bytes: Vec<u8> = audio_data
            .iter()
            .flat_map(|&sample| sample.to_le_bytes())
            .collect();

        if secret_id.is_empty() || secret_key.is_empty() || appid.is_empty() {
            error!("Missing TencentCloud credentials");
            return Err(anyhow::anyhow!("Missing TencentCloud credentials"));
        }

        let session_id = Uuid::new_v4().to_string();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Create request body
        let request_data = json!({
            "ProjectId": 0,
            "SubServiceType": 2,
            "EngSerViceType": engine_type,
            "VoiceFormat": "pcm",
            "UsrAudioKey": session_id,
            "Data": BASE64_STANDARD.encode(&audio_bytes),
            "DataLen": audio_bytes.len(),
            "SourceType": 1,
            "FilterDirty": 0,
            "FilterModal": 0,
            "FilterPunc": 0,
            "ConvertNumMode": 1,
            // "Language" parameter is not recognized by the API
        });

        debug!(
            "ASR request data: {}",
            serde_json::to_string_pretty(&request_data).unwrap_or_default()
        );

        let host = "asr.tencentcloudapi.com";
        let signature = match self.generate_signature(
            &secret_id,
            &secret_key,
            host,
            "POST",
            timestamp,
            &request_data.to_string(),
        ) {
            Ok(sig) => sig,
            Err(e) => {
                error!("Failed to generate signature: {}", e);
                return Err(e);
            }
        };

        let authorization = format!(
                "TC3-HMAC-SHA256 Credential={}/{}/asr/tc3_request, SignedHeaders=content-type;host;x-tc-action, Signature={}",
                secret_id,
                date,
                signature
            );

        // Record request start time for TTFB measurement
        let request_start_time = std::time::Instant::now();
        // Send request to TencentCloud ASR API
        let response = match self
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
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to send request to TencentCloud ASR API: {}", e);
                return Err(anyhow::anyhow!("Failed to send request: {}", e));
            }
        };

        // Calculate TTFB
        let ttfb = request_start_time.elapsed().as_millis() as u64;
        debug!("ASR API response TTFB: {} ms", ttfb);

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
        let response_text = match response.text().await {
            Ok(text) => text,
            Err(e) => {
                error!("Failed to get response text: {}", e);
                return Err(anyhow::anyhow!("Failed to get response text: {}", e));
            }
        };
        info!("ASR API Raw Response: {}", response_text);

        // Try to parse it as a regular JSON object to see what we're receiving
        match serde_json::from_str::<serde_json::Value>(&response_text) {
            Ok(json_value) => {
                info!("Parsed response as JSON: {:?}", json_value);

                // Check if the response has an error field
                if let Some(error) = json_value.get("Error") {
                    error!("Tencent Cloud API returned an error: {:?}", error);
                    return Err(anyhow::anyhow!("API Error: {:?}", error));
                }

                // Try to extract the result directly if the regular parsing fails
                if let Some(result) = json_value
                    .get("Response")
                    .and_then(|resp| resp.get("Result"))
                    .and_then(|res| res.as_str())
                {
                    info!("Extracted ASR result: {}", result);
                    return Ok(Some(result.to_string()));
                }
            }
            Err(e) => {
                error!("Failed to parse response as JSON: {}", e);
                // Continue to try the original parsing method
            }
        }

        // Parse the response using our struct
        let response: TencentCloudAsrResponse = match serde_json::from_str(&response_text) {
            Ok(resp) => resp,
            Err(e) => {
                error!("Failed to parse response as TencentCloudAsrResponse: {}", e);
                return Err(anyhow::anyhow!("Failed to parse response: {}", e));
            }
        };

        if !response.response.result.is_empty() {
            // Log the result
            info!("ASR result: {}", response.response.result);
            Ok(Some(response.response.result))
        } else {
            Ok(None)
        }
    }
}
