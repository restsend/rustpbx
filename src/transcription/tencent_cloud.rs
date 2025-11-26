use crate::event::{EventSender, SessionEvent};
use crate::media::codecs;
use crate::media::{Sample, TrackId};
use crate::transcription::{
    TranscriptionClient, TranscriptionOption, handle_wait_for_answer_with_audio_drop,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use chrono;
use futures::{SinkExt, StreamExt};
use http::{Request, StatusCode, Uri};
use rand::random;
use ring::hmac;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use urlencoding;
use uuid::Uuid;

/// Type alias to simplify complex return type
type TranscriptionClientFuture =
    Pin<Box<dyn Future<Output = Result<Box<dyn TranscriptionClient>>> + Send>>;

/// API Tencent Cloud streaming ASR
/// https://cloud.tencent.com/document/api/1093/48982
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrResult {
    pub slice_type: u32,
    pub index: u32,
    pub start_time: u32,
    pub end_time: u32,
    pub voice_text_str: String,
    pub word_size: u32,
    pub word_list: Vec<TencentCloudAsrWord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrWord {
    pub word: String,
    pub start_time: u32,
    pub end_time: u32,
    pub stable_flag: u32,
}
// TencentCloud WebSocket ASR response structure
#[derive(Debug, Deserialize)]
pub struct TencentCloudAsrResponse {
    pub code: i32,
    pub message: String,
    pub result: Option<TencentCloudAsrResult>,
}

struct TencentCloudAsrClientInner {
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    option: TranscriptionOption,
}

pub struct TencentCloudAsrClient {
    inner: Arc<TencentCloudAsrClientInner>,
}

pub struct TencentCloudAsrClientBuilder {
    option: TranscriptionOption,
    track_id: Option<String>,
    token: Option<CancellationToken>,
    event_sender: EventSender,
}

impl TencentCloudAsrClientBuilder {
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
    pub fn with_secret_id(mut self, secret_id: String) -> Self {
        self.option.secret_id = Some(secret_id);
        self
    }

    pub fn with_secret_key(mut self, secret_key: String) -> Self {
        self.option.secret_key = Some(secret_key);
        self
    }

    pub fn with_appid(mut self, appid: String) -> Self {
        self.option.app_id = Some(appid);
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
    pub async fn build(self) -> Result<TencentCloudAsrClient> {
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();

        let event_sender_rx = match self.option.start_when_answer {
            Some(true) => Some(self.event_sender.subscribe()),
            _ => None,
        };

        let inner = Arc::new(TencentCloudAsrClientInner {
            audio_tx,
            option: self.option,
        });

        let client = TencentCloudAsrClient {
            inner: inner.clone(),
        };

        let track_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let token = self.token.unwrap_or_default();
        let event_sender = self.event_sender;

        info!(track_id, "Starting TencentCloud ASR client");

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

            let ws_stream = match inner.connect_websocket(track_id.as_str()).await {
                Ok(stream) => stream,
                Err(e) => {
                    warn!(
                        track_id,
                        "Failed to connect to TencentCloud ASR WebSocket: {}", e
                    );
                    let _ = event_sender.send(SessionEvent::Error {
                        timestamp: crate::media::get_timestamp(),
                        track_id: track_id.clone(),
                        sender: "TencentCloudAsrClient".to_string(),
                        error: format!("Failed to connect to TencentCloud ASR WebSocket: {}", e),
                        code: Some(500),
                    });
                    return Err(e);
                }
            };

            match TencentCloudAsrClient::handle_websocket_message(
                track_id.clone(),
                ws_stream,
                audio_rx,
                event_sender.clone(),
                token,
            )
            .await
            {
                Ok(_) => {
                    debug!(track_id, "WebSocket message handling completed");
                }
                Err(e) => {
                    info!(track_id, "Error in handle_websocket_message: {}", e);
                    event_sender
                        .send(SessionEvent::Error {
                            track_id,
                            timestamp: crate::media::get_timestamp(),
                            sender: "tencent_cloud_asr".to_string(),
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

impl TencentCloudAsrClientInner {
    fn generate_signature(
        &self,
        secret_key: &str,
        host: &str,
        request_body: &str,
    ) -> Result<String> {
        // Create HMAC-SHA1 instance with secret key
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret_key.as_bytes());

        let url_to_sign = format!("{}{}", host, request_body);
        let hmac = hmac::sign(&key, url_to_sign.as_bytes());
        let base64_sig = BASE64_STANDARD.encode(hmac.as_ref());
        Ok(urlencoding::encode(&base64_sig).into_owned())
    }

    // Establish WebSocket connection to TencentCloud ASR service
    async fn connect_websocket(
        &self,
        voice_id: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let secret_id = self
            .option
            .secret_id
            .as_ref()
            .ok_or_else(|| anyhow!("No secret_id provided"))?;
        let secret_key = self
            .option
            .secret_key
            .as_ref()
            .ok_or_else(|| anyhow!("No secret_key provided"))?;
        let appid = self
            .option
            .app_id
            .as_ref()
            .ok_or_else(|| anyhow!("No appid provided"))?;

        let engine_model_type = self.option.model_type.as_deref().unwrap_or("16k_zh_en");

        let timestamp = chrono::Utc::now().timestamp() as u64;
        let nonce = timestamp.to_string(); // Use timestamp as nonce
        let expired = timestamp + 24 * 60 * 60; // 24 hours expiration
        let timestamp_str = timestamp.to_string();
        let expired_str = expired.to_string();

        // Build query parameters
        let mut query_params = vec![
            ("secretid", secret_id.as_str()),
            ("timestamp", timestamp_str.as_str()),
            ("expired", expired_str.as_str()),
            ("nonce", nonce.as_str()),
            ("engine_model_type", engine_model_type),
            ("voice_id", voice_id),
            ("voice_format", "1"), // PCM format
        ];

        let extra_options = vec![
            ("needvad", "1"),
            ("vad_silence_time", "700"),
            ("filter_modal", "0"),
            ("filter_punc", "0"),
            ("convert_num_mode", "1"),
            ("word_info", "1"),
            ("max_speak_time", "60000"),
        ];

        for mut option in extra_options {
            if let Some(extra) = self.option.extra.as_ref() {
                if let Some(value) = extra.get(option.0) {
                    option.1 = value;
                }
            }
            query_params.push(option);
        }

        // Sort query parameters by key
        query_params.sort_by(|a, b| a.0.cmp(b.0));

        // Build query string
        let query_string = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let url_path = format!("/asr/v2/{}", appid);

        // Generate signature
        let host = "asr.cloud.tencent.com";
        let signature = self.generate_signature(
            secret_key.as_str(),
            host,
            &format!("{}?{}", url_path, query_string),
        )?;

        let ws_url = format!(
            "wss://{}{}?{}&signature={}",
            host, url_path, query_string, signature
        );
        let request = Request::builder()
            .uri(ws_url.parse::<Uri>()?)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                BASE64_STANDARD.encode(random::<[u8; 16]>()),
            )
            .header("X-TC-Version", "2019-06-14")
            .header("X-TC-Region", "ap-guangzhou")
            .header("Content-Type", "application/json")
            .body(())?;

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

impl TencentCloudAsrClient {
    async fn handle_websocket_message(
        track_id: TrackId,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        event_sender: EventSender,
        token: CancellationToken,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let begin_time = crate::media::get_timestamp();
        let start_time = Arc::new(AtomicU64::new(0));
        let start_time_ref = start_time.clone();
        let track_id_clone = track_id.clone();

        let recv_loop = async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TencentCloudAsrResponse>(&text) {
                            Ok(response) => {
                                if response.code != 0 {
                                    warn!(
                                        track_id,
                                        "Error from ASR service: {} ({})",
                                        response.message,
                                        response.code
                                    );
                                    return Err(anyhow!(
                                        "Error from ASR service: {} ({})",
                                        response.message,
                                        response.code
                                    ));
                                }
                                response.result.and_then(|result| {
                                    let event = if result.slice_type == 2 {
                                        SessionEvent::AsrFinal {
                                            track_id: track_id.clone(),
                                            index: result.index,
                                            text: result.voice_text_str,
                                            timestamp: crate::media::get_timestamp(),
                                            start_time: Some(begin_time + result.start_time as u64),
                                            end_time: Some(begin_time + result.end_time as u64),
                                        }
                                    } else {
                                        SessionEvent::AsrDelta {
                                            track_id: track_id.clone(),
                                            index: result.index,
                                            text: result.voice_text_str,
                                            timestamp: crate::media::get_timestamp(),
                                            start_time: Some(begin_time + result.start_time as u64),
                                            end_time: Some(begin_time + result.end_time as u64),
                                        }
                                    };
                                    event_sender.send(event).ok()?;

                                    let diff_time = crate::media::get_timestamp()
                                        - start_time.load(Ordering::Relaxed);
                                    let metrics_event = if result.slice_type == 2 {
                                        start_time.store(0, Ordering::Relaxed);
                                        SessionEvent::Metrics {
                                            timestamp: crate::media::get_timestamp(),
                                            key: "completed.asr.tencent".to_string(),
                                            data: serde_json::json!({
                                                "index": result.index,
                                            }),
                                            duration: diff_time as u32,
                                        }
                                    } else {
                                        SessionEvent::Metrics {
                                            timestamp: crate::media::get_timestamp(),
                                            key: "ttfb.asr.tencent".to_string(),
                                            data: serde_json::json!({
                                                "index": result.index,
                                            }),
                                            duration: diff_time as u32,
                                        }
                                    };
                                    event_sender.send(metrics_event).ok()
                                });
                            }
                            Err(e) => {
                                warn!(track_id, "Failed to parse ASR response: {} {}", e, text);
                                return Err(anyhow!("Failed to parse ASR response: {}", e));
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        info!(track_id, "WebSocket connection closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        warn!(track_id, "WebSocket error: {}", e);
                        return Err(anyhow!("WebSocket error: {}", e));
                    }
                    _ => {}
                }
            }
            Result::<(), anyhow::Error>::Ok(())
        };

        let send_loop = async move {
            let mut total_bytes_sent = 0;
            while let Some(samples) = audio_rx.recv().await {
                if samples.is_empty() {
                    ws_sender
                        .send(Message::Text("{\"type\": \"end\"}".into()))
                        .await?;
                    start_time_ref.store(crate::media::get_timestamp(), Ordering::Relaxed);
                    continue;
                }
                total_bytes_sent += samples.len();
                if start_time_ref.load(Ordering::Relaxed) == 0 {
                    start_time_ref.store(crate::media::get_timestamp(), Ordering::Relaxed);
                }

                match ws_sender.send(Message::Binary(samples.into())).await {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(anyhow!("Failed to send audio data: {}", e));
                    }
                }
            }
            info!(
                track_id = track_id_clone,
                "Audio sender task completed. Total bytes sent: {}", total_bytes_sent
            );
            Result::<(), anyhow::Error>::Ok(())
        };
        tokio::select! {
            r = recv_loop => {r},
            r = send_loop => {r},
            _ = token.cancelled() => {Ok(())}
        }
    }
}

#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        self.inner
            .audio_tx
            .send(codecs::samples_to_bytes(samples))?;
        Ok(())
    }
}
