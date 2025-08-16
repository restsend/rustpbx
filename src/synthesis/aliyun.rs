use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::{TTSEvent, TTSSubtitle};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt, stream::BoxStream};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing::{debug, warn};
use uuid::Uuid;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api
#[derive(Debug)]
pub struct AliyunTtsClient {
    option: SynthesisOption,
}

#[derive(Debug, Serialize)]
struct Command {
    header: CommandHeader,
    payload: CommandPayload,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum CommandPayload {
    RunTask(RunTaskPayload),
    ContinueTask(ContinueTaskPayload),
    FinishTask(FinishTaskPayload),
}

impl Command {
    fn run_task(option: &SynthesisOption, task_id: &str) -> Self {
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

        let format = option.codec.as_deref().unwrap_or("pcm");

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let volume = (option.volume.unwrap_or(5) * 10) as u32; // Convert to 0 - 100 range
        let rate = option.speed.unwrap_or(1.0);

        Command {
            header: CommandHeader {
                action: "run-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: CommandPayload::RunTask(RunTaskPayload {
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
                },
                input: EmptyInput {},
            }),
        }
    }

    fn continue_task(task_id: &str, text: &str) -> Self {
        Command {
            header: CommandHeader {
                action: "continue-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: CommandPayload::ContinueTask(ContinueTaskPayload {
                input: PayloadInput {
                    text: text.to_string(),
                },
            }),
        }
    }

    fn finish_task(task_id: &str) -> Self {
        Command {
            header: CommandHeader {
                action: "finish-task".to_string(),
                task_id: task_id.to_string(),
                streaming: "duplex".to_string(),
            },
            payload: CommandPayload::FinishTask(FinishTaskPayload {
                input: EmptyInput {},
            }),
        }
    }
}

#[derive(Debug, Serialize)]
struct CommandHeader {
    action: String,
    task_id: String,
    streaming: String,
}

#[derive(Debug, Serialize)]
struct RunTaskPayload {
    task_group: String,
    task: String,
    function: String,
    model: String,
    parameters: RunTaskParameters,
    input: EmptyInput,
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
}

#[derive(Debug, Serialize)]
struct ContinueTaskPayload {
    input: PayloadInput,
}

#[derive(Debug, Serialize, Deserialize)]
struct PayloadInput {
    text: String,
}

#[derive(Debug, Serialize)]
struct FinishTaskPayload {
    input: EmptyInput,
}

#[derive(Debug, Serialize)]
struct EmptyInput {}

