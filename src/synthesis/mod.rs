use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;
mod aliyun;
mod deepgram;
mod tencent_cloud;
mod tencent_cloud_basic;
mod voiceapi;
pub use aliyun::AliyunTtsClient;
pub use deepgram::DeepegramTtsClient;
pub use tencent_cloud::TencentCloudTtsClient;
pub use tencent_cloud_basic::TencentCloudTtsBasicClient;
pub use voiceapi::VoiceApiTtsClient;

#[derive(Clone, Default)]
pub struct SynthesisCommand {
    pub text: String,
    pub speaker: Option<String>,
    pub play_id: Option<String>,
    pub streaming: bool,
    pub end_of_stream: bool,
    pub option: SynthesisOption,
    pub base64: bool,
}
pub type SynthesisCommandSender = mpsc::UnboundedSender<SynthesisCommand>;
pub type SynthesisCommandReceiver = mpsc::UnboundedReceiver<SynthesisCommand>;
pub use self::tencent_cloud::strip_emoji_chars;

#[derive(Debug, Clone, Serialize, Hash, Eq, PartialEq)]
pub enum SynthesisType {
    #[serde(rename = "tencent")]
    TencentCloud,
    #[serde(rename = "voiceapi")]
    VoiceApi,
    #[serde(rename = "aliyun")]
    Aliyun,
    #[serde(rename = "deepgram")]
    Deepgram,
    #[serde(rename = "other")]
    Other(String),
}

impl std::fmt::Display for SynthesisType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthesisType::TencentCloud => write!(f, "tencent"),
            SynthesisType::VoiceApi => write!(f, "voiceapi"),
            SynthesisType::Aliyun => write!(f, "aliyun"),
            SynthesisType::Deepgram => write!(f, "deepgram"),
            SynthesisType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

impl<'de> Deserialize<'de> for SynthesisType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "tencent" => Ok(SynthesisType::TencentCloud),
            "voiceapi" => Ok(SynthesisType::VoiceApi),
            "aliyun" => Ok(SynthesisType::Aliyun),
            "deepgram" => Ok(SynthesisType::Deepgram),
            _ => Ok(SynthesisType::Other(value)),
        }
    }
}

#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct SynthesisOption {
    pub samplerate: Option<i32>,
    pub provider: Option<SynthesisType>,
    pub speed: Option<f32>,
    pub app_id: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub volume: Option<i32>,
    pub speaker: Option<String>,
    pub codec: Option<String>,
    pub subtitle: Option<bool>,
    /// emotion: neutral、sad、happy、angry、fear、news、story、radio、poetry、
    /// call、sajiao、disgusted、amaze、peaceful、exciting、aojiao、jieshuo
    pub emotion: Option<String>,
    pub endpoint: Option<String>,
    pub extra: Option<HashMap<String, String>>,
    pub max_concurrent_tasks: Option<usize>,
}

impl SynthesisOption {
    pub fn merge_with(&self, option: Option<SynthesisOption>) -> Self {
        if let Some(other) = option {
            Self {
                samplerate: other.samplerate.or(self.samplerate),
                provider: other.provider.or(self.provider.clone()),
                speed: other.speed.or(self.speed),
                app_id: other.app_id.or(self.app_id.clone()),
                secret_id: other.secret_id.or(self.secret_id.clone()),
                secret_key: other.secret_key.or(self.secret_key.clone()),
                volume: other.volume.or(self.volume),
                speaker: other.speaker.or(self.speaker.clone()),
                codec: other.codec.or(self.codec.clone()),
                subtitle: other.subtitle.or(self.subtitle),
                emotion: other.emotion.or(self.emotion.clone()),
                endpoint: other.endpoint.or(self.endpoint.clone()),
                extra: other.extra.or(self.extra.clone()),
                max_concurrent_tasks: other.max_concurrent_tasks.or(self.max_concurrent_tasks),
            }
        } else {
            self.clone()
        }
    }
}

#[derive(Debug)]
pub enum SynthesisEvent {
    /// Raw audio data chunk
    AudioChunk(Bytes),
    /// Progress information including completion status
    Subtitles(Vec<Subtitle>),
    Finished,
}

#[derive(Debug, Clone)]
pub struct Subtitle {
    pub text: String,
    pub begin_time: u32,
    pub end_time: u32,
    pub begin_index: u32,
    pub end_index: u32,
}

impl Subtitle {
    pub fn new(
        text: String,
        begin_time: u32,
        end_time: u32,
        begin_index: u32,
        end_index: u32,
    ) -> Self {
        Self {
            text,
            begin_time,
            end_time,
            begin_index,
            end_index,
        }
    }
}

// calculate audio duration from bytes size and sample rate
pub fn bytes_size_to_duration(bytes: usize, sample_rate: u32) -> u32 {
    (500.0 * bytes as f32 / sample_rate as f32) as u32
}

#[async_trait]
pub trait SynthesisClient: Send {
    // provider of the synthesis client.
    fn provider(&self) -> SynthesisType;

    // connect to the synthesis service.
    // (cmd_seq, result), return the cmd_seq that passed from `synthesize`
    async fn start(
        &mut self,
    ) -> Result<BoxStream<'static, (Option<usize>, Result<SynthesisEvent>)>>;

    // send text to the synthesis service.
    // `cmd_seq` and `option` are used for non streaming mode
    // for streaming mode, `cmd_seq` and `option` are None
    async fn synthesize(
        &mut self,
        text: &str,
        cmd_seq: Option<usize>,
        option: Option<SynthesisOption>,
    ) -> Result<()>;

    async fn stop(&mut self) -> Result<()>;
}

impl Default for SynthesisOption {
    fn default() -> Self {
        Self {
            samplerate: Some(16000),
            provider: None,
            speed: Some(1.0),
            app_id: None,
            secret_id: None,
            secret_key: None,
            volume: Some(5), // 0-10
            speaker: None,
            codec: Some("pcm".to_string()),
            subtitle: None,
            emotion: None,
            endpoint: None,
            extra: None,
            max_concurrent_tasks: None,
        }
    }
}

impl SynthesisOption {
    pub fn check_default(&mut self) {
        if let Some(provider) = &self.provider {
            match provider.to_string().as_str() {
                "tencent" | "tencent_basic" => {
                    if self.app_id.is_none() {
                        self.app_id = std::env::var("TENCENT_APPID").ok();
                    }
                    if self.secret_id.is_none() {
                        self.secret_id = std::env::var("TENCENT_SECRET_ID").ok();
                    }
                    if self.secret_key.is_none() {
                        self.secret_key = std::env::var("TENCENT_SECRET_KEY").ok();
                    }
                }
                "voiceapi" => {
                    // Set the endpoint from environment variable if not already set
                    if self.endpoint.is_none() {
                        self.endpoint = std::env::var("VOICEAPI_ENDPOINT")
                            .ok()
                            .or_else(|| Some("http://localhost:8000".to_string()));
                    }
                    // Set speaker ID from environment variable if not already set
                    if self.speaker.is_none() {
                        self.speaker = std::env::var("VOICEAPI_SPEAKER_ID")
                            .ok()
                            .or_else(|| Some("0".to_string()));
                    }
                }
                "aliyun" => {
                    if self.secret_key.is_none() {
                        self.secret_key = std::env::var("DASHSCOPE_API_KEY").ok();
                    }
                }
                "deepgram" => {
                    if self.secret_key.is_none() {
                        self.secret_key = std::env::var("DEEPGRAM_API_KEY").ok();
                    }
                }
                _ => {}
            }
        }
    }
}
