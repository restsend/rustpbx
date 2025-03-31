use crate::{
    event::EventSender,
    media::{
        processor::Processor,
        track::{Track, TrackConfig, TrackId, TrackPacketSender},
    },
    AudioFrame, Samples,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::io::Cursor;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub struct TtsTrack {
    id: TrackId,
    processors: Vec<Box<dyn Processor>>,
    config: TrackConfig,
    cancel_token: CancellationToken,
    text: Option<String>,
    use_cache: bool,
    play_id: Option<String>,
    speaker: Option<String>,
}

impl TtsTrack {
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            processors: Vec::new(),
            config: TrackConfig::default(),
            cancel_token: CancellationToken::new(),
            text: None,
            use_cache: true,
            play_id: None,
            speaker: None,
        }
    }

    pub fn with_play_id(mut self, play_id: Option<String>) -> Self {
        self.play_id = play_id;
        self
    }

    pub fn with_speaker(mut self, speaker: Option<String>) -> Self {
        self.speaker = speaker;
        self
    }
    pub fn with_text(mut self, text: String) -> Self {
        self.text = Some(text);
        self
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);
        self
    }

    pub fn with_ptime(mut self, ptime: Duration) -> Self {
        self.config = self.config.with_ptime(ptime);
        self
    }

    pub fn with_cache_enabled(mut self, use_cache: bool) -> Self {
        self.use_cache = use_cache;
        self
    }
}

#[async_trait]
impl Track for TtsTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.cancel_token.cancel();
        Ok(())
    }

    async fn send_packet(&self, _packet: &AudioFrame) -> Result<()> {
        Ok(())
    }
}
