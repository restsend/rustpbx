use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::handle_wait_for_answer_with_audio_drop;
use crate::event::{EventSender, SessionEvent};
use crate::media::{Sample, TrackId};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use futures::{SinkExt, StreamExt};
use http::{Request, StatusCode, Uri};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Type alias to simplify complex return type
type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

use super::{TranscriptionClient, TranscriptionOption};

/// https://github.com/ruzhila/voiceapi
/// A simple and clean voice transcription/synthesis API with sherpa-onnx
///
/// VoiceAPI ASR Result structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceApiAsrResult {
    pub text: String,
    pub finished: bool,
    pub idx: u32,
}

struct VoiceApiAsrClientInner {
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    option: TranscriptionOption,
}
/// VoiceAPI ASR client
pub struct VoiceApiAsrClient {
    inner: Arc<VoiceApiAsrClientInner>,
}

pub struct VoiceApiAsrClientBuilder {
    option: TranscriptionOption,
    track_id: Option<String>,
    cancel_token: Option<CancellationToken>,
    event_sender: EventSender,
}

impl VoiceApiAsrClientBuilder {
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
            cancel_token: None,
            track_id: None,
            event_sender,
        }
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_track_id(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }

    pub async fn build(self) -> Result<VoiceApiAsrClient> {
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();

        let inner = Arc::new(VoiceApiAsrClientInner {
            audio_tx,
            option: self.option.clone(),
        });

        let event_sender_rx = match self.option.start_when_answer {
            Some(true) => Some(self.event_sender.subscribe()),
            _ => None,
        };

        let sample_rate = self.option.samplerate.unwrap_or(16000);
        let token = self.cancel_token.unwrap_or_default();
        let event_sender = self.event_sender;
        let track_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        info!(%track_id, sample_rate, "VoiceAPI ASR client started");
        let inner_ref = inner.clone();

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

            let ws_stream = match inner_ref.connect_websocket(&track_id, sample_rate).await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!("Failed to connect to VoiceAPI ASR WebSocket: {}", e);
                    let _ = event_sender.send(SessionEvent::Error {
                        timestamp: crate::media::get_timestamp(),
                        track_id,
                        sender: "VoiceApiAsrClient".to_string(),
                        error: format!("Failed to connect to VoiceAPI ASR WebSocket: {}", e),
                        code: Some(500),
                    });
                    return Err(e);
                }
            };
            match inner_ref
                .handle_websocket_message(track_id, ws_stream, audio_rx, event_sender, token)
                .await
            {
                Ok(_) => {
                    debug!("WebSocket message handling completed");
                }
                Err(e) => {
                    info!("Error in handle_websocket_message: {}", e);
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        Ok(VoiceApiAsrClient { inner })
    }
}

impl VoiceApiAsrClientInner {
    // Establish WebSocket connection to VoiceAPI ASR service
    async fn connect_websocket(
        &self,
        voice_id: &str,
        sample_rate: u32,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        // Get the host and port from the options or use defaults
        let endpoint = self
            .option
            .endpoint
            .as_deref()
            .unwrap_or("ws://localhost:8000");

        // Create the websocket URL with the sample rate parameter
        let ws_url = format!("{}/asr?samplerate={}", endpoint, sample_rate);
        debug!("Connecting to WebSocket URL: {}", ws_url);
        let ws_url = ws_url.parse::<Uri>()?;
        let request = Request::builder()
            .uri(&ws_url)
            .header("Host", ws_url.host().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                BASE64_STANDARD.encode(random::<[u8; 16]>()),
            )
            .body(())?;

        let (ws_stream, response) = connect_async(request).await?;
        debug!(
            voice_id,
            "WebSocket connection established. Response: {}",
            response.status()
        );

        match response.status() {
            StatusCode::SWITCHING_PROTOCOLS => Ok(ws_stream),
            _ => Err(anyhow!(
                "Failed to connect to WebSocket server: {}",
                response.status()
            )),
        }
    }

