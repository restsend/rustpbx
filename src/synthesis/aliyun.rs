use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures::{stream, SinkExt, Stream, StreamExt};
use http::{Request, StatusCode, Uri};
use rand::random;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, warn};
use uuid::Uuid;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api?spm=a2c4g.11186623.help-menu-2400256.d_2_5_0_2.470054e08UnCMU
#[derive(Debug)]
pub struct AliyunTtsClient {
    option: SynthesisOption,
}

/// run-task command structure
#[derive(Debug, Serialize)]
struct RunTaskCommand {
    task_id: String,
    command: String,
    model: String,
    function: String,
    parameters: RunTaskParameters,
}

#[derive(Debug, Serialize)]
struct RunTaskParameters {
    format: String,
    sample_rate: i32,
    voice: String,
    volume: f32,
    speed: f32,
    enable_ssml: bool,
}

/// continue-task command structure
#[derive(Debug, Serialize)]
struct ContinueTaskCommand {
    task_id: String,
    command: String,
    text: String,
}

/// finish-task command structure
#[derive(Debug, Serialize)]
struct FinishTaskCommand {
    task_id: String,
    command: String,
}

/// WebSocket event response structure
#[derive(Debug, Deserialize)]
struct WebSocketEvent {
    event: String,
    task_id: Option<String>,
    message: Option<String>,
    #[serde(flatten)]
    extra: Value,
}

impl AliyunTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self { option }
    }

    /// Build WebSocket connection URL
    fn build_websocket_url(&self, option: &SynthesisOption) -> String {
        let endpoint = option
            .endpoint
            .as_ref()
            .map(|e| e.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "wss://dashscope.aliyuncs.com".to_string());

        format!("{}/api/v1/services/aigc/text2speech/synthesis", endpoint)
    }

    /// Get API Key from configuration or environment
    fn get_api_key(&self, option: &SynthesisOption) -> Result<String> {
        option
            .secret_key
            .clone()
            .or_else(|| std::env::var("DASHSCOPE_API_KEY").ok())
            .ok_or_else(|| {
                anyhow!("Aliyun API Key not configured, please set DASHSCOPE_API_KEY environment variable or specify secret_key in configuration")
            })
    }

    /// Create run-task command
    fn create_run_task_command(&self, option: &SynthesisOption, task_id: &str) -> RunTaskCommand {
        let model = option
            .extra
            .as_ref()
            .and_then(|e| e.get("model"))
            .map(|s| s.clone())
            .unwrap_or_else(|| "cosyvoice-v1".to_string());

        let voice = option
            .speaker
            .clone()
            .unwrap_or_else(|| "zhichu_emo".to_string());

        let format = match option.codec.as_deref() {
            Some("mp3") => "mp3",
            Some("wav") => "wav",
            _ => "pcm",
        };

        let sample_rate = option.samplerate.unwrap_or(16000);
        let volume = option.volume.unwrap_or(5) as f32 / 10.0; // Convert to 0.0-1.0 range
        let speed = option.speed.unwrap_or(1.0);

        RunTaskCommand {
            task_id: task_id.to_string(),
            command: "run-task".to_string(),
            model,
            function: "text2speech".to_string(),
            parameters: RunTaskParameters {
                format: format.to_string(),
                sample_rate,
                voice,
                volume,
                speed,
                enable_ssml: false,
            },
        }
    }

    /// Create continue-task command
    fn create_continue_task_command(&self, task_id: &str, text: &str) -> ContinueTaskCommand {
        ContinueTaskCommand {
            task_id: task_id.to_string(),
            command: "continue-task".to_string(),
            text: text.to_string(),
        }
    }

    /// Create finish-task command
    fn create_finish_task_command(&self, task_id: &str) -> FinishTaskCommand {
        FinishTaskCommand {
            task_id: task_id.to_string(),
            command: "finish-task".to_string(),
        }
    }
}

