//! Mock implementations of [`AudioSender`] and [`AudioReceiver`] for
//! conference media-bridge integration tests (mirrors the in-crate
//! `call::runtime::test_utils` used by unit tests).

use std::sync::Arc;

use rustrtc::media::MediaSample;
use tokio::sync::mpsc;

use rustpbx::call::runtime::conference_media_bridge::{
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
        self::MockAudioSender::new()
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
