use anyhow::Result;
use async_trait::async_trait;

use super::TranscriptionClient;
use super::TranscriptionConfig;

// Default Transcription client implementation (mock)
#[derive(Debug)]
pub struct DefaultAsrClient {}

impl DefaultAsrClient {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl TranscriptionClient for DefaultAsrClient {
    async fn transcribe(
        &self,
        _audio_data: &[i16],
        _sample_rate: u32,
        _config: &TranscriptionConfig,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}
