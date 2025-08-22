use std::sync::Mutex;

use crate::synthesis::{SynthesisEvent, SynthesisEventReceiver, SynthesisEventSender};

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use http::{Request, StatusCode, Uri};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
/// https://github.com/ruzhila/voiceapi
/// A simple and clean voice transcription/synthesis API with sherpa-onnx
///
#[derive(Debug)]
pub struct VoiceApiTtsClient {
    option: SynthesisOption,
    tx: SynthesisEventSender,
    rx: Mutex<Option<SynthesisEventReceiver>>,
}

/// VoiceAPI TTS Request structure
#[derive(Debug, Serialize, Deserialize, Clone)]
struct TtsRequest {
    text: String,
    sid: i32,
    samplerate: i32,
    speed: f32,
}

/// VoiceAPI TTS metadata response
#[derive(Debug, Serialize, Deserialize)]
struct TtsResult {
    progress: f32,
    elapsed: String,
    duration: String,
    size: i32,
}

impl VoiceApiTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }
    pub fn new(option: SynthesisOption) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            option,
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }
    // WebSocket-based TTS synthesis
    async fn ws_synthesize(
        &self,
        text: &str,
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        let cache_key = option.as_ref().map(|opt| opt.cache_key.clone()).flatten();
        let option = self.option.merge_with(option);
        let endpoint = option
            .endpoint
            .clone()
            .unwrap_or("ws://localhost:8080".to_string());

        // Convert http endpoint to websocket if needed
        let ws_endpoint = if endpoint.starts_with("http") {
            endpoint
                .replace("http://", "ws://")
                .replace("https://", "wss://")
        } else {
            endpoint
        };
        let chunk_size = 4 * 640;
        let ws_url = format!("{}/tts?chunk_size={}&split=false", ws_endpoint, chunk_size);

        debug!("Connecting to WebSocket URL: {}", ws_url);

        let ws_url = ws_url.parse::<Uri>()?;
        // Create WebSocket request
        let request = Request::builder()
            .uri(&ws_url)
            .header("Host", ws_url.host().unwrap_or("localhost"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", STANDARD.encode(random::<[u8; 16]>()))
            .body(())?;

        // Connect to WebSocket
        let (ws_stream, response) = connect_async(request).await?;

        // Check if the connection was successful
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            self.tx.send(Err(anyhow!(
                "Failed to establish WebSocket connection: {}",
                response.status()
            )))?;
            return Err(anyhow!(
                "Failed to establish WebSocket connection: {}",
                response.status()
            ));
        }
        debug!("WebSocket connection established");
        // Split WebSocket stream into sender and receiver
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        // Send the TTS request
        ws_sender.send(Message::Text(text.into())).await?;

        while let Some(message) = ws_receiver.next().await {
            match message {
                Ok(Message::Binary(data)) => {
                    // Send audio chunk to the event channel
                    self.tx.send(Ok(SynthesisEvent::AudioChunk(data.into())))?;
                }
                Ok(Message::Text(text_data)) => {
                    match serde_json::from_str::<TtsResult>(&text_data) {
                        Ok(metadata) => {
                            debug!(
                                "Received metadata: progress={}, elapsed={}, duration={}, size={}",
                                metadata.progress,
                                metadata.elapsed,
                                metadata.duration,
                                metadata.size
                            );

                            if metadata.progress >= 1.0 {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse metadata: {}", e);
                            self.tx.send(Err(anyhow!(
                                "Failed to parse metadata: {}, {}",
                                text_data,
                                e
                            )))?;
                            return Err(anyhow!("Failed to parse metadata: {}, {}", text_data, e));
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("WebSocket closed by remote");
                    self.tx
                        .send(Err(anyhow!("VoiceAPI TTS websocket closed by remote")))?;
                    return Ok(());
                }
                Err(e) => {
                    warn!("WebSocket error: {}", e);
                    self.tx.send(Err(anyhow!("WebSocket error: {}", e)))?;
                    return Err(anyhow!("WebSocket error: {}", e));
                }
                _ => {}
            }
        }
        self.tx.send(Ok(SynthesisEvent::Finished {
            end_of_stream,
            cache_key,
        }))?;
        Ok(())
    }
}

#[async_trait]
impl SynthesisClient for VoiceApiTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::VoiceApi
    }
    async fn start(
        &self,
        _cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<SynthesisEvent>>> {
        let rx = self.rx.lock().unwrap().take().ok_or_else(|| {
            anyhow!("VoiceApiTtsClient: Receiver already taken, cannot start new stream")
        })?;
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Some(event) => Some((event, rx)),
                None => None,
            }
        })))
    }
    async fn synthesize(
        &self,
        text: &str,
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        self.ws_synthesize(text, end_of_stream, option).await
    }
}
