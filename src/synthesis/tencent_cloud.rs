use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::{Subtitle, SynthesisEvent};
use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Duration;
use futures::{
    SinkExt, StreamExt,
    stream::{BoxStream, SplitSink, SplitStream},
};
use ring::hmac;
use serde::{Deserialize, Serialize};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use unic_emoji::char::is_emoji;
use unic_emoji::char::is_emoji_component;
use unic_emoji::char::is_emoji_modifier;
use unic_emoji::char::is_emoji_modifier_base;
use urlencoding;
use uuid::Uuid;

const HOST: &str = "tts.cloud.tencent.com";
const PATH: &str = "/stream_wsv2";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSource = SplitStream<WsStream>;
type WsSink = SplitSink<WsStream, Message>;
/// TencentCloud TTS Response structure
/// https://cloud.tencent.com/document/product/1073/94308   

#[derive(Debug, Serialize)]
struct WebSocketRequest {
    session_id: String,
    message_id: String,
    action: String,
    data: String,
}

impl WebSocketRequest {
    fn synthesis_action(session_id: &str, message_id: &str, text: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            action: "ACTION_SYNTHESIS".to_string(),
            data: text.to_string(),
        }
    }

    fn complete_action(session_id: &str, message_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            action: "ACTION_COMPLETE".to_string(),
            data: "".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WebSocketResponse {
    code: i32,
    message: String,
    r#final: i32,
    result: WebSocketResult,
    ready: u32,
    heartbeat: u32,
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
    session_id: String,
    message_id: String,
    sink: Mutex<Option<WsSink>>,
}

impl TencentCloudTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self {
            option,
            session_id: Uuid::new_v4().into(),
            message_id: Uuid::new_v4().into(),
            sink: Mutex::new(None),
        }
    }

    async fn connect(&self) -> Result<WsStream> {
        let url = self.generate_websocket_url();
        let request = url.into_client_request()?;
        let (mut ws_stream, _) = connect_async(request).await?;
        while let Some(message) = ws_stream.next().await {
            if let Ok(Message::Text(text)) = message {
                let response = serde_json::from_str::<WebSocketResponse>(&text)?;
                if response.ready == 1 {
                    debug!("TencentCloud TTS: connected");
                    break;
                }

                if response.code != 0 {
                    return Err(anyhow::anyhow!(
                        "TencentCloud TTS: failed, code: {}, message: {}",
                        response.code,
                        response.message
                    ));
                }
            }
        }

        Ok(ws_stream)
    }

    fn generate_websocket_url(&self) -> String {
        let secret_id = self.option.secret_id.clone().unwrap_or_default();
        let secret_key = self.option.secret_key.clone().unwrap_or_default();
        let app_id = self.option.app_id.clone().unwrap_or_default();
        let volume = self.option.volume.unwrap_or(0);
        let speed = self.option.speed.unwrap_or(0.0);
        let codec = self
            .option
            .codec
            .clone()
            .unwrap_or_else(|| "pcm".to_string());
        let sample_rate = self.option.samplerate.unwrap_or(16000);
        let now = chrono::Utc::now();
        let timestamp = now.timestamp();
        let tomorrow = now + Duration::days(1);
        let expired = tomorrow.timestamp();
        let expired_str = expired.to_string();
        let sample_rate_str = sample_rate.to_string();
        let speed_str = speed.to_string();
        let timestamp_str = timestamp.to_string();
        let volume_str = volume.to_string();
        let voice_type = self
            .option
            .speaker
            .clone()
            .unwrap_or_else(|| "101001".to_string());
        let mut query_params = vec![
            ("Action", "TextToStreamAudioWSv2"),
            ("AppId", &app_id),
            ("SecretId", &secret_id),
            ("Timestamp", &timestamp_str),
            ("Expired", &expired_str),
            ("SessionId", &self.session_id),
            ("VoiceType", &voice_type),
            ("Volume", &volume_str),
            ("Speed", &speed_str),
            ("SampleRate", &sample_rate_str),
            ("Codec", &codec),
            ("EnableSubtitle", "true"),
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

        format!(
            "wss://{}{}?{}&Signature={}",
            HOST,
            PATH,
            encoded_query_string,
            urlencoding::encode(&signature)
        )
    }
}

fn event_stream(ws_stream: WsSource) -> BoxStream<'static, Result<SynthesisEvent>> {
    let stream = ws_stream.filter_map(move |message| async move {
        match message {
            Ok(Message::Binary(data)) => Some(Ok(SynthesisEvent::AudioChunk(data.to_vec()))),
            Ok(Message::Text(text)) => {
                let response =
                    if let Ok(response) = serde_json::from_str::<WebSocketResponse>(&text) {
                        response
                    } else {
                        return Some(Err(anyhow::anyhow!(
                            "Tencent TTS invalid response: {}",
                            text
                        )));
                    };

                if response.r#final == 1 {
                    return Some(Ok(SynthesisEvent::Finished {
                        end_of_stream: Some(true),
                        cache_key: None,
                    }));
                }

                if response.code != 0 {
                    return Some(Err(anyhow::anyhow!(
                        "Tencent TTS failed, code: {}, message: {}",
                        response.code,
                        response.message
                    )));
                }

                if response.heartbeat == 1 {
                    return None;
                }

                if let Some(subtitles) = response.result.subtitles {
                    let subtitles: Vec<Subtitle> = subtitles.iter().map(Into::into).collect();
                    return Some(Ok(SynthesisEvent::Subtitles(subtitles)));
                }

                None
            }
            Ok(Message::Close(_)) => {
                Some(Err(anyhow::anyhow!("Tencent TTS closed by server")))
            }
            Err(e) => Some(Err(anyhow::anyhow!("Tencent TTS websocket error: {}", e))),
            _ => None,
        }
    });

    Box::pin(stream)
}

#[async_trait]
impl SynthesisClient for TencentCloudTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }

    async fn start(
        &self,
        _cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<SynthesisEvent>>> {
        let stream = self.connect().await?;
        let (ws_sink, ws_stream) = stream.split();
        *self.sink.lock().await = Some(ws_sink);
        Ok(event_stream(ws_stream))
    }

    async fn synthesize(
        &self,
        text: &str,
        end_of_stream: Option<bool>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        match self.sink.lock().await.as_mut() {
            Some(sink) => {
                let text = remove_emoji(text);

                if !text.is_empty() {
                    let request = WebSocketRequest::synthesis_action(
                        &self.session_id,
                        &self.message_id,
                        &text,
                    );
                    let data = serde_json::to_string(&request)?;
                    sink.send(Message::Text(data.into())).await?;
                }

                if let Some(true) = end_of_stream {
                    let request =
                        WebSocketRequest::complete_action(&self.session_id, &self.message_id);
                    let data = serde_json::to_string(&request)?;
                    sink.send(Message::Text(data.into())).await?;
                }
                Ok(())
            }
            None => Err(anyhow::anyhow!("should call start first")),
        }
    }
}

// tencent cloud will crash if text contains emoji
fn remove_emoji(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !(is_emoji(*c)
                || is_emoji_component(*c)
                || is_emoji_modifier(*c)
                || is_emoji_modifier_base(*c))
        })
        .collect()
}
