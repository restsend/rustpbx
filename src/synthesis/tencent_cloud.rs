use std::sync::Mutex;

use crate::synthesis::{Subtitle, SynthesisEvent, SynthesisEventReceiver, SynthesisEventSender};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{StreamExt, stream::BoxStream};
use ring::hmac;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
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
    subtitles: Option<Vec<TencentSubtitle>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TencentSubtitle {
    begin_time: u32,
    end_time: u32,
    begin_index: u32,
    end_index: u32,
}

impl From<&TencentSubtitle> for Subtitle {
    fn from(subtitle: &TencentSubtitle) -> Self {
        Subtitle::new(
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
    tx: SynthesisEventSender,
    rx: Mutex<Option<SynthesisEventReceiver>>,
}

impl TencentCloudTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            option,
            tx,
            rx: Mutex::new(Some(rx)),
        }
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
    async fn synthesize_text(&self, text: &str, option: Option<SynthesisOption>) -> Result<()> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let url = self.generate_websocket_url(text, &session_id, option)?;
        debug!(session_id, text, "connecting to WebSocket URL: {}", url);

        // Create a request with custom headers
        let request = url.into_client_request()?;

        // Connect to WebSocket with custom configuration
        let (mut ws_stream, resp) = connect_async_with_config(request, None, false).await?;
        match resp.status() {
            reqwest::StatusCode::SWITCHING_PROTOCOLS => (),
            _ => {
                info!(
                    session_id,
                    text,
                    "WebSocket connection failed: {}",
                    resp.status()
                );
                self.tx.send(Err(anyhow!(
                    "Failed to establish WebSocket connection: {}",
                    resp.status()
                )))?;
                return Err(anyhow::anyhow!(
                    "WebSocket connection failed: {}",
                    resp.status()
                ));
            }
        }

        while let Some(message) = ws_stream.next().await {
            let session_id = session_id.clone();
            match message {
                Ok(Message::Binary(data)) => {
                    self.tx
                        .send(Ok(SynthesisEvent::AudioChunk(data.to_vec())))?;
                }
                Ok(Message::Text(text)) => {
                    if let Ok(response) = serde_json::from_str::<WebSocketResponse>(&text) {
                        if response.code != 0 {
                            info!(
                                session_id,
                                code = response.code,
                                "Failed Tencent TTS response: {:?}",
                                response
                            );
                            match self.tx.send(Err(anyhow!("{}", response.message))) {
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(session_id, "Failed to send error: {}", e);
                                }
                            }
                            break;
                        }

                        if response.r#final == 1 {
                            break;
                        }

                        if let Some(subtitles) = response.result.subtitles {
                            let subtitles: Vec<Subtitle> =
                                subtitles.iter().map(Into::into).collect();
                            self.tx.send(Ok(SynthesisEvent::Subtitles(subtitles)))?;
                        }
                    } else {
                        warn!(session_id, " failed to deserialize response: {}", text);
                        self.tx.send(Err(anyhow!(
                            "Tencent TTS session: {} failed to deserialize response: {}",
                            session_id,
                            text
                        )))?;
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!(session_id, "websocket closed by remote");
                    self.tx.send(Err(anyhow!(
                        "Tencent TTS session: {} closed by remote",
                        session_id
                    )))?;
                    break;
                }
                Err(e) => {
                    self.tx.send(Err(anyhow!(
                        "Tencent TTS session: {} websocket error: {}",
                        session_id,
                        e
                    )))?;
                    warn!(session_id, "websocket error: {}", e);
                    return Err(anyhow!(
                        "Tencent TTS websocket error, Session: {}, error: {}",
                        session_id,
                        e
                    ));
                }
                _ => {}
            }
        }
        self.tx.send(Ok(SynthesisEvent::Finished))?;
        Ok(())
    }
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }
    async fn start(&self, _cancel_token: CancellationToken) -> Result<BoxStream<'static, Result<SynthesisEvent>>> {
        let rx = self.rx.lock().unwrap().take().ok_or_else(|| {
            anyhow!("TencentCloudTtsClient: Receiver already taken, cannot start new stream")
        })?;
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Some(event) => Some((event, rx)),
                None => None,
            }
        })))
    }
    async fn synthesize(
        &self,
        text: &str,
        _end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        self.synthesize_text(text, option).await
    }
}
