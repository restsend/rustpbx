use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRequest {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTranscript {
    pub version: u8,
    pub source: String,
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub segments: Vec<StoredTranscriptSegment>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub analysis: Option<StoredTranscriptAnalysis>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTranscriptSegment {
    #[serde(default)]
    pub idx: Option<u32>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    #[serde(default)]
    pub channel: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTranscriptAnalysis {
    #[serde(default)]
    pub elapsed: Option<f64>,
    #[serde(default)]
    pub rtf: Option<f64>,
    #[serde(default)]
    pub word_count: usize,
    #[serde(default)]
    pub asr_model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SenseVoiceCliSegment {
    #[serde(default)]
    pub start_sec: Option<f64>,
    #[serde(default)]
    pub end_sec: Option<f64>,
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "tags")]
    pub _tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SenseVoiceCliChannel {
    #[serde(default)]
    pub channel: Option<u32>,
    #[serde(default)]
    pub duration_sec: Option<f64>,
    #[serde(default)]
    pub rtf: Option<f64>,
    #[serde(default)]
    pub segments: Vec<SenseVoiceCliSegment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TranscriptSettingsUpdate {
    pub command: Option<String>,
    pub models_path: Option<String>,
    pub default_language: Option<String>,
    pub timeout_secs: Option<u64>,
    pub hf_endpoint: Option<String>,
}
