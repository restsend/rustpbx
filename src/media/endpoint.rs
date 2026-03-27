use crate::media::recorder::{Leg, Recorder};
use crate::media::source::{
    AudioMapping, DtmfMapping, FileSource, IdleSource, MediaSource, TrackSource, TranscodeSpec,
    TransformFilter,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use audio_codec::CodecType;
use rustrtc::media::error::MediaResult;
use rustrtc::media::frame::{AudioFrame, MediaKind as TrackMediaKind, MediaSample};
use rustrtc::media::track::{MediaStreamTrack, TrackState};
use rustrtc::{MediaKind, PeerConnection, RtpSender};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, Notify};

// ---------------------------------------------------------------------------
// RecordingTap — records inline, passes through unmodified
// ---------------------------------------------------------------------------

/// A tap that records each sample before passing it through.
/// Generic over the upstream source — zero-cost composition.
struct RecordingTap<S: MediaSource> {
    inner: S,
    binding: RecorderBinding,
}

#[async_trait]
impl<S: MediaSource> MediaSource for RecordingTap<S> {
    async fn recv(&mut self) -> MediaResult<MediaSample> {
        let sample = self.inner.recv().await?;
        if let Ok(mut guard) = self.binding.recorder.lock() {
            if let Some(recorder) = guard.as_mut() {
                let _ = recorder.write_sample(
                    self.binding.leg,
                    &sample,
                    self.binding.dtmf_pt,
                    self.binding.dtmf_clock_rate,
                    self.binding.codec_hint,
                );
            }
        }
        Ok(sample)
    }
}

// ---------------------------------------------------------------------------
// OutputRtpState — RTP continuity across source switches
// ---------------------------------------------------------------------------

/// RTP continuity state owned by PeerOutput.
/// Rewrites outbound timestamps/sequences to maintain a continuous timeline
/// across source switches (peer → file → peer).
struct OutputRtpState {
    next_output_timestamp: u32,
    next_output_sequence: u16,
    last_input_timestamp: Option<u32>,
    last_input_sequence: Option<u16>,
    last_clock_rate: Option<u32>,
    last_step: u32,
}

impl Default for OutputRtpState {
    fn default() -> Self {
        Self {
            next_output_timestamp: rand::random(),
            next_output_sequence: rand::random(),
            last_input_timestamp: None,
            last_input_sequence: None,
            last_clock_rate: None,
            last_step: 160,
        }
    }
}

impl OutputRtpState {
    fn estimate_step(frame: &AudioFrame) -> u32 {
        (frame.clock_rate / 50).max(1)
    }

    fn rewrite(&mut self, frame: &mut AudioFrame) {
        let input_timestamp = frame.rtp_timestamp;
        let input_sequence = frame.sequence_number;
        let step = match (
            self.last_input_timestamp,
            self.last_input_sequence,
            self.last_clock_rate,
            input_sequence,
        ) {
            (Some(last_ts), Some(last_seq), Some(last_rate), Some(input_seq))
                if last_rate == frame.clock_rate && input_seq == last_seq.wrapping_add(1) =>
            {
                let delta = input_timestamp.wrapping_sub(last_ts);
                if delta > 0 && delta < frame.clock_rate.saturating_mul(5) {
                    delta
                } else {
                    Self::estimate_step(frame)
                }
            }
            _ => Self::estimate_step(frame),
        };

        frame.rtp_timestamp = self.next_output_timestamp;
        frame.sequence_number = Some(self.next_output_sequence);

        self.next_output_timestamp = self.next_output_timestamp.wrapping_add(step);
        self.next_output_sequence = self.next_output_sequence.wrapping_add(1);
        self.last_input_timestamp = Some(input_timestamp);
        self.last_input_sequence = input_sequence;
        self.last_clock_rate = Some(frame.clock_rate);
        self.last_step = step;
    }
}

// ---------------------------------------------------------------------------
// PeerOutput — output side of a phone endpoint
// ---------------------------------------------------------------------------

/// Output side of a phone endpoint.
///
/// Owns the current media source (switchable), RTP timing continuity,
/// and implements `MediaStreamTrack` so rustrtc's `RtpSender` can poll it.
pub struct PeerOutput {
    id: String,
    kind: TrackMediaKind,
    source: Mutex<Box<dyn MediaSource>>,
    pending: StdMutex<Option<Box<dyn MediaSource>>>,
    wake: Notify,
    rtp_state: StdMutex<OutputRtpState>,
}

