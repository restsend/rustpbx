//! Per-leg egress pipeline — produces target-encoded audio frames at the
//! negotiated ptime cadence and pushes them into the PeerConnection's sender.
//!
//! The [`EgressSource`] enum unifies ALL egress behaviours as mutually
//! exclusive modes:
//!
//! - [`EgressSource::RewriteRelay`] — fast-path: transport-level zero-copy
//!   relay. The pacing loop is parked (no frames produced) and the ICE send
//!   channel is exclusively owned by rustrtc's rewrite bridge. `track.recv()`
//!   on the receiver track will NOT work (rustrtc skips the mpsc listener
//!   dispatch when a rewrite bridge is active).
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
use std::time::{Duration, Instant};

use anyhow::Result;
use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_encoder};

use bytes::Bytes;
use parking_lot::Mutex;
use rustrtc::media::MediaStreamTrack;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::SampleStreamSource;
use rustrtc::{PeerConnection, RtpRewriteBridgeOptions, RtpRewriteRule};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::audio_source::{AudioSource, ResamplingAudioSource};

/// Default ptime (packetization interval) in milliseconds.
const DEFAULT_PTIME_MS: u32 = 20;

/// Callback fired when a [`EgressSource::Media`] source stops producing audio:
/// natural EOF (`false`) or interrupted by switching away from Media / stop
/// (`true`). Use to signal playback completion to the app/IVR layer.
pub type EgressEndCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// What a leg sends to its remote peer. Variants are mutually exclusive —
/// only one is active at a time.
pub enum EgressSource {
    /// **Fast-path**: transport-level zero-copy relay. The pacing loop is
    /// parked (tick skipped) and the ICE send channel is exclusively owned by
    /// the rewrite bridge. The peer sets up the bridge on its own PC.
    ///
    /// The relay is payload-type-aware: `rules` may include audio (catch-all),
    /// DTMF and video rules, each rewriting to its own destination SSRC / PT.
    ///
    /// `on_arm_failed` fires (once) if the rewrite bridge cannot be armed —
    /// e.g. a WebRTC leg's DTLS/SRTP transport never becomes ready. The owner
    /// (MediaBridge) uses it to fall back to transcoding so the call keeps
    /// media instead of silently going silent.
    RewriteRelay {
        peer_pc: PeerConnection,
        options: RtpRewriteBridgeOptions,
        rules: Vec<RtpRewriteRule>,
        on_arm_failed: Option<Arc<dyn Fn() + Send + Sync>>,
    },
    /// Emit silence frames (mute / hold placeholder / idle).
    Silence,
    /// Play an [`AudioSource`] (wav/mp3/http), decoding to PCM then encoding
    /// to the target codec. When `loop_playback` is false and the source
    /// reaches EOF the pipeline switches to [`EgressSource::Silence`] and
    /// fires `on_end(false)`. When the source is replaced (switch to another
    /// source or stop) while a Media source is active, `on_end(true)` is fired.
    Media {
        audio: Box<dyn AudioSource>,
        loop_playback: bool,
        on_end: Option<EgressEndCallback>,
    },
    /// External app pushes pre-encoded [`MediaSample`] frames. When the
    /// channel is empty the pipeline emits silence to keep cadence.
    Inject {
        rx: Mutex<mpsc::Receiver<MediaSample>>,
    },
    /// **Transcoding**: pull a frame from the peer's receiver track, decode it
    /// to PCM, auto-resample to this leg's codec sample rate, re-encode, and
    /// push to the sender. `track.recv()` works (no rewrite bridge).
    TranscodePeer {
        peer: Arc<dyn MediaStreamTrack>,
        decoder: Box<dyn Decoder>,
        /// The source leg's negotiated voice payload type. Other RTP packets
        /// on this audio track are treated as telephone-event packets.
        source_audio_payload_type: u8,
        /// Source codec sample rate (for auto-resampling to `EgressCodec`).
        src_sample_rate: u32,
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
    cmd_tx: mpsc::Sender<EgressCmd>,
    cancel: CancellationToken,
}

enum EgressCmd {
    SetSource(EgressSource),
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

        let playback_timestamp_base = rand::random::<u32>();
        let task = EgressTask {
            sender,
            codec,
            encoder: create_encoder(codec.codec),
            source: initial,
            resampler: None,
            ptime,
            gate,
            playback_timestamp_base,
            playback_started_at: Instant::now(),
            sequence_number: 0,
            marker_pending: false,
            dtmf_event_timestamp: None,
            pcm_buf: vec![0i16; pcm_samples_per_frame(codec.codec, ptime)],
            noise_state: 0x9E37_79B9,
            noise_amplitude,
            noise_lp: 0.0,
        };
        tokio::spawn(task.run(cmd_rx, ptime, cancel.clone()));

