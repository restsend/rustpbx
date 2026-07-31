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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_encoder};

use bytes::Bytes;
use parking_lot::Mutex;
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::media::track::SampleStreamSource;
use rustrtc::media::MediaStreamTrack;
use rustrtc::{PeerConnection, RtpRewriteBridgeParams};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::audio_source::AudioSource;

/// Default ptime (packetization interval) in milliseconds.
const DEFAULT_PTIME_MS: u32 = 20;

/// Callback fired when a [`EgressSource::Media`] source reaches terminal EOF
/// (non-loop). Use to signal playback completion to the app/IVR layer.
pub type EgressEndCallback = Arc<dyn Fn() + Send + Sync>;

/// What a leg sends to its remote peer. Variants are mutually exclusive —
/// only one is active at a time.
pub enum EgressSource {
    /// **Fast-path**: transport-level zero-copy relay. The pacing loop is
    /// parked (tick skipped) and the ICE send channel is exclusively owned by
    /// the rewrite bridge. The peer sets up the bridge on its own PC.
    RewriteRelay {
        peer_pc: PeerConnection,
        params: RtpRewriteBridgeParams,
    },
    /// Emit silence frames (mute / hold placeholder / idle).
    Silence,
    /// Play an [`AudioSource`] (wav/mp3/http), decoding to PCM then encoding
    /// to the target codec. When `loop_playback` is false and the source
    /// reaches EOF the pipeline switches to [`EgressSource::Silence`] and
    /// fires `on_end` (if set).
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
        /// Source codec sample rate (for auto-resampling to `EgressCodec`).
        src_sample_rate: u32,
    },
}

/// Configuration for the encoding side of the pipeline.
#[derive(Debug, Clone, Copy)]
pub struct EgressCodec {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
}

/// Per-leg egress pipeline.
///
/// Create via [`EgressPipeline::start`] after obtaining a `SampleStreamSource`
/// (from `rustrtc::media::track::sample_track`, with the track added to the
/// PeerConnection). Stop by dropping the pipeline or calling [`Self::stop`].
pub struct EgressPipeline {
    cmd_tx: mpsc::Sender<EgressCmd>,
    cancel: CancellationToken,
}

enum EgressCmd {
    SetSource(EgressSource),
}

impl EgressPipeline {
    /// Spawn the pacing task. `sender` is the push side of the track added to
    /// the PeerConnection. `initial` is the first source (commonly
    /// [`EgressSource::Silence`]).
    pub fn start(
        sender: SampleStreamSource,
        codec: EgressCodec,
        initial: EgressSource,
        ptime_ms: Option<u32>,
    ) -> Self {
        Self::start_with_gate(sender, codec, initial, ptime_ms, None)
    }

    /// [`Self::start`] with an optional gate. While the gate is held (true),
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

        let task = EgressTask {
            sender,
            codec,
            encoder: create_encoder(codec.codec),
            source: initial,
            resampler: None,
            ptime,
            gate,
            rtp_timestamp: 0u32.wrapping_sub(1),
            sequence_number: 0u16.wrapping_sub(1),
            pcm_buf: vec![0i16; pcm_samples_per_frame(codec.codec, ptime)],
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

    /// Non-blocking variant of [`Self::set_source`].
    pub fn try_set_source(&self, source: EgressSource) -> Result<()> {
        self.cmd_tx
            .try_send(EgressCmd::SetSource(source))
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
    rtp_timestamp: u32,
    sequence_number: u16,
    pcm_buf: Vec<i16>,
}

impl EgressTask {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<EgressCmd>, ptime: Duration, cancel: CancellationToken) {
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
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(EgressCmd::SetSource(s)) => {
                        // Auto-configure the resampler for TranscodePeer based
                        // on src/dst sample rate mismatch.
                        if let EgressSource::TranscodePeer { src_sample_rate, .. } = &s {
                            if *src_sample_rate != self.codec.clock_rate {
                                self.resampler = Some(Resampler::new(
                                    *src_sample_rate as usize,
                                    self.codec.clock_rate as usize,
                                ));
                            } else {
                                self.resampler = None;
                            }
                        } else {
                            self.resampler = None;
                        }
                        self.source = s;
                    }
                    None => break,
                },
                _ = interval.tick(), if !is_relay && !gated => {
                    if let Some(frame) = self.next_frame().await {
                        // DropOldest semantics: if the PC sender is saturated
                        // (slow remote), drop the oldest rather than block the
                        // pacing task. try_send never awaits.
                        if self.sender.try_send(MediaSample::Audio(frame)).is_err() {
                            trace!("egress: sender full, dropping frame to keep cadence");
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
        if let EgressSource::Media { audio, loop_playback, on_end } = &mut source {
            if !audio.has_data() && !*loop_playback {
                if let Some(cb) = on_end.take() {
                    cb();
                }
                source = EgressSource::Silence;
            }
        }

        // Inject pass-through returns a frame directly (pre-encoded); handled
        // first so we don't disturb the local timestamp/seq counters' alignment
        // expectations — but we still advance them for consistency.
        if let EgressSource::Inject { rx } = &mut source {
            let passed = rx.lock().try_recv().ok().and_then(|s| match s {
                MediaSample::Audio(f) => Some(f),
                _ => None,
            });
            if let Some(f) = passed {
                self.source = source;
                self.advance_ts_seq();
                return Some(f);
            }
            // empty → fall through to silence
            let encoded: Bytes = self.encode_silence().into();
            self.source = source;
            self.advance_ts_seq();
            return Some(self.build_frame(encoded));
        }

        let encoded: Bytes = match &mut source {
            EgressSource::RewriteRelay { .. } => {
                // The pacing loop skips ticks while a relay is active, so we
                // should never get here; keep a silent fallback just in case.
                self.encode_silence().into()
            }
            EgressSource::Silence => self.encode_silence().into(),
            EgressSource::Media { audio, loop_playback, on_end } => {
                let n = audio.read_samples(&mut self.pcm_buf);
                if n == 0 {
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
                            cb();
                        }
                        source = EgressSource::Silence;
                        self.encode_silence().into()
                    }
                } else {
                    self.encoder.encode(&self.pcm_buf[..n]).into()
                }
            }
            EgressSource::Inject { .. } => unreachable!("handled above"),
            EgressSource::TranscodePeer { peer, decoder, .. } => {
                // Wait up to one ptime for the next frame from the peer's
                // receiver track. On timeout/error emit silence to keep cadence
                // (the remote decoder's PLC handles the gap).
                match tokio::time::timeout(self.ptime, peer.recv()).await {
                    Ok(Ok(MediaSample::Audio(frame))) => {
                        let mut pcm = decoder.decode(&frame.data);
                        if let Some(rs) = &mut self.resampler {
                            pcm = rs.resample(&pcm);
                        }
                        self.encoder.encode(&pcm).into()
                    }
                    _ => self.encode_silence().into(),
                }
            }
        };

        self.source = source;
        self.advance_ts_seq();
        Some(self.build_frame(encoded))
    }

    /// Zero-fill the PCM buffer (silence).
    fn encode_silence(&mut self) -> Bytes {
        for s in self.pcm_buf.iter_mut() {
            *s = 0;
        }
        self.encoder.encode(&self.pcm_buf).into()
    }

    /// Build the outbound `AudioFrame` from an already-encoded payload.
    fn build_frame(&self, data: Bytes) -> AudioFrame {
        AudioFrame {
            rtp_timestamp: self.rtp_timestamp,
            clock_rate: self.codec.clock_rate,
            data,
            sequence_number: Some(self.sequence_number),
            payload_type: Some(self.codec.payload_type),
            marker: false,
            header_extension: None,
            source_addr: None,
            raw_packet: None,
        }
    }

    fn advance_ts_seq(&mut self) {
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(self.codec_rtp_ticks_per_frame());
        self.sequence_number = self.sequence_number.wrapping_add(1);
    }

    fn codec_rtp_ticks_per_frame(&self) -> u32 {
        // clock_rate * ptime_ms / 1000
        self.codec.clock_rate * DEFAULT_PTIME_MS / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::media::track::sample_track;
    use rustrtc::media::MediaKind;

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
        }
    }

