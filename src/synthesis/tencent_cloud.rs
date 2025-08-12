use crate::synthesis::{TTSEvent, TTSSubtitle};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{StreamExt, stream::BoxStream};
use ring::hmac;
use serde::Deserialize;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tracing::debug;
use urlencoding;
use uuid;

const HOST: &str = "tts.cloud.tencent.com";
const PATH: &str = "/stream_ws";
/// TencentCloud TTS Response structure
/// https://cloud.tencent.com/document/product/1073/94308   
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WebSocketResponse {
    code: i32,
    message: String,
    session_id: String,
    request_id: String,
    message_id: String,
    r#final: i32,
    result: WebSocketResult,
}

#[derive(Debug, Deserialize)]
struct WebSocketResult {
    subtitles: Option<Vec<Subtitle>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Subtitle {
    text: String,
    begin_time: u32,
    end_time: u32,
    begin_index: u32,
    end_index: u32,
}

impl From<&Subtitle> for TTSSubtitle {
    fn from(subtitle: &Subtitle) -> Self {
        TTSSubtitle::new(
            &subtitle.text,
            subtitle.begin_time,
            subtitle.end_time,
            subtitle.begin_index,
            subtitle.end_index,
        )
    }
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
        session_id: &str,
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
    ) -> Result<BoxStream<'_, Result<TTSEvent>>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let url = self.generate_websocket_url(text, &session_id, option)?;
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

        let stream = ws_stream.filter_map(move |message| {
            let session_id = session_id.clone();
            async move {
                match message {
                    Ok(Message::Binary(data)) => Some(Ok(TTSEvent::AudioChunk(data.to_vec()))),
                    Ok(Message::Text(text)) => {
                        if let Ok(response) = serde_json::from_str::<WebSocketResponse>(&text) {
                            debug!(
                                "Tencent TTS session: {}, response: {:?}",
                                session_id, response
                            );

                            if response.code != 0 {
                                return Some(Err(anyhow!(
                                    "Tencent TTS faild, Session: {}, error: {}",
                                    session_id,
                                    response.message
                                )));
                            }

                            if response.r#final == 1 {
                                return Some(Ok(TTSEvent::Finished));
                            }

                            if let Some(subtitles) = response.result.subtitles {
                                let subtitles: Vec<TTSSubtitle> =
                                    subtitles.iter().map(Into::into).collect();
                                return Some(Ok(TTSEvent::Subtitles(subtitles)));
                            }

                            None
                        } else {
                            Some(Err(anyhow!(
                                "TTS Session: {} failed to deserialize {}",
                                session_id,
                                text
                            )))
                        }
                    }
                    Ok(Message::Close(_)) => {
                        return Some(Err(anyhow!(
                            "Tencent TTS session: {} closed by remote",
                            session_id
                        )));
                    }
                    Err(e) => Some(Err(anyhow!(
                        "Tencent TTS websocket error, Session: {}, error: {}",
                        session_id,
                        e
                    ))),
                    _ => None,
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }
    async fn synthesize(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<Result<TTSEvent>>> {
        // Use the new WebSocket streaming implementation
        self.synthesize_text_stream(text, option).await
    }
}
