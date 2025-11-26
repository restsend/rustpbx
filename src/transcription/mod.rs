use crate::event::SessionEvent;
use crate::media::AudioFrame;
use crate::media::Sample;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

mod aliyun;
mod tencent_cloud;
mod voiceapi;

pub use aliyun::AliyunAsrClient;
pub use aliyun::AliyunAsrClientBuilder;
pub use tencent_cloud::TencentCloudAsrClient;
pub use tencent_cloud::TencentCloudAsrClientBuilder;
pub use voiceapi::VoiceApiAsrClient;
pub use voiceapi::VoiceApiAsrClientBuilder;

/// Common helper function for handling wait_for_answer logic with audio dropping
pub async fn handle_wait_for_answer_with_audio_drop(
    event_rx: Option<crate::event::EventReceiver>,
    audio_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    token: &CancellationToken,
) {
    tokio::select! {
        _ = token.cancelled() => {
            debug!("Cancelled before answer");
        }
        // drop audio if not started after answer
        _ = async {
            while (audio_rx.recv().await).is_some() {}
        } => {}
        _ = async {
            if let Some(mut rx) = event_rx {
                while let Ok(event) = rx.recv().await {
                    if let SessionEvent::Answer { .. } = event {
                        debug!("Received answer event, starting transcription");
                        break;
                    }
                }
            }
        } => {
            debug!("Wait for answer completed");
        }
    }
}

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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct TranscriptionOption {
    pub provider: Option<TranscriptionType>,
    pub language: Option<String>,
    pub app_id: Option<String>,
    pub secret_id: Option<String>,
    pub secret_key: Option<String>,
    pub model_type: Option<String>,
    pub buffer_size: Option<usize>,
    pub samplerate: Option<u32>,
    pub endpoint: Option<String>,
    pub extra: Option<HashMap<String, String>>,
    pub start_when_answer: Option<bool>,
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

impl TranscriptionOption {
    pub fn check_default(&mut self) {
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
