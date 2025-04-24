use anyhow::Result;
use async_trait::async_trait;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, pin::Pin};
mod tencent_cloud;
mod voiceapi;
pub use tencent_cloud::TencentCloudTtsClient;
pub use voiceapi::VoiceApiTtsClient;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SynthesisType {
    #[serde(rename = "tencent")]
    TencentCloud,
    #[serde(rename = "voiceapi")]
    VoiceApi,
    #[serde(rename = "other")]
    Other(String),
}

impl std::fmt::Display for SynthesisType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthesisType::TencentCloud => write!(f, "tencent"),
            SynthesisType::VoiceApi => write!(f, "voiceapi"),
            SynthesisType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct SynthesisOption {
    pub samplerate: i32,
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
}

#[async_trait]
pub trait SynthesisClient: Send + Sync {
    fn provider(&self) -> SynthesisType;
    /// Synthesize text to audio and return a stream of audio chunks
    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>>;
}

impl Default for SynthesisOption {
    fn default() -> Self {
        Self {
            samplerate: 16000,
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
        SynthesisType::Other(provider) => {
            return Err(anyhow::anyhow!("Unsupported provider: {}", provider));
        }
    }
}