impl PeerOutput {
    /// Attach a new PeerOutput to a PeerConnection's audio transceiver.
    pub fn attach(track_id: &str, target_pc: &PeerConnection) -> Result<Arc<Self>> {
        let output = Arc::new(Self {
            id: track_id.to_string(),
            kind: TrackMediaKind::Audio,
            source: Mutex::new(Box::new(IdleSource::new())),
            pending: StdMutex::new(None),
            wake: Notify::new(),
            rtp_state: StdMutex::new(OutputRtpState::default()),
        });

        let target_transceiver = target_pc
            .get_transceivers()
            .into_iter()
            .find(|t| t.kind() == MediaKind::Audio)
            .ok_or_else(|| anyhow!("no audio transceiver on target pc"))?;

        let existing_sender = target_transceiver
            .sender()
            .ok_or_else(|| anyhow!("no sender on target audio transceiver"))?;

        let sender = RtpSender::builder(
            output.clone() as Arc<dyn MediaStreamTrack>,
            existing_sender.ssrc(),
        )
        .stream_id(existing_sender.stream_id().to_string())
        .params(existing_sender.params())
        .build();

        target_transceiver.set_sender(Some(sender));

        Ok(output)
    }

    /// Replace the active media source. The old source is dropped on next recv().
    pub fn set_source(&self, source: Box<dyn MediaSource>) {
        *self.pending.lock().unwrap() = Some(source);
        self.wake.notify_waiters();
    }

    /// Remove the active source (switch to idle/silent).
    pub fn clear_source(&self) {
        self.set_source(Box::new(IdleSource::new()));
    }

    /// Convenience: install a FileSource for playback.
    pub fn set_file_source(&self, source: FileSource) {
        self.set_source(Box::new(source));
    }
}

#[async_trait]
impl MediaStreamTrack for PeerOutput {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> TrackMediaKind {
        self.kind
    }

    fn state(&self) -> TrackState {
        TrackState::Live
    }

    async fn recv(&self) -> MediaResult<MediaSample> {
        let mut source = self.source.lock().await;
        loop {
            if let Some(new) = self.pending.lock().unwrap().take() {
                *source = new;
            }
            tokio::select! {
                result = source.recv() => {
                    let mut sample = result?;
                    if let MediaSample::Audio(frame) = &mut sample {
                        if let Ok(mut rtp_state) = self.rtp_state.lock() {
                            rtp_state.rewrite(frame);
                        }
                    }
                    return Ok(sample);
                },
                _ = self.wake.notified() => continue,
            }
        }
    }

    async fn request_key_frame(&self) -> MediaResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PeerEndpoint — one phone leg (caller or callee)
// ---------------------------------------------------------------------------

/// Recording binding on the input side of a PeerEndpoint.
#[derive(Clone)]
struct RecorderBinding {
    recorder: Arc<StdMutex<Option<Recorder>>>,
    leg: Leg,
    dtmf_pt: Option<u8>,
    dtmf_clock_rate: Option<u32>,
    codec_hint: Option<CodecType>,
}

/// A phone endpoint representing one leg of a call.
///
/// Input side: reads from the remote peer's receiver track, records original packets.
/// Output side: `PeerOutput` sends RTP to the remote peer with continuous timing.
pub struct PeerEndpoint {
    receiver_track: Arc<dyn MediaStreamTrack>,
    recorder: Option<RecorderBinding>,
    output: Arc<PeerOutput>,
}

impl PeerEndpoint {
    pub fn new(
        receiver_track: Arc<dyn MediaStreamTrack>,
        output: Arc<PeerOutput>,
    ) -> Self {
        Self {
            receiver_track,
            recorder: None,
            output,
        }
    }

    /// Attach a recorder to the input side of this endpoint.
    /// Records original packets (pre-transform) for the given leg.
    pub fn set_recorder(
        &mut self,
        recorder: Arc<StdMutex<Option<Recorder>>>,
        leg: Leg,
        dtmf_pt: Option<u8>,
        dtmf_clock_rate: Option<u32>,
        codec_hint: Option<CodecType>,
    ) {
        self.recorder = Some(RecorderBinding {
            recorder,
            leg,
            dtmf_pt,
            dtmf_clock_rate,
            codec_hint,
        });
    }

    /// Get the receiver track.
    pub fn receiver_track(&self) -> Arc<dyn MediaStreamTrack> {
        self.receiver_track.clone()
    }

    /// Get the output for setting sources on it.
    pub fn output(&self) -> &Arc<PeerOutput> {
        &self.output
    }

