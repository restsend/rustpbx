use crate::event::{EventSender, SessionEvent};
use crate::media::codecs;
use crate::transcription::{TranscriptionClient, TranscriptionConfig};
use crate::TrackId;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono;
use futures::{SinkExt, StreamExt};
use http::{Request, StatusCode, Uri};
use rand::random;
use ring::hmac;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use urlencoding;
use uuid::Uuid;

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
    pub voice_id: String,
    pub result: Option<TencentCloudAsrResult>,
}

pub struct TencentCloudAsrClient {
    config: TranscriptionConfig,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
}

pub struct TencentCloudAsrClientBuilder {
    config: TranscriptionConfig,
    track_id: Option<String>,
    cancellation_token: Option<CancellationToken>,
    event_sender: EventSender,
}

impl TencentCloudAsrClientBuilder {
    pub fn new(config: TranscriptionConfig, event_sender: EventSender) -> Self {
        Self {
            config,
            cancellation_token: None,
            track_id: None,
            event_sender,
        }
    }
    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = Some(cancellation_token);
        self
    }
    pub fn with_secret_id(mut self, secret_id: String) -> Self {
        self.config.secret_id = Some(secret_id);
        self
    }

    pub fn with_secret_key(mut self, secret_key: String) -> Self {
        self.config.secret_key = Some(secret_key);
        self
    }

    pub fn with_appid(mut self, appid: String) -> Self {
        self.config.app_id = Some(appid);
        self
    }

    pub fn with_model_type(mut self, model_type: String) -> Self {
        self.config.model_type = model_type;
        self
    }
    pub fn with_track_id(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }
    pub async fn build(self) -> Result<TencentCloudAsrClient> {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();

        let client = TencentCloudAsrClient {
            config: self.config,
            audio_tx,
        };
        let voice_id = self.track_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let ws_stream = client.connect_websocket(voice_id.as_str()).await?;
        let cancellation_token = self.cancellation_token.unwrap_or(CancellationToken::new());
        let event_sender = self.event_sender;
        let track_id = voice_id.clone();
        tokio::spawn(async move {
            match TencentCloudAsrClient::handle_websocket_message(
                track_id,
                ws_stream,
                audio_rx,
                event_sender,
                cancellation_token,
            )
            .await
            {
                Ok(_) => {
                    debug!("WebSocket message handling completed");
                }
                Err(e) => {
                    info!("Error in handle_websocket_message: {}", e);
                }
            }
        });
        Ok(client)
    }
}

impl TencentCloudAsrClient {
    pub fn new() -> Self {
        Self {
            config: TranscriptionConfig::default(),
            audio_tx: mpsc::unbounded_channel().0,
        }
    }

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
        let base64_sig = STANDARD.encode(hmac.as_ref());
        Ok(urlencoding::encode(&base64_sig).into_owned())
    }

    // Establish WebSocket connection to TencentCloud ASR service
    async fn connect_websocket(
        &self,
        voice_id: &str,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let secret_id = self
            .config
            .secret_id
            .as_ref()
            .ok_or_else(|| anyhow!("No secret_id provided"))?;
        let secret_key = self
            .config
            .secret_key
            .as_ref()
            .ok_or_else(|| anyhow!("No secret_key provided"))?;
        let appid = self
            .config
            .app_id
            .as_ref()
            .ok_or_else(|| anyhow!("No appid provided"))?;

        let engine_type = self.config.model_type.as_str();

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
            ("engine_model_type", engine_type),
            ("voice_id", voice_id),
            ("voice_format", "1"), // PCM format
            ("needvad", "1"),
            ("filter_dirty", "0"),
            ("filter_modal", "0"),
            ("filter_punc", "0"),
            ("convert_num_mode", "1"),
            ("word_info", "1"),
            ("max_speak_time", "60000"),
        ];

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
        debug!("Connecting to WebSocket URL: {}", ws_url);
        let ws_key = STANDARD.encode(random::<[u8; 16]>());

        let request = Request::builder()
            .uri(ws_url.parse::<Uri>()?)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", ws_key)
            .header("X-TC-Version", "2019-06-14")
            .header("X-TC-Region", "ap-guangzhou")
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
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let recv_loop = async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        debug!("ASR response: {}", text);
                        match serde_json::from_str::<TencentCloudAsrResponse>(&text) {
                            Ok(response) => {
                                if response.code != 0 {
                                    warn!(
                                        "Error from ASR service: {} ({})",
                                        response.message, response.code
                                    );
                                    break;
                                }
                                response.result.map(|result| {
                                    let event = if result.slice_type == 2 {
                                        SessionEvent::TranscriptionFinal {
                                            track_id: track_id.clone(),
                                            index: result.index,
                                            text: result.voice_text_str,
                                            timestamp: crate::get_timestamp(),
                                            start_time: Some(result.start_time),
                                            end_time: Some(result.end_time),
                                        }
                                    } else {
                                        SessionEvent::TranscriptionDelta {
                                            track_id: track_id.clone(),
                                            index: result.index,
                                            text: result.voice_text_str,
                                            timestamp: crate::get_timestamp(),
                                            start_time: Some(result.start_time),
                                            end_time: Some(result.end_time),
                                        }
                                    };
                                    event_sender.send(event).ok();
                                });
                            }
                            Err(e) => {
                                warn!("Failed to parse ASR response: {} {}", e, text);
                                return Err(anyhow!("Failed to parse ASR response: {}", e));
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        info!("WebSocket connection closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        warn!("WebSocket error: {}", e);
                        return Err(anyhow!("WebSocket error: {}", e));
                    }
                    _ => {}
                }
            }
            debug!("WebSocket receiver task completed");
            Result::<(), anyhow::Error>::Ok(())
        };

        let send_loop = async move {
            let mut total_bytes_sent = 0;
            while let Some(samples) = audio_rx.recv().await {
                total_bytes_sent += samples.len();
                debug!(
                    "Sending audio chunk: {} bytes (total sent: {} bytes)",
                    samples.len(),
                    total_bytes_sent
                );
                match ws_sender.send(Message::Binary(samples.into())).await {
                    Ok(_) => {}
                    Err(e) => {
                        return Err(anyhow!("Failed to send audio data: {}", e));
                    }
                }
            }
            info!(
                "Audio sender task completed. Total bytes sent: {}",
                total_bytes_sent
            );
            Result::<(), anyhow::Error>::Ok(())
        };

        tokio::select! {
            r = recv_loop => {r} ,
            r = send_loop => {r},
            _ = cancellation_token.cancelled() => {Ok(())}
        }
    }
}

#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    fn send_audio(&self, samples: &[i16]) -> Result<()> {
        self.audio_tx.send(codecs::convert_s16_to_u8(samples))?;
        Ok(())
    }
}
