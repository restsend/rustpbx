use anyhow::Result;

use super::AsrClient;
use super::AsrConfig;

// Default ASR client implementation (mock)
pub struct DefaultAsrClient {}

impl DefaultAsrClient {
    pub fn new() -> Self {
        Self {}
    }
}

impl AsrClient for DefaultAsrClient {
    fn transcribe<'a>(
        &'a self,
        _audio_data: &'a [i16],
        _sample_rate: u32,
        _config: &'a AsrConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // In a production implementation, this would call the ASR service
        // For demonstration purposes, we'll just simulate a response
        Box::pin(async { Ok("Sample transcription".to_string()) })
    }
}
