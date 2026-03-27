use crate::media::audio_frame_timing;
use crate::media::audio_source::AudioSourceManager;
use crate::media::negotiate::CodecInfo;
use crate::media::transcoder::{Transcoder, rewrite_dtmf_duration};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::MediaStreamTrack;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, Interval, MissedTickBehavior};

// ---------------------------------------------------------------------------
// Core traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait MediaSource: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}

// ---------------------------------------------------------------------------
// Mapping config types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AudioMapping {
    pub source_pt: u8,
    pub target_pt: u8,
    pub source_clock_rate: u32,
    pub target_clock_rate: u32,
    pub source_codec: audio_codec::CodecType,
    pub target_codec: audio_codec::CodecType,
}

#[derive(Clone, Debug)]
pub struct DtmfMapping {
    pub source_pt: u8,
    pub target_pt: Option<u8>,
    pub source_clock_rate: u32,
    pub target_clock_rate: Option<u32>,
}

#[derive(Clone)]
pub struct TranscodeSpec {
    pub source_codec: audio_codec::CodecType,
    pub target_codec: audio_codec::CodecType,
    pub target_pt: u8,
}

// ---------------------------------------------------------------------------
// TrackSource — adapts MediaStreamTrack into MediaSource
// ---------------------------------------------------------------------------

/// Wraps an `Arc<dyn MediaStreamTrack>` into a `MediaSource`.
pub struct TrackSource {
    inner: Arc<dyn MediaStreamTrack>,
}

impl TrackSource {
    pub fn new(track: Arc<dyn MediaStreamTrack>) -> Self {
        Self { inner: track }
    }
}

#[async_trait]
impl MediaSource for TrackSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        self.inner.recv().await
    }
}

// ---------------------------------------------------------------------------
// IdleSource — waits forever, produces nothing
// ---------------------------------------------------------------------------

pub struct IdleSource;

impl IdleSource {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MediaSource for IdleSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        std::future::pending::<MediaResult<MediaSample>>().await
    }
}

// ---------------------------------------------------------------------------
// TransformFilter — PT filtering, DTMF remapping, transcoding
// ---------------------------------------------------------------------------

/// Composable filter that applies PT filtering, DTMF remapping, and transcoding
/// on samples from an upstream `MediaSource`.
pub struct TransformFilter<S: MediaSource> {
    inner: S,
    audio_mapping: Option<AudioMapping>,
    dtmf_mapping: Option<DtmfMapping>,
    transcoder: Option<Transcoder>,
}

impl<S: MediaSource> TransformFilter<S> {
    pub fn new(
        inner: S,
        audio_mapping: Option<AudioMapping>,
        dtmf_mapping: Option<DtmfMapping>,
        transcode: Option<TranscodeSpec>,
    ) -> Self {
        let transcoder = transcode.map(|spec| {
            Transcoder::new(spec.source_codec, spec.target_codec, spec.target_pt)
        });
        Self {
            inner,
            audio_mapping,
            dtmf_mapping,
            transcoder,
        }
    }
}

#[async_trait]
impl<S: MediaSource> MediaSource for TransformFilter<S> {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        loop {
            let mut sample = self.inner.recv().await?;
            let MediaSample::Audio(frame) = &mut sample else {
                return Ok(sample);
            };

            if let Some(mapping) = self.dtmf_mapping.as_ref() {
                if frame.payload_type == Some(mapping.source_pt) {
                    let Some(target_pt) = mapping.target_pt else {
                        continue;
                    };
                    frame.payload_type = Some(target_pt);
                    if let Some(target_rate) = mapping.target_clock_rate {
                        if mapping.source_clock_rate != target_rate {
                            frame.data = rewrite_dtmf_duration(
                                &frame.data,
                                mapping.source_clock_rate,
                                target_rate,
                            );
                        }
                        frame.clock_rate = target_rate;
                    }
                    return Ok(sample);
                }
            }

            if let Some(mapping) = self.audio_mapping.as_ref() {
                if frame.payload_type != Some(mapping.source_pt) {
                    continue;
                }

                if let Some(transcoder) = self.transcoder.as_mut() {
                    let output = transcoder.transcode(frame);
                    return Ok(MediaSample::Audio(output));
                }

                frame.payload_type = Some(mapping.target_pt);
                frame.clock_rate = mapping.target_clock_rate;
                return Ok(sample);
            }

