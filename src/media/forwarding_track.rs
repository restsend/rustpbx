use crate::media::recorder::Leg;
use crate::media::transcoder::{RtpTiming, Transcoder, rewrite_dtmf_duration};
use async_trait::async_trait;
use parking_lot::Mutex;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct AudioMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
}

/// Mapping for a single DTMF PT from source to target, or drop if target is absent.
#[derive(Clone, Debug)]
pub struct DtmfMapping {
    pub source_pt: u8,
    pub target_pt: Option<u8>,
    pub source_clock_rate: u32,
    pub target_clock_rate: Option<u32>,
}

/// A wrapper track that sits between a source PC's receiver and a target PC's sender.
///
/// RtpSender's built-in loop calls `recv()` on this track, so all forwarding logic
/// (recording, transcoding, DTMF handling) happens inline with zero additional tasks.
pub struct ForwardingTrack {
    track_id: String,
    inner: Arc<dyn MediaStreamTrack>,
    transcoder: Mutex<Option<Transcoder>>,
    audio_mapping: Option<AudioMapping>,
    /// Timestamp rewriting for audio frames (when audio clock rates differ)
    audio_timing: Mutex<Option<RtpTiming>>,
    /// Timestamp rewriting for DTMF frames (when DTMF clock rates differ)
    dtmf_timing: Mutex<Option<RtpTiming>>,
    /// Issue #171: recording is dispatched via a bounded channel to a
    /// dedicated task so that codec decoding and disk I/O never block the
    /// RTP forwarding hot path.  The channel is intentionally bounded;
    /// if the recorder task falls behind (e.g. disk pressure) we drop
    /// the sample rather than accumulate unbounded memory.
    recorder_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
    recorder_leg: Leg,
    dtmf_mapping: Option<DtmfMapping>,
}