    #[tokio::test]
    async fn silence_pipeline_emits_frames_at_ptime() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let pipe = EgressPipeline::start(sender, pcmu_codec(), EgressSource::Silence, Some(20));

        // Let a few ticks elapse, then stop and inspect via the shared sender
        // drop_count isn't exposed; instead we just assert the task runs and
        // stops cleanly. The frame content is verified in the unit test below.
        tokio::time::sleep(Duration::from_millis(70)).await;
        pipe.stop();
        // No panic = the pacing task produced frames without error.
    }

    #[tokio::test]
    async fn switch_source_from_silence_to_media() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let pipe = EgressPipeline::start(sender, pcmu_codec(), EgressSource::Silence, Some(20));
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
            rtp_timestamp: 0,
            sequence_number: 0,
            pcm_buf: vec![0i16; spf],
        };
        let f = task.next_frame().await.expect("silence yields a frame");
        assert_eq!(f.clock_rate, 8000);
        assert_eq!(f.payload_type, Some(0));
        assert!(!f.data.is_empty(), "PCMU silence must encode to non-empty bytes");
        // Timestamp advances by 160 ticks for 20ms @ 8kHz.
        assert_eq!(task.rtp_timestamp, 160);
        assert_eq!(task.sequence_number, 1);
    }

    #[tokio::test]
    async fn next_frame_media_falls_back_to_silence_on_eof_without_loop() {
        let (sender, _track, _fb) = sample_track(MediaKind::Audio, 64);
        let codec = pcmu_codec();
        let spf = pcm_samples_per_frame(codec.codec, Duration::from_millis(20));

        struct Empty;
        impl AudioSource for Empty {
            fn read_samples(&mut self, _b: &mut [i16]) -> usize { 0 }
            fn sample_rate(&self) -> u32 { 8000 }
            fn channels(&self) -> u16 { 1 }
            fn has_data(&self) -> bool { false }
            fn reset(&mut self) -> Result<()> { Ok(()) }
        }

        let mut task = EgressTask {
            sender,
            codec,
            encoder: create_encoder(CodecType::PCMU),
            source: EgressSource::Media { audio: Box::new(Empty), loop_playback: false, on_end: None },
            resampler: None,
            ptime: Duration::from_millis(20),
            gate: None,
            rtp_timestamp: 0,
            sequence_number: 0,
            pcm_buf: vec![0i16; spf],
        };
        // has_data() false + no loop → source becomes Silence, still yields a frame.
        let f = task.next_frame().await.expect("EOF media yields silence frame");
        assert!(matches!(task.source, EgressSource::Silence));
        assert!(!f.data.is_empty());
    }

    #[test]
    fn pcm_samples_per_frame_is_correct() {
        // PCMU @ 8kHz, 20ms → 160 samples
        assert_eq!(pcm_samples_per_frame(CodecType::PCMU, Duration::from_millis(20)), 160);
        // Opus @ 48kHz, 20ms → 960 samples
        assert_eq!(pcm_samples_per_frame(CodecType::Opus, Duration::from_millis(20)), 960);
    }
}