            if self.dtmf_mapping.is_none() {
                return Ok(sample);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// FileSource — playback from audio files, self-paced
// ---------------------------------------------------------------------------

pub struct FileSource {
    audio_source_manager: Arc<AudioSourceManager>,
    encoder: Box<dyn audio_codec::Encoder>,
    codec_info: CodecInfo,
    pcm_buf: Vec<i16>,
    rtp_ticks_per_frame: u32,
    ticker: Interval,
    sequence_number: u16,
    rtp_timestamp: u32,
    loop_playback: bool,
    completion_notify: Arc<Notify>,
    completion_fired: bool,
    finished: bool,
}

impl FileSource {
    pub fn new(
        file_path: String,
        loop_playback: bool,
        codec_info: CodecInfo,
        completion_notify: Arc<Notify>,
    ) -> Result<Self> {
        let is_remote = file_path.starts_with("http://") || file_path.starts_with("https://");
        if !is_remote && !std::path::Path::new(&file_path).exists() {
            return Err(anyhow!("Audio file not found: {}", file_path));
        }

        let timing = audio_frame_timing(codec_info.codec, codec_info.clock_rate);
        let audio_source_manager = Arc::new(AudioSourceManager::new(timing.pcm_sample_rate));
        audio_source_manager.switch_to_file(file_path, loop_playback)?;
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        Ok(Self {
            audio_source_manager,
            encoder: audio_codec::create_encoder(codec_info.codec),
            codec_info,
            pcm_buf: vec![0i16; timing.pcm_samples_per_frame],
            rtp_ticks_per_frame: timing.rtp_ticks_per_frame,
            ticker,
            sequence_number: rand::random(),
            rtp_timestamp: rand::random(),
            loop_playback,
            completion_notify,
            completion_fired: false,
            finished: false,
        })
    }

    fn fire_completion_once(&mut self) {
        if !self.completion_fired {
            self.completion_fired = true;
            self.completion_notify.notify_waiters();
        }
    }

    async fn finish_pending(&mut self) -> MediaResult<MediaSample> {
        self.finished = true;
        self.fire_completion_once();
        std::future::pending::<MediaResult<MediaSample>>().await
    }
}

#[async_trait]
impl MediaSource for FileSource {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        if self.finished {
            return std::future::pending::<MediaResult<MediaSample>>().await;
        }

        loop {
            self.ticker.tick().await;

            let read = self.audio_source_manager.read_samples(&mut self.pcm_buf);

            if read == 0 {
                if self.loop_playback {
                    continue;
                }
                return self.finish_pending().await;
            }

            let encoded = self.encoder.encode(&self.pcm_buf[..read]);
            let frame = AudioFrame {
                rtp_timestamp: self.rtp_timestamp,
                clock_rate: self.codec_info.clock_rate,
                data: Bytes::from(encoded),
                sequence_number: Some(self.sequence_number),
                payload_type: Some(self.codec_info.payload_type),
                marker: false,
                raw_packet: None,
                source_addr: None,
            };

            self.rtp_timestamp = self
                .rtp_timestamp
                .wrapping_add(self.rtp_ticks_per_frame);
            self.sequence_number = self.sequence_number.wrapping_add(1);

            return Ok(MediaSample::Audio(frame));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{CodecType, create_encoder};
    use bytes::Bytes;
    use rustrtc::media::frame::AudioFrame;
    use std::collections::VecDeque;

    struct FakeSource {
        samples: VecDeque<MediaSample>,
    }

    #[async_trait]
    impl MediaSource for FakeSource {
        async fn recv(&mut self) -> MediaResult<MediaSample> {
            self.samples
                .pop_front()
                .ok_or(rustrtc::media::error::MediaError::EndOfStream)
        }
    }

    fn pcmu_frame(payload_type: u8, seq: u16, timestamp: u32) -> MediaSample {
        let mut encoder = create_encoder(CodecType::PCMU);
        let payload = encoder.encode(&vec![0i16; 160]);
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: timestamp,
            clock_rate: 8000,
            data: Bytes::from(payload),
            sequence_number: Some(seq),
            payload_type: Some(payload_type),
            marker: false,
            raw_packet: None,
            source_addr: None,
        })
    }

    #[tokio::test]
    async fn test_transform_filter_remaps_same_codec_payload_type() {
        let source = FakeSource {
            samples: VecDeque::from(vec![pcmu_frame(0, 10, 160)]),
        };

        let mut transform = TransformFilter::new(
            source,
            Some(AudioMapping {
                source_pt: 0,
                target_pt: 8,
                source_clock_rate: 8000,
                target_clock_rate: 8000,
                source_codec: CodecType::PCMU,
                target_codec: CodecType::PCMU,
            }),
            None,
            None,
        );

        let MediaSample::Audio(frame) = transform.recv().await.unwrap() else {
            panic!("expected audio frame");
        };
        assert_eq!(frame.payload_type, Some(8));
    }

    #[tokio::test]
    async fn test_transform_filter_drops_unmapped_dtmf() {
        let dtmf = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 0,
            clock_rate: 8000,
            data: Bytes::from_static(&[5, 0x80, 0x00, 0xA0]),
            sequence_number: Some(1),
            payload_type: Some(101),
            marker: false,
            raw_packet: None,
            source_addr: None,
        });
        let source = FakeSource {
            samples: VecDeque::from(vec![dtmf, pcmu_frame(0, 2, 160)]),
        };

        let mut transform = TransformFilter::new(
            source,
            Some(AudioMapping {
                source_pt: 0,
                target_pt: 0,
                source_clock_rate: 8000,
                target_clock_rate: 8000,
                source_codec: CodecType::PCMU,
                target_codec: CodecType::PCMU,
            }),
            Some(DtmfMapping {
                source_pt: 101,
                target_pt: None,
                source_clock_rate: 8000,
                target_clock_rate: None,
            }),
            None,
        );

        let MediaSample::Audio(frame) = transform.recv().await.unwrap() else {
            panic!("expected audio frame");
        };
        assert_eq!(frame.payload_type, Some(0));
    }

    #[tokio::test]
    async fn test_transform_filter_transcodes() {
        let source = FakeSource {
            samples: VecDeque::from(vec![pcmu_frame(0, 10, 160)]),
        };

        let mut transform = TransformFilter::new(
            source,
            Some(AudioMapping {
                source_pt: 0,
                target_pt: 8,
                source_clock_rate: 8000,
                target_clock_rate: 8000,
                source_codec: CodecType::PCMU,
                target_codec: CodecType::PCMA,
            }),
            None,
            Some(TranscodeSpec {
                source_codec: CodecType::PCMU,
                target_codec: CodecType::PCMA,
                target_pt: 8,
            }),
        );

        let MediaSample::Audio(frame) = transform.recv().await.unwrap() else {
            panic!("expected audio frame");
        };
        assert_eq!(frame.payload_type, Some(8));
        assert_eq!(frame.clock_rate, 8000);
    }
}
