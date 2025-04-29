use crate::event::{EventSender, SessionEvent};
use crate::media::codecs;
use crate::transcription::{TranscriptionClient, TranscriptionOption};
use crate::{Sample, TrackId};
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

pub struct TencentCloudAsrClient {
    option: TranscriptionOption,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
}

pub struct TencentCloudAsrClientBuilder {
    option: TranscriptionOption,
    track_id: Option<String>,
    token: Option<CancellationToken>,
    event_sender: EventSender,
}

impl TencentCloudAsrClientBuilder {
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
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();

        let client = TencentCloudAsrClient {
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
                    debug!("WebSocket message handling completed");
                }
                Err(e) => {
                    info!("Error in handle_websocket_message: {}", e);
                    event_sender
                        .send(SessionEvent::Error {
                            track_id,
                            timestamp: crate::get_timestamp(),
                            sender: "tencent_cloud_asr".to_string(),
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

impl TencentCloudAsrClient {
    pub fn new() -> Self {
        Self {
            option: TranscriptionOption::default(),
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
        info!("Connecting to WebSocket URL: {}", ws_url);
        let request = Request::builder()
            .uri(ws_url.parse::<Uri>()?)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", STANDARD.encode(random::<[u8; 16]>()))
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
        token: CancellationToken,
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
                                            timestamp: crate::get_timestamp(),
                                            start_time: Some(result.start_time),
                                            end_time: Some(result.end_time),
                                        }
                                    } else {
                                        SessionEvent::AsrDelta {
                                            track_id: track_id.clone(),
                                            index: result.index,
                                            text: result.voice_text_str,
                                            timestamp: crate::get_timestamp(),
                                            start_time: Some(result.start_time),
                                            end_time: Some(result.end_time),
                                        }
                                    };
                                    event_sender.send(event).ok()
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
                if samples.len() == 0 {
                    ws_sender
                        .send(Message::Text("{\"type\": \"end\"}".into()))
                        .await?;
                    continue;
                }
                total_bytes_sent += samples.len();
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
            r = recv_loop => {r},
            r = send_loop => {r},
            _ = token.cancelled() => {Ok(())}
        }
    }
}

#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    fn send_audio(&self, samples: &[Sample]) -> Result<()> {
        self.audio_tx.send(codecs::samples_to_bytes(samples))?;
        Ok(())
    }
}
