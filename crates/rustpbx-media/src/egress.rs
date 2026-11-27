//! Per-leg egress pipeline — produces target-encoded audio frames at the
//! negotiated ptime cadence and pushes them into the PeerConnection's sender.
//!
//! The [`EgressSource`] enum unifies ALL egress behaviours as mutually
//! exclusive modes:
//!
//! - [`EgressSource::RewriteRelay`] — fast-path: transport-level zero-copy
//!   relay of the peer's packets onto this leg. The pacing loop is parked (no
//!   frames produced) and the rewrite bridge on the peer's transport writes
//!   this leg's wire. `track.recv()` on the peer's receiver track will NOT
//!   work (rustrtc skips the mpsc listener dispatch when a rewrite bridge is
//!   active); ingress observers still see every packet.
//! - [`EgressSource::Silence`] — mute / hold placeholder / idle.
//! - [`EgressSource::Media`] — IVR greeting / hold music / announcement
//!   (wav / mp3 / http via [`crate::audio_source::AudioSource`]).
//! - [`EgressSource::Inject`] — external app pushes pre-encoded frames (MCU).
//! - [`EgressSource::TranscodePeer`] — different-codec transcoding: pull from
//!   the peer's receiver track, decode, auto-resample, re-encode.
//!
//! ## ICE 独占 (exclusive send ownership)
//!
//! At any instant, either the rewrite bridge (RewriteRelay) or the sender
//! (all other sources) writes to the remote peer. This prevents two RTP
//! streams (real audio + silence) colliding on the same ICE connection.
//!
//! ## Architecture
//!
//! One pacing task owns the active source's mutable state (single owner — no
//! locks on the hot path). Source switches arrive via a command channel and
//! are applied between ticks. Every tick (ptime) the task pulls/encodes one
//! frame and `try_send`s it to the [`SampleStreamSource`]. The pipeline
//! ALWAYS emits on each tick (sources fall back to silence) so the outgoing
//! stream never gaps — important for the remote decoder's PLC.
//!
//! The [`SampleStreamSource`] / `SampleStreamTrack` pair is created by the
//! caller (Leg) and the track is added to the PeerConnection; the pipeline
//! only holds the push side. This keeps the pipeline unit-testable without a
//! real PeerConnection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::instrument::WithSubscriber;

use anyhow::Result;
use audio_codec::opus::OpusEncoder;
use audio_codec::{BoxedResampler, CodecType, Decoder, Encoder, create_encoder};

use bytes::Bytes;
use parking_lot::Mutex;
use rustrtc::media::MediaStreamTrack;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::SampleStreamSource;
use rustrtc::{PeerConnection, RtpRewriteBridgeOptions, RtpRewriteRule};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{trace, warn};

use crate::audio_source::{AudioSource, ResamplingAudioSource};

/// Default ptime (packetization interval) in milliseconds.
const DEFAULT_PTIME_MS: u32 = 20;

/// Maximum number of consecutive catch-up frames the egress emits after a
/// pacing stall. The pacing clock is wall-clock anchored, so after a stall the
/// following ticks fire immediately until the schedule is caught up; this caps
/// that burst (5 frames = 100 ms at the default 20 ms ptime). Anything beyond
/// the cap is dropped by re-anchoring the schedule, so a multi-second stall
/// (GC pause, host suspend, debug-build CPU starvation) can never dump an
/// unbounded burst of frames into the sender and the network.
const MAX_CATCHUP_FRAMES: u32 = 5;

/// Grace tail after a non-looping Media source reaches natural EOF. The
/// pipeline keeps streaming silence for this long before firing
/// `on_end(false)` so the final frames already handed to the sender can drain
/// through the queue, the network and the remote jitter buffer. Apps hang up
/// the moment they receive the completion event; without the tail the BYE
/// tears the bridge down while the remote is still playing the prompt,
/// clipping its last syllable (observed with TTS prompts).
const PLAYBACK_EOF_TAIL_MS: u32 = 300;

/// Inject-source re-anchor threshold: after this many consecutive producer
/// frames arrived behind the leg's timeline (~1 s at 20 ms ptime), the
/// timestamp offset is re-established once. A single late frame never
/// triggers it; only a rebuilt producer (new random timestamp base) does.
/// Non-blocking `now_or_never` poll on a `MediaStreamTrack` using a noop waker
/// so draining never registers `EgressTask`'s real waker on the peer track.
fn try_recv_sample(
    peer: &dyn MediaStreamTrack,
) -> Option<rustrtc::media::MediaResult<MediaSample>> {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match peer.recv().as_mut().poll(&mut cx) {
        std::task::Poll::Ready(res) => Some(res),
        std::task::Poll::Pending => None,
    }
}

/// Callback fired when a [`EgressSource::Media`] source stops producing audio:
/// natural EOF (`false`) or interrupted by switching away from Media / stop
/// (`true`). Use to signal playback completion to the app/IVR layer.
pub type EgressEndCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// What a leg sends to its remote peer. Variants are mutually exclusive —
/// only one is active at a time.
pub enum EgressSource {
    /// **Fast-path**: transport-level zero-copy relay. The pacing loop is
    /// parked (tick skipped) and the ICE send channel is exclusively owned by
    /// the rewrite bridge. Switching to it arms the bridge on the peer's PC
    /// (the peer's ingress relayed onto this leg); switching away removes it,
    /// so this leg's wire has one writer at a time.
    ///
    /// The relay is per-media-kind and payload-type-aware: each entry in
    /// `routes` carries a media kind (`Audio`, `Video`) and its own rewrite
    /// rules (catch-all, DTMF, video PT rules).
    ///
    /// `on_arm_failed` fires (once) if the rewrite bridge cannot be armed —
    /// e.g. a WebRTC leg's DTLS/SRTP transport never becomes ready. The owner
    /// (MediaBridge) uses it to fall back to transcoding so the call keeps
    /// media instead of silently going silent.
    RewriteRelay {
        peer_pc: PeerConnection,
        options: RtpRewriteBridgeOptions,
        routes: Vec<(rustrtc::MediaKind, Vec<RtpRewriteRule>)>,
        on_arm_failed: Option<Arc<dyn Fn() + Send + Sync>>,
    },
    /// Emit silence frames (mute / hold placeholder / idle).
    Silence,
    /// Play an [`AudioSource`] (file, stream, or generated audio), converting
    /// its PCM to the target codec. When `loop_playback` is false and the
    /// source reaches EOF the pipeline streams silence for a short grace tail
    /// ([`PLAYBACK_EOF_TAIL_MS`]) and then switches to
    /// [`EgressSource::Silence`], firing `on_end(false)` only after the tail
    /// so in-flight frames reach the remote first. When the source is
    /// replaced (switch to another source or stop) while a Media source is
    /// active — including during the tail — `on_end(true)` is fired
    /// immediately.
    Media {
        audio: Box<dyn AudioSource>,
        loop_playback: bool,
        on_end: Option<EgressEndCallback>,
    },
    /// External app pushes one ptime of audio encoded for this leg per frame.
    /// The sender stamps it on the leg's RTP timeline; producer timestamps
    /// and sequence numbers are ignored. An empty channel emits silence.
    Inject {
        rx: Mutex<mpsc::Receiver<MediaSample>>,
    },
    /// **Transcoding**: pull a frame from the peer's receiver track, decode it
    /// to PCM, auto-resample to this leg's codec sample rate, re-encode, and
    /// push to the sender. `track.recv()` works (no rewrite bridge).
    TranscodePeer {
        peer: Arc<dyn MediaStreamTrack>,
        decoder: Box<dyn Decoder>,
        /// Source leg's negotiated audio codec (for DTX/SID classification and
        /// codec-specific decoder state preservation).
        source_codec: CodecType,
        /// The source leg's negotiated voice payload type.
        source_audio_payload_type: u8,
        /// Negotiated RFC 4733 telephone-event payload types on the source leg.
        /// Only packets carrying one of these PTs are forwarded as DTMF; RFC
        /// 3389 PT 13 CN enters DTX silence and unknown PTs are ignored.
        source_dtmf_payload_types: Vec<u8>,
        /// Once the first real audio frame or explicit DTX signal has been
        /// observed, empty ticks fall back to silence. Before that we emit
        /// nothing so ICE/DTLS setup does not inflate the callee's silence_frames.
        primed: bool,
    },
}

/// Configuration for the encoding side of the pipeline.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EgressCodec {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
    /// Destination telephone-event payload type. Its clock is assumed to be
    /// the same as the destination audio RTP clock.
    pub dtmf_payload_type: Option<u8>,
    /// Emit low-level comfort noise instead of digital silence when the source
    /// has no data. Keeps the outbound stream continuous (fixed seq/ts cadence
    /// regardless) while avoiding dead-air between playback/legs.
    pub comfort_noise: bool,
    /// Comfort noise level in dBFS (e.g. -35.0). Ignored when
    /// [`Self::comfort_noise`] is false.
    pub comfort_noise_level_db: f32,
}

/// Per-leg egress pipeline.
///
/// Create via [`EgressPipeline::start`] after obtaining a `SampleStreamSource`
/// (from `rustrtc::media::track::sample_track`, with the track added to the
/// PeerConnection). Stop by dropping the pipeline or calling [`Self::stop`].
pub(crate) struct EgressPipeline {
    volume: crate::volume_stats::VolumeSink,
    cmd_tx: mpsc::Sender<EgressCmd>,
    cancel: CancellationToken,
}

const VOLUME_FINALIZE_TIMEOUT: Duration = Duration::from_secs(1);

enum EgressCmd {
    FinalizeVolume(oneshot::Sender<()>),
    VolumeStats {
        enabled: bool,
        config: crate::quality_stats::QualityStatsConfig,
        identity: Option<(String, String)>,
    },
    SetSource(EgressSource, oneshot::Sender<()>),
    UpdateCodec(EgressCodec),
}

impl EgressPipeline {
    /// Spawn the pacing task. `sender` is the push side of the track added to
    /// the PeerConnection. `initial` is the first source (commonly
    /// [`EgressSource::Silence`]). While the gate is held (true),
    /// the pipeline parks (produces no frames) so a leg never emits audio to
    /// its remote peer before the call is answered — the rewrite relay opens
    /// the gate via [`EgressGate`] when both legs accept.
    pub fn start_with_gate(
        sender: SampleStreamSource,
        codec: EgressCodec,
        initial: EgressSource,
        ptime_ms: Option<u32>,
        gate: Option<Arc<AtomicBool>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let ptime = Duration::from_millis(ptime_ms.unwrap_or(DEFAULT_PTIME_MS) as u64);
        let noise_amplitude = 10f32.powf(codec.comfort_noise_level_db / 20.0) * i16::MAX as f32;

        let volume = Arc::new(parking_lot::Mutex::new(Default::default()));
        let task = EgressTask {
            volume: None,
            volume_sink: volume.clone(),
            sender,
            codec,
            encoder: create_egress_encoder(codec.codec),
            source: initial,
            resampler: None,
            ptime,
            gate,
            marker_pending: false,
            pcm_buf: vec![0i16; pcm_samples_per_frame(codec.codec, ptime)],
            noise_state: 0x9E37_79B9,
            noise_amplitude,
            noise_lp: 0.0,
            eof_tail_total: PLAYBACK_EOF_TAIL_MS.div_ceil(ptime_ms.unwrap_or(DEFAULT_PTIME_MS)),
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };
        tokio::spawn(task.run(cmd_rx, ptime, cancel.clone()).with_current_subscriber());

        Self {
            cmd_tx,
            cancel,
            volume,
        }
    }

    pub(crate) fn set_volume_stats(&self, enabled: bool, config: crate::quality_stats::QualityStatsConfig,
                                  identity: Option<(String, String)>) -> anyhow::Result<()> {
        self.cmd_tx
            .try_send(EgressCmd::VolumeStats { enabled, config, identity })
            .map_err(|error| anyhow::anyhow!("volume stats configuration failed: {error}"))
    }
    pub(crate) async fn finalize_volume(&self) -> Result<()> {
        let (completed, receiver) = oneshot::channel();
        self.cmd_tx.try_send(EgressCmd::FinalizeVolume(completed))
            .map_err(|error| anyhow::anyhow!("volume finalization command failed: {error}"))?;
        tokio::time::timeout(VOLUME_FINALIZE_TIMEOUT, receiver).await
            .map_err(|_| anyhow::anyhow!("volume finalization timed out"))?
            .map_err(|_| anyhow::anyhow!("volume finalization actor stopped"))
    }

    pub(crate) fn recorded_volume(&self) -> Option<crate::volume_stats::VolumeSummary> {
        self.volume.lock().last.clone()
    }
    pub(crate) fn volume_summary(
        &self,
        window: Duration,
    ) -> Option<crate::volume_stats::VolumeSummary> {
        crate::volume_stats::summary(&self.volume, window)
    }

    /// Switch the active source between ticks and discard queued old output.
    /// Waits until the switch is applied; already dequeued packets remain in flight.
    /// Returns Err if the pipeline has stopped.
    pub async fn set_source(&self, source: EgressSource) -> Result<()> {
        let (completed, receiver) = oneshot::channel();
        self.cmd_tx
            .send(EgressCmd::SetSource(source, completed))
            .await
            .map_err(|_| anyhow::anyhow!("egress pipeline stopped"))?;
        receiver.await.map_err(|_| anyhow::anyhow!("egress pipeline stopped"))
    }

    /// Swap the encoding codec (e.g. after a re-INVITE changes the negotiated
    /// audio codec). Rebuilds the encoder and re-sizes the PCM staging buffer.
    /// Existing media source (if any) keeps playing; subsequent frames are
    /// encoded with the new codec.
    pub async fn update_codec(&self, codec: EgressCodec) -> Result<()> {
        self.cmd_tx
            .send(EgressCmd::UpdateCodec(codec))
            .await
            .map_err(|_| anyhow::anyhow!("egress pipeline stopped"))
    }

