//! Shared test utilities for conference / media bridge tests.
//!
//! Provides mock implementations of [`AudioSender`] and [`AudioReceiver`]
//! so unit and integration tests can exercise conference media bridging
//! without real SIP media tracks.

use std::sync::Arc;

use rustrtc::media::MediaSample;
use tokio::sync::mpsc;

use crate::call::runtime::conference_media_bridge::{
    AudioReceiver, AudioSender, PcmAudioFrame,
};

/// Mock audio sender that records all sent samples.
pub struct MockAudioSender {
    pub samples: Arc<tokio::sync::Mutex<Vec<MediaSample>>>,
}

impl MockAudioSender {
    pub fn new() -> Self {
        Self {
            samples: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    pub async fn get_samples(&self) -> Vec<MediaSample> {
        self.samples.lock().await.clone()
    }

    pub fn clone_with_shared(&self) -> Self {
        Self {
            samples: self.samples.clone(),
        }
    }
}

impl Default for MockAudioSender {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSender for MockAudioSender {
    async fn send(
        &self,
        sample: MediaSample,
    ) -> Result<(), mpsc::error::SendError<MediaSample>> {
        self.samples.lock().await.push(sample);
        Ok(())
    }
}

/// Mock audio receiver that provides predefined PCM frames.
pub struct MockAudioReceiver {
    pub frames: Vec<PcmAudioFrame>,
    pub index: usize,
}

impl MockAudioReceiver {
    pub fn new(frames: Vec<PcmAudioFrame>) -> Self {
        Self { frames, index: 0 }
    }
}

impl AudioReceiver for MockAudioReceiver {
    fn recv(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PcmAudioFrame>> + Send + '_>>
    {
        Box::pin(async move {
            if self.index < self.frames.len() {
                let frame = self.frames[self.index].clone();
                self.index += 1;
                Some(frame)
            } else {
                None
            }
        })
    }
}