impl ForwardingTrack {
    pub fn new(
        track_id: String,
        inner: Arc<dyn MediaStreamTrack>,
        transcoder: Option<Transcoder>,
        audio_mapping: Option<AudioMapping>,
        recorder_tx: Option<mpsc::Sender<(Leg, MediaSample)>>,
        recorder_leg: Leg,
        dtmf_mapping: Option<DtmfMapping>,
    ) -> Self {
        let audio_timing = audio_mapping.as_ref().and_then(|mapping| {
            if mapping.source_clock_rate != mapping.target_clock_rate
                || mapping.source_pt != mapping.target_pt
            {
                Some(RtpTiming::default())
            } else {
                None
            }
        });

        let dtmf_timing = dtmf_mapping.as_ref().and_then(|mapping| {
            mapping.target_clock_rate.and_then(|target_clock_rate| {
                if mapping.source_clock_rate != target_clock_rate {
                    Some(RtpTiming::default())
                } else {
                    None
                }
            })
        });

        Self {
            track_id,
            inner,
            transcoder: Mutex::new(transcoder),
            audio_mapping,
            audio_timing: Mutex::new(audio_timing),
            dtmf_timing: Mutex::new(dtmf_timing),
            recorder_tx,
            recorder_leg,
            dtmf_mapping,
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
        loop {
            let sample = self.inner.recv().await?;

            if let Some(tx) = &self.recorder_tx {
                let _ = tx.try_send((self.recorder_leg, sample.clone()));
            }

            if let MediaSample::Audio(ref frame) = sample {
                let matched_dtmf = self
                    .dtmf_mapping
                    .as_ref()
                    .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));
                let matched_audio = self
                    .audio_mapping
                    .as_ref()
                    .is_some_and(|mapping| frame.payload_type == Some(mapping.source_pt));

                if (self.audio_mapping.is_some() || self.dtmf_mapping.is_some())
                    && !matched_audio
                    && !matched_dtmf
                {
                    continue;
                }

                if let Some(mapping) = self.dtmf_mapping.as_ref().filter(|_| matched_dtmf) {
                    if let Some(target_pt) = mapping.target_pt {
                        let mut dtmf_frame = frame.clone();
                        dtmf_frame.payload_type = Some(target_pt);

                        if let Some(target_clock_rate) = mapping.target_clock_rate {
                            if mapping.source_clock_rate != target_clock_rate {
                                dtmf_frame.data = rewrite_dtmf_duration(
                                    &dtmf_frame.data,
                                    mapping.source_clock_rate,
                                    target_clock_rate,
                                );
                                let mut guard = self.dtmf_timing.lock();
                                if let Some(timing) = guard.as_mut() {
                                    timing.rewrite(
                                        &mut dtmf_frame,
                                        mapping.source_clock_rate,
                                        target_clock_rate,
                                        target_pt,
                                    );
                                }
                            }
                        }

                        return Ok(MediaSample::Audio(dtmf_frame));
                    }

                    // Source sent telephone-event but the target leg did not negotiate it.
                    continue;
                }

                if let Some(audio_mapping) = self.audio_mapping.as_ref().filter(|_| matched_audio) {
                    let mut guard = self.transcoder.lock();
                    if let Some(transcoder) = guard.as_mut() {
                        let mut output = transcoder.transcode(frame);

                        let mut timing_guard = self.audio_timing.lock();
                        if let Some(timing) = timing_guard.as_mut() {
                            timing.rewrite(
                                &mut output,
                                audio_mapping.source_clock_rate,
                                audio_mapping.target_clock_rate,
                                audio_mapping.target_pt,
                            );
                        }

                        return Ok(MediaSample::Audio(output));
                    }

                    if frame.payload_type != Some(audio_mapping.target_pt)
                        || audio_mapping.source_clock_rate != audio_mapping.target_clock_rate
                    {
                        let mut output = frame.clone();
                        let mut timing_guard = self.audio_timing.lock();
                        if let Some(timing) = timing_guard.as_mut() {
                            timing.rewrite(
                                &mut output,
                                audio_mapping.source_clock_rate,
                                audio_mapping.target_clock_rate,
                                audio_mapping.target_pt,
                            );
                        } else {
                            output.payload_type = Some(audio_mapping.target_pt);
                            output.clock_rate = audio_mapping.target_clock_rate;
                        }

                        return Ok(MediaSample::Audio(output));
                    }
                }
            }

            return Ok(sample);
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        self.inner.request_key_frame().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rustrtc::media::frame::AudioFrame;

    /// Minimal track that yields exactly one pre-defined sample then blocks.
    struct OneShotTrack {
        sample: tokio::sync::Mutex<Option<MediaSample>>,
    }

    impl OneShotTrack {
        fn new(sample: MediaSample) -> Arc<Self> {
            Arc::new(Self {
                sample: tokio::sync::Mutex::new(Some(sample)),
            })
        }
    }

    #[async_trait::async_trait]
    impl MediaStreamTrack for OneShotTrack {
        fn id(&self) -> &str {
            "one-shot"
        }
        fn kind(&self) -> MediaKind {
            MediaKind::Audio
        }
        fn state(&self) -> TrackState {
            TrackState::Live
        }
        async fn recv(&self) -> MediaResult<MediaSample> {
            loop {
                let mut guard = self.sample.lock().await;
                if let Some(s) = guard.take() {
                    return Ok(s);
                }
                // Block until the test polls again — simulate a quiet stream.
                drop(guard);
                tokio::time::sleep(std::time::Duration::from_secs(999)).await;
            }
        }
        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    fn audio_sample(pt: u8) -> MediaSample {
        let frame = AudioFrame {
            payload_type: Some(pt),
            clock_rate: 8000,
            data: Bytes::from_static(&[0u8; 160]),
            ..Default::default()
        };
        MediaSample::Audio(frame)
    }

    /// Issue #171: when a recorder_tx is wired the sample must be forwarded to
    /// the channel without blocking recv(), and must NOT be dropped.
    #[tokio::test]
    async fn sample_forwarded_to_recorder_channel() {
        let (tx, mut rx) = mpsc::channel::<(Leg, MediaSample)>(256);
        let sample = audio_sample(0 /* PCMU */);
        let track = OneShotTrack::new(sample.clone());

        let ft = ForwardingTrack::new(
            "test".to_string(),
            track,
            None,
            None,
            Some(tx),
            Leg::A,
            None,
        );

        // Drive recv() once; because there is no audio_mapping the sample is
        // returned immediately AND should also have been sent to the channel.
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        // The forwarded sample is returned to the caller unchanged.
        assert!(matches!(result, MediaSample::Audio(_)));

        // The channel must have received the sample too (non-blocking assertion).
        let (leg, _chan_sample) = rx.try_recv().expect("sample must be in recorder channel");
        assert_eq!(leg, Leg::A);
    }

    /// Without a recorder_tx, recv() must still work without panicking.
    #[tokio::test]
    async fn no_recorder_tx_is_noop() {
        let sample = audio_sample(0);
        let track = OneShotTrack::new(sample);

        let ft = ForwardingTrack::new(
            "test-no-rec".to_string(),
            track,
            None,
            None,
            None, // no recorder channel
            Leg::B,
            None,
        );

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), ft.recv())
            .await
            .expect("recv timed out")
            .expect("recv error");

        assert!(matches!(result, MediaSample::Audio(_)));
    }
}
