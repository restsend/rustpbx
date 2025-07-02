use crate::AudioFrame;
use crate::Sample;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;

mod aliyun;
mod tencent_cloud;
mod voiceapi;

pub use aliyun::AliyunAsrClient;
pub use aliyun::AliyunAsrClientBuilder;
pub use tencent_cloud::TencentCloudAsrClient;
pub use tencent_cloud::TencentCloudAsrClientBuilder;
pub use voiceapi::VoiceApiAsrClient;
pub use voiceapi::VoiceApiAsrClientBuilder;

#[derive(Debug, Clone, Serialize, Hash, Eq, PartialEq)]
pub enum TranscriptionType {
    #[serde(rename = "tencent")]
    TencentCloud,
    #[serde(rename = "voiceapi")]
    VoiceApi,
    #[serde(rename = "aliyun")]
    Aliyun,
    Other(String),
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

impl std::fmt::Display for TranscriptionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TranscriptionType::TencentCloud => write!(f, "tencent"),
            TranscriptionType::VoiceApi => write!(f, "voiceapi"),
            TranscriptionType::Aliyun => write!(f, "aliyun"),
            TranscriptionType::Other(provider) => write!(f, "{}", provider),
        }
    }
}

impl<'de> Deserialize<'de> for TranscriptionType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "tencent" => Ok(TranscriptionType::TencentCloud),
            "voiceapi" => Ok(TranscriptionType::VoiceApi),
            "aliyun" => Ok(TranscriptionType::Aliyun),
            _ => Ok(TranscriptionType::Other(value)),
        }
    }
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
            Some(TranscriptionType::Aliyun) => {
                if self.secret_key.is_none() {
                    self.secret_key = std::env::var("DASHSCOPE_API_KEY").ok();
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