    /// Build a source pipeline from this endpoint's receiver track and install
    /// it on the target output.
    ///
    /// Pipeline composition (generics, boxed once at the end):
    /// - With recorder: TrackSource → RecordingTap → TransformFilter → box → PeerOutput
    /// - Without recorder: TrackSource → TransformFilter → box → PeerOutput
    pub fn bridge_to(
        &self,
        target_output: &Arc<PeerOutput>,
        audio_mapping: Option<AudioMapping>,
        dtmf_mapping: Option<DtmfMapping>,
        transcode: Option<TranscodeSpec>,
    ) {
        let track = TrackSource::new(self.receiver_track.clone());

        let source: Box<dyn MediaSource> = if let Some(binding) = &self.recorder {
            let tap = RecordingTap {
                inner: track,
                binding: binding.clone(),
            };
            Box::new(TransformFilter::new(tap, audio_mapping, dtmf_mapping, transcode))
        } else {
            Box::new(TransformFilter::new(track, audio_mapping, dtmf_mapping, transcode))
        };

        target_output.set_source(source);
    }
}

// ---------------------------------------------------------------------------
// DirectionConfig — per-direction transform config
// ---------------------------------------------------------------------------

/// Transform config for one direction of a bridge.
#[derive(Clone, Default)]
pub struct DirectionConfig {
    pub audio: Option<AudioMapping>,
    pub dtmf: Option<DtmfMapping>,
    pub transcode: Option<TranscodeSpec>,
}

impl DirectionConfig {
    /// Build a DirectionConfig from negotiated leg profiles (source → target).
    pub fn from_profiles(
        source: &crate::media::negotiate::NegotiatedLegProfile,
        target: &crate::media::negotiate::NegotiatedLegProfile,
    ) -> Self {
        let audio = match (&source.audio, &target.audio) {
            (Some(sa), Some(ta)) => Some(AudioMapping {
                source_pt: sa.payload_type,
                target_pt: ta.payload_type,
                source_clock_rate: sa.clock_rate,
                target_clock_rate: ta.clock_rate,
                source_codec: sa.codec,
                target_codec: ta.codec,
            }),
            _ => None,
        };

        let transcode = match (&source.audio, &target.audio) {
            (Some(sa), Some(ta)) if sa.codec != ta.codec => Some(TranscodeSpec {
                source_codec: sa.codec,
                target_codec: ta.codec,
                target_pt: ta.payload_type,
            }),
            _ => None,
        };

        let dtmf = source.dtmf.as_ref().map(|sd| DtmfMapping {
            source_pt: sd.payload_type,
            target_pt: target.dtmf.as_ref().map(|d| d.payload_type),
            source_clock_rate: sd.clock_rate,
            target_clock_rate: target.dtmf.as_ref().map(|d| d.clock_rate),
        });

        Self { audio, dtmf, transcode }
    }
}

// ---------------------------------------------------------------------------
// BidirectionalBridge
// ---------------------------------------------------------------------------

/// Bidirectional media bridge between two PeerEndpoints.
pub struct BidirectionalBridge {
    caller: PeerEndpoint,
    callee: PeerEndpoint,
    caller_to_callee: DirectionConfig,
    callee_to_caller: DirectionConfig,
}

impl BidirectionalBridge {
    /// Start building a bridge with the two required endpoints.
    pub fn builder(caller: PeerEndpoint, callee: PeerEndpoint) -> BridgeBuilder {
        BridgeBuilder {
            caller,
            callee,
            caller_to_callee: DirectionConfig::default(),
            callee_to_caller: DirectionConfig::default(),
        }
    }

    /// Wire both directions: caller.input → transform → callee.output and vice versa.
    pub fn bridge(&self) {
        let c2c = &self.caller_to_callee;
        self.caller.bridge_to(
            self.callee.output(),
            c2c.audio.clone(),
            c2c.dtmf.clone(),
            c2c.transcode.clone(),
        );
        let c2c_rev = &self.callee_to_caller;
        self.callee.bridge_to(
            self.caller.output(),
            c2c_rev.audio.clone(),
            c2c_rev.dtmf.clone(),
            c2c_rev.transcode.clone(),
        );
    }

    /// Remove both directions (switch to idle).
    pub fn unbridge(&self) {
        self.caller.output().clear_source();
        self.callee.output().clear_source();
    }

    /// Access the caller endpoint.
    pub fn caller(&self) -> &PeerEndpoint {
        &self.caller
    }

    /// Access the callee endpoint.
    pub fn callee(&self) -> &PeerEndpoint {
        &self.callee
    }
}

pub struct BridgeBuilder {
    caller: PeerEndpoint,
    callee: PeerEndpoint,
    caller_to_callee: DirectionConfig,
    callee_to_caller: DirectionConfig,
}

impl BridgeBuilder {
    pub fn caller_to_callee(mut self, config: DirectionConfig) -> Self {
        self.caller_to_callee = config;
        self
    }

    pub fn callee_to_caller(mut self, config: DirectionConfig) -> Self {
        self.callee_to_caller = config;
        self
    }

    pub fn build(self) -> BidirectionalBridge {
        BidirectionalBridge {
            caller: self.caller,
            callee: self.callee,
            caller_to_callee: self.caller_to_callee,
            callee_to_caller: self.callee_to_caller,
        }
    }
}
