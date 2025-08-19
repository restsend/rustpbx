use std::sync::Mutex;

use crate::synthesis::{Subtitle, SynthesisEvent};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{
    SinkExt, StreamExt,
    stream::{BoxStream, SplitSink, SplitStream},
};
use ring::hmac;
use serde::{Deserialize, Serialize};
use tokio::{net::TcpStream, select, sync::mpsc};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tokio_util::sync::CancellationToken;
use urlencoding;
const HOST: &str = "tts.cloud.tencent.com";
const PATH: &str = "/stream_wsv2";

type WSSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WSStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

// tencent cloud streaming api: https://cloud.tencent.com/document/product/1073/108595
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
    result: WebSocketResult,
    ready: u32,
    r#final: u32,
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
pub struct TencentCloudStreamingTtsClient {
    option: SynthesisOption,
    session_id: String,
    message_id: String,
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl TencentCloudStreamingTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone()); // TODO: replace with a real token
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self {
            option,
            session_id: String::new(),
            message_id: String::new(),
            tx: Mutex::new(None),
        }
    }

    async fn connect(&self) -> Result<(WSSink, WSStream)> {
        let url = self.generate_websocket_url();
        let request = url.into_client_request()?;
        let (mut ws_stream, _resp) = connect_async(request).await?;
        while let Some(message) = ws_stream.next().await {
            if let Ok(Message::Text(text)) = message {
                let response = serde_json::from_str::<WebSocketResponse>(&text)?;
                if response.ready == 1 {
                    break;
                }
            }
        }

        Ok(ws_stream.split())
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
        let timestamp = chrono::Utc::now().timestamp() as u64;
        let expired = timestamp + 24 * 60 * 60; // 24 hours expiration

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

        format!(
            "wss://{}{}?{}&Signature={}",
            HOST,
            PATH,
            query_string,
            urlencoding::encode(&signature)
        )
    }
}

fn event_stream(ws_stream: WSStream) -> BoxStream<'static, Result<SynthesisEvent>> {
    let stream = ws_stream.filter_map(move |message| async move {
        match message {
            Ok(Message::Binary(data)) => Some(Ok(SynthesisEvent::AudioChunk(data.to_vec()))),
            Ok(Message::Text(text)) => {
                let response = serde_json::from_str::<WebSocketResponse>(&text).unwrap();
                if response.r#final == 1 {
                    return Some(Ok(SynthesisEvent::Finished));
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
            Ok(Message::Close(_)) => None,
            Err(e) => Some(Err(anyhow::anyhow!("Tencent TTS websocket error: {}", e))),
            _ => None,
        }
    });

    Box::pin(stream)
}

async fn text_sending_task(
    mut text_rc: mpsc::UnboundedReceiver<String>,
    session_id: String,
    message_id: String,
    mut ws_sink: WSSink,
    token: CancellationToken,
) -> Result<()> {
    loop {
        select! {
            text = text_rc.recv() => {
                match text {
                    Some(text) => {
                        let request = WebSocketRequest::synthesis_action(&session_id, &message_id, &text);
                        let data = serde_json::to_string(&request).unwrap();
                        ws_sink.send(Message::Text(data.into())).await?;
                    }
                    None => {
                        let request = WebSocketRequest::complete_action(&session_id, &message_id);
                        let data = serde_json::to_string(&request).unwrap();
                        ws_sink.send(Message::Text(data.into())).await?;
                        break;
                    }
                }
            }
            _ = token.cancelled() => {
                break;
            }
        }
    }
    Ok(())
}

#[async_trait]
impl SynthesisClient for TencentCloudStreamingTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloudStreaming
    }

    async fn start(
        &self,
        _cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<SynthesisEvent>>> {
        let (ws_sink, ws_stream) = self.connect().await?;
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut txx = self.tx.lock().unwrap();
            *txx = Some(tx);
        }
        let session_id = self.session_id.clone();
        let message_id = self.message_id.clone();
        tokio::spawn(text_sending_task(
            rx,
            session_id,
            message_id,
            ws_sink,
            CancellationToken::new(),
        ));
        let stream = event_stream(ws_stream);
        Ok(stream)
    }

    async fn synthesize(
        &self,
        text: &str,
        _end_of_stream: Option<bool>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        let mut sender = self.tx.lock().unwrap();

        if !text.is_empty() {
            let sender = sender
                .as_ref()
                .ok_or_else(|| anyhow!("Tencent Cloud Streaming tts: should call start first"))?;

            sender.send(text.to_string())?;
        }

        if let Some(true) = _end_of_stream {
            *sender = None;
        }

        Ok(())
    }
}