    async fn handle_websocket_message(
        &self,
        track_id: TrackId,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        event_sender: EventSender,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let start_time = Arc::new(AtomicU64::new(0));
        let start_time_ref = start_time.clone();

        let send_task = async move {
            while let Some(audio) = audio_rx.recv().await {
                // Convert samples to websocket binary message
                if start_time_ref.load(Ordering::Relaxed) == 0 {
                    start_time_ref.store(crate::media::get_timestamp(), Ordering::Relaxed);
                }
                if let Err(e) = ws_sender.send(Message::Binary(audio.into())).await {
                    warn!("Error sending audio: {}", e);
                    break;
                }
            }
            Result::<(), anyhow::Error>::Ok(())
        };
        let track_id_clone = track_id.clone();
        let recv_task = async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        debug!("received text message: {}", text);
                        match serde_json::from_str::<VoiceApiAsrResult>(&text) {
                            Ok(result) => {
                                let evt = if result.finished {
                                    SessionEvent::AsrFinal {
                                        track_id: track_id_clone.clone(),
                                        text: result.text.clone(),
                                        timestamp: crate::media::get_timestamp(),
                                        index: result.idx,
                                        start_time: None,
                                        end_time: None,
                                    }
                                } else {
                                    SessionEvent::AsrDelta {
                                        track_id: track_id_clone.clone(),
                                        index: result.idx,
                                        timestamp: crate::media::get_timestamp(),
                                        start_time: None,
                                        end_time: None,
                                        text: result.text.clone(),
                                    }
                                };
                                if let Err(e) = event_sender.send(evt) {
                                    warn!("Failed to send event: {}", e);
                                    break;
                                }
                                let diff_time = crate::media::get_timestamp()
                                    - start_time.load(Ordering::Relaxed);
                                let metrics_event = if result.finished {
                                    start_time.store(0, Ordering::Relaxed);
                                    SessionEvent::Metrics {
                                        timestamp: crate::media::get_timestamp(),
                                        key: "completed.asr.voiceapi".to_string(),
                                        data: serde_json::json!({
                                            "index": result.idx,
                                        }),
                                        duration: diff_time as u32,
                                    }
                                } else {
                                    SessionEvent::Metrics {
                                        timestamp: crate::media::get_timestamp(),
                                        key: "ttfb.asr.voiceapi".to_string(),
                                        data: serde_json::json!({
                                            "index": result.idx,
                                        }),
                                        duration: diff_time as u32,
                                    }
                                };
                                event_sender.send(metrics_event).ok();
                            }
                            Err(e) => {
                                warn!("Failed to parse ASR result: {}", e);
                                break;
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        debug!("WebSocket closed by server");
                        break;
                    }
                    Ok(Message::Frame(_)) => {
                        // Ignore frame messages
                    }
                    Err(e) => {
                        warn!("Error receiving message: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            Result::<(), anyhow::Error>::Ok(())
        };

        // Run the tasks concurrently until one completes or the cancellation token is triggered
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                debug!("Cancelled by token");
            }
            res = send_task => {
                if let Err(e) = res {
                    warn!("Send task error: {}", e);
                }
            }
            res = recv_task => {
                if let Err(e) = res {
                    warn!("Receive task error: {}", e);
                }
            }
        }

        info!("WebSocket handler completed for track: {}", track_id);
        Ok(())
    }
}

#[async_trait]
impl TranscriptionClient for VoiceApiAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        // Convert i16 samples to bytes
        let mut buffer = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            buffer.extend_from_slice(&sample.to_le_bytes());
        }

        // Send PCM data to the audio channel
        if let Err(e) = self.inner.audio_tx.send(buffer) {
            warn!("Failed to send audio: {}", e);
            return Err(anyhow!("Failed to send audio: {}", e));
        }

        Ok(())
    }
}
