use crate::media::audio_frame_timing;
use crate::media::audio_source::AudioSourceManager;
use crate::media::negotiate::{CodecInfo, NegotiatedCodec, NegotiatedLegProfile};
use crate::media::recorder::{Leg, Recorder};
use crate::media::transcoder::{Transcoder, rewrite_dtmf_duration};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::MediaStreamTrack;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::{Duration, Interval, MissedTickBehavior};

#[async_trait]
pub trait PeerInputSource: Send {
    async fn recv(&mut self) -> MediaResult<MediaSample>;
}

#[derive(Clone, Default)]
pub struct BridgeInputConfig {
    pub source: NegotiatedLegProfile,
    pub target: NegotiatedLegProfile,
    pub recorder: Option<SharedRecorder>,
    pub record_leg: Option<Leg>,
}

pub type SharedRecorder = Arc<Mutex<Option<Recorder>>>;

impl BridgeInputConfig {
    pub fn from_profiles(source: &NegotiatedLegProfile, target: &NegotiatedLegProfile) -> Self {
        Self {
            source: source.clone(),
            target: target.clone(),
            recorder: None,
            record_leg: None,
        }
    }

    pub fn with_recorder(mut self, recorder: SharedRecorder, record_leg: Leg) -> Self {
        self.recorder = Some(recorder);
        self.record_leg = Some(record_leg);
        self
    }

    pub fn is_identity_config(&self) -> bool {
        self.source.audio.is_none()
            && self.source.dtmf.is_none()
            && self.target.audio.is_none()
            && self.target.dtmf.is_none()
            && self.recorder.is_none()
            && self.record_leg.is_none()
    }
}

/// Input adapter built from a peer's inbound media track.
///
/// This owns all per-direction adaptation for a single bridge direction:
/// selected PT admission, recorder tap, optional transcoding, and RTP
/// timestamp rescaling.
pub struct BridgeInputAdapter {
    track: Arc<dyn MediaStreamTrack>,
    config: BridgeInputConfig,
    transcoder: Option<Transcoder>,
    timestamp_anchor: Option<(u32, u32)>,
}

impl BridgeInputAdapter {
    pub fn new(track: Arc<dyn MediaStreamTrack>, config: BridgeInputConfig) -> Self {
        let transcoder = match (&config.source.audio, &config.target.audio) {
            (Some(source), Some(target)) if source.codec != target.codec => {
                Some(Transcoder::new(source.codec, target.codec, target.payload_type))
            }
            _ => None,
        };
        Self {
            track,
            config,
            transcoder,
            timestamp_anchor: None,
        }
    }

    fn source_audio(&self) -> Option<&NegotiatedCodec> {
        self.config.source.audio.as_ref()
    }

    fn target_audio(&self) -> Option<&NegotiatedCodec> {
        self.config.target.audio.as_ref()
    }

    fn source_dtmf(&self) -> Option<&NegotiatedCodec> {
        self.config.source.dtmf.as_ref()
    }

    fn target_dtmf(&self) -> Option<&NegotiatedCodec> {
        self.config.target.dtmf.as_ref()
    }

    fn rewrite_timestamp(&mut self, frame: &mut AudioFrame, source_clock_rate: u32, target_clock_rate: u32) {
        frame.clock_rate = target_clock_rate;
        if source_clock_rate == 0 || source_clock_rate == target_clock_rate {
            return;
        }

        let (input_anchor, output_anchor) = self
            .timestamp_anchor
            .get_or_insert((frame.rtp_timestamp, frame.rtp_timestamp));
        let input_delta = frame.rtp_timestamp.wrapping_sub(*input_anchor);
        let output_delta =
            (input_delta as u64 * target_clock_rate as u64 / source_clock_rate as u64) as u32;
        frame.rtp_timestamp = output_anchor.wrapping_add(output_delta);
    }

    fn write_to_recorder(
        &mut self,
        sample: &MediaSample,
        source_codec: Option<audio_codec::CodecType>,
        source_dtmf: Option<&NegotiatedCodec>,
    ) {
        let (Some(recorder), Some(record_leg)) = (&self.config.recorder, self.config.record_leg) else {
            return;
        };
        let Ok(mut guard) = recorder.lock() else {
            return;
        };
        let Some(recorder) = guard.as_mut() else {
            return;
        };
        let dtmf_pt = source_dtmf.map(|codec| codec.payload_type);
        let dtmf_clock_rate = source_dtmf.map(|codec| codec.clock_rate);
        let _ = recorder.write_sample(record_leg, sample, dtmf_pt, dtmf_clock_rate, source_codec);
    }
}

#[async_trait]
impl PeerInputSource for BridgeInputAdapter {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        loop {
            let mut sample = self.track.recv().await?;
            let MediaSample::Audio(frame) = &mut sample else {
                return Ok(sample);
            };

