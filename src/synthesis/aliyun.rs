use super::{SynthesisClient, SynthesisOption, SynthesisType};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::{stream, SinkExt, Stream, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{debug, warn};
use uuid::Uuid;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api
#[derive(Debug)]
pub struct AliyunTtsClient {
    option: SynthesisOption,
}

/// run-task command structure
#[derive(Debug, Serialize)]
struct RunTaskCommand {
    header: CommandHeader,
    payload: RunTaskPayload,
}

#[derive(Debug, Serialize)]
struct CommandHeader {
    action: String,
    task_id: String,
    stream: String,
}

#[derive(Debug, Serialize)]
struct RunTaskPayload {
    task_group: String,
    task: String,
    function: String,
    model: String,
    parameters: RunTaskParameters,
    input: Option<PayloadInput>,
}

#[derive(Debug, Serialize)]
struct RunTaskParameters {
    text_type: String,
    voice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pitch: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_ssml: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PayloadInput {
    text: String,
}

/// finish-task command structure
#[derive(Debug, Serialize)]
struct FinishTaskCommand {
    header: CommandHeader,
    payload: FinishTaskPayload,
}

#[derive(Debug, Serialize)]
struct FinishTaskPayload {
    input: EmptyInput,
}

#[derive(Debug, Serialize)]
struct EmptyInput {}

/// WebSocket event response structure
#[derive(Debug, Deserialize)]
struct WebSocketEvent {
    header: WebSocketEventHeader,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct WebSocketEventHeader {
    task_id: String,
    event: String,
    error_code: Option<String>,
    error_message: Option<String>,
}

impl AliyunTtsClient {
    pub fn create(option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        Self { option }
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
    fn create_run_task_command(
        &self,
        option: &SynthesisOption,
        task_id: &str,
        text: &str,
    ) -> RunTaskCommand {
        let model = option
            .extra
            .as_ref()
            .and_then(|e| e.get("model"))
            .cloned()
            .unwrap_or_else(|| "cosyvoice-v2".to_string());

        let voice = option
            .speaker
            .clone()
            .unwrap_or_else(|| "longyumi_v2".to_string());

        let format = match option.codec.as_deref() {
            Some("mp3") => "mp3",
            Some("wav") => "wav",
            _ => "pcm",
        };

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let volume = (option.volume.unwrap_or(5) * 10) as u32; // Convert to 0 - 100 range
        let rate = option.speed.unwrap_or(1.0);

        RunTaskCommand {
            header: CommandHeader {
                action: "run-task".to_string(),
                task_id: task_id.to_string(),
                stream: "duplex".to_string(),
            },
            payload: RunTaskPayload {
                task_group: "audio".to_string(),
                task: "tts".to_string(),
                function: "SpeechSynthesizer".to_string(),
                model,
                parameters: RunTaskParameters {
                    text_type: "PlainText".to_string(),
                    voice,
                    format: Some(format.to_string()),
                    sample_rate: Some(sample_rate),
                    volume: Some(volume),
                    rate: Some(rate),
                    pitch: None,
                    enable_ssml: None,
                },
                input: Some(PayloadInput {
                    text: text.to_string(),
                }),
            },
        }
    }

    /// Create finish-task command
    fn create_finish_task_command(&self, task_id: &str) -> FinishTaskCommand {
        FinishTaskCommand {
            header: CommandHeader {
                action: "finish-task".to_string(),
                task_id: task_id.to_string(),
                stream: "duplex".to_string(),
            },
            payload: FinishTaskPayload {
                input: EmptyInput {},
            },
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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        let option = self.option.merge_with(option);
        let api_key = self.get_api_key(&option)?;
        let task_id = Uuid::new_v4().to_string();
        let ws_url = option
            .endpoint
            .as_deref()
            .unwrap_or("wss://dashscope.aliyuncs.com/api-ws/v1/inference");
        debug!("Connecting to Aliyun WebSocket URL: {}", ws_url);

        let mut request = ws_url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
        headers.insert("X-DashScope-DataInspection", "enable".parse()?);

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
        let run_task_cmd = self.create_run_task_command(&option, &task_id, text);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        debug!("Sending run-task command: {}", run_task_json);
        ws_sink
            .send(Message::text(run_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send run-task command: {}", e))?;

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
                                debug!("Received event: {:?}", event);
                                match event.header.event.as_str() {
                                    "task-started" => {
                                        debug!("Task started");
                                        Some((Ok(Vec::new()), (ws_stream, false)))
                                    }
                                    "task-finished" => {
                                        debug!("Task finished");
                                        Some((Ok(Vec::new()), (ws_stream, true)))
                                    }
                                    "task-failed" => {
                                        let error_code = event
                                            .header
                                            .error_code
                                            .unwrap_or_else(|| "Unknown error".to_string());
                                        let error_msg = event
                                            .header
                                            .error_message
                                            .unwrap_or_else(|| "Unknown error".to_string());
                                        let error_msg =
                                            format!("Task failed: {} {}", error_code, error_msg);
                                        warn!("Task failed: {}:{}", error_code, error_msg);
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
                                        debug!("Ignoring unknown event: {}", event.header.event);
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
