use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::{Subtitle, SynthesisEvent};
use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::Duration;
use futures::{
    SinkExt, Stream, StreamExt, future,
    stream::{self, BoxStream, SplitSink},
};
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{
    net::TcpStream,
    sync::{Notify, mpsc},
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::protocol::Message,
};
use tracing::{debug, warn};
use unic_emoji::char::is_emoji;
use urlencoding;
use uuid::Uuid;

const HOST: &str = "tts.cloud.tencent.com";
const NON_STREAMING_PATH: &str = "/stream_ws";
const STREAMING_PATH: &str = "/stream_wsv2";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

#[derive(Debug, Serialize)]
struct WebSocketRequest {
    session_id: String,
    message_id: String,
    action: String,
    data: String,
}

impl WebSocketRequest {
    fn synthesis_action(session_id: &str, text: &str) -> Self {
        let message_id = Uuid::new_v4().to_string();
        Self {
            session_id: session_id.to_string(),
            message_id,
            action: "ACTION_SYNTHESIS".to_string(),
            data: text.to_string(),
        }
    }

    fn complete_action(session_id: &str) -> Self {
        let message_id = Uuid::new_v4().to_string();
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
pub(super) struct TencentSubtitle {
    text: String,
    begin_time: u32,
    end_time: u32,
    begin_index: u32,
    end_index: u32,
}

impl From<&TencentSubtitle> for Subtitle {
    fn from(subtitle: &TencentSubtitle) -> Self {
        Subtitle::new(
            subtitle.text.clone(),
            subtitle.begin_time,
            subtitle.end_time,
            subtitle.begin_index,
            subtitle.end_index,
        )
    }
}

// tencent cloud will crash if text contains emoji
// Only remove non-ASCII emoji characters. Keep all ASCII (digits, letters, punctuation),
// since some ASCII (e.g., '0'..'9', '#', '*') are marked with the Unicode Emoji property
// due to keycap sequences but are safe and expected in text.
pub fn strip_emoji_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| c.is_ascii() || !is_emoji(c))
        .collect()
}

// construct request url
// for non-streaming client, text is Some
// session_id is used for tencent cloud tts service, not the session_id of media session
fn construct_request_url(option: &SynthesisOption, session_id: &str, text: Option<&str>) -> String {
    let streaming = text.is_none();
    let action = if !streaming {
        "TextToStreamAudioWS"
    } else {
        "TextToStreamAudioWSv2"
    };
    let secret_id = option.secret_id.clone().unwrap_or_default();
    let secret_key = option.secret_key.clone().unwrap_or_default();
    let app_id = option.app_id.clone().unwrap_or_default();
    let volume = option.volume.unwrap_or(0);
    let speed = option.speed.unwrap_or(0.0);
    let codec = option.codec.clone().unwrap_or_else(|| "pcm".to_string());
    let sample_rate = option.samplerate.unwrap_or(16000);
    let now = chrono::Utc::now();
    let timestamp = now.timestamp();
    let tomorrow = now + Duration::days(1);
    let expired = tomorrow.timestamp();
    let expired_str = expired.to_string();
    let sample_rate_str = sample_rate.to_string();
    let speed_str = speed.to_string();
    let timestamp_str = timestamp.to_string();
    let volume_str = volume.to_string();
    let voice_type = option
        .speaker
        .clone()
        .unwrap_or_else(|| "101001".to_string());
    let mut query_params = vec![
        ("Action", action),
        ("AppId", &app_id),
        ("SecretId", &secret_id),
        ("Timestamp", &timestamp_str),
        ("Expired", &expired_str),
        ("SessionId", &session_id),
        ("VoiceType", &voice_type),
        ("Volume", &volume_str),
        ("Speed", &speed_str),
        ("SampleRate", &sample_rate_str),
        ("Codec", &codec),
        ("EnableSubtitle", "true"),
    ];

    if let Some(text) = text {
        query_params.push(("Text", text));
    }

    // Sort query parameters by key
    query_params.sort_by(|a, b| a.0.cmp(b.0));

    // Build query string without URL encoding
    let query_string = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    let path = if streaming {
        STREAMING_PATH
    } else {
        NON_STREAMING_PATH
    };

    let string_to_sign = format!("GET{}{}?{}", HOST, path, query_string);
    // Calculate signature using HMAC-SHA1
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret_key.as_bytes());
    let tag = hmac::sign(&key, string_to_sign.as_bytes());
    let signature: String = BASE64_STANDARD.encode(tag.as_ref());
    // URL encode parameters for final URL
    let encoded_query_string = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!(
        "wss://{}{}?{}&Signature={}",
        HOST,
        path,
        encoded_query_string,
        urlencoding::encode(&signature)
    )
}