    /// Stop the pacing task (idempotent). Also happens on Drop.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

impl Drop for EgressPipeline {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Build the encoder used by paced egress.
///
/// Opus always encodes **mono**: egress PCM is mono, and Chrome's common offer
/// (`a=fmtp:111 minptime=10;useinbandfec=1` without `stereo=1`) prefers mono
/// receive. `create_encoder(Opus)` defaults to stereo TOC + mono→stereo upmix,
/// which is heavier (debug underproduction) and mismatches the negotiated
/// preference.
fn create_egress_encoder(codec: CodecType) -> Box<dyn Encoder> {
    if codec == CodecType::Opus {
        Box::new(OpusEncoder::new(48_000, 1))
    } else {
        create_encoder(codec)
    }
}

/// PCM samples produced per ptime frame for the codec's sample rate.
fn pcm_samples_per_frame(codec: CodecType, ptime: Duration) -> usize {
    let rate = codec.samplerate();
    (rate as u64 * ptime.as_millis() as u64 / 1000) as usize
}

/// Advance the wall-clock egress pacing schedule for one emitted frame.
///
/// The schedule is an absolute deadline (`next_deadline += ptime`), so a short
/// stall is made up by the following ticks firing immediately. That catch-up
/// burst is capped at [`MAX_CATCHUP_FRAMES`]; when the cap is exceeded the
/// remaining backlog is dropped by re-anchoring the deadline to `now + ptime`,
/// and the dropped duration is returned so the caller can log it.
fn advance_pacing(
    next_deadline: &mut tokio::time::Instant,
    now: tokio::time::Instant,
    ptime: Duration,
    catchup_frames: &mut u32,
) -> Option<Duration> {
    *next_deadline += ptime;
    if *next_deadline <= now {
        *catchup_frames += 1;
        if *catchup_frames > MAX_CATCHUP_FRAMES {
            let behind = now.saturating_duration_since(*next_deadline);
            *next_deadline = now + ptime;
            *catchup_frames = 0;
            return Some(behind);
        }
    } else {
        *catchup_frames = 0;
    }
    None
}

/// Wrap a Media audio source so it yields PCM at the egress codec's sample
/// rate. The encoder (e.g. opus/48000) must receive samples at its rate; a
/// file decoded at a different native rate (e.g. a 24 kHz MP3) would otherwise
/// be encoded as-is → wrong tempo / pitch. No-op passthrough when rates match.
fn media_source_for_codec(audio: Box<dyn AudioSource>, codec: CodecType) -> Box<dyn AudioSource> {
    let target = codec.samplerate();
    if audio.sample_rate() != target {
        Box::new(ResamplingAudioSource::new(audio, target))
    } else {
        audio
    }
}

struct EgressTask {
    volume: Option<crate::volume_stats::VolumeSampler>,
    volume_sink: crate::volume_stats::VolumeSink,
    sender: SampleStreamSource,
    codec: EgressCodec,
    encoder: Box<dyn Encoder>,
    source: EgressSource,
    resampler: Option<BoxedResampler>,
    ptime: Duration,
    /// While held (true) the pipeline parks. Opened by the relay when both
    /// legs accept.
    gate: Option<Arc<AtomicBool>>,
    marker_pending: bool,
    pcm_buf: Vec<i16>,
    /// LCG state for comfort-noise generation (continuous across frames so the
    /// noise does not repeat per-frame).
    noise_state: u32,
    /// Comfort-noise amplitude in 16-bit PCM units (from `codec` level dBFS).
    noise_amplitude: f32,
    /// One-pole lowpass state for a softer, less "harsh static" comfort tone.
    noise_lp: f32,
    /// Post-EOF grace tail length in ticks (see [`PLAYBACK_EOF_TAIL_MS`]).
    eof_tail_total: u32,
    /// Remaining grace ticks while draining a finished non-looping Media
    /// source. 0 = idle.
    eof_tail_remaining: u32,
    /// Jitter absorber for TranscodePeer: buffering until two full frames of
    /// decoded PCM are on hand, then consuming one frame per tick. Frames
    /// arriving a tick late merge into the buffer instead of punching CN
    /// holes; only a REAL network gap drains it to comfort-noise.
    transcode_armed: bool,
    /// True while the `TranscodePeer` source is in explicit DTX silence (after
    /// a G.729B SID, Opus short DTX marker, or RFC 3389 PT 13 CN). Drains any
    /// trailing partial speech frame, suppresses SID/PLC bursts, and re-arms
    /// `transcode_armed` for the next talkspurt.
    transcode_in_dtx: bool,
    /// True when a mixed G.729 speech+SID packet's 10 ms SID gap has not yet
    /// been materialized (flushed to silence once speech finishes draining, or
    /// inserted as 10 ms zeros if speech resumes before the tail drains).
    transcode_sid_silence_pending: bool,
    /// TranscodePeer: decoded + resampled PCM carried over between ticks.
    /// Peers whose ptime does not divide evenly into this leg's ptime (e.g. a
    /// 10 ms opus peer into a 20 ms ptime leg) leave partial frames here that
    /// combine with the next tick's input, so no audio is lost between the
    /// pull-cadence mismatch and the fixed output cadence.
    transcode_pending: Vec<i16>,
    /// PCM samples dropped from `transcode_pending` because a peer produces
    /// faster than this leg consumes for a sustained period (bounded queue —
    /// drop-oldest, mirroring the receiver ring's own overflow policy).
    transcode_overflow_samples: u64,
}

/// Maximum PCM carried between ticks for [`EgressSource::TranscodePeer`], in
/// output frames (8 × 20 ms ≈ 160 ms). Bounds memory and added latency when a
/// burst of peer frames arrives; steady-state peers never reach it.
const TRANSCODE_PENDING_MAX_FRAMES: usize = 8;

impl EgressTask {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<EgressCmd>,
        ptime: Duration,
        cancel: CancellationToken,
    ) {
        // Wall-clock anchored pacing. Every tick emits exactly one ptime of
        // audio and advances the RTP timeline by that ptime. A plain
        // `tokio::time::interval` with `MissedTickBehavior::Skip` does NOT make
        // up a late tick, so the egress clock permanently falls behind the wall
        // clock (measured 91.7% of real time on a debug build over 20 s): the
        // receiver's jitter buffer drains, PLC starts filling gaps and the
        // audio turns muffled/watery. Here the schedule is an absolute
        // deadline, so short stalls are caught up by the following ticks, and
        // the catch-up burst is capped at [`MAX_CATCHUP_FRAMES`] to keep a long
        // stall from flooding the sender.
        let mut next_deadline = tokio::time::Instant::now();
        let mut catchup_frames: u32 = 0;
        // Tracks the relay/gated parked state so the pacing clock can be
        // re-anchored on both edges: while parked no tick is polled, and the
        // pending schedule would otherwise replay the whole parked span as one
        // back-to-back burst of frames.
        let mut was_parked = false;
        loop {
            // RewriteRelay parks the pacing loop: the rewrite bridge owns the
            // ICE send channel exclusively, so no frames must be produced.
            let is_relay = matches!(self.source, EgressSource::RewriteRelay { .. });
            // While gated (call not answered yet) the pipeline parks too — a
            // leg must not emit audio (even silence) to its remote before the
            // call is answered. Early-media playback switches the source to
            // Media, which still produces frames; only Silence is parked.
            let gated = self
                .gate
                .as_ref()
                .is_some_and(|g| g.load(Ordering::Acquire))
                && matches!(self.source, EgressSource::Silence);
            // While parked (relay or pre-answer silence) no frames are produced,
            // so don't arm the ptime timer at all: 1600 legs waking at 50 Hz to
            // run an empty tick branch is pure CPU. Use a pending future so the
            // select only wakes on commands/cancel.
            let parked = is_relay || gated;
            if parked != was_parked {
                // Entering/leaving the parked state is a pacing discontinuity:
                // drop whatever backlog the schedule accumulated so the unpark
                // cannot dump the parked span into the sender at once, and mark
                // the first resumed frame after a parked gap.
                if !parked {
                    self.marker_pending = true;
                }
                next_deadline = tokio::time::Instant::now();
                catchup_frames = 0;
                was_parked = parked;
            }
            let tick: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                if parked {
                    Box::pin(std::future::pending())
                } else {
                    Box::pin(tokio::time::sleep_until(next_deadline))
                };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(EgressCmd::FinalizeVolume(completed)) => {
                        // Terminal CDR capture owns the final window; later ticks cannot overwrite it.
                        if let Some(mut volume) = self.volume.take() { volume.finish(); }
                        let _ = completed.send(());
                    }
                    Some(EgressCmd::VolumeStats { enabled, config, identity }) => {
                        self.volume = enabled.then(|| crate::volume_stats::VolumeSampler::new(config, self.volume_sink.clone(), identity));
                        if !enabled { *self.volume_sink.lock() = Default::default(); }
                    }
                    Some(EgressCmd::SetSource(s, completed)) => {
                        self.sender.discard_pending();
                        if let Some(volume) = self.volume.as_mut() { volume.reset(); }
                        // If we are switching AWAY from an active Media source,
                        // fire its on_end as interrupted so the app knows the
                        // playback was cut short (e.g. stop_play / DTMF barge).
                        let prev_on_end = match &self.source {
                            EgressSource::Media { on_end, .. } => on_end.clone(),
                            _ => None,
                        };
                        if let Some(cb) = prev_on_end {
                            cb(true);
                        }
                        // A pending EOF tail belongs to the previous source;
                        // the switch above already reported it as interrupted.
                        self.eof_tail_remaining = 0;
                        // Resample Media sources to the codec's sample rate
                        // before encoding (e.g. 24 kHz MP3 → 48 kHz opus).
                        let mut s = match s {
                            EgressSource::Media {
                                audio,
                                loop_playback,
                                on_end,
                            } => EgressSource::Media {
                                audio: media_source_for_codec(audio, self.codec.codec),
                                loop_playback,
                                on_end,
                            },
                            other => other,
                        };
                        // Auto-configure the resampler for TranscodePeer based
                        // on src/dst sample rate mismatch. Use the codec's actual
                        // PCM sample rate (samplerate), NOT the RTP clock_rate —
                        // G.722 has clock_rate 8000 but samplerate 16000; using
                        // clock_rate skips the resampler and doubles the pitch.
                        let mut startup_in_dtx = false;
                        if let EgressSource::TranscodePeer {
                            peer,
                            decoder,
                            source_codec,
                            source_audio_payload_type,
                            primed,
                            ..
                        } = &mut s
                        {
                            // IVR/hold can leave incoming audio queued without a
                            // consumer. Start this live route at current audio,
                            // not at the oldest frame retained by the receiver.
                            // Drain here, between ticks, before this task starts
                            // consuming the new source; never wait for input.
                            // Retain an explicit DTX signal if the peer already
                            // went quiet before the bridge formed so egress
                            // keeps pacing silence instead of going dark.
                            while let Some(Ok(sample)) = try_recv_sample(peer.as_ref()) {
                                if let MediaSample::Audio(frame) = sample {
                                    match crate::dtx::classify_rtp_audio_dtx(
                                        *source_codec,
                                        *source_audio_payload_type,
                                        frame.payload_type,
                                        &frame.data,
                                    ) {
                                        crate::dtx::PacketDtxSignal::EnterDtx => {
                                            startup_in_dtx = true;
                                        }
                                        crate::dtx::PacketDtxSignal::Speech => {
                                            startup_in_dtx = false;
                                        }
                                        crate::dtx::PacketDtxSignal::Ignore => {}
                                    }
                                }
                            }
                            if startup_in_dtx {
                                *primed = true;
                            }
                            let src_sample_rate = decoder.sample_rate();
                            let dst_sample_rate = self.codec.codec.samplerate();
                            if src_sample_rate != dst_sample_rate {
                                self.resampler = Some(
                                    BoxedResampler::new(
                                        src_sample_rate as usize,
                                        dst_sample_rate as usize,
                                    )
                                    .expect("valid sample rates"),
                                );
                            } else {
                                self.resampler = None;
                            }
                        } else {
                            self.resampler = None;
                        }
                        // A pending transcode accumulator belongs to the
                        // previous peer/source — never carry it across.
                        self.transcode_pending.clear();
                        self.transcode_armed = false;
                        self.transcode_in_dtx = startup_in_dtx;
                        self.transcode_sid_silence_pending = false;
                        // Re-anchor the pacing clock on a source switch. While
                        // the loop is parked (pre-answer gate / relay) the
                        // schedule keeps accumulating; dropping it here keeps
                        // the unpark from dumping the parked span at once.
                        next_deadline = tokio::time::Instant::now();
                        catchup_frames = 0;
                        self.source = s;
                        let _ = completed.send(());
                    }
                    Some(EgressCmd::UpdateCodec(new_codec)) => {
                        self.sender.discard_pending();
                        self.codec = new_codec;
                        self.encoder = create_egress_encoder(new_codec.codec);
                        // Rebuild the PCM staging buffer for the new sample rate.
                        self.pcm_buf = vec![0i16; pcm_samples_per_frame(new_codec.codec, self.ptime)];
                        // Drop any resampler: TranscodePeer re-derives it on next SetSource.
                        self.resampler = None;
                        self.marker_pending = true;
                        // Pending PCM was decoded for the old codec rate.
                        self.transcode_pending.clear();
                        self.transcode_armed = false;
                        self.transcode_in_dtx = false;
                        self.transcode_sid_silence_pending = false;
                    }
                    None => break,
                },
                _ = tick => {
                    let now = tokio::time::Instant::now();
                    if let Some(behind) = advance_pacing(
                        &mut next_deadline,
                        now,
                        ptime,
                        &mut catchup_frames,
                    ) && behind >= ptime
                    {
                        warn!(
                            behind_ms = behind.as_millis() as u64,
                            max_catchup_frames = MAX_CATCHUP_FRAMES,
                            "egress pacing fell behind; dropped the remaining backlog"
                        );
                    }
                    if let Some(frame) = self.next_frame().await {
                        let clear_marker = self.marker_pending;
                        if self.sender.try_send_audio(frame).is_err() {
                            trace!("egress: sender full, dropped frame before RTP assignment");
                        } else if clear_marker {
                            self.marker_pending = false;
                        }
                    }
                }
            }
        }
        if let Some(volume) = self.volume.as_mut() { volume.finish(); }
    }

    /// Produce exactly one frame for this tick. Sources that have no data fall
    /// back to silence so the outgoing stream never gaps.
    async fn next_frame(&mut self) -> Option<AudioFrame> {
        // Move the source out so other `self` fields (encoder, pcm_buf) are
        // freely accessible inside the match (avoids &mut-self / &mut-source
        // aliasing). Put it back at the end.
        let mut source = std::mem::replace(&mut self.source, EgressSource::Silence);

        let frame = match &mut source {
            EgressSource::RewriteRelay { .. } => {
                // The pacing loop skips ticks while a relay is active, so we
                // should never get here; keep a silent fallback just in case.
                let encoded = self.encode_silence();
                self.build_frame(encoded)
            }
            EgressSource::Silence => {
                let encoded = self.encode_silence();
                self.build_frame(encoded)
            }
            EgressSource::Media {
                audio,
                loop_playback,
                on_end,
                ..
            } => {
                let n = audio.read_samples(&mut self.pcm_buf);
                let encoded: Bytes = if n == 0 {
                    if !audio.has_data() {
                        // True EOF (file ended / channel sender dropped + drained).
                        //
                        // Keep streaming silence for a short grace tail before
                        // signaling completion: frames already handed to the
                        // sender still need queue + network + remote
                        // jitter-buffer time to actually play out, and apps
                        // hang up the moment they get the completion event.
                        if !*loop_playback && self.eof_tail_total > 0 {
                            if self.eof_tail_remaining == 0 {
                                self.eof_tail_remaining = self.eof_tail_total;
                            }
                            self.eof_tail_remaining -= 1;
                            if self.eof_tail_remaining == 0 {
                                if let Some(cb) = on_end.take() {
                                    cb(false);
                                }
                                source = EgressSource::Silence;
                            }
                            // else: stay in Media; the tail keeps ticking.
                        } else {
                            if let Some(cb) = on_end.take() {
                                cb(false);
                            }
                            source = EgressSource::Silence;
                        }
                        self.encode_silence()
                    } else if *loop_playback {
                        let _ = audio.reset();
                        let n2 = audio.read_samples(&mut self.pcm_buf);
                        if n2 == 0 {
                            self.encode_silence()
                        } else {
                            self.encode_pcm_frame(n2)
                        }
                    } else {
                        // Live source underrun (ChannelAudioSource between app
                        // TTS chunks). Keep RTP cadence with digital silence —
                        // comfort noise here pops when the next chunk arrives.
                        self.encode_digital_silence()
                    }
                } else {
                    self.encode_pcm_frame(n)
                };
                self.build_frame(encoded)
            }
            EgressSource::Inject { rx } => {
                let passed = rx.lock().try_recv().ok().and_then(|sample| match sample {
                    MediaSample::Audio(frame) => Some(frame),
                    MediaSample::Video(_) => None,
                });
                match passed {
                    Some(frame) => {
                        // Mixed audio and underrun silence share the leg's
                        // paced timeline.
                        let mut output = self.build_frame(frame.data);
                        output.marker |= frame.marker;
                        output
                    }
                    None => {
                        // Same as live Media underrun: do not insert CNG between
                        // app-injected bursts.
                        let encoded = self.encode_digital_silence();
                        self.build_frame(encoded)
                    }
                }
            }
            EgressSource::TranscodePeer {
                peer,
                decoder,
                source_codec,
                source_audio_payload_type,
                source_dtmf_payload_types,
                primed,
            } => {
                // Jitter-tolerant transcode pull. Per tick: non-blockingly
                // drain EVERY frame the peer's receiver track already holds
                // (decode → accumulate), then emit one ptime from the
                // accumulator. A frame that arrives late is decoded on the
                // next tick and carried in `transcode_pending` instead of
                // colliding with an inserted silence frame — under network
                // jitter the old per-tick blocking `timeout(ptime, recv)`
                // punched silent slots between speech frames (production
                // incident 2026-11-13: broadband noise after hold/resume on
                // G729↔PCMA calls). Only a REAL network gap or explicit DTX
                // drains to silence here.
                let frame_samples = self.pcm_buf.len();
                let mut dtmf_input: Option<AudioFrame> = None;
                let can_send_dtmf = self.codec.dtmf_payload_type.is_some();
                let sid_dst_samples = (self.codec.codec.samplerate() as usize / 100).max(1);
                loop {
                    // Cap first: trim the oldest surplus even when no new
                    // frame arrives this round (burst guard).
                    let excess = self
                        .transcode_pending
                        .len()
                        .saturating_sub(frame_samples * TRANSCODE_PENDING_MAX_FRAMES);
                    if excess > 0 {
                        self.transcode_pending.drain(..excess);
                        self.transcode_overflow_samples += excess as u64;
                    }
                    if dtmf_input.is_some()
                        || self.transcode_pending.len()
                            >= frame_samples * TRANSCODE_PENDING_MAX_FRAMES
                    {
                        break;
                    }
                    match try_recv_sample(peer.as_ref()) {
                        Some(Ok(MediaSample::Audio(input))) => {
                            if input.payload_type == Some(crate::dtx::COMFORT_NOISE_PT)
                                && *source_audio_payload_type != crate::dtx::COMFORT_NOISE_PT
                            {
                                // RFC 3389 Comfort Noise (static PT 13):
                                // enter DTX silence without treating CN as DTMF.
                                self.transcode_in_dtx = true;
                                self.transcode_sid_silence_pending = false;
                                if let Some(rs) = &mut self.resampler {
                                    rs.reset();
                                }
                                *primed = true;
                            } else if input
                                .payload_type
                                .is_none_or(|pt| pt == *source_audio_payload_type)
                            {
                                match crate::dtx::decode_audio_packet(
                                    *source_codec,
                                    decoder.as_mut(),
                                    &input.data,
                                ) {
                                    crate::dtx::DecodedAudio::Speech { mut pcm, enter_dtx } => {
                                        if self.transcode_sid_silence_pending {
                                            // A mixed G.729 speech+SID packet was followed
                                            // by speech before its trailing silence flushed:
                                            // preserve the 10 ms SID duration as zeros.
                                            self.transcode_pending.resize(
                                                self.transcode_pending.len() + sid_dst_samples,
                                                0,
                                            );
                                            self.transcode_sid_silence_pending = false;
                                        }
                                        if let Some(rs) = &mut self.resampler {
                                            pcm = rs.resample(&pcm);
                                            if enter_dtx {
                                                rs.reset();
                                            }
                                        }
                                        self.transcode_pending.extend_from_slice(&pcm);
                                        *primed = true;
                                        self.transcode_in_dtx = enter_dtx;
                                        self.transcode_sid_silence_pending = enter_dtx;
                                    }
                                    crate::dtx::DecodedAudio::Silence => {
                                        self.transcode_in_dtx = true;
                                        self.transcode_sid_silence_pending = false;
                                        if let Some(rs) = &mut self.resampler {
                                            rs.reset();
                                        }
                                        *primed = true;
                                    }
                                    crate::dtx::DecodedAudio::Ignore => {}
                                }
                            } else if can_send_dtmf
                                && input.payload_type.is_some_and(|pt| {
                                    source_dtmf_payload_types.contains(&pt)
                                })
                            {
                                // Negotiated telephone-event frame popped mid-drain:
                                // emit it on this tick, exactly like a DTMF frame
                                // arriving before the drain.
                                dtmf_input = Some(input);
                                break;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    }
                }
                if let Some(frame) = dtmf_input
                    .as_ref()
                    .and_then(|input| self.build_dtmf_frame(input))
                {
                    frame
                } else if self.transcode_in_dtx {
                    // Explicit DTX: drain any buffered speech (including a trailing
                    // partial frame from a 12-byte G.729 speech+SID or a short
                    // utterance before the 2-frame prebuffer armed), then re-arm
                    // the jitter prebuffer for the next talkspurt and emit silence.
                    if self.transcode_pending.len() >= frame_samples {
                        let encoded = self.encode_pending_frame(frame_samples);
                        self.build_frame(encoded)
                    } else if !self.transcode_pending.is_empty() {
                        self.transcode_sid_silence_pending = false;
                        self.transcode_armed = false;
                        let encoded = self.encode_pending_padded(frame_samples);
                        self.build_frame(encoded)
                    } else {
                        self.transcode_sid_silence_pending = false;
                        self.transcode_armed = false;
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    }
                } else if !self.transcode_armed {
                    // Prebuffer: hold emitting until two full frames are on
                    // hand so ordinary network jitter (±1 tick arrival) is
                    // absorbed by the buffer instead of emitting CN holes.
                    if self.transcode_pending.len() >= frame_samples * 2 {
                        self.transcode_armed = true;
                        let encoded = self.encode_pending_frame(frame_samples);
                        self.build_frame(encoded)
                    } else if *primed {
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    } else {
                        self.source = source;
                        return None;
                    }
                } else if self.transcode_pending.len() >= frame_samples {
                    // At least one full frame accumulated: emit exactly one
                    // ptime, carry the remainder over.
                    let encoded = self.encode_pending_frame(frame_samples);
                    self.build_frame(encoded)
                } else if *primed {
                    // Real network gap: comfort-noise so the remote PLC
                    // covers it. Late frames continue to merge into the
                    // following slots without punching holes.
                    tracing::trace!("transcode: accumulator drained, emitting comfort noise");
                    let encoded = self.encode_silence();
                    self.build_frame(encoded)
                } else {
                    // Still unprimed (ICE/DTLS setup): skip the tick entirely
                    // so the setup window does not inflate the callee's
                    // silence_frames.
                    self.source = source;
                    return None;
                }
            }
        };

        self.source = source;
        Some(frame)
    }

    /// Relay one telephone-event packet of the peer through this transcoding
    /// leg. It keeps the peer's RTP header (`raw_packet`, sequence number,
    /// timestamp and clock) so the sender can keep one timestamp for the
    /// event while assigning sequence numbers alongside transcoded audio.
    fn build_dtmf_frame(&self, source: &AudioFrame) -> Option<AudioFrame> {
        let payload_type = self.codec.dtmf_payload_type?;
        let payload = crate::telephone_event::map_telephone_event_duration(
            &source.data,
            source.clock_rate,
            self.codec.clock_rate,
        )?;
        Some(AudioFrame {
            rtp_timestamp: source.rtp_timestamp,
            clock_rate: source.clock_rate,
            data: Bytes::from(payload),
            sequence_number: source.sequence_number,
            payload_type: Some(payload_type),
            marker: source.marker,
            header_extension: None,
            source_addr: None,
            raw_packet: source.raw_packet.clone(),
        })
    }

    fn encode_observed_pcm(&mut self) -> Bytes {
        if let Some(volume) = self.volume.as_mut() {
            volume.observe(&self.pcm_buf);
        }
        self.encoder.encode(&self.pcm_buf).into()
    }

    /// Encode a full ptime PCM frame. Short reads (EOF tail) are zero-padded
    /// so Opus/PCMU always see a complete frame matching the RTP timestamp step.
    fn encode_pcm_frame(&mut self, n: usize) -> Bytes {
        if n < self.pcm_buf.len() {
            self.pcm_buf[n..].fill(0);
        }
        self.encode_observed_pcm()
    }

    /// True digital silence (zeros). Used for live underruns so the decoder
    /// does not hear comfort-noise → speech transitions as clicks.
    fn encode_digital_silence(&mut self) -> Bytes {
        self.pcm_buf.fill(0);
        self.encode_observed_pcm()
    }

    /// Fill `buf` with comfort noise (when enabled) or digital silence (zeros).
    fn fill_silence(&mut self, buf: &mut [i16]) {
        if self.codec.comfort_noise && self.noise_amplitude > 0.0 {
            for s in buf.iter_mut() {
                // LCG uniform in (-1, 1).
                self.noise_state = self
                    .noise_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                let white = ((self.noise_state as f32 / u32::MAX as f32) * 2.0) - 1.0;
                // One-pole lowpass → soft "room tone" instead of harsh static.
                self.noise_lp += 0.15 * (white - self.noise_lp);
                *s = (self.noise_lp * self.noise_amplitude) as i16;
            }
        } else {
            buf.fill(0);
        }
    }

    /// Zero-fill (or CNG-fill) the PCM buffer (silence).
    fn encode_silence(&mut self) -> Bytes {
        let mut buf = std::mem::take(&mut self.pcm_buf);
        self.fill_silence(&mut buf);
        self.pcm_buf = buf;
        self.encode_observed_pcm()
    }

    /// Encode exactly one ptime frame from the transcode accumulator,
    /// consuming the samples. Caller guarantees at least `frame_samples`
    /// samples are pending (`frame_samples == self.pcm_buf.len()`).
    fn encode_pending_frame(&mut self, frame_samples: usize) -> Bytes {
        self.pcm_buf
            .copy_from_slice(&self.transcode_pending[..frame_samples]);
        self.transcode_pending.drain(..frame_samples);
        self.encode_observed_pcm()
    }

    /// Encode a silence-padded frame from a partial transcode accumulator
    /// (fewer than one frame pending), consuming the samples: every sample
    /// is emitted exactly once. Used when explicit DTX ends a talkspurt on a
    /// partial frame (e.g. a 12-byte G.729 10 ms speech + 2 B SID packet) so
    /// trailing speech is never stranded until the next talkspurt.
    fn encode_pending_padded(&mut self, frame_samples: usize) -> Bytes {
        let n = self.transcode_pending.len().min(frame_samples);
        let mut buf = std::mem::take(&mut self.pcm_buf);
        buf[..n].copy_from_slice(&self.transcode_pending[..n]);
        self.fill_silence(&mut buf[n..]);
        self.pcm_buf = buf;
        self.transcode_pending.drain(..n);
        self.encode_observed_pcm()
    }

    /// The rustrtc sender assigns wire sequence numbers and ptime-stepped
    /// timestamps when consuming the frame.
    fn build_frame(&self, data: Bytes) -> AudioFrame {
        AudioFrame {
            clock_rate: self.codec.clock_rate,
            data,
            payload_type: Some(self.codec.payload_type),
            marker: self.marker_pending,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::media::MediaKind;
    use rustrtc::media::track::sample_track;

    /// A minimal AudioSource that emits a constant-amplitude PCM sine-ish ramp,
    /// looping forever.
    struct LoopingBeep {
        rate: u32,
        pos: usize,
    }

    impl AudioSource for LoopingBeep {
        fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
            for (i, s) in buffer.iter_mut().enumerate() {
                *s = ((self.pos + i) as i32 * 1000) as i16;
            }
            self.pos += buffer.len();
            buffer.len()
        }
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            1
        }
        fn has_data(&self) -> bool {
            true
        }
        fn reset(&mut self) -> Result<()> {
            self.pos = 0;
            Ok(())
        }
    }

    fn pcmu_codec() -> EgressCodec {
        EgressCodec {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: false,
            comfort_noise_level_db: -35.0,
        }
    }

    #[test]
    fn egress_opus_encoder_uses_mono_toc() {
        let mut enc = create_egress_encoder(CodecType::Opus);
        let pkt = enc.encode(&vec![1_000i16; 960]);
        assert!(!pkt.is_empty(), "mono Opus must encode");
        assert_eq!(
            pkt[0] & 0x04,
            0,
            "egress Opus TOC must be mono (Chrome offer has no stereo=1)"
        );
    }

    #[test]
    fn stock_create_encoder_opus_is_stereo_toc() {
        // Guards against silently flipping create_egress_encoder back to the
        // stock stereo default — that path is what we intentionally avoid.
        let mut enc = create_encoder(CodecType::Opus);
        let pkt = enc.encode(&vec![1_000i16; 960]);
        assert!(!pkt.is_empty());
        assert_ne!(pkt[0] & 0x04, 0, "stock create_encoder(Opus) is stereo");
    }

    #[tokio::test]
    async fn silence_pipeline_emits_frames_at_ptime() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let pipe = EgressPipeline::start_with_gate(
            sender,
            pcmu_codec(),
            EgressSource::Silence,
            Some(20),
            None,
        );

        assert!(pipe.volume_summary(Duration::from_secs(2)).is_none());
        let config = crate::quality_stats::QualityStatsConfig {
            sample_interval_ms: 250, ..Default::default()
        };
        pipe.set_volume_stats(true, config.clone(), None)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let volume = pipe.volume_summary(Duration::from_secs(2)).unwrap();
        assert!(volume.sample_count > 0);
        assert_eq!(volume.rms, 0.0);
        assert_eq!(volume.peak, 0);
        assert_eq!(volume.attenuation_dbov, 127, "observed silence is distinct from unavailable PCM");
        pipe.set_volume_stats(false, config, None)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(pipe.volume_summary(Duration::from_secs(2)).is_none());
        // Let a few ticks elapse, then stop and inspect via the shared sender
        // drop_count isn't exposed; instead we just assert the task runs and
        // stops cleanly. The frame content is verified in the unit test below.
        tokio::time::sleep(Duration::from_millis(70)).await;
        pipe.stop();
        // No panic = the pacing task produced frames without error.
    }

    #[tokio::test]
    async fn playback_pads_and_encodes_configured_ptime() {
        for (codec_type, clock_rate, ptime_ms) in [
            (CodecType::PCMU, 8000, 30),
            (CodecType::G722, 8000, 30),
            (CodecType::Opus, 48000, 10),
        ] {
            let mut codec = pcmu_codec();
            codec.codec = codec_type;
            codec.clock_rate = clock_rate;
            codec.payload_type = codec_type.payload_type();
            let (sender, track, _) = sample_track(MediaKind::Audio, 8);
            // Short playback is padded to a full ptime; EOF silence frames keep
            // the same generated-frame identity (`raw_packet: None`).
            let audio = crate::audio_source::ToneAudioSource::new(440, Duration::from_millis(5), 8000).unwrap();
            let pipe = EgressPipeline::start_with_gate(sender, codec,
                EgressSource::Media { audio: Box::new(audio), loop_playback: false, on_end: None },
                Some(ptime_ms), None);
            for _ in 0..3 {
                let sample = tokio::time::timeout(Duration::from_secs(1), track.recv()).await.unwrap().unwrap();
                let MediaSample::Audio(frame) = sample else { panic!("audio expected") };
                assert!(frame.sequence_number.is_none());
                assert_eq!(frame.clock_rate, clock_rate);
                assert!(frame.raw_packet.is_none());
                assert!(!frame.data.is_empty());
                if codec_type != CodecType::Opus {
                    assert_eq!(frame.data.len(), (clock_rate * ptime_ms / 1000) as usize);
                }
            }
            pipe.stop();
        }
    }

    #[tokio::test]
    async fn comfort_noise_emits_nonzero_silence_frames() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = EgressCodec {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: true,
            comfort_noise_level_db: -30.0,
        };
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Silence,
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-30.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };
        // With CNG on, the encoded silence frame must differ from a pure
        // zero-encode: a zero PCMU frame is all 0xFF (μ-law of 0) and any
        // deviation proves non-zero samples reached the encoder.
        let with_noise = task.next_frame().await.expect("frame").data;
        let zeros = vec![0u8; with_noise.len()];
        assert_ne!(
            &with_noise[..],
            &zeros[..],
            "CNG must not be digital silence"
        );
        assert!(
            with_noise.iter().any(|&b| b != 0xFF),
            "CNG Silence path must not encode as digital μ-law silence (0xFF)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queued_playback_emits_generated_frames_without_assigning_numbers() {
        let (sender, track, _) = sample_track(MediaKind::Audio, 1);
        let pipe = EgressPipeline::start_with_gate(sender, pcmu_codec(), EgressSource::Silence,
            Some(20), None);
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..2 {
            let MediaSample::Audio(frame) = track.recv().await.unwrap() else { panic!("audio expected") };
            assert!(frame.sequence_number.is_none());
            assert_eq!(frame.rtp_timestamp, 0);
            assert!(frame.raw_packet.is_none());
        }
        pipe.stop();
    }

    /// Regression: app/TTS streaming (`ChannelAudioSource`) underruns used to
    /// insert comfort noise. CNG→speech at every chunk boundary sounded like
    /// a periodic pop (~1 s). Hold/`Silence` may still use CNG; live underruns
    /// must stay digital silence. Prior tests never exercised this path
    /// (IVR file play has no underrun; p2p uses RewriteRelay).
    #[tokio::test]
    async fn live_channel_underrun_emits_digital_silence_not_cng() {
        let (pcm_tx, pcm_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(8);
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = EgressCodec {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: true,
            comfort_noise_level_db: -20.0,
        };
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Media {
                audio: Box::new(crate::audio_source::ChannelAudioSource::new(pcm_rx, 8000)),
                loop_playback: false,
                on_end: None,
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-20.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };

        pcm_tx.try_send(vec![2_000i16; spf]).unwrap();
        let speech = task.next_frame().await.expect("speech frame");

        let mut ref_enc = create_encoder(CodecType::PCMU);
        let digital_ref: Bytes = ref_enc.encode(&vec![0i16; spf]).into();
        assert_ne!(
            speech.data, digital_ref,
            "speech frame should not be silence"
        );

        // No more PCM queued → live underrun (sender still alive).
        let gap = task.next_frame().await.expect("underrun frame");
        assert_eq!(
            gap.data, digital_ref,
            "live underrun must match digital-silence encode, not CNG"
        );
        assert!(
            matches!(task.source, EgressSource::Media { .. }),
            "underrun must not EOF while the PCM sender is alive"
        );

        // Resume with another chunk — still Media, still contiguous cadence.
        pcm_tx.try_send(vec![3_000i16; spf]).unwrap();
        let resume = task.next_frame().await.expect("resumed speech");
        assert_ne!(resume.data, digital_ref);
        assert!(gap.sequence_number.is_none());
        assert!(resume.sequence_number.is_none());

        // Separate sanity: Silence+CNG is distinguishable from digital silence.
        task.source = EgressSource::Silence;
        let cng = task.next_frame().await.expect("cng frame");
        assert_ne!(
            cng.data, digital_ref,
            "sanity: EgressSource::Silence with CNG must not equal digital silence"
        );
    }

    /// Same underrun contract for [`EgressSource::Inject`] (MCU / app push).
    #[tokio::test]
    async fn inject_underrun_emits_digital_silence_not_cng() {
        let (inj_tx, inj_rx) = tokio::sync::mpsc::channel::<MediaSample>(4);
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = EgressCodec {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: true,
            comfort_noise_level_db: -20.0,
        };
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Inject {
                rx: parking_lot::Mutex::new(inj_rx),
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-20.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };

        let mut ref_enc = create_encoder(CodecType::PCMU);
        let digital_ref: Bytes = ref_enc.encode(&vec![0i16; spf]).into();
        let gap = task.next_frame().await.expect("inject underrun frame");
        assert_eq!(
            gap.data, digital_ref,
            "Inject underrun must match digital-silence encode, not CNG"
        );

        // A mixer supplies its own unrelated timestamp/sequence. Its payload
        // must use the same leg timeline as the silence before and after it.
        let injected = AudioFrame {
            rtp_timestamp: 1_000_000,
            clock_rate: 8000,
            data: ref_enc.encode(&vec![2_000i16; spf]).into(),
            sequence_number: Some(5000),
            payload_type: Some(0),
            marker: true,
            header_extension: None,
            raw_packet: None,
            source_addr: None,
        };
        inj_tx.try_send(MediaSample::Audio(injected.clone())).unwrap();
        let speech = task.next_frame().await.unwrap();
        assert_eq!(speech.data, injected.data);
        assert!(speech.marker);
        assert!(speech.sequence_number.is_none());

        let underrun = task.next_frame().await.unwrap();
        assert_eq!(underrun.data, digital_ref);
        assert!(underrun.sequence_number.is_none());

        inj_tx.try_send(MediaSample::Audio(AudioFrame {
            rtp_timestamp: 1_000_160,
            sequence_number: Some(5001),
            ..injected.clone()
        })).unwrap();
        let resumed = task.next_frame().await.unwrap();
        assert_eq!(resumed.data, injected.data);
        assert!(resumed.sequence_number.is_none());

        task.source = EgressSource::Silence;
        let after = task.next_frame().await.unwrap();
        assert!(after.sequence_number.is_none());
    }

    /// Partial app chunks must not be zero-padded into a frame (that click
    /// every chunk boundary). Channel holds until a full ptime is available;
    /// egress then sees underrun → digital silence, then one clean frame.
    #[tokio::test]
    async fn partial_app_chunk_does_not_emit_padded_speech_frame() {
        let (pcm_tx, pcm_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(8);
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = EgressCodec {
            codec: CodecType::PCMU,
            payload_type: 0,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: true,
            comfort_noise_level_db: -20.0,
        };
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        assert_eq!(spf, 160);
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Media {
                audio: Box::new(crate::audio_source::ChannelAudioSource::new(pcm_rx, 8000)),
                loop_playback: false,
                on_end: None,
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-20.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };

        let mut ref_enc = create_encoder(CodecType::PCMU);
        let digital_ref: Bytes = ref_enc.encode(&vec![0i16; spf]).into();

        // Half a ptime of loud samples — old code would pad with zeros and
        // emit a mixed frame (pop). New code holds and underruns instead.
        pcm_tx.try_send(vec![10_000i16; 80]).unwrap();
        let first = task.next_frame().await.expect("held → underrun silence");
        assert_eq!(
            first.data, digital_ref,
            "partial chunk must not produce a padded speech frame"
        );

        pcm_tx.try_send(vec![10_000i16; 80]).unwrap();
        let second = task.next_frame().await.expect("completed frame");
        assert_ne!(
            second.data, digital_ref,
            "once a full ptime is buffered, speech must go out"
        );
    }

    #[tokio::test]
    async fn switch_source_from_silence_to_media() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let pipe = EgressPipeline::start_with_gate(
            sender,
            pcmu_codec(),
            EgressSource::Silence,
            Some(20),
            None,
        );
        // Switch to a looping media source mid-stream.
        pipe.set_source(EgressSource::Media {
            audio: Box::new(LoopingBeep { rate: 8000, pos: 0 }),
            loop_playback: true,
            on_end: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        pipe.stop();
    }

    #[tokio::test]
    async fn playback_after_relay_marks_once_and_preserves_ptime() {
        let (sender, track, _fb) = sample_track(MediaKind::Audio, 64);
        let peer_pc = PeerConnection::new(rustrtc::RtcConfiguration::webrtc());
        let pipe = EgressPipeline::start_with_gate(
            sender,
            pcmu_codec(),
            EgressSource::RewriteRelay {
                peer_pc: peer_pc.clone(),
                options: RtpRewriteBridgeOptions::default(),
                routes: Vec::new(),
                on_arm_failed: None,
            },
            Some(20),
            None,
        );

        pipe.set_source(EgressSource::Media {
            audio: Box::new(LoopingBeep { rate: 8000, pos: 0 }),
            loop_playback: true,
            on_end: None,
        })
        .await
        .unwrap();

        let first = tokio::time::timeout(Duration::from_secs(1), track.recv())
            .await
            .expect("first playback frame timeout")
            .expect("first playback frame");
        let MediaSample::Audio(first) = first else {
            panic!("expected audio");
        };
        assert!(
            first.marker,
            "first local packet after relay must be marked"
        );

        let second = tokio::time::timeout(Duration::from_secs(1), track.recv())
            .await
            .expect("second playback frame timeout")
            .expect("second playback frame");
        let MediaSample::Audio(second) = second else {
            panic!("expected audio");
        };
        assert!(!second.marker, "marker is only for the source transition");

        pipe.set_source(EgressSource::RewriteRelay {
            peer_pc: peer_pc.clone(),
            options: RtpRewriteBridgeOptions::default(),
            routes: Vec::new(),
            on_arm_failed: None,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        pipe.set_source(EgressSource::Media {
            audio: Box::new(LoopingBeep { rate: 8000, pos: 0 }),
            loop_playback: true,
            on_end: None,
        })
        .await
        .unwrap();
        let third = tokio::time::timeout(Duration::from_secs(1), track.recv())
            .await
            .expect("next playback frame timeout")
            .expect("next playback frame");
        let MediaSample::Audio(third) = third else {
            panic!("expected audio");
        };

        assert!(
            third.marker,
            "playback after relay must mark the SSRC switch"
        );
        assert!(third.sequence_number.is_none());
        assert!(third.raw_packet.is_none());
        assert!(second.raw_packet.is_none());

        pipe.stop();
        peer_pc.close();
    }

    /// A Media source whose sample rate differs from the egress codec must be
    /// resampled to the codec's rate before encoding. Here a 24 kHz source
    /// feeds an opus (48 kHz) codec: each 20 ms opus frame must consume only
    /// 480 source samples (24k * 20ms), not 960 (48k * 20ms) — otherwise the
    /// audio plays at 2× tempo.
    #[tokio::test]
    async fn media_source_at_different_rate_is_resampled_to_codec_rate() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct CountingSource {
            rate: u32,
            consumed: Arc<AtomicUsize>,
        }
        impl AudioSource for CountingSource {
            fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
                let n = buffer.len();
                self.consumed.fetch_add(n, AtomicOrdering::Relaxed);
                buffer.fill(2000);
                n
            }
            fn sample_rate(&self) -> u32 {
                self.rate
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                true
            }
            fn reset(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let codec = EgressCodec {
            codec: CodecType::Opus,
            payload_type: 111,
            clock_rate: 48000,
            dtmf_payload_type: Some(101),
            comfort_noise: false,
            comfort_noise_level_db: -35.0,
        };
        let (sender, track, _fb) = sample_track(MediaKind::Audio, 64);
        let pipe =
            EgressPipeline::start_with_gate(sender, codec, EgressSource::Silence, Some(20), None);

        let consumed = Arc::new(AtomicUsize::new(0));
        pipe.set_source(EgressSource::Media {
            audio: Box::new(CountingSource {
                rate: 24000,
                consumed: consumed.clone(),
            }),
            loop_playback: true,
            on_end: None,
        })
        .await
        .unwrap();

        // Drain until the media source is definitely being read (consumed > 0),
        // then observe how many source samples 8 more frames consume.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while consumed.load(AtomicOrdering::Relaxed) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "media source never consumed"
            );
            let _ = tokio::time::timeout(Duration::from_millis(500), track.recv()).await;
        }
        let before = consumed.load(AtomicOrdering::Relaxed);
        for _ in 0..8 {
            let _ = tokio::time::timeout(Duration::from_millis(500), track.recv()).await;
        }
        let after = consumed.load(AtomicOrdering::Relaxed);
        let consumed_in_8 = after - before;

        // 8 frames × 480 = 3840 for correct 24k→48k resampling; 8 × 960 = 7680
        // if the source is (wrongly) read at the codec's 48 kHz rate.
        assert!(
            consumed_in_8 >= 3072 && consumed_in_8 <= 4800,
            "expected ~3840 source samples consumed over 8 opus frames (24k→48k resample), got {consumed_in_8}"
        );

        pipe.stop();
    }

    #[tokio::test]
    async fn next_frame_silence_encodes_nonempty() {
        let (_sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        // Build a task directly to inspect frame contents.
        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender: _sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Silence,
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };
        let f = task.next_frame().await.expect("silence yields a frame");
        assert_eq!(f.clock_rate, 8000);
        assert_eq!(f.payload_type, Some(0));
        assert!(
            !f.data.is_empty(),
            "PCMU silence must encode to non-empty bytes"
        );
        assert!(f.sequence_number.is_none(), "encoding alone does not allocate wire numbering");
    }

    #[tokio::test]
    async fn starting_transcode_discards_ivr_backlog_and_forwards_live_audio() {
        let (peer_sender, peer_track, _) = sample_track(MediaKind::Audio, 64);
        let (sender, output_track, _) = sample_track(MediaKind::Audio, 64);
        let mut opus_encoder = create_egress_encoder(CodecType::Opus);
        let old_audio = opus_encoder.encode(&vec![0i16; 960]);
        for i in 0..64 {
            peer_sender.try_send(MediaSample::Audio(AudioFrame {
                data: Bytes::from(old_audio.clone()),
                payload_type: Some(111),
                rtp_timestamp: i * 960,
                clock_rate: 48_000,
                ..Default::default()
            })).unwrap();
        }
        let pipeline = EgressPipeline::start_with_gate(
            sender, pcmu_codec(), EgressSource::Silence, Some(20),
            Some(Arc::new(AtomicBool::new(true))),
        );
        pipeline.set_source(EgressSource::TranscodePeer {
            peer: peer_track,
            decoder: audio_codec::create_decoder(CodecType::Opus),
            source_codec: CodecType::Opus,
            source_audio_payload_type: 111,
            source_dtmf_payload_types: vec![101],
            primed: false,
        }).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(100), output_track.recv()).await.is_err(),
            "pre-connection audio must not be replayed to the newly connected peer");

        let pcm: Vec<i16> = (0..960).map(|i| {
            ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 8000.0) as i16
        }).collect();
        // Send two frames so the 2-frame jitter-absorber prebuffer arms and
        // emits live speech (the first tick emits prebuffer comfort noise).
        for idx in 0..2u32 {
            peer_sender.try_send(MediaSample::Audio(AudioFrame {
                data: Bytes::from(opus_encoder.encode(&pcm)),
                payload_type: Some(111),
                rtp_timestamp: (64 + idx) * 960,
                clock_rate: 48_000,
                ..Default::default()
            })).unwrap();
        }
        let mut decoder = audio_codec::create_decoder(CodecType::PCMU);
        let mut saw_speech = false;
        for _ in 0..4 {
            let MediaSample::Audio(frame) = tokio::time::timeout(Duration::from_millis(500), output_track.recv())
                .await.expect("new speech must be forwarded without the queued 1.28-second delay").unwrap()
            else { panic!("expected audio"); };
            assert_eq!(frame.payload_type, Some(0));
            let decoded = decoder.decode(&frame.data);
            if decoded.iter().any(|s| s.unsigned_abs() > 100) {
                saw_speech = true;
                break;
            }
        }
        assert!(saw_speech, "fresh speech must survive transcoding");
        pipeline.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn transcode_peer_relays_dtmf_with_the_peer_header() {
        use rustrtc::media::AudioStreamTrack;
        let (peer_sender, peer_track, _peer_fb) = sample_track(MediaKind::Audio, 8);
        let (sender, output_track, _output_fb) = sample_track(MediaKind::Audio, 8);

        let dtmf = |duration: u16, end: bool| AudioFrame {
            rtp_timestamp: 77_000,
            clock_rate: 48_000,
            data: Bytes::from(vec![
                5,
                if end { 0x80 | 7 } else { 7 },
                duration.to_be_bytes()[0],
                duration.to_be_bytes()[1],
            ]),
            sequence_number: Some(if end { 302 } else { 300 }),
            payload_type: Some(101),
            marker: !end,
            header_extension: None,
            source_addr: None,
            raw_packet: Some(rustrtc::rtp::RtpPacket::new(
                rustrtc::rtp::RtpHeader::new(101, if end { 302 } else { 300 }, 77_000, 0xB),
                vec![5, if end { 0x87 } else { 7 }, duration.to_be_bytes()[0], duration.to_be_bytes()[1]],
            )),
        };
        peer_sender
            .try_send(MediaSample::Audio(dtmf(960, false)))
            .unwrap();
        peer_sender
            .try_send(MediaSample::Audio(dtmf(4800, true)))
            .unwrap();

        let mut opus_encoder = create_encoder(CodecType::Opus);
        let opus = opus_encoder.encode(&vec![0i16; 960]);
        peer_sender
            .try_send(MediaSample::Audio(AudioFrame {
                rtp_timestamp: 78_000,
                clock_rate: 48_000,
                data: Bytes::from(opus.clone()),
                sequence_number: None,
                payload_type: Some(111),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: Some(rustrtc::rtp::RtpPacket::new(
                    rustrtc::rtp::RtpHeader::new(111, 303, 78_000, 0xB),
                    opus,
                )),
            }))
            .unwrap();

        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::TranscodePeer {
                peer: peer_track,
                decoder: audio_codec::create_decoder(CodecType::Opus),
                source_codec: CodecType::Opus,
                source_audio_payload_type: 111,
                source_dtmf_payload_types: vec![101],
                primed: true,
            },
            resampler: Some(BoxedResampler::new(48_000, 8000).expect("valid sample rates")),
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };

        let first = task.next_frame().await.expect("first DTMF frame");
        let second = task.next_frame().await.expect("second DTMF frame");
        let audio = task.next_frame().await.expect("transcoded audio frame");

        // DTMF keeps peer identity until the sender maps it as a packet,
        // scaling both timestamp and duration to the destination clock.
        assert_eq!(first.sequence_number, Some(300));
        assert_eq!(second.sequence_number, Some(302));
        assert_eq!(first.raw_packet.as_ref().unwrap().header.ssrc, 0xB);
        assert_eq!(second.raw_packet.as_ref().unwrap().header.sequence_number, 302);
        assert!(audio.raw_packet.is_none(), "transcoded RTP must become frame behavior");
        assert!(audio.sequence_number.is_none(), "generated audio is numbered by rustrtc");
        assert_eq!(first.payload_type, Some(101));
        assert_eq!((first.rtp_timestamp, first.clock_rate), (77_000, 48_000));
        assert_eq!(u16::from_be_bytes([first.data[2], first.data[3]]), 160);
        assert_eq!(second.payload_type, Some(101));
        assert_eq!(second.rtp_timestamp, first.rtp_timestamp);
        assert_eq!(u16::from_be_bytes([second.data[2], second.data[3]]), 800);
        // The generated audio keeps this pipeline's own numbering.
        assert_eq!(audio.payload_type, Some(0));
        assert_eq!(audio.rtp_timestamp, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), output_track.recv())
                .await
                .is_err(),
            "next_frame must return packets without sending them directly"
        );

        // Hold the consumer back: retain packet identity and generated ptime
        // in queue order for assignment by the sender.
        peer_sender.try_send_audio(dtmf(960, false)).unwrap();
        peer_sender.try_send_audio(dtmf(4800, true)).unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let run = tokio::spawn(task.run(cmd_rx, Duration::from_millis(20), cancel.clone()));
        tokio::time::sleep(Duration::from_millis(65)).await;
        let first = output_track.recv_audio().await.unwrap();
        let second = output_track.recv_audio().await.unwrap();
        let audio = output_track.recv_audio().await.unwrap();
        assert_eq!(first.sequence_number, Some(300));
        assert_eq!(second.sequence_number, Some(302), "source packet loss stays visible");
        assert_eq!(second.rtp_timestamp, first.rtp_timestamp, "DTMF event time stays fixed");
        assert!(audio.sequence_number.is_none());
        assert!(audio.raw_packet.is_none());
        assert!(first.raw_packet.is_some(), "DTMF retains packet identity for the sender");

        let peer_pc = PeerConnection::new(rustrtc::RtcConfiguration::webrtc());
        let (completed, ack) = oneshot::channel();
        cmd_tx.send(EgressCmd::SetSource(EgressSource::RewriteRelay {
            peer_pc: peer_pc.clone(),
            options: RtpRewriteBridgeOptions::default(),
            routes: Vec::new(),
            on_arm_failed: None,
        }, completed)).await.unwrap();
        ack.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(40), output_track.recv()).await.is_err(),
            "switch acknowledgement must discard queued playback and park the producer");
        cancel.cancel();
        run.await.unwrap();
        peer_pc.close();
    }

    #[tokio::test]
    async fn next_frame_media_falls_back_to_silence_on_eof_without_loop() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));

        struct Empty;
        impl AudioSource for Empty {
            fn read_samples(&mut self, _b: &mut [i16]) -> usize {
                0
            }
            fn sample_rate(&self) -> u32 {
                8000
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                false
            }
            fn reset(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Media {
                audio: Box::new(Empty),
                loop_playback: false,
                on_end: None,
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };
        // has_data() false + no loop → source becomes Silence, still yields a frame.
        let f = task
            .next_frame()
            .await
            .expect("EOF media yields silence frame");
        assert!(matches!(task.source, EgressSource::Silence));
        assert!(!f.data.is_empty());
    }

    #[test]
    fn pcm_samples_per_frame_is_correct() {
        // PCMU @ 8kHz, 20ms → 160 samples
        assert_eq!(
            pcm_samples_per_frame(CodecType::PCMU, Duration::from_millis(20)),
            160
        );
        // Opus @ 48kHz, 20ms → 960 samples
        assert_eq!(
            pcm_samples_per_frame(CodecType::Opus, Duration::from_millis(20)),
            960
        );
    }

    /// The catch-up after a stall is wall-clock anchored but bounded: at most
    /// [`MAX_CATCHUP_FRAMES`] frames go out back to back, then the remaining
    /// backlog is dropped by re-anchoring `next_deadline` to `now + ptime`.
    #[test]
    fn pacing_catchup_is_bounded_after_a_stall() {
        use tokio::time::{Duration, Instant};

        let ptime = Duration::from_millis(20);
        let t0 = Instant::now();
        let mut deadline = t0;
        let mut catchup = 0u32;

        // On-time cadence: no catch-up, no drops, deadline stays wall-anchored.
        for i in 0..5u32 {
            let now = t0 + ptime * i;
            assert_eq!(
                advance_pacing(&mut deadline, now, ptime, &mut catchup),
                None
            );
            assert_eq!(catchup, 0);
        }
        assert_eq!(deadline, t0 + ptime * 5);

        // A 300 ms stall (15 frames of backlog): only MAX_CATCHUP_FRAMES
        // back-to-back frames plus the current one are emitted; the rest is
        // dropped and the deadline re-anchors one ptime after `now`.
        let now = t0 + Duration::from_millis(300);
        let mut dropped = Duration::ZERO;
        for _ in 0..=MAX_CATCHUP_FRAMES {
            if let Some(d) = advance_pacing(&mut deadline, now, ptime, &mut catchup) {
                dropped = d;
            }
        }
        assert!(
            dropped >= ptime,
            "a 300 ms stall must drop the excess backlog, dropped {dropped:?}"
        );
        assert_eq!(deadline, now + ptime, "schedule re-anchors to now + ptime");
        assert_eq!(catchup, 0, "cap resets the catch-up counter");

        // The re-anchored schedule resumes the normal cadence: the next frame
        // is on time, not an immediate catch-up tick.
        assert_eq!(
            advance_pacing(&mut deadline, now + ptime, ptime, &mut catchup),
            None
        );
        assert_eq!(catchup, 0);
    }

    /// `media_source_for_codec` must wrap a 24 kHz source in a resampler that
    /// produces 48 kHz PCM: a 960-sample read (20 ms @48k) consumes exactly 480
    /// source samples and the output, once decoded from opus, is 960 @48k.
    #[test]
    fn media_source_for_codec_resamples_upsample_24k_to_48k() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct FixedRateSource {
            rate: u32,
            consumed: Arc<AtomicUsize>,
        }
        impl AudioSource for FixedRateSource {
            fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
                let n = buffer.len();
                self.consumed.fetch_add(n, AtomicOrdering::Relaxed);
                buffer.fill(2000);
                n
            }
            fn sample_rate(&self) -> u32 {
                self.rate
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                true
            }
            fn reset(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let consumed = Arc::new(AtomicUsize::new(0));
        let mut wrapped = media_source_for_codec(
            Box::new(FixedRateSource {
                rate: 24000,
                consumed: consumed.clone(),
            }),
            CodecType::Opus,
        );
        assert_eq!(
            wrapped.sample_rate(),
            48000,
            "resampled source reports codec rate"
        );

        let mut buf = vec![0i16; 960];
        let read = wrapped.read_samples(&mut buf);
        assert_eq!(read, 960, "one 20ms frame must yield 960 PCM samples");
        assert_eq!(
            consumed.load(AtomicOrdering::Relaxed),
            480,
            "24000→48000: 960 output samples must consume 480 source samples"
        );
    }

    /// `media_source_for_codec` is a passthrough when the source rate already
    /// matches the codec rate (no resampler, no extra buffering).
    #[test]
    fn media_source_for_codec_passthrough_when_rates_match() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct FixedRateSource {
            rate: u32,
            consumed: Arc<AtomicUsize>,
        }
        impl AudioSource for FixedRateSource {
            fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
                let n = buffer.len();
                self.consumed.fetch_add(n, AtomicOrdering::Relaxed);
                buffer.fill(2000);
                n
            }
            fn sample_rate(&self) -> u32 {
                self.rate
            }
            fn channels(&self) -> u16 {
                1
            }
            fn has_data(&self) -> bool {
                true
            }
            fn reset(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let consumed = Arc::new(AtomicUsize::new(0));
        let mut source = media_source_for_codec(
            Box::new(FixedRateSource {
                rate: 8000,
                consumed: consumed.clone(),
            }),
            CodecType::PCMU,
        );
        assert_eq!(source.sample_rate(), 8000);

        let mut buf = vec![0i16; 160];
        let read = source.read_samples(&mut buf);
        assert_eq!(read, 160);
        assert_eq!(
            consumed.load(AtomicOrdering::Relaxed),
            160,
            "matching rates → direct read, no intermediate buffer"
        );
    }

    /// A Media source with a finite tail of data before EOF.
    struct FiniteSource {
        frames_left: u32,
    }
    impl AudioSource for FiniteSource {
        fn read_samples(&mut self, buffer: &mut [i16]) -> usize {
            if self.frames_left == 0 {
                return 0;
            }
            self.frames_left -= 1;
            buffer.fill(2_000);
            buffer.len()
        }
        fn sample_rate(&self) -> u32 {
            8000
        }
        fn channels(&self) -> u16 {
            1
        }
        fn has_data(&self) -> bool {
            self.frames_left > 0
        }
        fn reset(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// Regression for the clipped-final-syllable bug: natural EOF must not
    /// fire `on_end(false)` immediately. The pipeline streams silence for
    /// `eof_tail_total` ticks first, so the frames already handed to the
    /// sender can drain through the queue / network / remote jitter buffer
    /// before the app sees the completion event (apps may hang up on it).
    #[tokio::test]
    async fn natural_eof_fires_on_end_after_grace_tail() {
        let (end_tx, mut end_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let tail = 3;
        let mut task = EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Media {
                audio: Box::new(FiniteSource { frames_left: 1 }),
                loop_playback: false,
                on_end: Some(Arc::new(move |interrupted| {
                    end_tx.send(interrupted).unwrap();
                })),
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-35.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: tail,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        };

        let speech_ref: Bytes = create_encoder(CodecType::PCMU)
            .encode(&vec![2_000i16; spf])
            .into();
        let silence_ref: Bytes = create_encoder(CodecType::PCMU)
            .encode(&vec![0i16; spf])
            .into();

        // Tick 1: the only real frame.
        let f = task.next_frame().await.expect("speech frame");
        assert_eq!(f.data, speech_ref, "first tick must carry the source audio");

        // Ticks 2..tail: the grace tail streams silence, completion not yet
        // fired.
        for _ in 0..tail - 1 {
            let f = task.next_frame().await.expect("tail frame");
            assert_eq!(f.data, silence_ref, "tail ticks must emit silence");
            assert!(
                matches!(task.source, EgressSource::Media { .. }),
                "tail must keep the Media source alive"
            );
            assert!(
                end_rx.try_recv().is_err(),
                "on_end must not fire before the tail expires"
            );
        }

        // Tail expiry: on_end(false) fires exactly once and Media drains to
        // Silence.
        let f = task.next_frame().await.expect("tail expiry frame");
        assert!(matches!(task.source, EgressSource::Silence));
        assert_eq!(f.data, silence_ref);
        assert_eq!(
            end_rx.try_recv().ok(),
            Some(false),
            "on_end must fire with interrupted=false at tail expiry"
        );
        assert!(end_rx.try_recv().is_err(), "on_end must not fire twice");
    }

    /// Switching sources during the grace tail reports the playback as
    /// interrupted immediately (barge-in / stop_play must not wait out the
    /// tail).
    #[tokio::test]
    async fn source_switch_during_tail_fires_interrupted() {
        let (end_tx, mut end_rx) = tokio::sync::mpsc::unbounded_channel();
        let (sender, track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = pcmu_codec();
        let pipe =
            EgressPipeline::start_with_gate(sender, codec, EgressSource::Silence, Some(20), None);

        pipe.set_source(EgressSource::Media {
            audio: Box::new(FiniteSource { frames_left: 1 }),
            loop_playback: false,
            on_end: Some(Arc::new(move |interrupted| {
                end_tx.send(interrupted).unwrap();
            })),
        })
        .await
        .unwrap();

        // Drain the real frame plus one tail frame (default tail ≫ 1 tick),
        // then switch away mid-tail.
        let _ = tokio::time::timeout(Duration::from_millis(500), track.recv())
            .await
            .expect("speech frame")
            .expect("sample");
        let _ = tokio::time::timeout(Duration::from_millis(500), track.recv())
            .await
            .expect("first tail frame")
            .expect("sample");

        pipe.set_source(EgressSource::Silence).await.unwrap();
        let interrupted = tokio::time::timeout(Duration::from_millis(500), end_rx.recv())
            .await
            .expect("on_end fired promptly on switch")
            .expect("callback sent");
        assert!(
            interrupted,
            "switch during tail must report interrupted=true, not natural EOF"
        );
        assert!(end_rx.try_recv().is_err(), "on_end must not fire twice");

        pipe.stop();
    }
}

/// Regression tests for the TranscodePeer drain fix: peers whose ptime is
/// shorter than this leg's ptime (e.g. 10 ms opus into a 20 ms ptime leg)
/// must not lose frames to the receiver ring's drop-oldest overflow between
/// pacing ticks.
#[cfg(test)]
mod transcode_drain_tests {
    use super::*;
    use audio_codec::{create_decoder, create_encoder};
    use rustrtc::media::MediaKind;
    use rustrtc::media::track::sample_track;

    fn pcma_codec() -> EgressCodec {
        EgressCodec {
            codec: CodecType::PCMA,
            payload_type: 8,
            clock_rate: 8000,
            dtmf_payload_type: Some(101),
            comfort_noise: false,
            comfort_noise_level_db: -35.0,
        }
    }

    /// Build an `EgressTask` with a `TranscodePeer` source (PCMU peer →
    /// `codec` output) without spawning the pacing loop, so tests can drive
    /// `next_frame()` deterministically.
    fn make_transcode_task(
        sender: SampleStreamSource,
        peer: Arc<dyn MediaStreamTrack>,
        codec: EgressCodec,
    ) -> EgressTask {
        EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec,
            encoder: create_egress_encoder(codec.codec),
            source: EgressSource::TranscodePeer {
                peer,
                decoder: create_decoder(CodecType::PCMU),
                source_codec: CodecType::PCMU,
                source_audio_payload_type: 0,
                source_dtmf_payload_types: vec![101],
                primed: false,
            },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; pcm_samples_per_frame(codec.codec, Duration::from_millis(20))],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        }
    }

    /// A PCMU RTP audio frame: `ms` milliseconds of μ-law silence
    /// (8 samples per ms at 8 kHz, one byte per sample).
    fn pcmu_frame(ms: u32, ts: u32, seq: u16) -> MediaSample {
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate: 8000,
            data: Bytes::from(vec![0xFFu8; ms as usize * 8]),
            sequence_number: Some(seq),
            payload_type: Some(0),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        })
    }

    /// THE regression: a 10 ms-ptime peer feeding a 20 ms ptime leg must come
    /// out lossless. Before the drain, one frame per tick was consumed and
    /// every other frame was dropped by the receiver ring (recording duration
    /// ≈ half the call duration).
    #[tokio::test]
    async fn drain_consumes_short_peer_frames_without_loss() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 512);
        for i in 0..200u16 {
            in_sender
                .try_send(MediaSample::Audio(AudioFrame {
                    rtp_timestamp: i as u32 * 80,
                    clock_rate: 8000,
                    data: Bytes::from(vec![0xFFu8; 80]),
                    sequence_number: Some(i),
                    payload_type: Some(0),
                    marker: false,
                    header_extension: None,
                    source_addr: None,
                    raw_packet: None,
                }))
                .expect("ring holds all frames");
        }
        assert_eq!(in_sender.drop_count(), 0, "input ring sized to fit");

        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());
        for i in 0..100 {
            let frame = task
                .next_frame()
                .await
                .unwrap_or_else(|| panic!("frame {i} missing — input was pre-buffered"));
            assert_eq!(frame.data.len(), 160, "each output is one full 20 ms PCMA frame");
        }
        // 200 × 10 ms in → 100 × 20 ms out: zero loss, nothing pending,
        // nothing dropped by either the ring or the accumulator cap.
        assert_eq!(task.transcode_pending.len(), 0);
        assert_eq!(task.transcode_overflow_samples, 0);
        assert_eq!(
            in_sender.drop_count(),
            0,
            "steady drain must keep the receiver ring empty (drop-oldest never fires)"
        );
    }

    /// Unchanged behaviour guard: a peer whose ptime equals the leg's ptime
    /// maps one frame to one frame, exactly like the pre-drain code path.
    #[tokio::test]
    async fn same_ptime_peer_maps_one_to_one() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 256);
        for i in 0..100u16 {
            in_sender
                .try_send(pcmu_frame(20, i as u32 * 160, i))
                .expect("ring holds all frames");
        }
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());
        for _ in 0..100 {
            let frame = task.next_frame().await.expect("frame expected");
            assert_eq!(frame.data.len(), 160);
            assert!(frame.sequence_number.is_none());
            assert!(frame.raw_packet.is_none());
        }
        assert_eq!(task.transcode_pending.len(), 0);
        assert_eq!(task.transcode_overflow_samples, 0);
        assert_eq!(in_sender.drop_count(), 0);
    }

    /// Peer ptime longer than the leg's ptime (30 ms → 20 ms): carry the
    /// remainder across ticks, including the initial jitter-buffer fill.
    /// Once the peer goes quiet, flush all complete frames.
    #[tokio::test(start_paused = true)]
    async fn long_ptime_peer_carries_remainder_without_loss() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 64);
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());
        // Each tick after the initial buffer fill consumes one 20 ms frame.
        for i in 0..6u16 {
            in_sender.try_send(pcmu_frame(30, i as u32 * 240, i)).unwrap();
            let frame = task.next_frame().await.expect("frame while peer alive");
            assert_eq!(frame.data.len(), 160);
        }
        // Queues drained: tick 0 prebuffered (240 < 320), ticks 1..=5 emitted
        // 5 frames (leaving 640 samples = 4 × 20 ms frames) — 4 flush ticks
        // drain all remaining PCM.
        for _ in 0..4 {
            let frame = task.next_frame().await.expect("carry flush tick");
            assert_eq!(frame.data.len(), 160);
        }
        // 6 × 30 ms = 180 ms = 1440 samples = exactly 9 × 20 ms frames.
        assert_eq!(task.transcode_pending.len(), 0, "all PCM flushed");
        assert_eq!(task.transcode_overflow_samples, 0, "carry never hits the cap");
        assert_eq!(in_sender.drop_count(), 0);
    }

    /// Sustained overload (peer produces 1.5× the leg's output rate) must be
    /// bounded: the accumulator drops oldest beyond its cap and counts the
    /// dropped samples instead of growing without limit.
    #[tokio::test]
    async fn accumulator_cap_bounds_sustained_overload() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 128);
        for i in 0..64u16 {
            in_sender
                .try_send(pcmu_frame(30, i as u32 * 240, i))
                .expect("ring holds all frames");
        }
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());
        for _ in 0..64 {
            let _ = task.next_frame().await;
        }
        assert!(
            task.transcode_overflow_samples > 0,
            "1.5× overload must eventually hit the cap"
        );
        assert!(
            task.transcode_pending.len()
                <= 160 * TRANSCODE_PENDING_MAX_FRAMES,
            "accumulator must stay within the cap"
        );
    }

    /// A telephone-event frame encountered mid-drain is emitted on the very
    /// tick that popped it (pre-drain semantics), and audio continues after.
    #[tokio::test]
    async fn dtmf_frame_mid_drain_emits_and_recovers() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 64);
        // a1, a2 (10 ms audio), then an RFC 2833 event, then a3 (20 ms) + a4 (10 ms).
        for (idx, sample) in [
            pcmu_frame(10, 0, 0),
            pcmu_frame(10, 80, 1),
            MediaSample::Audio(AudioFrame {
                rtp_timestamp: 160,
                clock_rate: 8000,
                // event=5, flags=volume 10 (no end bit), duration=160
                data: Bytes::from_static(&[0x05, 0x0A, 0x00, 0xA0]),
                sequence_number: Some(2),
                payload_type: Some(101),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: Some(rustrtc::rtp::RtpPacket::new(
                    rustrtc::rtp::RtpHeader::new(101, 2, 160, 0xB),
                    vec![0x05, 0x0A, 0x00, 0xA0],
                )),
            }),
            pcmu_frame(20, 240, 3),
            pcmu_frame(10, 400, 4),
        ]
        .into_iter()
        .enumerate()
        {
            in_sender
                .try_send(sample)
                .unwrap_or_else(|e| panic!("push {idx}: {e}"));
        }
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());

        // Tick 1 pops a1 + a2 (160 samples) via drain, hits the DTMF frame, emits it.
        let first = task.next_frame().await.expect("dtmf tick");
        assert_eq!(first.payload_type, Some(101), "DTMF frame emitted on its tick");
        assert!(first.raw_packet.is_some(), "mid-drain DTMF keeps packet behavior");

        // Tick 2 drains a3 + a4 (400 samples >= 320 prebuffer -> arms) and emits
        // one full 20 ms frame; tick 3 emits the second full 20 ms frame, leaving
        // the 80-sample tail carried for future audio.
        let second = task.next_frame().await.expect("audio tick after dtmf");
        assert_eq!(second.payload_type, Some(8));
        assert!(second.raw_packet.is_none(), "transcoded audio uses frame behavior");
        assert_eq!(second.data.len(), 160);
        let third = task.next_frame().await.expect("second audio tick after dtmf");
        assert_eq!(third.payload_type, Some(8));
        assert_eq!(third.data.len(), 160);

        // Everything consumed: a1+a2+a3+a4 = 400 samples; 320 emitted across
        // two 20 ms frames, 80-sample tail stays carried (no loss, no overflow).
        assert_eq!(task.transcode_pending.len(), 80);
        assert_eq!(task.transcode_overflow_samples, 0);
        assert_eq!(in_sender.drop_count(), 0);
    }

    /// Before the first audio, skip ticks. After it arrives, keep the
    /// output cadence while the jitter buffer fills and after it drains.
    #[tokio::test(start_paused = true)]
    async fn transcode_keeps_cadence_after_priming() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 64);
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());

        assert!(
            task.next_frame().await.is_none(),
            "timeout before first audio must skip the tick"
        );

        in_sender.try_send(pcmu_frame(20, 0, 0)).unwrap();
        let first = task.next_frame().await.expect("first audio primes");
        assert_eq!(first.payload_type, Some(8));
        assert!(matches!(
            task.source,
            EgressSource::TranscodePeer { primed: true, .. }
        ));
        // First frame primes the stream and stays in the 2-frame prebuffer.
        assert_eq!(task.transcode_pending.len(), 160);

        // Second frame arms the prebuffer and emits the first real frame.
        in_sender.try_send(pcmu_frame(20, 160, 1)).unwrap();
        let second = task.next_frame().await.expect("second audio arms");
        assert_eq!(second.payload_type, Some(8));
        assert!(task.transcode_armed);
        assert_eq!(task.transcode_pending.len(), 160);

        // Next tick flushes the remaining buffered frame; the following empty
        // tick emits silence (never None — the paced stream must not gap).
        let third = task.next_frame().await.expect("flush buffered frame");
        assert_eq!(third.payload_type, Some(8));
        assert_eq!(task.transcode_pending.len(), 0);

        let silence = task.next_frame().await.expect("post-primed silence");
        assert_eq!(silence.payload_type, Some(8));
        assert_eq!(task.transcode_pending.len(), 0);
    }

    /// Partial 10 ms frames fed one-per-tick on a 20 ms leg are buffered and
    /// merged into clean 20 ms frames once the 2-frame prebuffer arms — each
    /// 10 ms segment appears exactly once in order, never repeated or dropped.
    #[tokio::test(start_paused = true)]
    async fn buffered_partial_frames_are_emitted_once() {
        let (out_sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let (in_sender, in_track, _ifb) = sample_track(MediaKind::Audio, 64);
        let mut task = make_transcode_task(out_sender, in_track, pcma_codec());
        let mut pcma_dec = create_decoder(CodecType::PCMA);
        let mut pcmu_enc = create_encoder(CodecType::PCMU);

        // Feed four distinct 10 ms segments one per tick (4 × 80 = 320 samples
        // = 2 full 20 ms frames). Ticks 0..2 prebuffer; tick 3 arms and emits
        // [6000, -6000]; tick 4 flushes [3000, -3000].
        let amps = [6_000i16, -6_000, 3_000, -3_000];
        let mut emitted = Vec::new();
        for (i, amp) in amps.into_iter().enumerate() {
            in_sender
                .try_send(MediaSample::Audio(AudioFrame {
                    rtp_timestamp: i as u32 * 80,
                    clock_rate: 8000,
                    data: pcmu_enc.encode(&[amp; 80]).into(),
                    sequence_number: Some(i as u16),
                    payload_type: Some(0),
                    marker: false,
                    header_extension: None,
                    source_addr: None,
                    raw_packet: None,
                }))
                .unwrap();

            let frame = task.next_frame().await.expect("paced frame");
            assert_eq!(frame.data.len(), 160, "one full 20 ms PCMA frame");
            if i == 3 {
                emitted.push(pcma_dec.decode(&frame.data));
            }
        }
        // Flush the second buffered 20 ms frame.
        let flush = task.next_frame().await.expect("flush second frame");
        assert_eq!(flush.data.len(), 160);
        emitted.push(pcma_dec.decode(&flush.data));

        let expected_halves: [(i16, i16); 2] = [(6_000, -6_000), (3_000, -3_000)];
        for (idx, (decoded, (first_amp, second_amp))) in
            emitted.iter().zip(expected_halves).enumerate()
        {
            assert_eq!(decoded.len(), 160);
            for (j, s) in decoded[..80].iter().enumerate() {
                assert_eq!(
                    s.signum(),
                    first_amp.signum(),
                    "frame {idx} first half sample {j}: got {s}, expected {first_amp}"
                );
                assert!(
                    ((*s as i32) - first_amp as i32).abs() < (first_amp as i32).abs() / 2 + 64,
                    "frame {idx} first half sample {j}: {s} not close to {first_amp}"
                );
            }
            for (j, s) in decoded[80..].iter().enumerate() {
                assert_eq!(
                    s.signum(),
                    second_amp.signum(),
                    "frame {idx} second half sample {j}: got {s}, expected {second_amp}"
                );
                assert!(
                    ((*s as i32) - second_amp as i32).abs() < (second_amp as i32).abs() / 2 + 64,
                    "frame {idx} second half sample {j}: {s} not close to {second_amp}"
                );
            }
        }
        // All four segments were fully consumed: nothing carried, nothing dropped.
        assert_eq!(task.transcode_pending.len(), 0);
        assert_eq!(task.transcode_overflow_samples, 0);
        assert_eq!(in_sender.drop_count(), 0);
    }
}

