use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::SynthesisEvent;
use anyhow::Result;
use async_trait::async_trait;
use futures::{
    FutureExt, SinkExt, Stream, StreamExt,
    stream::{self, BoxStream},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, warn};

/// https://github.com/ruzhila/voiceapi
/// A simple and clean voice transcription/synthesis API with sherpa-onnx
///
#[derive(Debug)]
pub struct VoiceApiTtsClient {
    option: SynthesisOption,
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

#[allow(dead_code)]
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
    pub fn create(_streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        let client = Self::new(option.clone());
        Ok(Box::new(client))
    }
    pub fn new(option: SynthesisOption) -> Self {
        Self { option, tx: None }
    }

    // construct request url
    // for non-streaming client, text is Some
    // session_id is used for tencent cloud tts service, not the session_id of media session
    fn construct_request_url(option: &SynthesisOption) -> String {
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
        format!("{}/tts?chunk_size={}&split=false", ws_endpoint, chunk_size)
    }
}

// convert websocket to event stream
// text and cmd_seq and cache key are used for non-streaming mode (realtime client)
// text is for debuging purpose
fn ws_to_event_stream<T>(
    ws_stream: T,
    cmd_seq: Option<usize>,
) -> BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>
where
    T: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
{
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();
    ws_stream
        .take_until(notify.notified_owned())
        .filter_map(move |message| {
            let notify = notify_clone.clone();
            async move {
                match message {
                    Ok(Message::Binary(data)) => {
                        Some((cmd_seq, Ok(SynthesisEvent::AudioChunk(data))))
                    }
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TtsResult>(&text) {
                            Ok(metadata) => {
                                debug!(
                                    "Received metadata: progress={}, elapsed={}, duration={}, size={}",
                                    metadata.progress,
                                    metadata.elapsed,
                                    metadata.duration,
                                    metadata.size
                                );

                                if metadata.progress >= 1.0 {
                                    notify.notify_one();
                                    return Some((cmd_seq, Ok(SynthesisEvent::Finished)));
                                }
                            }
                            Err(e) => {
                                notify.notify_one();
                                warn!("Failed to parse metadata: {}", e);
                                return Some((
                                    cmd_seq,
                                    Err(anyhow::anyhow!(
                                        "VoiceAPPI TTS error, Failed to parse metadata: {}, {}", text, e)),
                                ));
                            }
                        }
                        None
                    }
                    Ok(Message::Close(_)) => {
                        notify.notify_one();
                        warn!("VoiceAPI TTS closed by remote, {:?}", cmd_seq);
                        None
                    }
                    Err(e) => {
                        notify.notify_one();
                        Some((
                            cmd_seq,
                            Err(anyhow::anyhow!(
                                "VoiceAPI TTS websocket error: {:?}, {:?}",
                                cmd_seq,
                                e
                            )),
                        ))
                    }
                    _ => None,
                }
            }
        })
        .boxed()
}
#[async_trait]
impl SynthesisClient for VoiceApiTtsClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::VoiceApi
    }
    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let client_option = self.option.clone();
        let max_concurrent_tasks = client_option.max_concurrent_tasks.unwrap_or(1);
        let stream = UnboundedReceiverStream::new(rx).flat_map_unordered(
            max_concurrent_tasks,
            move |(text, cmd_seq, option)| {
                // each reequest have its own session_id
                let option = client_option.merge_with(option);
                let url = Self::construct_request_url(&option);
                connect_async(url)
                    .then(async move |res| match res {
                        Ok((mut ws_stream, _)) => {
                            ws_stream.send(Message::text(text)).await.ok();
                            ws_to_event_stream(ws_stream, cmd_seq)
                        }
                        Err(e) => {
                            warn!("VoiceAPI TTS websocket error: {}", e);
                            stream::empty().boxed()
                        }
                    })
                    .flatten_stream()
                    .boxed()
            },
        );
        Ok(stream.boxed())
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
            return Err(anyhow::anyhow!("VoiceAPI TTS: missing client sender"));
        };
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}
