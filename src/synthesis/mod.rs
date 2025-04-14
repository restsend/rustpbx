use anyhow::Result;
use async_trait::async_trait;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
mod tencent_cloud;
pub use tencent_cloud::TencentCloudTtsClient;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SynthesisType {
    #[serde(rename = "tencent")]
    TencentCloud,
}
#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct SynthesisConfig {
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
}

#[async_trait]
pub trait SynthesisClient: Send + Sync {
    /// Synthesize text to audio and return a stream of audio chunks
    async fn synthesize<'a>(
        &'a self,
        text: &'a str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'a>>>;
}

impl Default for SynthesisConfig {
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
        }
    }
}