            let source_dtmf = self.source_dtmf().cloned();
            if source_dtmf
                .as_ref()
                .is_some_and(|codec| frame.payload_type == Some(codec.payload_type))
            {
                let Some(target_dtmf) = self.target_dtmf().cloned() else {
                    continue;
                };

                let original = MediaSample::Audio(frame.clone());
                self.write_to_recorder(&original, None, source_dtmf.as_ref());

                frame.payload_type = Some(target_dtmf.payload_type);
                if source_dtmf.as_ref().map(|codec| codec.clock_rate) != Some(target_dtmf.clock_rate)
                {
                    frame.data = rewrite_dtmf_duration(
                        &frame.data,
                        source_dtmf.as_ref().map(|codec| codec.clock_rate).unwrap_or(frame.clock_rate),
                        target_dtmf.clock_rate,
                    );
                }
                self.rewrite_timestamp(
                    frame,
                    source_dtmf.as_ref().map(|codec| codec.clock_rate).unwrap_or(frame.clock_rate),
                    target_dtmf.clock_rate,
                );
                frame.raw_packet = None;
                return Ok(sample);
            }

            if let Some(source_audio) = self.source_audio().cloned() {
                if frame.payload_type != Some(source_audio.payload_type) {
                    continue;
                }

                let original = MediaSample::Audio(frame.clone());
                self.write_to_recorder(&original, Some(source_audio.codec), None);

                let Some(target_audio) = self.target_audio().cloned() else {
                    continue;
                };
                if let Some(transcoder) = self.transcoder.as_mut() {
                    let mut output = transcoder.transcode(frame);
                    self.rewrite_timestamp(
                        &mut output,
                        source_audio.clock_rate,
                        target_audio.clock_rate,
                    );
                    output.payload_type = Some(target_audio.payload_type);
                    output.raw_packet = None;
                    return Ok(MediaSample::Audio(output));
                }

                frame.payload_type = Some(target_audio.payload_type);
                self.rewrite_timestamp(frame, source_audio.clock_rate, target_audio.clock_rate);
                frame.raw_packet = None;
                return Ok(sample);
            }

            if self.source_dtmf().is_none() {
                return Ok(sample);
            }
        }
    }
}

pub struct IdleInput;

impl IdleInput {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PeerInputSource for IdleInput {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        std::future::pending::<MediaResult<MediaSample>>().await
    }
}

pub struct FileInput {
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

impl FileInput {
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
impl PeerInputSource for FileInput {
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

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{CodecType, create_encoder};
    use bytes::Bytes;
    use rustrtc::media::error::MediaError;
    use rustrtc::media::frame::AudioFrame;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    struct FakeTrack {
        samples: tokio::sync::Mutex<VecDeque<MediaSample>>,
    }

    #[async_trait]
    impl MediaStreamTrack for FakeTrack {
        fn id(&self) -> &str {
            "fake"
        }

        fn kind(&self) -> rustrtc::media::frame::MediaKind {
            rustrtc::media::frame::MediaKind::Audio
        }

        fn state(&self) -> rustrtc::media::track::TrackState {
            rustrtc::media::track::TrackState::Live
        }

        async fn recv(&self) -> MediaResult<MediaSample> {
            self.samples
                .lock()
                .await
                .pop_front()
                .ok_or(MediaError::EndOfStream)
        }

        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
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

    fn audio_profile(payload_type: u8, codec: CodecType, clock_rate: u32) -> NegotiatedLegProfile {
        NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                codec,
                payload_type,
                clock_rate,
                channels: 1,
            }),
            dtmf: None,
        }
    }

    fn dtmf_profile(payload_type: u8, clock_rate: u32) -> NegotiatedLegProfile {
        NegotiatedLegProfile {
            audio: None,
            dtmf: Some(NegotiatedCodec {
                codec: CodecType::TelephoneEvent,
                payload_type,
                clock_rate,
                channels: 1,
            }),
        }
    }

