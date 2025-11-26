use super::{TranscriptionClient, TranscriptionOption, handle_wait_for_answer_with_audio_drop};
use crate::{
    event::{EventSender, SessionEvent},
    media::Sample,
    media::TrackId,
    media::codecs,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Aliyun DashScope Paraformer real-time speech recognition API
/// https://help.aliyun.com/zh/model-studio/paraformer-real-time-speech-recognition-api/
struct AliyunAsrClientInner {
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    option: TranscriptionOption,
}

pub struct AliyunAsrClient {
    inner: Arc<AliyunAsrClientInner>,
}

#[derive(Debug, Serialize)]
struct RunTaskCommand {
    header: CommandHeader,
    payload: RunTaskPayload,
}

impl RunTaskCommand {
    pub fn new(task_id: String, option: &TranscriptionOption) -> Self {
        let model_type = option
            .model_type
            .clone()
            .unwrap_or("paraformer-realtime-v2".to_string());
        let format = "pcm";
        let sample_rate = option.samplerate.unwrap_or(16000);
        let vocabulary_id = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("vocabulary_id"))
            .cloned();
        let disfluency_removal_enabled = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("disfluency_removal_enabled"))
            .and_then(|v| v.parse::<bool>().ok());
        let mut language_hints = Vec::new();
        if let Some(language) = option.language.clone() {
            language_hints.push(language);
        }
        let semantic_punctuation_enabled = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("semantic_punctuation_enabled"))
            .and_then(|v| v.parse::<bool>().ok());
        let max_sentence_silence = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("max_sentence_silence"))
            .and_then(|v| v.parse::<u32>().ok());

        let multi_threshold_mode_enabled = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("multi_threshold_mode_enabled"))
            .and_then(|v| v.parse::<bool>().ok());

        let punctuation_prediction_enabled = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("punctuation_prediction_enabled"))
            .and_then(|v| v.parse::<bool>().ok());

        let heartbeat = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("heartbeat"))
            .and_then(|v| v.parse::<bool>().ok());

        let inverse_text_normalization_enabled = option
            .extra
            .as_ref()
            .and_then(|extra| extra.get("inverse_text_normalization_enabled"))
            .and_then(|v| v.parse::<bool>().ok());

        Self {
            header: CommandHeader {
                action: "run-task".to_string(),
                task_id,
                streaming: "duplex".to_string(),
            },
            payload: RunTaskPayload {
                task_group: "audio".to_string(),
                task: "asr".to_string(),
                function: "recognition".to_string(),
                model: model_type,
                input: CommandInput {},
                parameters: RunTaskCommandParameters {
                    format: format.to_string(),
                    sample_rate,
                    vocabulary_id,
                    disfluency_removal_enabled,
                    language_hints,
                    semantic_punctuation_enabled,
                    max_sentence_silence,
                    multi_threshold_mode_enabled,
                    punctuation_prediction_enabled,
                    heartbeat,
                    inverse_text_normalization_enabled,
                },
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FinishTaskCommand {
    pub header: CommandHeader,
    pub payload: FinishTaskPayload,
}

impl FinishTaskCommand {
    fn new(task_id: String) -> Self {
        Self {
            header: CommandHeader {
                action: "finish-task".to_string(),
                task_id,
                streaming: "duplex".to_string(),
            },
            payload: FinishTaskPayload {
                input: CommandInput {},
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CommandHeader {
    pub action: String,
    pub task_id: String,
    pub streaming: String,
}

#[derive(Debug, Serialize)]
struct RunTaskPayload {
    task_group: String,
    task: String,
    function: String,
    model: String,
    input: CommandInput,
    parameters: RunTaskCommandParameters,
}

#[skip_serializing_none]
#[derive(Debug, Serialize)]
struct RunTaskCommandParameters {
    format: String,
    sample_rate: u32,
    vocabulary_id: Option<String>,
    disfluency_removal_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    language_hints: Vec<String>,
    semantic_punctuation_enabled: Option<bool>,
    max_sentence_silence: Option<u32>, // 200ms - 600ms,  works only when semantic_punctuation_enabled is false
    multi_threshold_mode_enabled: Option<bool>, // prevent vad cutted sentences too long, works only when semantic_punctuation_enabled is false
    punctuation_prediction_enabled: Option<bool>,
    heartbeat: Option<bool>,
    inverse_text_normalization_enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct CommandInput {}

#[derive(Debug, Serialize)]
pub struct FinishTaskPayload {
    pub input: CommandInput,
}

#[derive(Debug, Deserialize)]
pub struct AsrEvent {
    pub header: EventHeader,
    pub payload: Option<EventPayload>,
}

#[derive(Debug, Deserialize)]
pub struct EventHeader {
    pub task_id: String,
    pub event: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventPayload {
    pub output: Option<EventOutput>,
}

#[derive(Debug, Deserialize)]
pub struct EventOutput {
    pub sentence: OutputSentence,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct OutputSentence {
    pub sentence_id: u32,
    pub begin_time: u32,
    pub end_time: Option<u32>,
    pub text: String,
    pub words: Vec<OutputWord>,
    pub heartbeat: Option<bool>,
    pub sentence_end: bool,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct OutputWord {
    pub begin_time: u32,
    pub end_time: u32,
    pub text: String,
    pub punctuation: String,
}

impl AliyunAsrClientInner {
    // Establish WebSocket connection to Aliyun DashScope ASR service
    async fn connect_websocket(
        &self,
        voice_id: &String,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let api_key = self
            .option
            .secret_key
            .as_deref()
            .ok_or_else(|| anyhow!("No DASHSCOPE_API_KEY provided"))?;

        let ws_url = self
            .option
            .endpoint
            .as_deref()
            .unwrap_or("wss://dashscope.aliyuncs.com/api-ws/v1/inference");

        let mut request = ws_url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
        headers.insert("X-DashScope-DataInspection", "enable".parse()?);
        let (ws_stream, response) = connect_async(request).await?;
        debug!(
            voice_id,
            "WebSocket connection established. Response: {}",
            response.status()
        );
        match response.status() {
            StatusCode::SWITCHING_PROTOCOLS => Ok(ws_stream),
            _ => Err(anyhow!("Failed to connect to WebSocket: {:?}", response)),
        }
    }
}

/// Context for websocket message handling to reduce function arguments
struct WebSocketContext {
    track_id: TrackId,
    option: TranscriptionOption,
}

impl AliyunAsrClient {
    async fn handle_websocket_message(
        ctx: WebSocketContext,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        event_sender: EventSender,
        token: CancellationToken,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let begin_time = crate::media::get_timestamp();
        let start_msg = RunTaskCommand::new(ctx.track_id.clone(), &ctx.option);

        if let Ok(msg_json) = serde_json::to_string(&start_msg) {
            if let Err(e) = ws_sender.send(Message::Text(msg_json.into())).await {
                warn!(ctx.track_id, "Failed to send start message: {}", e);
                return Err(anyhow!("Failed to send start message: {}", e));
            }
        }

        let track_id_clone = ctx.track_id.clone();
        let recv_loop = async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => match serde_json::from_str::<AsrEvent>(&text) {
                        Ok(response) => {
                            let task_id = response.header.task_id.clone();
                            debug!(ctx.track_id, task_id, "Task: {}", response.header.event);
                            if response.header.event == "task-failed" {
                                let code = response.header.error_code.expect("mising_code");
                                let message =
                                    response.header.error_message.expect("mising error message");
                                let error =
                                    format!("Error from ASR service: {} ({})", code, message);
                                warn!("{}", error);
                                return Err(anyhow!(error));
                            }
                            if response.header.event == "task-finished" {
                                break;
                            }

                            if response.header.event == "task-started" {
                                continue;
                            }

                            let payload = response.payload.ok_or(anyhow!("missing payload"))?;
                            let output = payload.output.ok_or(anyhow!("missing output"))?;

                            if output.sentence.heartbeat.unwrap_or(false) {
                                continue;
                            }

                            let sentence = output.sentence;
                            let words = sentence.words;
                            let sentence_start_time = begin_time + sentence.begin_time as u64;
                            let sentence_end_time = sentence_start_time
                                + words.last().map(|w| w.end_time as u64).unwrap_or(0);
                            let text = sentence.text;

                            let event = if sentence.sentence_end {
                                SessionEvent::AsrFinal {
                                    track_id: task_id.clone(),
                                    index: sentence.sentence_id,
                                    text,
                                    timestamp: crate::media::get_timestamp(),
                                    start_time: Some(sentence_start_time),
                                    end_time: Some(sentence_end_time),
                                }
                            } else {
                                SessionEvent::AsrDelta {
                                    track_id: task_id.clone(),
                                    index: sentence.sentence_id,
                                    text,
                                    timestamp: crate::media::get_timestamp(),
                                    start_time: Some(sentence_start_time),
                                    end_time: Some(sentence_end_time),
                                }
                            };
                            event_sender.send(event).ok();

                            let diff_time = (crate::media::get_timestamp() - begin_time) as u32;
                            let metrics_event = if sentence.sentence_end {
                                SessionEvent::Metrics {
                                    timestamp: crate::media::get_timestamp(),
                                    key: "completed.asr.aliyun".to_string(),
                                    data: serde_json::json!({
                                        "sentence_id": sentence.sentence_id,
                                    }),
                                    duration: diff_time,
                                }
                            } else {
                                SessionEvent::Metrics {
                                    timestamp: crate::media::get_timestamp(),
                                    key: "ttfb.asr.aliyun".to_string(),
                                    data: serde_json::json!({
                                        "sentence_id": sentence.sentence_id,
                                    }),
                                    duration: diff_time,
                                }
                            };
                            event_sender.send(metrics_event).ok();
                        }
                        Err(e) => {
                            warn!(ctx.track_id, "Failed to parse ASR response: {}", e);
                        }
                    },
                    Ok(Message::Close(_)) => {
                        info!(ctx.track_id, "WebSocket connection closed by server");
                        break;
                    }
                    Err(e) => {
                        warn!(ctx.track_id, "WebSocket error: {}", e);
                        return Err(anyhow!("WebSocket error: {}", e));
                    }
                    _ => {
                        debug!("Received non-text message");
                    }
                }
            }
            Ok(())
        };

        let token_clone = token.clone();
        let send_loop = async move {
            while let Some(audio_data) = audio_rx.recv().await {
                if token_clone.is_cancelled() {
                    break;
                }

                if let Err(e) = ws_sender.send(Message::Binary(audio_data.into())).await {
                    warn!("Failed to send audio data: {}", e);
                    break;
                }
            }

            let end_msg = FinishTaskCommand::new(track_id_clone);

            if let Ok(msg_json) = serde_json::to_string(&end_msg) {
                if let Err(e) = ws_sender.send(Message::Text(msg_json.into())).await {
                    warn!("Failed to send end message: {}", e);
                }
            }

            Ok(())
        };

        tokio::select! {
            result = recv_loop => result,
            result = send_loop => result,
            _ = token.cancelled() => {
                Ok(())
            }
        }
    }
}

pub struct AliyunAsrClientBuilder {
    option: TranscriptionOption,
    track_id: Option<String>,
    token: Option<CancellationToken>,
    event_sender: EventSender,
}

/// Type alias to simplify complex return type
type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

impl AliyunAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> TranscriptionClientFuture {
        Box::pin(async move {
            let builder = Self::new(option, event_sender);
            builder
                .with_cancel_token(token)
                .with_track_id(track_id)
                .build()
                .await
                .map(|client| Box::new(client) as Box<dyn TranscriptionClient>)
        })
    }

    pub fn new(option: TranscriptionOption, event_sender: EventSender) -> Self {
        Self {
            option,
            token: None,
            track_id: None,
            event_sender,
        }
    }

    pub fn with_cancel_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.token = Some(cancellation_token);
        self
    }

    pub fn with_secret_key(mut self, secret_key: String) -> Self {
        self.option.secret_key = Some(secret_key);
        self
    }

    pub fn with_model_type(mut self, model_type: String) -> Self {
        self.option.model_type = Some(model_type);
        self
    }

    pub fn with_track_id(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }

    pub async fn build(self) -> Result<AliyunAsrClient> {
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();

        let event_sender_rx = match self.option.start_when_answer {
            Some(true) => Some(self.event_sender.subscribe()),
            _ => None,
        };

        let inner = Arc::new(AliyunAsrClientInner {
            audio_tx,
            option: self.option.clone(),
        });

        let client = AliyunAsrClient {
            inner: inner.clone(),
        };

        let track_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let token = self.token.unwrap_or_default();
        let event_sender = self.event_sender;
        info!(%track_id, "Starting Aliyun ASR client");

        tokio::spawn(async move {
            // Handle wait_for_answer if enabled
            if event_sender_rx.is_some() {
                handle_wait_for_answer_with_audio_drop(event_sender_rx, &mut audio_rx, &token)
                    .await;

                // Check if cancelled during wait
                if token.is_cancelled() {
                    debug!("Cancelled during wait for answer");
                    return Ok::<(), anyhow::Error>(());
                }
            }

            let ws_stream = match inner.connect_websocket(&track_id).await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!(track_id, "Failed to connect to Aliyun ASR WebSocket: {}", e);
                    let _ = event_sender.send(SessionEvent::Error {
                        timestamp: crate::media::get_timestamp(),
                        track_id,
                        sender: "AliyunAsrClient".to_string(),
                        error: format!("Failed to connect to Aliyun ASR WebSocket: {}", e),
                        code: Some(500),
                    });
                    return Err(e);
                }
            };

            let ctx = WebSocketContext {
                track_id: track_id.clone(),
                option: self.option,
            };

            match AliyunAsrClient::handle_websocket_message(
                ctx,
                ws_stream,
                audio_rx,
                event_sender.clone(),
                token,
            )
            .await
            {
                Ok(_) => {
                    debug!("WebSocket message handling completed");
                }
                Err(e) => {
                    info!("Error in handle_websocket_message: {}", e);
                    event_sender
                        .send(SessionEvent::Error {
                            track_id,
                            timestamp: crate::media::get_timestamp(),
                            sender: "aliyun_asr".to_string(),
                            error: e.to_string(),
                            code: None,
                        })
                        .ok();
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        Ok(client)
    }
}

#[async_trait]
impl TranscriptionClient for AliyunAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        let audio_data = codecs::samples_to_bytes(samples);
        self.inner
            .audio_tx
            .send(audio_data)
            .map_err(|_| anyhow!("Failed to send audio data"))?;
        Ok(())
    }
}