#[cfg(test)]
mod transcode_dtx_tests {
    use super::*;
    use audio_codec::{create_decoder, create_encoder};
    use rustrtc::media::MediaKind;
    use rustrtc::media::track::sample_track;

    fn sine_pcm(sample_rate: u32, samples: usize, freq_hz: f32) -> Vec<i16> {
        (0..samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((t * freq_hz * std::f32::consts::TAU).sin() * 12_000.0) as i16
            })
            .collect()
    }

    fn dst_codec(codec: CodecType, comfort_noise: bool) -> EgressCodec {
        EgressCodec {
            codec,
            payload_type: codec.payload_type(),
            clock_rate: codec.clock_rate(),
            dtmf_payload_type: Some(101),
            comfort_noise,
            comfort_noise_level_db: -35.0,
        }
    }

    fn make_task(
        peer: Arc<dyn MediaStreamTrack>,
        source_codec: CodecType,
        source_audio_payload_type: u8,
        source_dtmf_payload_types: Vec<u8>,
        dst: EgressCodec,
    ) -> EgressTask {
        let (sender, _out_track, _fb) = sample_track(MediaKind::Audio, 64);
        let decoder = create_decoder(source_codec);
        let src_sample_rate = decoder.sample_rate();
        let dst_sample_rate = dst.codec.samplerate();
        let resampler = (src_sample_rate != dst_sample_rate)
            .then(|| BoxedResampler::new(src_sample_rate as usize, dst_sample_rate as usize).unwrap());
        EgressTask {
            volume: None,
            volume_sink: Arc::new(parking_lot::Mutex::new(Default::default())),
            sender,
            codec: dst,
            encoder: create_egress_encoder(dst.codec),
            source: EgressSource::TranscodePeer {
                peer,
                decoder,
                source_codec,
                source_audio_payload_type,
                source_dtmf_payload_types,
                primed: false,
            },
            resampler,
            ptime: Duration::from_millis(20),
            gate: None,
            marker_pending: false,
            pcm_buf: vec![0i16; pcm_samples_per_frame(dst.codec, Duration::from_millis(20))],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(dst.comfort_noise_level_db / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
            eof_tail_total: 0,
            eof_tail_remaining: 0,
            transcode_pending: Vec::new(),
            transcode_overflow_samples: 0,
            transcode_armed: false,
            transcode_in_dtx: false,
            transcode_sid_silence_pending: false,
        }
    }

    fn raw_audio_sample(pt: u8, seq: u16, ts: u32, clock_rate: u32, payload: Vec<u8>) -> MediaSample {
        let raw = rustrtc::rtp::RtpPacket::new(
            rustrtc::rtp::RtpHeader::new(pt, seq, ts, 0xBEEF),
            payload.clone(),
        );
        MediaSample::Audio(AudioFrame {
            rtp_timestamp: ts,
            clock_rate,
            data: Bytes::from(payload),
            sequence_number: Some(seq),
            payload_type: Some(pt),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: Some(raw),
        })
    }

    /// RFC 3389 Comfort Noise (PT 13) — including 4-byte spectral CN that
    /// matches the length of a DTMF packet — must enter DTX silence instead of
    /// being misclassified and forwarded as PT 101 telephone-event DTMF.
    /// Unknown non-DTMF PTs (e.g. unsolicited audio PT) must also never be
    /// emitted as DTMF.
    #[tokio::test(start_paused = true)]
    async fn cn_and_unknown_pts_never_spoof_dtmf_in_transcode() {
        let (in_sender, in_track, _) = sample_track(MediaKind::Audio, 32);
        let mut task = make_task(
            in_track,
            CodecType::G729,
            18,
            vec![101],
            dst_codec(CodecType::PCMU, false),
        );
        let pcmu_silence: Bytes = create_encoder(CodecType::PCMU).encode(&vec![0i16; 160]).into();

        // 4-byte PT 13 CN (1 B noise level + 3 B spectral model): previously
        // fell into `else` → `build_dtmf_frame` and was sent as PT 101 DTMF!
        in_sender
            .try_send(raw_audio_sample(13, 1, 160, 8000, vec![0x05, 0x0A, 0x00, 0xA0]))
            .unwrap();
        let frame = task.next_frame().await.expect("CN primes and emits silence");
        assert_eq!(frame.payload_type, Some(0), "PT 13 CN must emit audio silence, never PT 101 DTMF");
        assert_eq!(frame.data, pcmu_silence);
        assert!(task.transcode_in_dtx);

        // 1-byte PT 13 CN followed immediately by 2 speech frames in the same
        // drain must not abort draining the queued speech behind it.
        let speech = create_encoder(CodecType::G729).encode(&sine_pcm(8000, 160, 440.0));
        in_sender
            .try_send(raw_audio_sample(13, 2, 320, 8000, vec![40]))
            .unwrap();
        in_sender
            .try_send(raw_audio_sample(18, 3, 480, 8000, speech.clone()))
            .unwrap();
        in_sender
            .try_send(raw_audio_sample(18, 4, 640, 8000, speech.clone()))
            .unwrap();
        let resumed = task.next_frame().await.expect("resumed speech after CN");
        assert_eq!(resumed.payload_type, Some(0));
        assert_ne!(resumed.data, pcmu_silence, "queued speech behind CN must decode on this tick");
        assert!(!task.transcode_in_dtx);
        assert!(task.transcode_armed);

        // Unsolicited unknown PT (e.g. 160-byte PCMU on PT 0, or PT 99 4-byte):
        // must be ignored rather than re-tagged as PT 101 DTMF.
        in_sender
            .try_send(raw_audio_sample(0, 5, 800, 8000, vec![0x7f; 160]))
            .unwrap();
        in_sender
            .try_send(raw_audio_sample(99, 6, 960, 8000, vec![1, 0x8a, 0, 160]))
            .unwrap();
        let after_unknown = task.next_frame().await.expect("buffered speech frame");
        assert_eq!(after_unknown.payload_type, Some(0), "unknown PTs must never spoof DTMF");
        assert_ne!(after_unknown.data, pcmu_silence);
    }

    /// G.729 Annex B DTX:
    /// - 22-byte mixed packet (20 ms speech + 2 B SID) emits speech cleanly
    ///   without stranding 80 CNG samples in `transcode_pending`.
    /// - Repeated standalone 2-byte SIDs during DTX never accumulate into
    ///   CNG bursts spliced into silence.
    /// - Exiting DTX re-arms the 2-frame jitter absorber (`transcode_armed`).
    #[tokio::test(start_paused = true)]
    async fn g729_sid_and_mixed_22b_suppress_cng_and_rearm_jitter_buffer() {
        let (in_sender, in_track, _) = sample_track(MediaKind::Audio, 32);
        let mut task = make_task(
            in_track,
            CodecType::G729,
            18,
            vec![101],
            dst_codec(CodecType::PCMU, false),
        );
        let mut enc = create_encoder(CodecType::G729);
        let speech_a = enc.encode(&sine_pcm(8000, 160, 440.0));
        let speech_b = enc.encode(&sine_pcm(8000, 160, 660.0));
        assert_eq!(speech_a.len(), 20);
        let pcmu_silence: Bytes = create_encoder(CodecType::PCMU).encode(&vec![0i16; 160]).into();

        // Prime + arm with 2 speech frames.
        in_sender.try_send(raw_audio_sample(18, 1, 160, 8000, speech_a.clone())).unwrap();
        in_sender.try_send(raw_audio_sample(18, 2, 320, 8000, speech_a.clone())).unwrap();
        let f1 = task.next_frame().await.expect("first speech");
        assert_ne!(f1.data, pcmu_silence);
        assert!(task.transcode_armed);
        assert_eq!(task.transcode_pending.len(), 160);

        // Send 22-byte mixed packet (20 ms speech + 2 B SID) and enter DTX.
        let mut mixed_22 = speech_b.clone();
        mixed_22.extend_from_slice(&[0x12, 0x34]);
        in_sender.try_send(raw_audio_sample(18, 3, 480, 8000, mixed_22)).unwrap();

        let f2 = task.next_frame().await.expect("buffered speech");
        assert_ne!(f2.data, pcmu_silence);
        assert!(task.transcode_in_dtx);
        assert_eq!(task.transcode_pending.len(), 160, "SID CNG must not add 80 samples");

        let f3 = task.next_frame().await.expect("mixed packet speech");
        assert_ne!(f3.data, pcmu_silence);
        assert_eq!(task.transcode_pending.len(), 0, "no stranded 80-sample SID tail");

        // Next tick transitions to pure silence and resets transcode_armed.
        let sil1 = task.next_frame().await.expect("DTX silence 1");
        assert_eq!(sil1.data, pcmu_silence);
        assert!(!task.transcode_armed, "DTX silence must reset jitter prebuffer");

        // Four standalone 2-byte SIDs during DTX must never accumulate into CNG bursts.
        for seq in 4..8u16 {
            in_sender
                .try_send(raw_audio_sample(18, seq, u32::from(seq) * 800, 8000, vec![0x12, 0x34]))
                .unwrap();
            let sil = task.next_frame().await.expect("SID tick must remain pure silence");
            assert_eq!(sil.data, pcmu_silence, "standalone SID must not emit g729-sys CNG");
            assert_eq!(task.transcode_pending.len(), 0);
            assert!(task.transcode_in_dtx);
        }

        // Speech resumes: 1st packet prebuffers (transcode_armed was reset),
        // 2nd packet re-arms and emits clean speech with zero 10 ms shift.
        in_sender.try_send(raw_audio_sample(18, 8, 8000, 8000, speech_a.clone())).unwrap();
        let prebuf = task.next_frame().await.expect("prebuffer tick on resume");
        assert_eq!(prebuf.data, pcmu_silence);
        assert!(!task.transcode_armed);
        assert!(!task.transcode_in_dtx);
        assert_eq!(task.transcode_pending.len(), 160);

        in_sender.try_send(raw_audio_sample(18, 9, 8160, 8000, speech_b)).unwrap();
        let resumed = task.next_frame().await.expect("re-armed speech");
        assert_ne!(resumed.data, pcmu_silence);
        assert!(task.transcode_armed);
        assert_eq!(task.transcode_pending.len(), 160);
    }

    /// A 12-byte G.729 packet (10 ms speech + 2 B SID) entering DTX must flush
    /// its trailing 10 ms of real speech (padded with 10 ms silence) instead of
    /// stranding it in `transcode_pending` or stalling in the prebuffer.
    #[tokio::test(start_paused = true)]
    async fn g729_mixed_12b_flushes_trailing_speech_and_preserves_mid_stream_sid_gap() {
        let (in_sender, in_track, _) = sample_track(MediaKind::Audio, 32);
        let mut task = make_task(
            in_track,
            CodecType::G729,
            18,
            vec![],
            dst_codec(CodecType::PCMU, false),
        );
        let mut enc = create_encoder(CodecType::G729);
        // Warm up G.729's 5 ms lookahead + LPC predictor so the 10 ms slice
        // carries steady-state tone energy rather than cold-start ramp-up.
        let _ = enc.encode(&sine_pcm(8000, 160, 440.0));
        let speech_20 = enc.encode(&sine_pcm(8000, 160, 440.0));
        let mut mixed_12 = speech_20[..10].to_vec();
        mixed_12.extend_from_slice(&[0x12, 0x34]);

        // Case 1: Single 12 B utterance while unarmed must flush immediately.
        in_sender.try_send(raw_audio_sample(18, 1, 160, 8000, mixed_12.clone())).unwrap();
        let frame = task.next_frame().await.expect("12 B utterance must flush");
        let decoded = create_decoder(CodecType::PCMU).decode(&frame.data);
        assert_eq!(decoded.len(), 160);
        assert!(decoded[..80].iter().any(|&s| s.unsigned_abs() > 100), "first 10 ms must be speech");
        assert!(decoded[80..].iter().all(|&s| s == 0), "second 10 ms must be digital silence");
        assert_eq!(task.transcode_pending.len(), 0);
        assert!(task.transcode_in_dtx);

        // Case 2: 12 B (10 ms speech + 2 B SID) followed immediately in the same
        // drain by 20 B speech preserves the 10 ms SID silence gap and stays
        // 20 ms aligned (80 speech + 80 silence + 160 speech = 320 = 2 frames).
        in_sender.try_send(raw_audio_sample(18, 2, 320, 8000, mixed_12)).unwrap();
        in_sender.try_send(raw_audio_sample(18, 3, 480, 8000, speech_20)).unwrap();
        let first = task.next_frame().await.expect("12 B + 10 ms SID frame");
        let second = task.next_frame().await.expect("following 20 B speech frame");
        let d1 = create_decoder(CodecType::PCMU).decode(&first.data);
        let d2 = create_decoder(CodecType::PCMU).decode(&second.data);
        assert!(d1[..80].iter().any(|&s| s.unsigned_abs() > 100));
        assert!(d1[80..].iter().all(|&s| s == 0), "10 ms SID gap must remain silent");
        assert!(d2.iter().any(|&s| s.unsigned_abs() > 100));
        assert_eq!(task.transcode_pending.len(), 0, "exact 20 ms alignment preserved");
    }

    /// Opus DTX (`usedtx=1`) in transcode:
    /// - 1-byte TOC (`[0xF8]`) and 2-byte multi-frame DTX (`[0xFA, 0]`, `[0xFB, 2]`)
    ///   emit clean silence rather than un-faded `opus-rs` PLC.
    /// - A flipped TOC stereo bit on an Opus DTX marker never resets decoder state.
    /// - Malformed short frames (`[0xFA]`, `[0xFB, 7]`) are ignored.
    #[tokio::test(start_paused = true)]
    async fn opus_dtx_suppresses_plc_and_preserves_decoder_channel_state() {
        let (in_sender, in_track, _) = sample_track(MediaKind::Audio, 32);
        let mut task = make_task(
            in_track,
            CodecType::Opus,
            111,
            vec![101],
            dst_codec(CodecType::PCMU, false),
        );
        let mut enc = create_encoder(CodecType::Opus);
        let speech1 = enc.encode(&sine_pcm(48_000, 960, 440.0));
        let speech2 = enc.encode(&sine_pcm(48_000, 960, 550.0));
        let resumed_pkt = enc.encode(&sine_pcm(48_000, 960, 660.0));
        let pcmu_silence: Bytes = create_encoder(CodecType::PCMU).encode(&vec![0i16; 160]).into();

        // Malformed short Opus packets before priming must be ignored (stay unprimed).
        in_sender.try_send(raw_audio_sample(111, 1, 960, 48_000, vec![0xFA])).unwrap();
        in_sender.try_send(raw_audio_sample(111, 2, 1920, 48_000, vec![0xFB, 7])).unwrap();
        assert!(task.next_frame().await.is_none(), "malformed Opus must not prime");

        // Prime + arm with 2 active Opus speech frames.
        in_sender.try_send(raw_audio_sample(111, 3, 2880, 48_000, speech1.clone())).unwrap();
        in_sender.try_send(raw_audio_sample(111, 4, 3840, 48_000, speech2.clone())).unwrap();
        let f1 = task.next_frame().await.expect("speech1");
        assert_ne!(f1.data, pcmu_silence);
        assert!(task.transcode_armed);

        // Enter Opus DTX with a flipped stereo bit in the 1-byte TOC marker.
        let marker = [speech2[0] & !3];
        let flipped_marker = [marker[0] ^ 0x04];
        in_sender.try_send(raw_audio_sample(111, 5, 4800, 48_000, flipped_marker.to_vec())).unwrap();

        // Drain the buffered speech2 frame first.
        let f2 = task.next_frame().await.expect("speech2");
        assert_ne!(f2.data, pcmu_silence);
        assert!(task.transcode_in_dtx);
        assert_eq!(task.transcode_pending.len(), 0);

        // Next tick and subsequent 1-byte / 2-byte DTX markers must emit pure
        // silence (NOT opus-rs PLC output) and keep transcode_pending empty.
        for (seq, dtx) in [
            (6u16, flipped_marker.to_vec()),
            (7, vec![0xFA, 0]),
            (8, vec![0xFB, 2]),
        ] {
            in_sender
                .try_send(raw_audio_sample(111, seq, u32::from(seq) * 19_200, 48_000, dtx))
                .unwrap();
            let sil = task.next_frame().await.expect("Opus DTX silence");
            assert_eq!(sil.data, pcmu_silence, "Opus DTX must emit clean silence, not PLC");
            assert_eq!(task.transcode_pending.len(), 0);
            assert!(!task.transcode_armed);
        }

        // Verify the internal Opus decoder kept its channel count (2, from stock
        // create_encoder) and did not reset across the flipped-channel marker.
        if let EgressSource::TranscodePeer { decoder, .. } = &task.source {
            let expected_channels = if speech2[0] & 0x04 != 0 { 2 } else { 1 };
            assert_eq!(decoder.channels(), expected_channels);
        }

        // Resumed speech clears DTX and produces audio once prebuffer re-arms.
        in_sender.try_send(raw_audio_sample(111, 9, 200_000, 48_000, resumed_pkt.clone())).unwrap();
        in_sender.try_send(raw_audio_sample(111, 10, 200_960, 48_000, resumed_pkt)).unwrap();
        let resumed = task.next_frame().await.expect("resumed Opus speech");
        assert_ne!(resumed.data, pcmu_silence);
        assert!(!task.transcode_in_dtx);
        assert!(task.transcode_armed);
    }

    /// Startup drain on `SetSource(TranscodePeer)` must retain a queued DTX
    /// signal (G.729B SID, Opus DTX, or PT 13 CN) so egress paces silence
    /// instead of going dark when the peer entered DTX before the bridge formed.
    #[tokio::test]
    async fn startup_drain_retains_queued_dtx_and_paces_silence() {
        for (label, src_codec, src_pt, clock, queued_packets) in [
            (
                "g729_sid",
                CodecType::G729,
                18u8,
                8000u32,
                vec![
                    (18u8, vec![0u8; 20]),
                    (18, vec![0x12, 0x34]),
                    (101, vec![1, 0x8a, 0, 160]), // trailing DTMF must not clear DTX
                ],
            ),
            (
                "g729_mixed_22b",
                CodecType::G729,
                18,
                8000,
                vec![(18, vec![0u8; 22])],
            ),
            (
                "opus_dtx",
                CodecType::Opus,
                111,
                48_000,
                vec![
                    (111, vec![0xF8]),
                    (111, vec![0xFA]), // malformed short frame must not clear DTX
                ],
            ),
            (
                "pt13_cn",
                CodecType::PCMU,
                0,
                8000,
                vec![(0, vec![0xFF; 160]), (13, vec![40])],
            ),
        ] {
            let (peer_sender, peer_track, _) = sample_track(MediaKind::Audio, 32);
            let (sender, output_track, _) = sample_track(MediaKind::Audio, 32);
            for (idx, (pt, data)) in queued_packets.into_iter().enumerate() {
                peer_sender
                    .try_send(raw_audio_sample(pt, idx as u16, idx as u32 * 160, clock, data))
                    .unwrap();
            }
            let pipeline = EgressPipeline::start_with_gate(
                sender,
                dst_codec(CodecType::PCMA, false),
                EgressSource::Silence,
                Some(20),
                None,
            );
            let dec = create_decoder(src_codec);
            pipeline
                .set_source(EgressSource::TranscodePeer {
                    peer: peer_track,
                    decoder: dec,
                    source_codec: src_codec,
                    source_audio_payload_type: src_pt,
                    source_dtmf_payload_types: vec![101],
                    primed: false,
                })
                .await
                .unwrap();

            let MediaSample::Audio(frame) =
                tokio::time::timeout(Duration::from_millis(200), output_track.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{label}: egress went dark instead of pacing DTX silence"))
                    .unwrap()
            else {
                panic!("expected audio frame");
            };
            let pcma_silence: Bytes = create_encoder(CodecType::PCMA).encode(&vec![0i16; 160]).into();
            assert_eq!(frame.payload_type, Some(8), "{label}: must emit PCMA");
            assert_eq!(frame.data, pcma_silence, "{label}: must emit clean silence");
            pipeline.stop();
        }
    }

    /// Resampled transcode (G.729 8 kHz → Opus 48 kHz) must keep exact 960-sample
    /// framing and flush the FIR resampler history across DTX boundaries.
    #[tokio::test(start_paused = true)]
    async fn g729_to_opus_resampled_dtx_keeps_alignment() {
        let (in_sender, in_track, _) = sample_track(MediaKind::Audio, 32);
        let mut task = make_task(
            in_track,
            CodecType::G729,
            18,
            vec![101],
            dst_codec(CodecType::Opus, false),
        );
        let mut enc = create_encoder(CodecType::G729);
        let speech = enc.encode(&sine_pcm(8000, 160, 440.0));
        let mut mixed_12 = speech[..10].to_vec();
        mixed_12.extend_from_slice(&[0x12, 0x34]);

        in_sender.try_send(raw_audio_sample(18, 1, 160, 8000, speech.clone())).unwrap();
        in_sender.try_send(raw_audio_sample(18, 2, 320, 8000, mixed_12)).unwrap();

        // Drain 20 ms speech + 10 ms speech (padded to 20 ms) → 2 Opus frames, 0 pending.
        let f1 = task.next_frame().await.expect("first 20 ms Opus frame");
        let f2 = task.next_frame().await.expect("flushed 10 ms + silence Opus frame");
        assert_eq!(f1.payload_type, Some(111));
        assert_eq!(f2.payload_type, Some(111));
        assert_eq!(task.transcode_pending.len(), 0);
        assert!(task.transcode_in_dtx);
        assert!(!task.transcode_armed);
        // Resampler history was reset on DTX: resampling silence next emits all zeros.
        let flushed = task.resampler.as_mut().unwrap().resample(&[0i16; 80]);
        assert!(flushed.iter().all(|&s| s == 0), "FIR resampler history must be reset on DTX");
    }
}
