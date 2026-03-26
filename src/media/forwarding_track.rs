use crate::media::recorder::{Leg, Recorder};
use crate::media::transcoder::Transcoder;
use async_trait::async_trait;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::{Arc, Mutex};

/// A wrapper track that sits between a source PC's receiver and a target PC's sender.
///
/// RtpSender's built-in loop calls `recv()` on this track, so all forwarding logic
/// (recording, transcoding) happens inline with zero additional tasks.
pub struct ForwardingTrack {
    track_id: String,
    inner: Arc<dyn MediaStreamTrack>,
    transcoder: Mutex<Option<Transcoder>>,
    recorder: Arc<Mutex<Option<Recorder>>>,
    recorder_leg: Leg,
}

impl ForwardingTrack {
    pub fn new(
        track_id: String,
        inner: Arc<dyn MediaStreamTrack>,
        transcoder: Option<Transcoder>,
        recorder: Arc<Mutex<Option<Recorder>>>,
        recorder_leg: Leg,
    ) -> Self {
        Self {
            track_id,
            inner,
            transcoder: Mutex::new(transcoder),
            recorder,
            recorder_leg,
        }
    }
}

#[async_trait]
impl MediaStreamTrack for ForwardingTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    fn kind(&self) -> MediaKind {
        self.inner.kind()
    }

    fn state(&self) -> TrackState {
        self.inner.state()
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        let sample = self.inner.recv().await?;

        // Tee to recorder (raw incoming, before transcode)
        if let Ok(mut recorder_guard) = self.recorder.lock() {
            if let Some(recorder) = recorder_guard.as_mut() {
                let _ = recorder.write_sample(self.recorder_leg, &sample, None, None, None);
            }
        }

        // Transcode payload if needed
        if let Ok(mut guard) = self.transcoder.lock() {
            if let Some(ref mut transcoder) = *guard {
                if let MediaSample::Audio(ref frame) = sample {
                    return Ok(MediaSample::Audio(transcoder.transcode(frame)));
                }
            }
        }

        Ok(sample)
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        self.inner.request_key_frame().await
    }
}
