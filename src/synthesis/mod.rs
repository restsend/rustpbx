use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
mod tencent_cloud;
pub use tencent_cloud::TencentCloudTtsClient;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SynthesisType {
    #[serde(rename = "tencent_cloud")]
    TencentCloud,
}
#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SynthesisConfig {
    pub voice: Option<String>,
    pub rate: Option<f32>,
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub volume: Option<i32>,
    pub speaker: Option<i32>,
    pub codec: Option<String>,
}
#[async_trait]
pub trait SynthesisClient: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>>;
}

impl Default for SynthesisConfig {
    fn default() -> Self {
        Self {
            voice: Some("1".to_string()),
            rate: Some(1.0),
            appid: None,
            secret_id: None,
            secret_key: None,
            volume: Some(5), // 0-10
            speaker: Some(1),
            codec: Some("pcm".to_string()),
        }
    }
}
