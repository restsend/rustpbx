use super::{TranscriptionClient, TranscriptionOption};
use crate::{
    Sample, TrackId,
    event::{EventSender, SessionEvent},
    media::codecs,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Aliyun DashScope Paraformer real-time speech recognition API
/// https://help.aliyun.com/zh/model-studio/paraformer-real-time-speech-recognition-api/
pub struct AliyunAsrClient {
    option: TranscriptionOption,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Debug, Serialize)]
pub struct RunTaskCommand {
    pub header: CommandHeader,
    pub payload: RunTaskPayload,
}

impl RunTaskCommand {
    pub fn new(
        task_id: String,
        sample_rate: u32,
        model: String,
        language_hints: Vec<String>,
    ) -> Self {
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
                model,
                input: CommandInput {},
                parameters: RunTaskCommandParameters {
                    format: "pcm".to_string(),
                    sample_rate,
                    language_hints,
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
pub struct RunTaskPayload {
    pub task_group: String,
    pub task: String,
    pub function: String,
    pub model: String,
    pub input: CommandInput,
    pub parameters: RunTaskCommandParameters,
}

#[derive(Debug, Serialize)]
pub struct RunTaskCommandParameters {
    pub format: String,
    pub sample_rate: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub language_hints: Vec<String>,
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
    pub heartbeat: bool,
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

impl AliyunAsrClient {
    // Establish WebSocket connection to Aliyun DashScope ASR service
    async fn connect_websocket(
        &self,
        _voice_id: &str,
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

        debug!("Connecting to Aliyun WebSocket URL: {}", ws_url);

        let mut request = ws_url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
        headers.insert("X-DashScope-DataInspection", "enable".parse()?);

        debug!("Connecting with request: {:?}", request);

        let (ws_stream, response) = connect_async(request).await?;
        debug!("WebSocket connection established. Response: {:?}", response);

        match response.status() {
            StatusCode::SWITCHING_PROTOCOLS => Ok(ws_stream),
            _ => Err(anyhow!("Failed to connect to WebSocket: {:?}", response)),
        }
    }

    async fn handle_websocket_message(
        track_id: TrackId,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        event_sender: EventSender,
        token: CancellationToken,
        model: String,
        sample_rate: u32,
        language_hints: Vec<String>,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let begin_time = crate::get_timestamp();
        let start_msg = RunTaskCommand::new(track_id.clone(), sample_rate, model, language_hints);

        if let Ok(msg_json) = serde_json::to_string(&start_msg) {
            if let Err(e) = ws_sender.send(Message::Text(msg_json.into())).await {
                warn!("Failed to send start message: {}", e);
                return Err(anyhow!("Failed to send start message: {}", e));
            }
        }

        let recv_loop = async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        debug!("ASR response: {}", text);
                        match serde_json::from_str::<AsrEvent>(&text) {
                            Ok(response) => {
                                let task_id = response.header.task_id.clone();
                                debug!("Task: {} event: {}", task_id, response.header.event);

                                if response.header.event == "task-failed" {
                                    let code = response.header.error_code.expect("mising_code");
                                    let message = response
                                        .header
                                        .error_message
                                        .expect("mising error message");
                                    let error =
                                        format!("Error from ASR service: {} ({})", code, message);
                                    warn!("{}", error);
                                    return Err(anyhow!(error));
                                }

                                if response.header.event == "task-finished" {
                                    debug!("Task: {} finished", task_id);
                                    break;
                                }

                                if response.header.event == "task-started" {
                                    debug!("Task: {} started", task_id);
                                    continue;
                                }

                                let payload = response.payload.ok_or(anyhow!("missing payload"))?;
                                let output = payload.output.ok_or(anyhow!("missing output"))?;

                                if output.sentence.heartbeat {
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
                                        timestamp: crate::get_timestamp(),
                                        start_time: Some(sentence_start_time),
                                        end_time: Some(sentence_end_time),
                                    }
                                } else {
                                    SessionEvent::AsrDelta {
                                        track_id: task_id.clone(),
                                        index: sentence.sentence_id,
                                        text,
                                        timestamp: crate::get_timestamp(),
                                        start_time: Some(sentence_start_time),
                                        end_time: Some(sentence_end_time),
                                    }
                                };
                                event_sender.send(event).ok();

                                let diff_time = (crate::get_timestamp() - begin_time) as u32;
                                let metrics_event = if sentence.sentence_end {
                                    SessionEvent::Metrics {
                                        timestamp: crate::get_timestamp(),
                                        key: "completed.asr.aliyun".to_string(),
                                        data: serde_json::json!({
                                            "sentence_id": sentence.sentence_id,
                                        }),
                                        duration: diff_time,
                                    }
                                } else {
                                    SessionEvent::Metrics {
                                        timestamp: crate::get_timestamp(),
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
                                warn!("Failed to parse ASR response: {}", e);
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        info!("WebSocket connection closed by server");
                        break;
                    }
                    Err(e) => {
                        warn!("WebSocket error: {}", e);
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

            let end_msg = FinishTaskCommand::new(track_id.clone());

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
                info!("WebSocket handling cancelled");
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

impl AliyunAsrClientBuilder {
    pub fn create(
        track_id: TrackId,
        token: CancellationToken,
        option: TranscriptionOption,
        event_sender: EventSender,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>> {
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
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let model_type = self
            .option
            .model_type
            .clone()
            .unwrap_or("paraformer-realtime-v2".to_string());
        let sample_rate = self.option.samplerate.unwrap_or(16000);
        let mut language_hints = Vec::new();
        if model_type == "paraformer-realtime-v2" {
            if let Some(language) = self.option.language.clone() {
                language_hints.push(language);
            }
        }
        let client = AliyunAsrClient {
            option: self.option,
            audio_tx,
        };

        let voice_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let ws_stream = client.connect_websocket(voice_id.as_str()).await?;
        let token = self.token.unwrap_or(CancellationToken::new());
        let event_sender = self.event_sender;
        let track_id = voice_id.clone();
        info!(
            "start track_id: {} voice_id: {} config: {:?}",
            track_id, voice_id, client.option
        );

        tokio::spawn(async move {
            match AliyunAsrClient::handle_websocket_message(
                track_id.clone(),
                ws_stream,
                audio_rx,
                event_sender.clone(),
                token,
                model_type,
                sample_rate,
                language_hints,
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
                            timestamp: crate::get_timestamp(),
                            sender: "aliyun_asr".to_string(),
                            error: e.to_string(),
                            code: None,
                        })
                        .ok();
                }
            }
        });

        Ok(client)
    }
}

#[async_trait]
impl TranscriptionClient for AliyunAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        let audio_data = codecs::samples_to_bytes(samples);
        self.audio_tx
            .send(audio_data)
            .map_err(|_| anyhow!("Failed to send audio data"))?;
        Ok(())
    }
}