// convert websocket to event stream
// text and cmd_seq and cache key are used for non-streaming mode (realtime client)
// text is for debuging purpose
fn ws_to_event_stream<T>(
    ws_stream: T,
) -> impl Stream<Item = Result<SynthesisEvent>> + Send + 'static
where
    T: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
{
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();
    ws_stream
        .take_until(notify.notified_owned())
        .filter_map(move |message| {
            let notify = notify_clone.clone();
            async move {
                match message {
                    Ok(Message::Binary(data)) => Some(Ok(SynthesisEvent::AudioChunk(data))),
                    Ok(Message::Text(text)) => {
                        let response: WebSocketResponse =
                            serde_json::from_str(&text).expect("Tencent TTS API changed!");

                        if response.code != 0 {
                            notify.notify_one();
                            return Some(Err(anyhow::anyhow!(
                                "Tencent TTS error, code: {}, message: {}",
                                response.code,
                                response.message
                            )));
                        }

                        if response.heartbeat == 1 {
                            return None;
                        }

                        if let Some(subtitles) = response.result.subtitles {
                            let subtitles: Vec<Subtitle> =
                                subtitles.iter().map(Into::into).collect();
                            return Some(Ok(SynthesisEvent::Subtitles(subtitles)));
                        }

                        // final == 1 means the synthesis is finished, should close the websocket
                        if response.r#final == 1 {
                            notify.notify_one();
                            return Some(Ok(SynthesisEvent::Finished));
                        }

                        None
                    }
                    Ok(Message::Close(_)) => {
                        notify.notify_one();
                        warn!("Tencent TTS closed by remote");
                        None
                    }
                    Err(e) => {
                        notify.notify_one();
                        Some(Err(anyhow::anyhow!("Tencent TTS websocket error: {:?}", e)))
                    }
                    _ => None,
                }
            }
        })
}

// tencent cloud realtime tts client, non-streaming
// https://cloud.tencent.com/document/product/1073/94308
// each tts command have one websocket connection, with different session_id
pub struct RealTimeClient {
    option: SynthesisOption,
    //item: (text, option), drop tx if `end_of_stream`
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

impl RealTimeClient {
    fn new(option: SynthesisOption) -> Self {
        Self { option, tx: None }
    }
}

#[async_trait]
impl SynthesisClient for RealTimeClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        // Tencent cloud alow 10 - 20 concurrent websocket connections for default setting, dependent on voice type
        // set the number more higher will lead to waiting for unordered results longer
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let client_option = self.option.clone();
        let max_concurrent_tasks = client_option.max_concurrent_tasks.unwrap_or(1);
        let stream = UnboundedReceiverStream::new(rx)
            .flat_map_unordered(max_concurrent_tasks, move |(text, cmd_seq, option)| {
                // each reequest have its own session_id
                let session_id = Uuid::new_v4().to_string();
                let option = client_option.merge_with(option);
                let url = construct_request_url(&option, &session_id, Some(&text));
                stream::once(connect_async(url))
                    .flat_map(move |res| match res {
                        Ok((ws_stream, _)) => ws_to_event_stream(ws_stream).boxed(),
                        Err(e) => stream::once(future::ready(Err(e.into()))).boxed(),
                    })
                    .map(move |x| (cmd_seq, x))
                    .boxed()
            })
            .boxed();
        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(tx) = &self.tx {
            let text = strip_emoji_chars(text);
            tx.send((text, cmd_seq, option))?;
        } else {
            return Err(anyhow::anyhow!("TencentCloud TTS: missing client sender"));
        };

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}

// tencent cloud streaming tts client
// https://cloud.tencent.com/document/product/1073/108595
// all the tts commands with same play_id belong to same websocket connection
struct StreamingClient {
    session_id: String,
    option: SynthesisOption,
    sink: Option<WsSink>,
}

impl StreamingClient {
    pub fn new(option: SynthesisOption) -> Self {
        let session_id = Uuid::new_v4().to_string();
        Self {
            session_id,
            option,
            sink: None,
        }
    }
}

impl StreamingClient {
    async fn connect(&self) -> Result<WsStream> {
        let url = construct_request_url(&self.option, &self.session_id, None);
        let (mut ws_stream, _) = connect_async(url).await?;
        // waiting for ready = 1
        while let Some(message) = ws_stream.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let response = serde_json::from_str::<WebSocketResponse>(&text)?;
                    if response.ready == 1 {
                        debug!("TencentCloud TTS streaming client connected");
                        return Ok(ws_stream);
                    }

                    if response.code != 0 {
                        return Err(anyhow::anyhow!(
                            "TencentCloud TTS streaming client connecting failed: code: {}, message: {}",
                            response.code,
                            response.message
                        ));
                    }
                }
                Ok(Message::Close { .. }) => {
                    return Err(anyhow::anyhow!(
                        "TencentCloud TTS streaming client connecting failed: closed by remote"
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "TencentCloud TTS streaming client connecting failed: websocket error: {}",
                        e
                    ));
                }
                _ => {}
            }
        }

        Err(anyhow::anyhow!(
            "TencentCloud TTS streaming client connecting failed"
        ))
    }
}

#[async_trait]
impl SynthesisClient for StreamingClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::TencentCloud
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let stream = self.connect().await?;
        let (ws_sink, ws_stream) = stream.split();
        self.sink = Some(ws_sink);
        let stream = ws_to_event_stream(ws_stream)
            .map(move |event| (None, event))
            .boxed();
        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        _cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(sink) = &mut self.sink {
            let text = strip_emoji_chars(text);
            let request = WebSocketRequest::synthesis_action(&self.session_id, &text);
            let data = serde_json::to_string(&request)?;
            sink.send(Message::Text(data.into())).await?;

            Ok(())
        } else {
            Err(anyhow::anyhow!("TencentCloud TTS streaming: missing sink"))
        }
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(sink) = &mut self.sink {
            let request = WebSocketRequest::complete_action(&self.session_id);
            let data = serde_json::to_string(&request)?;
            sink.send(Message::Text(data.into())).await?;
        }

        Ok(())
    }
}

pub struct TencentCloudTtsClient;

impl TencentCloudTtsClient {
    pub fn create(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        if streaming {
            Ok(Box::new(StreamingClient::new(option.clone())))
        } else {
            Ok(Box::new(RealTimeClient::new(option.clone())))
        }
    }
}
