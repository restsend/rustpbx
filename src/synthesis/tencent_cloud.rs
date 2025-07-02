use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::{stream, Stream, StreamExt};
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tracing::{debug, warn};
use urlencoding;
use uuid;

const HOST: &str = "tts.cloud.tencent.com";
const PATH: &str = "/stream_ws";
/// TencentCloud TTS Response structure
/// https://cloud.tencent.com/document/product/1073/94308   
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

/// WebSocket response structure for real-time TTS
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketResponse {
    #[serde(rename = "code")]
    pub code: i32,
    #[serde(rename = "message")]
    pub message: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    #[serde(rename = "request_id")]
    pub request_id: String,
    #[serde(rename = "message_id")]
    pub message_id: String,
    #[serde(rename = "final")]
    pub final_: i32,
    #[serde(rename = "result")]
    pub result: WebSocketResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSocketResult {
    #[serde(rename = "subtitles")]
    pub subtitles: Option<Vec<Subtitle>>,
}

#[derive(Debug)]
pub struct TencentCloudTtsClient {
    option: SynthesisOption,
}

impl TencentCloudTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self { option }
    }

    // Build with specific configuration
    pub fn with_option(mut self, option: SynthesisOption) -> Self {
        self.option = option;
        self
    }

    // Generate WebSocket URL for real-time TTS
    fn generate_websocket_url(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<String> {
        let option = self.option.merge_with(option);
        let secret_id = option.secret_id.clone().unwrap_or_default();
        let secret_key = option.secret_key.clone().unwrap_or_default();
        let app_id = option.app_id.clone().unwrap_or_default();

        let volume = option.volume.unwrap_or(0);
        let speed = option.speed.unwrap_or(0.0);
        let codec = option.codec.clone().unwrap_or_else(|| "pcm".to_string());
        let sample_rate = option.samplerate.unwrap_or(16000);
        let session_id = uuid::Uuid::new_v4().to_string();
        let timestamp = chrono::Utc::now().timestamp() as u64;
        let expired = timestamp + 24 * 60 * 60; // 24 hours expiration

        let expired_str = expired.to_string();
        let sample_rate_str = sample_rate.to_string();
        let speed_str = speed.to_string();
        let timestamp_str = timestamp.to_string();
        let volume_str = volume.to_string();
        let voice_type = option
            .speaker
            .clone()
            .unwrap_or_else(|| "601000".to_string());
        let mut query_params = vec![
            ("Action", "TextToStreamAudioWS"),
            ("AppId", app_id.as_str()),
            ("Codec", codec.as_str()),
            ("EnableSubtitle", "true"),
            ("Expired", &expired_str),
            ("SampleRate", &sample_rate_str),
            ("SecretId", secret_id.as_str()),
            ("SessionId", &session_id),
            ("Speed", &speed_str),
            ("Text", text),
            ("Timestamp", &timestamp_str),
            ("VoiceType", &voice_type),
            ("Volume", &volume_str),
        ];

        // Sort query parameters by key
        query_params.sort_by(|a, b| a.0.cmp(b.0));

        // Build query string without URL encoding
        let query_string = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let string_to_sign = format!("GET{}{}?{}", HOST, PATH, query_string);

        // Calculate signature using HMAC-SHA1
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret_key.as_bytes());
        let tag = hmac::sign(&key, string_to_sign.as_bytes());
        let signature = STANDARD.encode(tag.as_ref());

        // URL encode parameters for final URL
        let encoded_query_string = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // Build final WebSocket URL
        let url = format!(
            "wss://{}{}?{}&Signature={}",
            HOST,
            PATH,
            encoded_query_string,
            urlencoding::encode(&signature)
        );
        Ok(url)
    }

    // Internal function to synthesize text to audio using WebSocket
    async fn synthesize_text_stream(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        let url = self.generate_websocket_url(text, option)?;
        debug!("connecting to WebSocket URL: {}", url);

        // Create a request with custom headers
        let request = url.into_client_request()?;

        // Connect to WebSocket with custom configuration
        let (ws_stream, resp) = connect_async_with_config(request, None, false).await?;
        match resp.status() {
            reqwest::StatusCode::SWITCHING_PROTOCOLS => (),
            _ => {
                return Err(anyhow::anyhow!(
                    "WebSocket connection failed: {}",
                    resp.status()
                ));
            }
        }

        // Create a stream that will yield audio chunks
        let stream = Box::pin(stream::unfold(
            (ws_stream, false),
            move |(mut ws_stream, is_final)| async move {
                // If we've received the final message, end the stream
                if is_final {
                    return None;
                }

                // Receive message from WebSocket
                match ws_stream.next().await {
                    Some(Ok(Message::Binary(data))) => {
                        // Binary data is audio
                        Some((Ok(data.to_vec()), (ws_stream, false)))
                    }
                    Some(Ok(Message::Text(text))) => {
                        // Text data is metadata
                        match serde_json::from_str::<WebSocketResponse>(&text) {
                            Ok(response) => {
                                if response.code != 0 {
                                    return Some((
                                        Err(anyhow::anyhow!(
                                            "WebSocket error: {}",
                                            response.message
                                        )),
                                        (ws_stream, true),
                                    ));
                                }
                                // Check if this is the final message
                                if response.final_ == 1 {
                                    Some((Ok(Vec::new()), (ws_stream, true)))
                                } else {
                                    // Continue receiving data
                                    Some((Ok(Vec::new()), (ws_stream, false)))
                                }
                            }
                            Err(e) => {
                                warn!("failed to parse WebSocket response: {}", e);
                                Some((Ok(Vec::new()), (ws_stream, false)))
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        // Connection closed
                        None
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket error: {:?}", e);
                        // Error occurred
                        Some((
                            Err(anyhow::anyhow!("WebSocket error: {}", e)),
                            (ws_stream, true),
                        ))
                    }
                    None => {
                        // Stream ended
                        None
                    }
                    _ => {
                        // Ignore other message types
                        Some((Ok(Vec::new()), (ws_stream, false)))
                    }
                }
            },
        ));

        Ok(stream)
    }
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }
    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>> {
        // Use the new WebSocket streaming implementation
        self.synthesize_text_stream(text, option).await
    }
}
