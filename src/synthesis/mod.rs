use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
mod tencent_cloud;
pub use tencent_cloud::TencentCloudTtsClient;

#[cfg(test)]
mod tests;
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SynthesisConfig {
    pub url: String,
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