#[async_trait]
impl SynthesisClient for AliyunTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
        option: Option<SynthesisOption>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>> {
        let option = self.option.merge_with(option);
        let api_key = self.get_api_key(&option)?;
        let ws_url = self.build_websocket_url(&option);
        let task_id = Uuid::new_v4().to_string();

        debug!("Connecting to Aliyun WebSocket URL: {}", ws_url);

        // Parse WebSocket URL
        let ws_url = ws_url.parse::<Uri>()?;

        // Create WebSocket request
        let request = Request::builder()
            .uri(&ws_url)
            .header("Host", ws_url.host().unwrap_or("dashscope.aliyuncs.com"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", STANDARD.encode(random::<[u8; 16]>()))
            .header("Authorization", format!("Bearer {}", api_key))
            .body(())?;

        // Establish WebSocket connection
        let (ws_stream, response) = connect_async(request).await?;

        // Check connection status
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(anyhow!(
                "WebSocket connection failed: {}",
                response.status()
            ));
        }

        debug!("WebSocket connection established");

        // Split read/write streams
        let (mut ws_sink, ws_stream) = ws_stream.split();

        // Send run-task command
        let run_task_cmd = self.create_run_task_command(&option, &task_id);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        debug!("Sending run-task command: {}", run_task_json);

        ws_sink
            .send(Message::text(run_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send run-task command: {}", e))?;

        // Send continue-task command
        let continue_task_cmd = self.create_continue_task_command(&task_id, text);
        let continue_task_json = serde_json::to_string(&continue_task_cmd)?;
        debug!("Sending continue-task command: {}", continue_task_json);

        ws_sink
            .send(Message::text(continue_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send continue-task command: {}", e))?;

        // Send finish-task command
        let finish_task_cmd = self.create_finish_task_command(&task_id);
        let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
        debug!("Sending finish-task command: {}", finish_task_json);

        ws_sink
            .send(Message::text(finish_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send finish-task command: {}", e))?;

        // Create audio data stream
        let stream = Box::pin(stream::unfold(
            (ws_stream, false),
            |(mut ws_stream, finished)| async move {
                if finished {
                    return None;
                }

                match ws_stream.next().await {
                    Some(Ok(Message::Text(text))) => {
                        // Handle JSON event messages
                        match serde_json::from_str::<WebSocketEvent>(&text) {
                            Ok(event) => {
                                debug!("Received event: {}", event.event);
                                match event.event.as_str() {
                                    "task-started" => {
                                        debug!("Task started");
                                        Some((Ok(Vec::new()), (ws_stream, false)))
                                    }
                                    "task-finished" => {
                                        debug!("Task finished");
                                        Some((Ok(Vec::new()), (ws_stream, true)))
                                    }
                                    "task-failed" => {
                                        let error_msg = event
                                            .message
                                            .unwrap_or_else(|| "Unknown error".to_string());
                                        warn!("Task failed: {}", error_msg);
                                        Some((
                                            Err(anyhow!("Task failed: {}", error_msg)),
                                            (ws_stream, true),
                                        ))
                                    }
                                    "result-generated" => {
                                        debug!("Result generated event");
                                        Some((Ok(Vec::new()), (ws_stream, false)))
                                    }
                                    _ => {
                                        debug!("Ignoring unknown event: {}", event.event);
                                        Some((Ok(Vec::new()), (ws_stream, false)))
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to parse event message: {}", e);
                                Some((Ok(Vec::new()), (ws_stream, false)))
                            }
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        // Audio data
                        debug!("Received audio data: {} bytes", data.len());
                        Some((Ok(data.to_vec()), (ws_stream, false)))
                    }
                    Some(Ok(Message::Close(_))) => {
                        debug!("WebSocket connection closed");
                        None
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket error: {:?}", e);
                        Some((Err(anyhow!("WebSocket error: {}", e)), (ws_stream, true)))
                    }
                    None => {
                        debug!("WebSocket stream ended");
                        None
                    }
                    _ => {
                        // Ignore other message types (ping/pong etc.)
                        Some((Ok(Vec::new()), (ws_stream, false)))
                    }
                }
            },
        ));

        Ok(stream)
    }
}