/// WebSocket event response structure
#[derive(Debug, Deserialize)]
struct Event {
    header: EventHeader,
    payload: EventPayload,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EventHeader {
    task_id: String,
    event: String,
    error_code: Option<String>,
    error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventPayload {
    usage: Option<PayloadUsage>,
}

#[derive(Debug, Deserialize)]
struct PayloadUsage {
    characters: u32,
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
}

struct AliyunTTSState {
    task_id: String,
    text: String,
    sample_rate: u32,
    last_size: u32,
    current_size: u32,
    last_usage: u32,
}

impl AliyunTTSState {
    fn new(task_id: &str, text: &str, sample_rate: u32) -> Self {
        Self {
            task_id: task_id.to_string(),
            text: text.to_string(),
            sample_rate,
            last_size: 0,
            current_size: 0,
            last_usage: 0,
        }
    }

    fn size_to_time(&self, byte_size: u32) -> u32 {
        (500.0 * byte_size as f32 / self.sample_rate as f32) as u32
    }

    fn usage_to_index(&self, usage: u32, default: u32) -> u32 {
        self.text
            .chars()
            .enumerate()
            .position(|(i, _)| i >= usage as usize)
            .unwrap_or(default as usize) as u32
    }

    fn move_forward(&mut self, current_usage: u32) -> Vec<TTSSubtitle> {
        let begin_index = self.usage_to_index(self.last_usage, 0);
        let end_index = self.usage_to_index(current_usage, self.text.len() as u32);
        let begin_time = self.size_to_time(self.last_size);
        let end_time = self.size_to_time(self.current_size);
        self.last_size = self.current_size;
        self.last_usage = current_usage;
        vec![TTSSubtitle::new(
            begin_time,
            end_time,
            begin_index,
            end_index,
        )]
    }
}

#[async_trait]
impl SynthesisClient for AliyunTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn synthesize(
        &self,
        text: &str,
        _end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<BoxStream<Result<TTSEvent>>> {
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

        let (ws_stream, response) = connect_async(request).await?;

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(anyhow!(
                "WebSocket connection failed: {}",
                response.status()
            ));
        }

        let (mut ws_sink, ws_stream) = ws_stream.split();

        let run_task_cmd = Command::run_task(&option, &task_id);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        ws_sink
            .send(Message::text(run_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send run-task command: {}", e))?;

        // text send here
        let continue_task_cmd = Command::continue_task(&task_id, text);
        let continue_task_json = serde_json::to_string(&continue_task_cmd)?;
        ws_sink
            .send(Message::text(continue_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send continue-task command: {}", e))?;

        // oneshot task
        let finish_task_cmd = Command::finish_task(&task_id);
        let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
        ws_sink
            .send(Message::text(finish_task_json))
            .await
            .map_err(|e| anyhow!("Failed to send finish-task command: {}", e))?;

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let state = Arc::new(Mutex::new(AliyunTTSState::new(&task_id, text, sample_rate)));
        let stream = ws_stream.filter_map(move |message| {
            let state = state.clone();
            async move {
                let mut state = state.lock().unwrap();
                match message {
                    Ok(Message::Binary(data)) => {
                        state.current_size += data.len() as u32;
                        Some(Ok(TTSEvent::AudioChunk(data.to_vec())))
                    }
                    Ok(Message::Text(message)) => {
                        if let Ok(event) = serde_json::from_str::<Event>(&message) {
                            match event.header.event.as_str() {
                                "task-started" => {
                                    debug!("Aliyun TTS Task: {} started", state.task_id);
                                    None
                                }
                                "result-generated" => {
                                    if let Some(usage) = event.payload.usage {
                                        Some(Ok(TTSEvent::Subtitles(
                                            state.move_forward(usage.characters),
                                        )))
                                    } else {
                                        None
                                    }
                                }
                                "task-failed" => {
                                    let error_code = event
                                        .header
                                        .error_code
                                        .unwrap_or_else(|| "Unknown error code".to_string());
                                    let error_msg = event
                                        .header
                                        .error_message
                                        .unwrap_or_else(|| "Unknown error message".to_string());
                                    Some(Err(anyhow!(
                                        "Aliyun TTS Task: {} failed: {}, {}",
                                        state.task_id,
                                        error_code,
                                        error_msg
                                    )))
                                }
                                "task-finished" => {
                                    debug!("Aliyun TTS Task: {} finished", state.task_id);
                                    Some(Ok(TTSEvent::Finished))
                                }
                                _ => {
                                    warn!(
                                        "Aliyun TTS Task: {} unknown event: {:?}",
                                        state.task_id, event
                                    );
                                    None
                                }
                            }
                        } else {
                            Some(Err(anyhow!(
                                "Aliyun TTS Task: {} failed to deserialize {}",
                                state.task_id,
                                message
                            )))
                        }
                    }
                    Ok(Message::Close(_)) => Some(Err(anyhow!(
                        "Aliyun TTS Task: {} websocket closed by remote",
                        state.task_id
                    ))),
                    Err(e) => Some(Err(anyhow!(
                        "Aliyun TTS Task: {} websocket error: {}",
                        state.task_id,
                        e
                    ))),
                    _ => None,
                }
            }
        });

        Ok(Box::pin(stream))
    }
}
