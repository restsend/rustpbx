use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::SynthesisEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{
    SinkExt, StreamExt,
    stream::{BoxStream, SplitSink, SplitStream},
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing::{info, warn};
use uuid::Uuid;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSource = SplitStream<WsStream>;
type WsSink = SplitSink<WsStream, Message>;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api
#[derive(Debug)]
pub struct AliyunTtsClient {
    task_id: String,
    option: SynthesisOption,
    ws_sink: Mutex<Option<WsSink>>,
}

#[derive(Debug, Serialize)]
struct Command {
    header: CommandHeader,
    payload: CommandPayload,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum CommandPayload {
    Run(RunTaskPayload),
    Continue(ContinueTaskPayload),
    Finish(FinishTaskPayload),
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
            payload: CommandPayload::Run(RunTaskPayload {
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
            payload: CommandPayload::Continue(ContinueTaskPayload {
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
            payload: CommandPayload::Finish(FinishTaskPayload {
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

#[skip_serializing_none]
#[derive(Debug, Serialize)]
struct RunTaskParameters {
    text_type: String,
    voice: String,
    format: Option<String>,
    sample_rate: Option<u32>,
    volume: Option<u32>,
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
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EventHeader {
    task_id: String,
    event: String,
    error_code: Option<String>,
    error_message: Option<String>,
}

impl AliyunTtsClient {
    pub fn create(_streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }

    pub fn new(option: SynthesisOption) -> Self {
        let task_id = Uuid::new_v4().to_string();
        Self {
            task_id,
            option,
            ws_sink: Mutex::new(None),
        }
    }

    pub async fn connect(&self) -> Result<WsStream> {
        let api_key = self
            .option
            .secret_key
            .as_ref()
            .ok_or_else(|| anyhow!("Aliyun TTS: missing api key"))?;
        let ws_url = self
            .option
            .endpoint
            .as_deref()
            .unwrap_or("wss://dashscope.aliyuncs.com/api-ws/v1/inference");

        let mut request = ws_url.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
        headers.insert("X-DashScope-DataInspection", "enable".parse()?);

        let task_id = self.task_id.as_str();
        let (mut ws_stream, _) = connect_async(request).await?;
        let run_task_cmd = Command::run_task(&self.option, task_id);
        let run_task_json = serde_json::to_string(&run_task_cmd)?;
        ws_stream.send(Message::text(run_task_json)).await?;
        while let Some(message) = ws_stream.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let event = serde_json::from_str::<Event>(&text)?;
                    match event.header.event.as_str() {
                        "task-started" => {
                            break;
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
                            return Err(anyhow!(
                                "Aliyun TTS Task: {} failed: {}, {}",
                                task_id,
                                error_code,
                                error_msg
                            ))?;
                        }
                        _ => {
                            warn!("Aliyun TTS Task: {} unexpected event: {:?}", task_id, event);
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    return Err(anyhow!("Aliyun TTS start failed: closed by server"));
                }
                Err(e) => {
                    return Err(anyhow!("Aliyun TTS start failed:: {}", e));
                }
                _ => {}
            }
        }
        Ok(ws_stream)
    }
}

async fn event_stream(
    ws_stream: WsSource,
    task_id: String,
    cache_key: Option<String>,
) -> BoxStream<'static, Result<(Option<usize>, SynthesisEvent)>> {
    let stream = ws_stream.filter_map(move |message| {
        let task_id = task_id.clone();
        let cache_key = cache_key.clone();
        async move {
            match message {
                Ok(Message::Binary(data)) => Some(Ok((None, SynthesisEvent::AudioChunk(data)))),
                Ok(Message::Text(text)) => {
                    if let Ok(event) = serde_json::from_str::<Event>(&text) {
                        match event.header.event.as_str() {
                            "task-finished" => Some(Ok((
                                None,
                                SynthesisEvent::Finished {
                                    cache_key: cache_key.clone(),
                                },
                            ))),
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
                                    task_id,
                                    error_code,
                                    error_msg
                                )))
                            }
                            _ => {
                                info!("Aliyun TTS Task: {} event: {:?}", task_id, event);
                                None
                            }
                        }
                    } else {
                        Some(Err(anyhow!(
                            "Aliyun TTS Task: {} failed to deserialize event: {}",
                            task_id,
                            text
                        )))
                    }
                }
                Ok(Message::Close(_)) => {
                    Some(Err(anyhow!("Aliyun TTS Task: {task_id} closed by remote")))
                }
                Err(e) => Some(Err(anyhow!(
                    "Aliyun TTS Task: {task_id} websocket error: {e}"
                ))),
                _ => None,
            }
        }
    });

    Box::pin(stream)
}

#[async_trait]
impl SynthesisClient for AliyunTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, Result<(Option<usize>, SynthesisEvent)>>> {
        let ws_stream = self.connect().await?;
        let (ws_sink, ws_source) = ws_stream.split();
        let stream = event_stream(
            ws_source,
            self.task_id.clone(),
            self.option.cache_key.clone(),
        )
        .await;
        self.ws_sink.lock().await.replace(ws_sink);
        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        _cmd_seq: usize,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            if !text.is_empty() {
                let continue_task_cmd = Command::continue_task(self.task_id.as_str(), text);
                let continue_task_json = serde_json::to_string(&continue_task_cmd)?;
                ws_sink.send(Message::text(continue_task_json)).await?;
            }
        } else {
            return Err(anyhow!("Aliyun TTS Task: not connected"));
        }

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.lock().await.as_mut() {
            let finish_task_cmd = Command::finish_task(self.task_id.as_str());
            let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
            ws_sink.send(Message::text(finish_task_json)).await?;
        }

        Ok(())
    }
}
