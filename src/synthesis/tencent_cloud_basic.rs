use super::{SynthesisClient, SynthesisOption, SynthesisType};
use crate::synthesis::{SynthesisEvent, tencent_cloud::TencentSubtitle};
use anyhow::Result;
use async_trait::async_trait;
use base64::{Engine, prelude::BASE64_STANDARD};
use bytes::Bytes;
use futures::{
    FutureExt, StreamExt, future,
    stream::{self, BoxStream},
};
use rand::Rng;
use ring::hmac;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use unic_emoji::char::is_emoji;
use urlencoding;
use uuid::Uuid;

const HOST: &str = "tts.tencentcloudapi.com";
const PATH: &str = "/";

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(rename = "Response")]
    response: ResponseData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ResponseData {
    #[serde(default)]
    audio: String,
    #[serde(default)]
    subtitles: Vec<TencentSubtitle>,
    error: Option<TencentError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TencentError {
    code: String,
    message: String,
}

// tencent cloud will crash if text contains emoji
// Only remove non-ASCII emoji characters. Keep all ASCII (digits, letters, punctuation),
// since some ASCII (e.g., '0'..'9', '#', '*') are marked with the Unicode Emoji property
// due to keycap sequences but are safe and expected in text.
pub fn strip_emoji_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| c.is_ascii() || !is_emoji(c))
        .collect()
}

// construct request url
// for non-streaming client, text is Some
// session_id is used for tencent cloud tts service, not the session_id of media session
fn construct_request_url(option: &SynthesisOption, session_id: &str, text: &str) -> String {
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let nonce = rand::rng().random::<u64>().to_string();
    let session_id = session_id.to_string();
    let secret_id = option.secret_id.clone().unwrap_or_default();
    let secret_key = option.secret_key.clone().unwrap_or_default();
    let volume = option.volume.unwrap_or(0).to_string();
    let speed = option.speed.unwrap_or(0.0).to_string();
    let voice_type = option
        .speaker
        .as_ref()
        .map(String::as_str)
        .unwrap_or("501004");
    let sample_rate = option.samplerate.unwrap_or(16000).to_string();
    let codec = option.codec.as_ref().map(String::as_str).unwrap_or("pcm");
    let mut query_params = vec![
        ("Action", "TextToVoice"),
        ("Timestamp", &timestamp),
        ("Nonce", &nonce),
        ("SecretId", &secret_id),
        ("Version", "2019-08-23"),
        ("Text", &text),
        ("SessionId", &session_id),
        ("Volume", &volume),
        ("Speed", &speed),
        ("VoiceType", &voice_type),
        ("SampleRate", &sample_rate),
        ("Codec", &codec),
        ("EnableSubtitle", "true"),
    ];

    // Sort query parameters by key
    query_params.sort_by_key(|(k, _)| *k);
    // Build query string without URL encoding
    let query_string = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");
    let string_to_sign = format!("GET{}{}?{}", HOST, PATH, query_string);

    // Calculate signature using HMAC-SHA1
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret_key.as_bytes());
    let tag = hmac::sign(&key, string_to_sign.as_bytes());
    let signature: String = BASE64_STANDARD.encode(tag.as_ref());
    query_params.push(("Signature", &signature));

    // URL encode parameters for final URL
    let encoded_query_string = query_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("https://{}{}?{}", HOST, PATH, encoded_query_string)
}

// tencent cloud realtime tts client, non-streaming
// https://cloud.tencent.com/document/product/1073/94308
// each tts command have one websocket connection, with different session_id

#[async_trait]
impl SynthesisClient for TencentCloudTtsBasicClient {
    fn provider(&self) -> SynthesisType {
        SynthesisType::Other("tencent_basic".to_string())
    }

    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>> {
        // Tencent cloud alow 10 - 20 concurrent websocket connections for default setting, dependent on voice type
        // set the number more higher will lead to waiting for unordered results longer
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let client_option = self.option.clone();
        let max_concurrent_tasks = client_option.max_concurrent_tasks.unwrap_or(1);
        let stream = UnboundedReceiverStream::new(rx)
            .flat_map_unordered(max_concurrent_tasks, move |(text, seq, option)| {
                // each reequest have its own session_id
                let session_id = Uuid::new_v4().to_string();
                let option = client_option.merge_with(option);
                let url = construct_request_url(&option, &session_id, &text);
                // request tencent cloud tts
                let fut = reqwest::get(url).then(async |res| {
                    let resp = res?.json::<Response>().await?;
                    if let Some(error) = resp.response.error {
                        return Err(anyhow::anyhow!(
                            "Tencent TTS error, code: {}, message: {}",
                            error.code,
                            error.message
                        ));
                    }
                    let audio = BASE64_STANDARD.decode(resp.response.audio)?;
                    Ok((audio, resp.response.subtitles))
                });

                // convert result to events
                stream::once(fut)
                    .flat_map(|res| match res {
                        Ok((audio, subtitles)) => {
                            let mut events = Vec::new();
                            events.push(Ok(SynthesisEvent::AudioChunk(Bytes::from(audio))));
                            if !subtitles.is_empty() {
                                events.push(Ok(SynthesisEvent::Subtitles(
                                    subtitles.iter().map(Into::into).collect(),
                                )));
                            }
                            events.push(Ok(SynthesisEvent::Finished));
                            stream::iter(events).boxed()
                        }
                        Err(e) => stream::once(future::ready(Err(e))).boxed(),
                    })
                    .map(move |x| (seq, x))
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
            let text = strip_emoji_chars(text);
            tx.send((text, cmd_seq, option))?;
        } else {
            return Err(anyhow::anyhow!("TencentCloud TTS: missing client sender"));
        };

        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.tx.take();
        Ok(())
    }
}

// tencent basic tts
// https://cloud.tencent.com/document/product/1073/37995
pub struct TencentCloudTtsBasicClient {
    option: SynthesisOption,
    //item: (text, option), drop tx if `end_of_stream`
    tx: Option<mpsc::UnboundedSender<(String, Option<usize>, Option<SynthesisOption>)>>,
}

impl TencentCloudTtsBasicClient {
    pub fn create(_streaming: bool, option: &SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
        Ok(Box::new(Self {
            option: option.clone(),
            tx: None,
        }))
    }
}
