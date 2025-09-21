use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
mod aliyun;
mod tencent_cloud;
mod voiceapi;

pub use aliyun::AliyunTtsClient;
pub use tencent_cloud::TencentCloudTtsClient;
// pub use tencent_cloud_streaming::TencentCloudStreamingTtsClient;
pub use voiceapi::VoiceApiTtsClient;

#[derive(Clone, Default)]
pub struct SynthesisCommand {
    pub text: String,
    pub speaker: Option<String>,
    pub play_id: Option<String>,
    pub streaming: Option<bool>,
    pub end_of_stream: Option<bool>,
    pub option: SynthesisOption,
}
pub type SynthesisCommandSender = mpsc::UnboundedSender<SynthesisCommand>;
pub type SynthesisCommandReceiver = mpsc::UnboundedReceiver<SynthesisCommand>;
pub type SynthesisEventSender = mpsc::UnboundedSender<Result<SynthesisEvent>>;
pub type SynthesisEventReceiver = mpsc::UnboundedReceiver<Result<SynthesisEvent>>;

#[derive(Debug, Clone, Serialize, Hash, Eq, PartialEq)]
pub enum SynthesisType {
    #[serde(rename = "tencent")]
    TencentCloud,
    #[serde(rename = "voiceapi")]
    VoiceApi,
    #[serde(rename = "aliyun")]
    Aliyun,
    #[serde(rename = "other")]
    Other(String),
}

impl std::fmt::Display for SynthesisType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthesisType::TencentCloud => write!(f, "tencent"),
            SynthesisType::VoiceApi => write!(f, "voiceapi"),
            SynthesisType::Aliyun => write!(f, "aliyun"),
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
    pub cache_key: Option<String>,
}

impl SynthesisOption {
    pub fn merge_with(&self, option: Option<SynthesisOption>) -> Self {
        let mut merged = self.clone();
        if let Some(option) = option {
            if option.samplerate.is_some() {
                merged.samplerate = option.samplerate;
            }
            if option.speed.is_some() {
                merged.speed = option.speed;
            }
            if option.volume.is_some() {
                merged.volume = option.volume;
            }
            if option.speaker.is_some() {
                merged.speaker = option.speaker;
            }
            if option.codec.is_some() {
                merged.codec = option.codec;
            }
            if option.subtitle.is_some() {
                merged.subtitle = option.subtitle;
            }
            if option.emotion.is_some() {
                merged.emotion = option.emotion;
            }
            if option.endpoint.is_some() {
                merged.endpoint = option.endpoint;
            }
            if option.provider.is_some() {
                merged.provider = option.provider;
            }
            if option.app_id.is_some() {
                merged.app_id = option.app_id;
            }
            if option.secret_id.is_some() {
                merged.secret_id = option.secret_id;
            }
            if option.secret_key.is_some() {
                merged.secret_key = option.secret_key;
            }
            if option.cache_key.is_some() {
                merged.cache_key = option.cache_key;
            }
            if option.extra.is_some() {
                merged.extra = option.extra;
            }
        }
        merged
    }
}

#[derive(Debug)]
pub enum SynthesisEvent {
    /// Raw audio data chunk
    AudioChunk(Vec<u8>),
    /// Progress information including completion status
    Subtitles(Vec<Subtitle>),
    Finished {
        end_of_stream: Option<bool>,
        cache_key: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Subtitle {
    pub begin_time: u32,
    pub end_time: u32,
    pub begin_index: u32,
    pub end_index: u32,
}

impl Subtitle {
    pub fn new(begin_time: u32, end_time: u32, begin_index: u32, end_index: u32) -> Self {
        Self {
            begin_time,
            end_time,
            begin_index,
            end_index,
        }
    }
}

pub fn bytes_size_to_duration(bytes: usize, sample_rate: u32) -> u32 {
    (500.0 * bytes as f32 / sample_rate as f32) as u32
}

#[async_trait]
pub trait SynthesisClient: Send + Sync {
    /// Returns the provider type for this synthesis client.
    fn provider(&self) -> SynthesisType;
    async fn start(
        &self,
        cancel_token: CancellationToken,
    ) -> Result<BoxStream<'static, Result<SynthesisEvent>>>;
    // break out of stream polling loop when res is Err or Progress is finished
    async fn synthesize(
        &self,
        text: &str,
        end_of_stream: Option<bool>,
        option: Option<SynthesisOption>,
    ) -> Result<()>;
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
            cache_key: None,
        }
    }
}

impl SynthesisOption {
    pub fn check_default(&mut self) -> &Self {
        match self.provider {
            Some(SynthesisType::TencentCloud) => {
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
            Some(SynthesisType::VoiceApi) => {
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
            Some(SynthesisType::Aliyun) => {
                if self.secret_key.is_none() {
                    self.secret_key = std::env::var("DASHSCOPE_API_KEY").ok();
                }
            }
            _ => {}
        }
        self
    }
}

/// Create a synthesis client based on the provider type
pub fn create_synthesis_client(option: SynthesisOption) -> Result<Box<dyn SynthesisClient>> {
    let provider = option
        .provider
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No provider specified"))?;

    match provider {
        SynthesisType::TencentCloud => {
            let client = TencentCloudTtsClient::new(option);
            Ok(Box::new(client))
        }
        SynthesisType::VoiceApi => {
            let client = VoiceApiTtsClient::new(option);
            Ok(Box::new(client))
        }
        SynthesisType::Aliyun => {
            let client = AliyunTtsClient::new(option);
            Ok(Box::new(client))
        }
        SynthesisType::Other(provider) => {
            Err(anyhow::anyhow!("Unsupported provider: {}", provider))
        }
    }
}
