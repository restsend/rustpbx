use super::{TranscriptionClient, TranscriptionOption};
use crate::{
    event::{EventSender, SessionEvent},
    media::codecs,
    Sample, TrackId,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures::{SinkExt, StreamExt};
use http::{Request, StatusCode, Uri};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{net::TcpStream, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use urlencoding;
use uuid::Uuid;

/// Aliyun DashScope Paraformer real-time speech recognition API
/// https://help.aliyun.com/zh/model-studio/paraformer-real-time-speech-recognition-api/
pub struct AliyunAsrClient {
    option: TranscriptionOption,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
}

#[derive(Debug, Deserialize)]
pub struct AliyunAsrResponse {
    pub header: AliyunAsrHeader,
    pub payload: Option<AliyunAsrPayload>,
}

#[derive(Debug, Deserialize)]
pub struct AliyunAsrHeader {
    pub task_id: String,
    pub message: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct AliyunAsrPayload {
    pub result: String,
    pub sentence_id: u32,
    pub is_sentence_end: bool,
    pub begin_time: u64,
    pub end_time: u64,
}

#[derive(Debug, Serialize)]
pub struct AliyunAsrRequest {
    pub header: AliyunAsrRequestHeader,
    pub payload: AliyunAsrRequestPayload,
}

#[derive(Debug, Serialize)]
pub struct AliyunAsrRequestHeader {
    pub name: String,
    pub namespace: String,
    pub task_id: String,
}

#[derive(Debug, Serialize)]
pub struct AliyunAsrRequestPayload {
    pub model: String,
    pub task: String,
    pub audio: String, // base64 encoded
    pub audio_format: String,
    pub sample_rate: u32,
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
        let endpoint = self
            .option
            .endpoint
            .as_deref()
            .unwrap_or("dashscope.aliyuncs.com");
        // Aliyun DashScope WebSocket endpoint
        let ws_url = format!(
            "wss://{}/api/v1/services/audio/asr/ws?apikey={}",
            endpoint,
            urlencoding::encode(api_key)
        );

        info!("Connecting to Aliyun DashScope WebSocket URL: {}", ws_url);

        let request = Request::builder()
            .uri(ws_url.parse::<Uri>()?)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .body(())?;

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
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let begin_time = crate::get_timestamp();
        let start_time = Arc::new(AtomicU64::new(0));
        let start_time_ref = start_time.clone();
        let task_id = Uuid::new_v4().to_string();

        // Send start message
        let start_msg = AliyunAsrRequest {
            header: AliyunAsrRequestHeader {
                name: "StartRecognition".to_string(),
                namespace: "SpeechRecognizer".to_string(),
                task_id: task_id.clone(),
            },
            payload: AliyunAsrRequestPayload {
                model: "paraformer-realtime".to_string(),
                task: "asr".to_string(),
                audio: "".to_string(),
                audio_format: "pcm".to_string(),
                sample_rate: 16000,
            },
        };

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
                        match serde_json::from_str::<AliyunAsrResponse>(&text) {
                            Ok(response) => {
                                if response.header.status != "success" {
                                    warn!(
                                        "Error from ASR service: {} ({})",
                                        response.header.message, response.header.status
                                    );
                                    return Err(anyhow!(
                                        "Error from ASR service: {} ({})",
                                        response.header.message,
                                        response.header.status
                                    ));
                                }

                                if let Some(payload) = response.payload {
                                    if !payload.result.is_empty() {
                                        let event = if payload.is_sentence_end {
                                            start_time.store(0, Ordering::Relaxed);
                                            SessionEvent::AsrFinal {
                                                track_id: track_id.clone(),
                                                index: payload.sentence_id,
                                                text: payload.result,
                                                timestamp: crate::get_timestamp(),
                                                start_time: Some(begin_time + payload.begin_time),
                                                end_time: Some(begin_time + payload.end_time),
                                            }
                                        } else {
                                            if start_time.load(Ordering::Relaxed) == 0 {
                                                start_time.store(
                                                    crate::get_timestamp(),
                                                    Ordering::Relaxed,
                                                );
                                            }
                                            SessionEvent::AsrDelta {
                                                track_id: track_id.clone(),
                                                index: payload.sentence_id,
                                                text: payload.result,
                                                timestamp: crate::get_timestamp(),
                                                start_time: Some(begin_time + payload.begin_time),
                                                end_time: Some(begin_time + payload.end_time),
                                            }
                                        };
                                        event_sender.send(event).ok();

                                        let diff_time = crate::get_timestamp()
                                            - start_time_ref.load(Ordering::Relaxed);
                                        let metrics_event = if payload.is_sentence_end {
                                            SessionEvent::Metrics {
                                                timestamp: crate::get_timestamp(),
                                                key: "completed.asr.aliyun".to_string(),
                                                data: serde_json::json!({
                                                    "sentence_id": payload.sentence_id,
                                                }),
                                                duration: diff_time as u32,
                                            }
                                        } else {
                                            SessionEvent::Metrics {
                                                timestamp: crate::get_timestamp(),
                                                key: "ttfb.asr.aliyun".to_string(),
                                                data: serde_json::json!({
                                                    "sentence_id": payload.sentence_id,
                                                }),
                                                duration: diff_time as u32,
                                            }
                                        };
                                        event_sender.send(metrics_event).ok();
                                    }
                                }
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

                // Convert audio data to base64
                let audio_base64 = STANDARD.encode(&audio_data);

                let audio_msg = AliyunAsrRequest {
                    header: AliyunAsrRequestHeader {
                        name: "RecognitionAudio".to_string(),
                        namespace: "SpeechRecognizer".to_string(),
                        task_id: task_id.clone(),
                    },
                    payload: AliyunAsrRequestPayload {
                        model: "paraformer-realtime".to_string(),
                        task: "asr".to_string(),
                        audio: audio_base64,
                        audio_format: "pcm".to_string(),
                        sample_rate: 16000,
                    },
                };

                if let Ok(msg_json) = serde_json::to_string(&audio_msg) {
                    if let Err(e) = ws_sender.send(Message::Text(msg_json.into())).await {
                        warn!("Failed to send audio data: {}", e);
                        break;
                    }
                } else {
                    warn!("Failed to serialize audio message");
                }
            }

            // Send end message
            let end_msg = AliyunAsrRequest {
                header: AliyunAsrRequestHeader {
                    name: "StopRecognition".to_string(),
                    namespace: "SpeechRecognizer".to_string(),
                    task_id: task_id.clone(),
                },
                payload: AliyunAsrRequestPayload {
                    model: "paraformer-realtime".to_string(),
                    task: "asr".to_string(),
                    audio: "".to_string(),
                    audio_format: "pcm".to_string(),
                    sample_rate: 16000,
                },
            };

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
