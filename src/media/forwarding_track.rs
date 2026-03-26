use crate::media::recorder::{Leg, Recorder};
use crate::media::transcoder::{RtpTiming, Transcoder, rewrite_dtmf_duration};
use async_trait::async_trait;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{MediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use std::sync::{Arc, Mutex};

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
    recorder: Arc<Mutex<Option<Recorder>>>,
    recorder_leg: Leg,
    dtmf_mapping: Option<DtmfMapping>,
}

impl ForwardingTrack {
    pub fn new(
        track_id: String,
        inner: Arc<dyn MediaStreamTrack>,
        transcoder: Option<Transcoder>,
        audio_mapping: Option<AudioMapping>,
        recorder: Arc<Mutex<Option<Recorder>>>,
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
            recorder,
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

            // Tee to recorder (raw incoming, before transcode)
            if let Ok(mut recorder_guard) = self.recorder.lock() {
                if let Some(recorder) = recorder_guard.as_mut() {
                    let _ = recorder.write_sample(self.recorder_leg, &sample, None, None, None);
                }
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

                if let Some(mapping) = self
                    .dtmf_mapping
                    .as_ref()
                    .filter(|_| matched_dtmf)
                {
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
                                if let Ok(mut guard) = self.dtmf_timing.lock() {
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
                        }

                        return Ok(MediaSample::Audio(dtmf_frame));
                    }

                    // Source sent telephone-event but the target leg did not negotiate it.
                    continue;
                }

                if let Some(audio_mapping) = self
                    .audio_mapping
                    .as_ref()
                    .filter(|_| matched_audio)
                {
                    if let Ok(mut guard) = self.transcoder.lock() {
                        if let Some(transcoder) = guard.as_mut() {
                            let mut output = transcoder.transcode(frame);

                            if let Ok(mut timing_guard) = self.audio_timing.lock() {
                                if let Some(timing) = timing_guard.as_mut() {
                                    timing.rewrite(
                                        &mut output,
                                        audio_mapping.source_clock_rate,
                                        audio_mapping.target_clock_rate,
                                        audio_mapping.target_pt,
                                    );
                                }
                            }

                            return Ok(MediaSample::Audio(output));
                        }
                    }

                    if frame.payload_type != Some(audio_mapping.target_pt)
                        || audio_mapping.source_clock_rate != audio_mapping.target_clock_rate
                    {
                        let mut output = frame.clone();
                        if let Ok(mut timing_guard) = self.audio_timing.lock() {
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
