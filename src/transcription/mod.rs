use std::collections::HashMap;

use crate::AudioFrame;
use crate::Sample;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
mod tencent_cloud;
mod voiceapi;
pub use tencent_cloud::TencentCloudAsrClient;
pub use tencent_cloud::TencentCloudAsrClientBuilder;
pub use voiceapi::VoiceApiAsrClient;
pub use voiceapi::VoiceApiAsrClientBuilder;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TranscriptionType {
    #[serde(rename = "tencent")]
    TencentCloud,
    #[serde(rename = "voiceapi")]
    VoiceApi,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct TranscriptionOption {
    pub provider: Option<TranscriptionType>,
    pub model: Option<String>,
    pub language: Option<String>,
    pub app_id: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub model_type: Option<String>,
    pub buffer_size: Option<usize>,
    pub samplerate: Option<u32>,
    pub endpoint: Option<String>,
    pub extra: Option<HashMap<String, String>>,
}

// Default config for backward compatibility
impl Default for TranscriptionOption {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            language: None,
            app_id: None,
            secret_id: None,
            secret_key: None,
            model_type: None,
            buffer_size: None,
            samplerate: None,
            endpoint: None,
            extra: None,
        }
    }
}

impl TranscriptionOption {
    pub fn check_default(&mut self) -> &Self {
        match self.provider {
            Some(TranscriptionType::TencentCloud) => {
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
            Some(TranscriptionType::VoiceApi) => {
                // Set the host from environment variable if not already set
                if self.endpoint.is_none() {
                    self.endpoint = std::env::var("VOICEAPI_ENDPOINT").ok();
                }
            }
            _ => {}
        }
        self
    }
}
pub type TranscriptionSender = mpsc::UnboundedSender<AudioFrame>;
pub type TranscriptionReceiver = mpsc::UnboundedReceiver<AudioFrame>;

// Unified transcription client trait with async_trait support
#[async_trait]
pub trait TranscriptionClient: Send + Sync {
    fn send_audio(&self, samples: &[Sample]) -> Result<()>;
}

#[cfg(test)]
mod tests;
