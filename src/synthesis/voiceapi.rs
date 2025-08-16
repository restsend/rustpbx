use crate::synthesis::TTSEvent;

use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt, stream::BoxStream};
use http::{Request, StatusCode, Uri};
use rand::random;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, warn};
/// https://github.com/ruzhila/voiceapi
/// A simple and clean voice transcription/synthesis API with sherpa-onnx
///
#[derive(Debug)]
pub struct VoiceApiTtsClient {
    option: SynthesisOption,
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
        Self { option }
    }
    // WebSocket-based TTS synthesis
    async fn ws_synthesize(
        &self,
        text: &str,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<'_, Result<TTSEvent>>> {
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
            return Err(anyhow!(
                "Failed to establish WebSocket connection: {}",
                response.status()
            ));
        }
        debug!("WebSocket connection established");
        // Split WebSocket stream into sender and receiver
        let (mut ws_sender, ws_receiver) = ws_stream.split();
        // Send the TTS request
        ws_sender.send(Message::Text(text.into())).await?;

        let stream = ws_receiver.filter_map(|message| async {
            match message {
                Ok(Message::Binary(data)) => Some(Ok(TTSEvent::AudioChunk(data.to_vec()))),
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

                            if metadata.progress == 1.0 {
                                return Some(Ok(TTSEvent::Finished));
                            }

                            None
                        }
                        Err(e) => {
                            warn!("Failed to parse metadata: {}", e);
                            Some(Err(anyhow!(
                                "Failed to parse metadata: {}, {}",
                                text_data,
                                e
                            )))
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    Some(Err(anyhow!("VoiceAPI TTS websocket closed by remote")))
                }
                Err(e) => Some(Err(anyhow!("WebSocket error: {}", e))),
                _ => None,
            }
        });
        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl SynthesisClient for VoiceApiTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::VoiceApi
    }
    async fn synthesize(
        &self,
        text: &str,
        _end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<Result<TTSEvent>>> {
        self.ws_synthesize(text, option).await
    }
}
