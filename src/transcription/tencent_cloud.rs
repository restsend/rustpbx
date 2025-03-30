use crate::transcription::{TranscriptionClient, TranscriptionConfig, TranscriptionFrame};
use crate::{AudioFrame, Samples};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::Bytes;
use chrono;
use futures::{SinkExt, StreamExt};
use hex;
use http::{Request, StatusCode, Uri};
use rand::random;
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::{
    select,
    time::{sleep, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};
use uuid::Uuid;

// TencentCloud ASR Response structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrResponse {
    #[serde(rename = "Response")]
    pub response: TencentCloudAsrResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TencentCloudAsrResult {
    #[serde(rename = "Result")]
    pub result: String,
    #[serde(rename = "RequestId")]
    pub request_id: String,
}

// TencentCloud WebSocket ASR response structure
#[derive(Debug, Deserialize)]
pub struct TencentCloudWsAsrResponse {
    pub code: i32,
    pub message: String,
    pub result: Option<String>,
    pub is_final: Option<i32>,
}

pub struct TencentCloudAsrClient {
    config: TranscriptionConfig,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    frame_rx: Mutex<mpsc::UnboundedReceiver<TranscriptionFrame>>,
}

pub struct TencentCloudAsrClientBuilder {
    config: TranscriptionConfig,
    cancellation_token: Option<CancellationToken>,
}

impl TencentCloudAsrClientBuilder {
    pub fn new(config: TranscriptionConfig) -> Self {
        Self {
            config,
            cancellation_token: None,
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
        self.config.appid = Some(appid);
        self
    }

    pub fn with_engine_type(mut self, engine_type: String) -> Self {
        self.config.engine_type = engine_type;
        self
    }

    pub async fn build(self) -> Result<TencentCloudAsrClient> {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (frame_tx, frame_rx) = mpsc::unbounded_channel();

        let client = TencentCloudAsrClient {
            config: self.config,
            audio_tx,
            frame_rx: Mutex::new(frame_rx),
        };
        let voice_id = Uuid::new_v4().to_string();
        let ws_stream = client.connect_websocket(voice_id.as_str()).await?;
        let cancellation_token = self.cancellation_token.unwrap_or(CancellationToken::new());
        tokio::spawn(async move {
            TencentCloudAsrClient::handle_websocket_message(
                ws_stream,
                voice_id.as_str(),
                audio_rx,
                frame_tx,
                cancellation_token,
            )
            .await;
        });
        Ok(client)
    }
}

impl TencentCloudAsrClient {
    pub fn new() -> Self {
        Self {
            config: TranscriptionConfig::default(),
            audio_tx: mpsc::unbounded_channel().0,
            frame_rx: Mutex::new(mpsc::unbounded_channel().1),
        }
    }

    fn generate_signature(
        &self,
        secret_id: &str,
        secret_key: &str,
        host: &str,
        method: &str,
        timestamp: u64,
        request_body: &str,
    ) -> Result<String> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();

        // Step 1: Build canonical request
        let canonical_headers = format!("content-type:application/json\nhost:{}\n", host);
        let signed_headers = "content-type;host";
        let hashed_request_payload = {
            let mut hasher = Sha256::new();
            hasher.update(request_body.as_bytes());
            hex::encode(hasher.finalize())
        };

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            "/",
            request_body, // Use request_body as query string for WebSocket
            canonical_headers,
            signed_headers,
            hashed_request_payload
        );

        // Step 2: Build string to sign
        let credential_scope = format!("{}/asr/tc3_request", date);
        let hashed_canonical_request = {
            let mut hasher = Sha256::new();
            hasher.update(canonical_request.as_bytes());
            hex::encode(hasher.finalize())
        };

        let string_to_sign = format!(
            "TC3-HMAC-SHA256\n{}\n{}\n{}",
            timestamp, credential_scope, hashed_canonical_request
        );

        // Step 3: Calculate signature
        let tc3_secret = format!("TC3{}", secret_key);

        let mac = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, tc3_secret.as_bytes());
        let signature = hmac::sign(&mac, string_to_sign.as_bytes());
        Ok(STANDARD.encode(signature.as_ref()))
    }

    fn samples_to_bytes_le(samples: &[i16]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for &sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
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
            .appid
            .as_ref()
            .ok_or_else(|| anyhow!("No appid provided"))?;

        let engine_type = self.config.engine_type.as_str();

        let timestamp = chrono::Utc::now().timestamp() as u64;
        let nonce = random::<u64>().to_string();
        let expired = timestamp + 24 * 60 * 60; // 24 hours expiration
        let timestamp_str = timestamp.to_string();
        let expired_str = expired.to_string();
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
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

        let ws_url = format!(
            "wss://asr.cloud.tencent.com/asr/v2/{}?{}",
            engine_type, query_string
        );
        debug!("Connecting to WebSocket URL: {}", ws_url);

        // Generate WebSocket key
        let ws_key = STANDARD.encode(random::<[u8; 16]>());

        // Generate timestamp and signature
        let host = "asr.cloud.tencent.com";
        let method = "GET";
        let request_body = &query_string;
        let signature = self.generate_signature(
            secret_id.as_str(),
            secret_key.as_str(),
            host,
            method,
            timestamp,
            request_body,
        )?;

        // Create authorization header
        let authorization = format!(
            "TC3-HMAC-SHA256 Credential={}/{}/asr/tc3_request, SignedHeaders=content-type;host, Signature={}",
            secret_id,
            date,
            signature
        );

        debug!("Generated authorization: {}", authorization);

        let request = Request::builder()
            .uri(ws_url.parse::<Uri>()?)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", ws_key)
            .header("Authorization", authorization)
            .header("X-TC-Timestamp", timestamp_str)
            .header("X-TC-Version", "2019-06-14")
            .header("X-TC-Region", "ap-guangzhou")
            .header("X-TC-AppId", appid)
            .header("X-TC-VoiceId", voice_id)
            .header("X-TC-ProjectId", "0")
            .header("X-TC-RequestId", Uuid::new_v4().to_string())
            .header("X-TC-Language", "zh")
            .header("X-TC-EngineType", engine_type)
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
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        voice_id: &str,
        mut audio_rx: mpsc::UnboundedReceiver<Vec<u8>>,
        frame_tx: mpsc::UnboundedSender<TranscriptionFrame>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Send start message
        let start_msg = json!({
            "type": "start",
            "voice_id": voice_id,
            "voice_format": "pcm",
            "sample_rate": 16000,
            "nchannels": 1,
            "hotword_id": "",
            "word_info": 1,
            "filter_dirty": 0,
            "filter_modal": 0,
            "filter_punc": 0,
            "convert_num_mode": 1,
            "first_channel_only": true,
            "vad_silence_time": 500,
            "need_vad": 1,
            "need_word_info": 1,
            "max_speak_time": 60000,
            "need_magic_num": 0
        })
        .to_string();

        debug!("Sending start message: {}", start_msg);
        ws_sender.send(Message::Text(start_msg.into())).await?;

        let mut ping_interval = tokio::time::interval(Duration::from_secs(15));
        let last_pong = Arc::new(Mutex::new(std::time::Instant::now()));
        let last_pong_clone = last_pong.clone();

        let recv_loop = async move {
            let mut index = -1;
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        debug!("Received text message: {}", text);
                        match serde_json::from_str::<TencentCloudWsAsrResponse>(&text) {
                            Ok(response) => {
                                if response.code != 0 {
                                    error!(
                                        "Error from ASR service: {} ({})",
                                        response.message, response.code
                                    );
                                    break;
                                }

                                if let (Some(result), Some(is_final)) =
                                    (response.result, response.is_final)
                                {
                                    let timestamp = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as u32;

                                    let frame = TranscriptionFrame {
                                        timestamp,
                                        text: result.clone(),
                                        is_final: is_final == 1,
                                    };

                                    // Send begin event for new sentence or first result
                                    if index == -1 || is_final == 1 {
                                        index = 1;
                                        debug!("New sentence started: {}", result);
                                    }

                                    // Send the frame
                                    if let Err(e) = frame_tx.send(frame) {
                                        error!("Failed to send transcription frame: {}", e);
                                        break;
                                    }

                                    // If this is a final result, reset the index
                                    if is_final == 1 {
                                        index = -1;
                                        debug!("Sentence completed: {}", result);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse ASR response: {}", e);
                                debug!("Raw response text: {}", text);
                                break;
                            }
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        debug!("Received binary message: {} bytes", data.len());
                    }
                    Ok(Message::Pong(_)) => {
                        debug!("Received pong");
                        let mut last_pong = last_pong.lock().await;
                        *last_pong = std::time::Instant::now();
                    }
                    Ok(Message::Close(frame)) => {
                        debug!("WebSocket connection closed: {:?}", frame);
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            debug!("WebSocket receiver task completed");
            Result::<(), anyhow::Error>::Ok(())
        };

        let send_loop = async move {
            let mut total_bytes_sent = 0;
            let mut consecutive_errors = 0;
            let max_consecutive_errors = 3;
            let mut retry_count = 0;
            let max_retries = 3;
            let base_delay = Duration::from_millis(100);

            while let Some(samples) = audio_rx.recv().await {
                if samples.is_empty() {
                    debug!("Received empty audio chunk, sending end message");
                    let end_msg = json!({
                        "type": "end",
                        "voice_id": voice_id
                    })
                    .to_string();

                    // Try to send end message with retries
                    let mut retry_delay = base_delay;
                    for retry in 0..=max_retries {
                        match ws_sender.send(Message::Text(end_msg.clone().into())).await {
                            Ok(_) => {
                                debug!("Successfully sent end message");
                                break;
                            }
                            Err(e) => {
                                error!("Failed to send end message (attempt {}): {}", retry + 1, e);
                                if retry == max_retries {
                                    error!(
                                        "Failed to send end message after {} retries",
                                        max_retries
                                    );
                                    break;
                                }
                                sleep(retry_delay).await;
                                retry_delay *= 2; // Exponential backoff
                            }
                        }
                    }
                    break;
                }

                // Send ping if needed
                if ping_interval.tick().await.elapsed() >= Duration::from_secs(15) {
                    let last_pong = last_pong_clone.lock().await;
                    if last_pong.elapsed() >= Duration::from_secs(15) {
                        debug!("Sending ping");
                        if let Err(e) = ws_sender.send(Message::Ping(Bytes::new())).await {
                            error!("Failed to send ping: {}", e);
                            break;
                        }
                    }
                }

                // Check for connection timeout
                let last_pong = last_pong_clone.lock().await;
                if last_pong.elapsed() >= Duration::from_secs(35) {
                    error!("No pong received for 35 seconds, connection is dead");
                    break;
                }
                drop(last_pong);

                total_bytes_sent += samples.len();
                debug!(
                    "Sending audio chunk: {} bytes (total sent: {} bytes)",
                    samples.len(),
                    total_bytes_sent
                );

                // Try to send audio data with retries
                let mut retry_delay = base_delay;
                let mut success = false;

                for retry in 0..=max_retries {
                    match ws_sender
                        .send(Message::Binary(samples.clone().into()))
                        .await
                    {
                        Ok(_) => {
                            consecutive_errors = 0;
                            retry_count = 0;
                            success = true;
                            // Add a small delay between chunks for flow control
                            sleep(Duration::from_millis(50)).await;
                            break;
                        }
                        Err(e) => {
                            error!("Failed to send audio data (attempt {}): {}", retry + 1, e);
                            consecutive_errors += 1;
                            retry_count += 1;

                            if retry == max_retries {
                                error!("Failed to send audio data after {} retries", max_retries);
                                break;
                            }

                            if consecutive_errors >= max_consecutive_errors {
                                error!("Too many consecutive errors, breaking send loop");
                                break;
                            }

                            sleep(retry_delay).await;
                            retry_delay *= 2; // Exponential backoff
                        }
                    }
                }

                if !success {
                    error!("Failed to send audio chunk after all retries");
                    break;
                }
            }
            debug!(
                "Audio sender task completed. Total bytes sent: {}",
                total_bytes_sent
            );
            Result::<(), anyhow::Error>::Ok(())
        };

        tokio::select! {
            _ = recv_loop => Ok(()),
            _ = send_loop => Ok(()),
            _ = cancellation_token.cancelled() => Ok(())
        }
    }
}

#[async_trait]
impl TranscriptionClient for TencentCloudAsrClient {
    async fn send_audio(&self, samples: &[i16]) -> Result<()> {
        self.audio_tx.send(Self::samples_to_bytes_le(samples))?;
        Ok(())
    }

    async fn next(&self) -> Option<TranscriptionFrame> {
        self.frame_rx.lock().await.recv().await
    }
}
