use crate::AudioFrame;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
mod tencent_cloud;
pub use tencent_cloud::TencentCloudAsrClient;
pub use tencent_cloud::TencentCloudAsrClientBuilder;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TranscriptionType {
    #[serde(rename = "tencent_cloud")]
    TencentCloud,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptionConfig {
    pub model: Option<String>,
    pub language: Option<String>,
    pub appid: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub engine_type: String,
    pub buffer_size: usize,
    pub sample_rate: u32,
}

// Default config for backward compatibility
impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            model: None,
            language: None,
            appid: None,
            secret_id: None,
            secret_key: None,
            engine_type: "16k_zh".to_string(),
            buffer_size: 8000, // 500ms at 16kHz
            sample_rate: 16000,
        }
    }
}

pub type TranscriptionSender = mpsc::UnboundedSender<AudioFrame>;
pub type TranscriptionReceiver = mpsc::UnboundedReceiver<AudioFrame>;

// Unified transcription client trait with async_trait support
#[async_trait]
pub trait TranscriptionClient: Send + Sync {
    fn send_audio(&self, data: &[i16]) -> Result<()>;
}

#[cfg(test)]
mod tests;
