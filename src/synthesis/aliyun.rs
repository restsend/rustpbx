use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::SynthesisEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{
    FutureExt, SinkExt, Stream, StreamExt, future,
    stream::{self, BoxStream, SplitSink},
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::sync::Arc;
use tokio::{
    net::TcpStream,
    sync::{Notify, mpsc},
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, client::IntoClientRequest},
};
use tracing::warn;
use uuid::Uuid;
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

/// Aliyun CosyVoice WebSocket API Client
/// https://help.aliyun.com/zh/model-studio/cosyvoice-websocket-api

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
        let voice = option
            .speaker
            .clone()
            .unwrap_or_else(|| "longyumi_v2".to_string());

        let format = option.codec.as_deref().unwrap_or("pcm");

        let sample_rate = option.samplerate.unwrap_or(16000) as u32;
        let volume = option.volume.unwrap_or(50) as u32;
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
                model: "cosyvoice-v2".to_string(),
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

async fn connect(task_id: String, option: SynthesisOption) -> Result<WsStream> {
    let api_key = option
        .secret_key
        .as_ref()
        .ok_or_else(|| anyhow!("Aliyun TTS: missing api key"))?;
    let ws_url = option
        .endpoint
        .as_deref()
        .unwrap_or("wss://dashscope.aliyuncs.com/api-ws/v1/inference");

    let mut request = ws_url.into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Authorization", format!("Bearer {}", api_key).parse()?);
    headers.insert("X-DashScope-DataInspection", "enable".parse()?);

    let (mut ws_stream, _) = connect_async(request).await?;
    let run_task_cmd = Command::run_task(&option, task_id.as_str());
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

fn event_stream<T>(ws_stream: T) -> impl Stream<Item = Result<SynthesisEvent>> + Send + 'static
where
    T: Stream<Item = Result<Message, tungstenite::Error>> + Send + 'static,
{
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();
    ws_stream
        .take_until(notify.notified_owned())
        .filter_map(move |message| {
            let notify = notify_clone.clone();
            async move {
                match message {
                    Ok(Message::Binary(data)) => Some(Ok(SynthesisEvent::AudioChunk(data))),
                    Ok(Message::Text(text)) => {
                        let event: Event =
                            serde_json::from_str(&text).expect("Aliyun TTS API changed!");

                        match event.header.event.as_str() {
                            "task-finished" => {
                                notify.notify_one();
                                Some(Ok(SynthesisEvent::Finished))
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
                                notify.notify_one();
                                Some(Err(anyhow!(
                                    "Aliyun TTS Task: {} failed: {}, {}",
                                    event.header.task_id,
                                    error_code,
                                    error_msg
                                )))
                            }
                            _ => None,
                        }
                    }
                    Ok(Message::Close(_)) => {
                        notify.notify_one();
                        warn!("Aliyun TTS: closed by remote");
                        None
                    }
                    Err(e) => {
                        notify.notify_one();
                        Some(Err(anyhow!("Aliyun TTS: websocket error: {:?}", e)))
                    }
                    _ => None,
                }
            }
        })
}
#[derive(Debug)]
pub struct StreamingClient {
    task_id: String,
    option: SynthesisOption,
    ws_sink: Option<WsSink>,
}

impl StreamingClient {
    pub fn new(option: SynthesisOption) -> Self {
        Self {
            task_id: Uuid::new_v4().to_string(),
            option,
            ws_sink: None,
        }
    }
}

#[async_trait]
impl SynthesisClient for StreamingClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let ws_stream = connect(self.task_id.clone(), self.option.clone()).await?;
        let (ws_sink, ws_source) = ws_stream.split();
        self.ws_sink.replace(ws_sink);
        Ok(event_stream(ws_source).map(move |x| (None, x)).boxed())
    }

    async fn synthesize(
        &mut self,
        text: &str,
        _cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(ws_sink) = self.ws_sink.as_mut() {
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
        if let Some(ws_sink) = self.ws_sink.as_mut() {
            let finish_task_cmd = Command::finish_task(self.task_id.as_str());
            let finish_task_json = serde_json::to_string(&finish_task_cmd)?;
            ws_sink.send(Message::text(finish_task_json)).await?;
        }

        Ok(())
    }
}

pub struct NonStreamingClient {
    option: SynthesisOption,
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

impl NonStreamingClient {
    pub fn new(option: SynthesisOption) -> Self {
        Self { option, tx: None }
    }
}

#[async_trait]
impl SynthesisClient for NonStreamingClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Aliyun
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx.replace(tx);
        let client_option = self.option.clone();
        let max_concurrent_tasks = client_option.max_concurrent_tasks.unwrap_or(1);

        let stream = UnboundedReceiverStream::new(rx)
            .flat_map_unordered(max_concurrent_tasks, move |(text, cmd_seq, option)| {
                let option = client_option.merge_with(option);
                let task_id = Uuid::new_v4().to_string();
                let text_clone = text.clone();
                let task_id_clone = task_id.clone();
                connect(task_id, option)
                    .then(async move |res| match res {
                        Ok(mut ws_stream) => {
                            let continue_task_cmd =
                                Command::continue_task(task_id_clone.as_str(), text_clone.as_str());
                            let continue_task_json = serde_json::to_string(&continue_task_cmd)
                                .expect("Aliyun TTS API changed!");
                            ws_stream.send(Message::text(continue_task_json)).await.ok();
                            let finish_task_cmd = Command::finish_task(task_id_clone.as_str());
                            let finish_task_json = serde_json::to_string(&finish_task_cmd)
                                .expect("Aliyun TTS API changed!");
                            ws_stream.send(Message::text(finish_task_json)).await.ok();
                            event_stream(ws_stream).boxed()
                        }
                        Err(e) => {
                            warn!("Aliyun TTS: websocket error: {:?}, {:?}", cmd_seq, e);
                            stream::once(future::ready(Err(e.into()))).boxed()
                        }
                    })
                    .flatten_stream()
                    .map(move |x| (cmd_seq, x))
                    .boxed()
            })
            .boxed();
        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(tx) = &self.tx {
            tx.send((text.to_string(), cmd_seq, option))?;
        } else {
            return Err(anyhow!("Aliyun TTS Task: not connected"));
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}

pub struct AliyunTtsClient;
impl AliyunTtsClient {
    pub fn create(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        if streaming {
            Ok(Box::new(StreamingClient::new(option.clone())))
        } else {
            Ok(Box::new(NonStreamingClient::new(option.clone())))
        }
    }
}