        Self { cmd_tx, cancel }
    }

    /// Switch the active source (applied between ticks; never drops a frame
    /// mid-encode). Returns Err if the pipeline has stopped.
    pub async fn set_source(&self, source: EgressSource) -> Result<()> {
        self.cmd_tx
            .send(EgressCmd::SetSource(source))
            .await
            .map_err(|_| anyhow::anyhow!("egress pipeline stopped"))
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

/// PCM samples produced per ptime frame for the codec's sample rate.
fn pcm_samples_per_frame(codec: CodecType, ptime: Duration) -> usize {
    let rate = codec.samplerate();
    (rate as u64 * ptime.as_millis() as u64 / 1000) as usize
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
    sender: SampleStreamSource,
    codec: EgressCodec,
    encoder: Box<dyn Encoder>,
    source: EgressSource,
    resampler: Option<Resampler>,
    ptime: Duration,
    /// While held (true) the pipeline parks. Opened by the relay when both
    /// legs accept.
    gate: Option<Arc<AtomicBool>>,
    playback_timestamp_base: u32,
    playback_started_at: Instant,
    sequence_number: u16,
    marker_pending: bool,
    /// Source DTMF event timestamp and its timestamp on the output timeline.
    /// Every packet belonging to one event keeps the same RTP timestamp.
    dtmf_event_timestamp: Option<(u32, u32)>,
    pcm_buf: Vec<i16>,
    /// LCG state for comfort-noise generation (continuous across frames so the
    /// noise does not repeat per-frame).
    noise_state: u32,
    /// Comfort-noise amplitude in 16-bit PCM units (from `codec` level dBFS).
    noise_amplitude: f32,
    /// One-pole lowpass state for a softer, less "harsh static" comfort tone.
    noise_lp: f32,
}

impl EgressTask {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<EgressCmd>,
        ptime: Duration,
        cancel: CancellationToken,
    ) {
        let mut interval = tokio::time::interval(ptime);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
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
            let tick: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
                if is_relay || gated {
                    Box::pin(std::future::pending())
                } else {
                    Box::pin(async {
                        interval.tick().await;
                    })
                };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(EgressCmd::SetSource(s)) => {
                        let was_relay = matches!(&self.source, EgressSource::RewriteRelay { .. });
                        let will_relay = matches!(&s, EgressSource::RewriteRelay { .. });
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
                        // Resample Media sources to the codec's sample rate
                        // before encoding (e.g. 24 kHz MP3 → 48 kHz opus).
                        let s = match s {
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
                        if let EgressSource::TranscodePeer { src_sample_rate, .. } = &s {
                            let dst_sample_rate = self.codec.codec.samplerate();
                            if *src_sample_rate != dst_sample_rate {
                                self.resampler = Some(Resampler::new(
                                    *src_sample_rate as usize,
                                    dst_sample_rate as usize,
                                ));
                            } else {
                                self.resampler = None;
                            }
                        } else {
                            self.resampler = None;
                        }
                        if was_relay && !will_relay {
                            self.marker_pending = true;
                        }
                        self.dtmf_event_timestamp = None;
                        self.source = s;
                    }
                    Some(EgressCmd::UpdateCodec(new_codec)) => {
                        self.codec = new_codec;
                        self.encoder = create_encoder(new_codec.codec);
                        // Rebuild the PCM staging buffer for the new sample rate.
                        self.pcm_buf = vec![0i16; pcm_samples_per_frame(new_codec.codec, self.ptime)];
                        // Drop any resampler: TranscodePeer re-derives it on next SetSource.
                        self.resampler = None;
                        self.marker_pending = true;
                        self.dtmf_event_timestamp = None;
                    }
                    None => break,
                },
                _ = tick => {
                    if let Some(frame) = self.next_frame().await {
                        let clear_marker = self.marker_pending;
                        if self.sender.try_send(MediaSample::Audio(frame)).is_err() {
                            trace!("egress: sender full, dropping frame to keep cadence");
                        } else if clear_marker {
                            self.marker_pending = false;
                        }
                    }
                }
            }
        }
    }

    /// Produce exactly one frame for this tick. Sources that have no data fall
    /// back to silence so the outgoing stream never gaps.
    async fn next_frame(&mut self) -> Option<AudioFrame> {
        // Move the source out so other `self` fields (encoder, pcm_buf) are
        // freely accessible inside the match (avoids &mut-self / &mut-source
        // aliasing). Put it back at the end.
        let mut source = std::mem::replace(&mut self.source, EgressSource::Silence);

        // Promote Media→Silence on terminal EOF; fire on_end callback.
        if let EgressSource::Media {
            audio,
            loop_playback,
            on_end,
            ..
        } = &mut source
        {
            if !audio.has_data() && !*loop_playback {
                if let Some(cb) = on_end.take() {
                    cb(false);
                }
                source = EgressSource::Silence;
            }
        }

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
                    if *loop_playback {
                        let _ = audio.reset();
                        let n2 = audio.read_samples(&mut self.pcm_buf);
                        if n2 == 0 {
                            self.encode_silence().into()
                        } else {
                            self.encoder.encode(&self.pcm_buf[..n2]).into()
                        }
                    } else {
                        if let Some(cb) = on_end.take() {
                            cb(false);
                        }
                        source = EgressSource::Silence;
                        self.encode_silence().into()
                    }
                } else {
                    self.encoder.encode(&self.pcm_buf[..n]).into()
                };
                self.build_frame(encoded)
            }
            EgressSource::Inject { rx } => {
                let passed = rx.lock().try_recv().ok().and_then(|sample| match sample {
                    MediaSample::Audio(frame) => Some(frame),
                    MediaSample::Video(_) => None,
                });
                match passed {
                    Some(mut frame) => {
                        frame.marker |= self.marker_pending;
                        frame
                    }
                    None => {
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    }
                }
            }
            EgressSource::TranscodePeer {
                peer,
                decoder,
                source_audio_payload_type,
                ..
            } => {
                // Wait up to one ptime for the next frame from the peer's
                // receiver track. On timeout/error emit silence to keep cadence
                // (the remote decoder's PLC handles the gap).
                match tokio::time::timeout(self.ptime, peer.recv()).await {
                    Ok(Ok(MediaSample::Audio(input)))
                        if input
                            .payload_type
                            .is_none_or(|pt| pt == *source_audio_payload_type) =>
                    {
                        let mut pcm = decoder.decode(&input.data);
                        if let Some(rs) = &mut self.resampler {
                            pcm = rs.resample(&pcm);
                        }
                        let encoded: Bytes = self.encoder.encode(&pcm).into();
                        self.build_frame(encoded)
                    }
                    Ok(Ok(MediaSample::Audio(input))) => {
                        tracing::trace!(
                            pt = input.payload_type,
                            expected_pt = *source_audio_payload_type,
                            "transcode: non-audio PT frame (telephone-event?)"
                        );
                        match self.build_dtmf_frame(&input) {
                            Some(frame) => frame,
                            None => {
                                let encoded = self.encode_silence();
                                self.build_frame(encoded)
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(error = %e, "transcode: peer.recv() error, emitting silence");
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    }
                    Err(_) => {
                        tracing::trace!(
                            "transcode: peer.recv() timeout (no audio yet), emitting silence"
                        );
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    }
                    _ => {
                        let encoded = self.encode_silence();
                        self.build_frame(encoded)
                    }
                }
            }
        };

        self.source = source;
        self.advance_sequence();
        Some(frame)
    }

    fn build_dtmf_frame(&mut self, source: &AudioFrame) -> Option<AudioFrame> {
        let payload_type = self.codec.dtmf_payload_type?;
        let payload = crate::telephone_event::map_telephone_event_duration(
            &source.data,
            source.clock_rate,
            self.codec.clock_rate,
        )?;

        let rtp_timestamp = match self.dtmf_event_timestamp {
            Some((source_timestamp, output_timestamp))
                if source_timestamp == source.rtp_timestamp =>
            {
                output_timestamp
            }
            _ => {
                let output_timestamp = self.playback_timestamp();
                self.dtmf_event_timestamp = Some((source.rtp_timestamp, output_timestamp));
                output_timestamp
            }
        };

        Some(AudioFrame {
            rtp_timestamp,
            clock_rate: self.codec.clock_rate,
            data: Bytes::from(payload),
            sequence_number: Some(self.sequence_number),
            payload_type: Some(payload_type),
            marker: source.marker,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        })
    }

    /// Zero-fill the PCM buffer (silence).
    fn encode_silence(&mut self) -> Bytes {
        if self.codec.comfort_noise && self.noise_amplitude > 0.0 {
            for s in self.pcm_buf.iter_mut() {
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
            for s in self.pcm_buf.iter_mut() {
                *s = 0;
            }
        }
        self.encoder.encode(&self.pcm_buf).into()
    }

    /// Build the outbound `AudioFrame` from an already-encoded payload.
    fn build_frame(&self, data: Bytes) -> AudioFrame {
        AudioFrame {
            rtp_timestamp: self.playback_timestamp(),
            clock_rate: self.codec.clock_rate,
            data,
            sequence_number: Some(self.sequence_number),
            payload_type: Some(self.codec.payload_type),
            marker: self.marker_pending,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        }
    }

    fn advance_sequence(&mut self) {
        self.sequence_number = self.sequence_number.wrapping_add(1);
    }

    fn playback_timestamp(&self) -> u32 {
        let elapsed_frames = self.playback_started_at.elapsed().as_nanos() / self.ptime.as_nanos();
        self.playback_timestamp_base
            .wrapping_add((elapsed_frames as u32).wrapping_mul(self.codec_rtp_ticks_per_frame()))
    }

    fn codec_rtp_ticks_per_frame(&self) -> u32 {
        ((self.codec.clock_rate as u128 * self.ptime.as_nanos()) / 1_000_000_000) as u32
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

        // Let a few ticks elapse, then stop and inspect via the shared sender
        // drop_count isn't exposed; instead we just assert the task runs and
        // stops cleanly. The frame content is verified in the unit test below.
        tokio::time::sleep(Duration::from_millis(70)).await;
        pipe.stop();
        // No panic = the pacing task produced frames without error.
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
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Silence,
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            playback_timestamp_base: 0,
            playback_started_at: Instant::now(),
            sequence_number: 0,
            marker_pending: false,
            dtmf_event_timestamp: None,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 10f32.powf(-30.0 / 20.0) * i16::MAX as f32,
            noise_lp: 0.0,
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
    async fn playback_after_relay_marks_once_and_reuses_local_timeline() {
        let (sender, track, _fb) = sample_track(MediaKind::Audio, 64);
        let peer_pc = PeerConnection::new(rustrtc::RtcConfiguration::default());
        let pipe = EgressPipeline::start_with_gate(
            sender,
            pcmu_codec(),
            EgressSource::RewriteRelay {
                peer_pc: peer_pc.clone(),
                options: RtpRewriteBridgeOptions::default(),
                rules: Vec::new(),
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
            rules: Vec::new(),
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
        assert!(
            third.rtp_timestamp.wrapping_sub(second.rtp_timestamp) >= 480,
            "playback timestamp must include the relay gap"
        );
        assert_eq!(
            third.sequence_number,
            second.sequence_number.map(|seq| seq.wrapping_add(1)),
            "local playback sequence must continue"
        );

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
            sender: _sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Silence,
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            playback_timestamp_base: 0,
            playback_started_at: Instant::now(),
            sequence_number: 0,
            marker_pending: false,
            dtmf_event_timestamp: None,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
        };
        let f = task.next_frame().await.expect("silence yields a frame");
        assert_eq!(f.clock_rate, 8000);
        assert_eq!(f.payload_type, Some(0));
        assert!(
            !f.data.is_empty(),
            "PCMU silence must encode to non-empty bytes"
        );
        assert_eq!(task.sequence_number, 1);
    }

    #[tokio::test]
    async fn transcode_peer_returns_audio_or_dtmf_with_one_sequence_owner() {
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
            sequence_number: None,
            payload_type: Some(101),
            marker: !end,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
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
                data: Bytes::from(opus),
                sequence_number: None,
                payload_type: Some(111),
                marker: false,
                header_extension: None,
                source_addr: None,
                raw_packet: None,
            }))
            .unwrap();

        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));
        let mut task = EgressTask {
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::TranscodePeer {
                peer: peer_track,
                decoder: audio_codec::create_decoder(CodecType::Opus),
                source_audio_payload_type: 111,
                src_sample_rate: 48_000,
            },
            resampler: Some(Resampler::new(48_000, 8000)),
            ptime: Duration::from_millis(20),
            gate: None,
            playback_timestamp_base: 0,
            playback_started_at: Instant::now(),
            sequence_number: 0,
            marker_pending: false,
            dtmf_event_timestamp: None,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
        };

        let first = task.next_frame().await.expect("first DTMF frame");
        let second = task.next_frame().await.expect("second DTMF frame");
        let audio = task.next_frame().await.expect("transcoded audio frame");

        assert_eq!(first.payload_type, Some(101));
        assert_eq!(first.sequence_number, Some(0));
        assert_eq!(u16::from_be_bytes([first.data[2], first.data[3]]), 160);
        assert_eq!(second.payload_type, Some(101));
        assert_eq!(second.sequence_number, Some(1));
        assert_eq!(second.rtp_timestamp, first.rtp_timestamp);
        assert_eq!(u16::from_be_bytes([second.data[2], second.data[3]]), 800);
        assert_eq!(audio.payload_type, Some(0));
        assert_eq!(audio.sequence_number, Some(2));
        assert_eq!(task.sequence_number, 3);
        assert!(
            tokio::time::timeout(Duration::from_millis(1), output_track.recv())
                .await
                .is_err(),
            "next_frame must return packets without sending them directly"
        );
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
            playback_timestamp_base: 0,
            playback_started_at: Instant::now(),
            sequence_number: 0,
            marker_pending: false,
            dtmf_event_timestamp: None,
            pcm_buf: vec![0i16; spf],
            noise_state: 0x9E37_79B9,
            noise_amplitude: 0.0,
            noise_lp: 0.0,
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
}