    #[tokio::test]
    async fn bridge_input_adapter_remaps_audio_payload_type() {
        let track = Arc::new(FakeTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([pcmu_frame(0, 10, 1234)])),
        });
        let mut input = BridgeInputAdapter::new(
            track,
            BridgeInputConfig {
                source: audio_profile(0, CodecType::PCMU, 8000),
                target: audio_profile(8, CodecType::PCMA, 8000),
                recorder: None,
                record_leg: None,
            },
        );

        let sample = input.recv().await.unwrap();
        let MediaSample::Audio(frame) = sample else {
            panic!("expected audio sample");
        };
        assert_eq!(frame.payload_type, Some(8));
        assert_eq!(frame.clock_rate, 8000);
    }

    #[tokio::test]
    async fn bridge_input_adapter_drops_unselected_payload_type() {
        let track = Arc::new(FakeTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([
                pcmu_frame(111, 1, 1000),
                pcmu_frame(0, 2, 1160),
            ])),
        });
        let mut input = BridgeInputAdapter::new(
            track,
            BridgeInputConfig {
                source: audio_profile(0, CodecType::PCMU, 8000),
                target: audio_profile(8, CodecType::PCMA, 8000),
                recorder: None,
                record_leg: None,
            },
        );

        let sample = input.recv().await.unwrap();
        let MediaSample::Audio(frame) = sample else {
            panic!("expected audio sample");
        };
        assert_eq!(frame.payload_type, Some(8));
        assert_eq!(frame.rtp_timestamp, 1160);
    }

    #[tokio::test]
    async fn bridge_input_adapter_rewrites_dtmf() {
        let dtmf = MediaSample::Audio(AudioFrame {
            rtp_timestamp: 480,
            clock_rate: 48000,
            data: Bytes::from_static(&[1, 0x80, 0x12, 0xC0]),
            sequence_number: Some(7),
            payload_type: Some(101),
            marker: false,
            raw_packet: None,
            source_addr: None,
        });
        let track = Arc::new(FakeTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([dtmf])),
        });
        let mut input = BridgeInputAdapter::new(
            track,
            BridgeInputConfig {
                source: dtmf_profile(101, 48000),
                target: dtmf_profile(97, 8000),
                recorder: None,
                record_leg: None,
            },
        );

        let sample = input.recv().await.unwrap();
        let MediaSample::Audio(frame) = sample else {
            panic!("expected audio sample");
        };
        assert_eq!(frame.payload_type, Some(97));
        assert_eq!(frame.clock_rate, 8000);
        assert_eq!(&frame.data[..], &[1, 0x80, 0x03, 0x20]);
        assert_eq!(frame.rtp_timestamp, 480);
        assert_eq!(frame.sequence_number, Some(7));
    }

    #[tokio::test]
    async fn bridge_input_adapter_rescales_audio_timestamp_from_anchor() {
        let track = Arc::new(FakeTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([
                MediaSample::Audio(AudioFrame {
                    rtp_timestamp: 48_000,
                    clock_rate: 48_000,
                    data: Bytes::from_static(&[1, 2, 3]),
                    sequence_number: Some(9),
                    payload_type: Some(96),
                    marker: false,
                    raw_packet: None,
                    source_addr: None,
                }),
                MediaSample::Audio(AudioFrame {
                    rtp_timestamp: 48_960,
                    clock_rate: 48_000,
                    data: Bytes::from_static(&[4, 5, 6]),
                    sequence_number: Some(10),
                    payload_type: Some(96),
                    marker: false,
                    raw_packet: None,
                    source_addr: None,
                }),
            ])),
        });
        let mut input = BridgeInputAdapter::new(
            track,
            BridgeInputConfig {
                source: audio_profile(96, CodecType::Opus, 48_000),
                target: audio_profile(9, CodecType::G722, 8_000),
                recorder: None,
                record_leg: None,
            },
        );

        let first = input.recv().await.unwrap();
        let MediaSample::Audio(first) = first else {
            panic!("expected audio sample");
        };
        assert_eq!(first.rtp_timestamp, 48_000);
        assert_eq!(first.sequence_number, Some(9));

        let second = input.recv().await.unwrap();
        let MediaSample::Audio(second) = second else {
            panic!("expected audio sample");
        };
        assert_eq!(second.rtp_timestamp, 48_160);
        assert_eq!(second.sequence_number, Some(10));
    }

    #[tokio::test]
    async fn bridge_input_adapter_records_original_sample_before_rewrite() {
        let temp = NamedTempFile::with_suffix(".wav").unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let recorder = Arc::new(Mutex::new(Some(Recorder::new(&path, CodecType::PCMU).unwrap())));
        let track = Arc::new(FakeTrack {
            samples: tokio::sync::Mutex::new(VecDeque::from([pcmu_frame(0, 10, 1234)])),
        });
        let mut input = BridgeInputAdapter::new(
            track,
            BridgeInputConfig {
                source: audio_profile(0, CodecType::PCMU, 8000),
                target: audio_profile(0, CodecType::PCMU, 8000),
                recorder: Some(recorder.clone()),
                record_leg: Some(Leg::A),
            },
        );

        let sample = input.recv().await.unwrap();
        let MediaSample::Audio(frame) = sample else {
            panic!("expected audio sample");
        };
        assert_eq!(frame.payload_type, Some(0));
        assert_eq!(frame.rtp_timestamp, 1234);
        assert_eq!(frame.sequence_number, Some(10));

        let mut guard = recorder.lock().unwrap();
        let recorder = guard.as_mut().expect("recorder");
        recorder.finalize().unwrap();
        drop(guard);

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() > 44);
    }
}
