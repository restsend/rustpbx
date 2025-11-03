use crate::synthesis::{SynthesisClient, SynthesisEvent, SynthesisOption, SynthesisType};
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::SplitSink;
use futures::{Stream, stream::BoxStream};
use serde::Deserialize;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::warn;
use url::Url;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;

const DEEPGRAM_BASE_URL: &str = "https://api.deepgram.com/v1/speak";
const TERMINATORS: [char; 3] = ['.', '?', '!'];

// https://developers.deepgram.com/docs/tts-rest
pub struct RestClient {
    option: SynthesisOption,
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

#[derive(Serialize)]
struct Payload {
    text: String,
}

impl RestClient {
    pub fn new(option: SynthesisOption) -> Self {
        Self { option, tx: None }
    }
}

// deepgram model parameter format: details: https://developers.deepgram.com/docs/tts-models
// model: voice and language: [modelname]-[voicename]-[language],
// encoding: linear16,mulaw,alaw, etc.
// sample_rate: 8000, 16000, etc.
fn request_url(option: &SynthesisOption, protocol: &str) -> Url {
    let mut url = Url::parse(DEEPGRAM_BASE_URL).expect("Deepgram base url is invalid");
    url.set_scheme(protocol).expect("illegal url scheme");

    let mut query = url.query_pairs_mut();

    if let Some(speaker) = option.speaker.as_ref() {
        query.append_pair("model", speaker);
    }

    if let Some(codec) = option.codec.as_ref() {
        match codec.as_str() {
            "pcm" => query.append_pair("encoding", "linear16"),
            "pcmu" => query.append_pair("encoding", "mulaw"),
            "pcma" => query.append_pair("encoding", "alaw"),
            _ => query.append_pair("encoding", "linear16"),
        };
    } else {
        query.append_pair("encoding", "linear16");
    }

    let samplerate = option.samplerate.unwrap_or(16000);
    query.append_pair("sample_rate", samplerate.to_string().as_str());

    drop(query);
    url
}

async fn chunked_stream(
    option: SynthesisOption,
    text: String,
) -> Result<impl Stream<Item = Result<Bytes>>> {
    let url = request_url(&option, "https");
    let token = option
        .secret_key
        .as_ref()
        .ok_or_else(|| anyhow!("Deepegram tts: missing api key"))?;
    let payload = Payload { text };
    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Token {}", token))
        .json(&payload)
        .send()
        .await?;
    Ok(resp.bytes_stream().map_err(anyhow::Error::from))
}

#[async_trait]
impl SynthesisClient for RestClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Deepgram
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let max_concurrent_tasks = self.option.max_concurrent_tasks.unwrap_or(1);
        let client_option = self.option.clone();
        let stream = UnboundedReceiverStream::new(rx).flat_map_unordered(
            max_concurrent_tasks,
            move |(text, cmd_seq, cmd_option)| {
                let option = client_option.merge_with(cmd_option);
                chunked_stream(option, text)
                    .map(move |res| match res {
                        Ok(stream) => stream
                            .map(move |res| res.map(|bytes| SynthesisEvent::AudioChunk(bytes)))
                            .chain(stream::once(future::ready(Ok(SynthesisEvent::Finished))))
                            .boxed(),
                        Err(e) => stream::once(future::ready(Err(e))).boxed(),
                    })
                    .flatten_stream()
                    .map(move |res| (cmd_seq, res))
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
            return Err(anyhow::anyhow!("Deepgram TTS: missing client sender"));
        };
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}

struct StreamingClient {
    option: SynthesisOption,
    sink: Option<WsSink>,
}

impl StreamingClient {
    pub fn new(option: SynthesisOption) -> Self {
        Self { option, sink: None }
    }
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Command {
    Speak { text: String },
    Flush,
    Close,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum Event {
    Metadata {
        request_id: String,
        model_name: String,
        model_version: String,
        model_uuid: String,
    },
    Flushed {
        sequence_id: usize,
    },
    Cleared {
        sequence_id: usize,
    },
    Warning {
        description: String,
        code: String,
    },
}

async fn connect(option: SynthesisOption) -> Result<WsStream> {
    let url = request_url(&option, "wss");
    let mut request = url.as_str().into_client_request()?;
    let token = option
        .secret_key
        .as_ref()
        .ok_or_else(|| anyhow!("Deepegram tts: missing api key"))?;
    request
        .headers_mut()
        .insert("Authorization", format!("Token {}", token).parse()?);
    let (ws_stream, _) = connect_async(request).await?;
    Ok(ws_stream)
}

#[async_trait]
impl SynthesisClient for StreamingClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Deepgram
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        let (sink, source) = connect(self.option.clone()).await?.split();
        self.sink = Some(sink);
        let stream = source
            .filter_map(async move |message| match message {
                Ok(Message::Binary(bytes)) => Some(Ok(SynthesisEvent::AudioChunk(bytes))),
                Ok(Message::Text(text)) => {
                    let event: Event =
                        serde_json::from_str(&text).expect("Deepgram TTS API changed!");

                    if let Event::Warning { description, code } = event {
                        warn!("Deepgram TTS: warning: {}, {}", description, code);
                    }

                    None
                }
                Ok(Message::Close(_)) => Some(Ok(SynthesisEvent::Finished)),
                Err(e) => Some(Err(anyhow!("Deepgram TTS: websocket error: {:?}", e))),
                _ => None,
            })
            .map(|res| (None, res))
            .boxed();
        Ok(stream)
    }

    async fn synthesize(
        &mut self,
        text: &str,
        _cmd_seq: Option<usize>,
        _option: Option<SynthesisOption>,
    ) -> Result<()> {
        if let Some(sink) = &mut self.sink {
            // deepgram should mannualy flush, use terminators to split text

            for sentence in text.split_inclusive(&TERMINATORS[..]) {
                if !sentence.is_empty() {
                    let speak_cmd = Command::Speak {
                        text: sentence.to_string(),
                    };
                    let speak_json = serde_json::to_string(&speak_cmd)?;
                    sink.send(Message::text(speak_json)).await?;
                }

                if sentence.ends_with(&TERMINATORS[..]) {
                    let flush_cmd = Command::Flush;
                    let flush_json = serde_json::to_string(&flush_cmd)?;
                    sink.send(Message::text(flush_json)).await?;
                }
            }
        } else {
            return Err(anyhow::anyhow!("Deepgram TTS: missing sink"));
        };
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(mut sink) = self.sink.take() {
            let close_cmd = Command::Close;
            let close_json = serde_json::to_string(&close_cmd)?;
            sink.send(Message::text(close_json)).await?;
        } else {
            warn!("Deepgram TTS: missing sink");
        }
        Ok(())
    }
}

pub struct DeepegramTtsClient;

impl DeepegramTtsClient {
    pub fn create(streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        if streaming {
            Ok(Box::new(StreamingClient::new(option.clone())))
        } else {
            Ok(Box::new(RestClient::new(option.clone())))
        }
    }
}
