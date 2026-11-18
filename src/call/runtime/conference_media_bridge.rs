//! Conference Media Bridge
//!
//! Bridges conference mixed audio output (output_rx) to a SipSession's media track.
//! This enables participants in a conference to hear the mixed audio from all other participants.
//!
//! Now supports full-duplex bridging:
//! - Forward loop: conference mixer output → SIP track (mixed audio to participant)
//! - Reverse loop: SIP track → conference mixer input (participant audio to mixer)

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, trace, warn};

use crate::call::domain::LegId;
use crate::call::runtime::ConferenceManager;
use crate::media::conference_mixer::AudioFrame;

/// Trait for sending media samples to a track.
pub trait AudioSender: Send + Sync {
    fn send(
        &self,
        sample: rustrtc::media::MediaSample,
    ) -> impl std::future::Future<
        Output = Result<(), mpsc::error::SendError<rustrtc::media::MediaSample>>,
    > + Send;
}

impl AudioSender for tokio::sync::mpsc::Sender<rustrtc::media::MediaSample> {
    async fn send(
        &self,
        sample: rustrtc::media::MediaSample,
    ) -> Result<(), mpsc::error::SendError<rustrtc::media::MediaSample>> {
        self.send(sample).await
    }
}

/// Trait for receiving decoded PCM audio from a track.
pub trait AudioReceiver: Send + Sync {
    /// Receive the next PCM audio frame.
    /// Returns None when the receiver is closed.
    fn recv(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<PcmAudioFrame>> + Send + '_>>;
}

/// Decoded PCM audio frame from a participant. Alias of [`AudioFrame`].
pub type PcmAudioFrame = AudioFrame;

/// Bridges conference audio to a media track.
pub struct ConferenceMediaBridge {
    conference_manager: Arc<ConferenceManager>,
}

impl ConferenceMediaBridge {
    pub fn new(conference_manager: Arc<ConferenceManager>) -> Self {
        Self { conference_manager }
    }

    /// Start full-duplex bridge for a leg.
    ///
    /// This creates both forward and reverse loops:
    /// - Forward: conference mixer output → SIP track (mixed audio to participant)
    /// - Reverse: SIP track → conference mixer input (participant audio to mixer)
    pub async fn start_bridge_full_duplex<S>(
        &self,
        conf_id: &str,
        leg_id: &LegId,
        audio_sender: S,
        audio_receiver: Box<dyn AudioReceiver>,
        codec: audio_codec::CodecType,
        hold_gate: Option<HoldGateHandle>,
    ) -> anyhow::Result<ConferenceBridgeHandle>
    where
        S: AudioSender + Send + Sync + 'static,
    {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);

        // Add participant to conference and get channels
        let channels = self
            .conference_manager
            .add_participant(&conf_id_obj, leg_id.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to add participant to conference: {}", e))?;

        let input_tx = channels.input_tx;

        // The room's mix bus rate is the single resampling reference for both
        // directions: ingress decimates/interpolates to it, egress converts
        // back to the codec rate with the same stateful FIR.
        let mixer_sample_rate = self
            .conference_manager
            .conference_sample_rate(&conf_id_obj)
            .unwrap_or(8_000);

        // Take the output_rx for this leg
        let output_rx = self
            .conference_manager
            .take_participant_output_rx(leg_id)
            .await;
        let Some(output_rx) = output_rx else {
            let _ = self
                .conference_manager
                .remove_participant(&conf_id_obj, leg_id)
                .await;
            anyhow::bail!(
                "No output_rx found for leg {} in conference {}",
                leg_id,
                conf_id
            );
        };

        // Hold gate: pre-resample the hold music to the codec rate so the
        // forward loop can substitute frames without re-encoding state.

        info!(
            conf_id = %conf_id,
            leg_id = %leg_id,
            "Starting full-duplex conference media bridge"
        );
        crate::metrics::conference::created();

        let cancel_token = tokio_util::sync::CancellationToken::new();

        // Spawn forward loop: conference → SIP
        let forward_cancel = cancel_token.child_token();
        let leg_id_forward = leg_id.clone();
        let conf_id_forward = conf_id.to_string();
        let hold_gate_for_task = hold_gate.clone();
        let forward_handle = crate::utils::spawn(async move {
            Self::forward_loop(
                output_rx,
                audio_sender,
                leg_id_forward,
                conf_id_forward,
                forward_cancel,
                codec,
                hold_gate_for_task,
            )
            .await;
        });

        // Spawn reverse loop: SIP → conference
        let reverse_cancel = cancel_token.child_token();
        let leg_id_reverse = leg_id.clone();
        let conf_id_reverse = conf_id.to_string();
        let reverse_handle = crate::utils::spawn(async move {
            Self::reverse_loop(
                audio_receiver,
                input_tx,
                leg_id_reverse,
                conf_id_reverse,
                reverse_cancel,
                mixer_sample_rate,
            )
            .await;
        });

        Ok(ConferenceBridgeHandle {
            _tasks: vec![forward_handle, reverse_handle],
            cancel_token,
            hold: hold_gate,
        })
    }

    /// Forward loop: read mixed audio from conference, encode to RTP, and send to media track.
    pub async fn forward_loop<S>(
        mut output_rx: mpsc::Receiver<AudioFrame>,
        audio_sender: S,
        leg_id: LegId,
        conf_id: String,
        cancel_token: tokio_util::sync::CancellationToken,
        codec: audio_codec::CodecType,
        hold_gate: Option<HoldGateHandle>,
    ) where
        S: AudioSender + Send + Sync + 'static,
    {
        use audio_codec::create_encoder;
        use rustrtc::media::{AudioFrame as RtcAudioFrame, MediaSample};

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            codec = ?codec,
            "Conference media bridge forward loop started"
        );

        let mut encoder = create_encoder(codec);
        let payload_type = codec.payload_type();
        let clock_rate = codec.clock_rate() as u32;
        let sample_rate = encoder.sample_rate();
        let mut rtp_timestamp: u32 = rand::random();
        let mut sequence_number: u16 = rand::random();
        let interval_ms = 20u64;
        let samples_per_frame = (sample_rate * interval_ms as u32 / 1000) as usize;
        let rtp_ticks_per_frame = clock_rate * interval_ms as u32 / 1000;

        // Reusable buffers — this loop runs every 20ms per participant, so
        // per-frame `Vec` allocations are worth avoiding.
        let mut pcm_buf: Vec<i16> = Vec::with_capacity(samples_per_frame * 4);
        let mut frame_buf: Vec<i16> = vec![0i16; samples_per_frame];
        // One stateful FIR for the lifetime of this leg: rebuilding it per
        // frame would discard the anti-alias history and re-introduce the
        // aliasing (and a phase discontinuity at every frame boundary).
        let mut resampler: Option<(u32, u32, audio_codec::BoxedResampler)> = None;

        // Hold-gate pacer: while held, the hold music is emitted on this
        // self-paced 20ms ticker — INDEPENDENT of the mixer output cadence.
        // The room mixer starves when the remote party holds (their uplink
        // gate closes), and its starvation handling pads/re-arms in ways
        // that would break a mixer-paced hold source (silence → stale-buffer
        // garbage). Self-pacing decouples the hold audio from all of that.
        let mut hold_pacer = tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
        hold_pacer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let is_held = || hold_gate.as_ref().is_some_and(|g| g.is_held());

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference media bridge forward loop cancelled"
                    );
                    break;
                }
                _ = hold_pacer.tick(), if is_held() => {
                    // Emit one hold-music frame on the pacer tick.
                    frame_buf.fill(0);
                    if let Some(gate) = &hold_gate {
                        gate.next_frame_into(&mut frame_buf);
                    }

                    // Drain-and-discard the mixer output that keeps arriving
                    // while held. The room mixer never stops; without this the
                    // channel (capacity 100 frames = 2 s) accumulates stale
                    // audio and the gate release replays the whole hold tail
                    // before reaching live audio (production incident
                    // 2026-11-13: garbled audio after hold/resume on a
                    // transfer-room call). Discarding keeps the release at
                    // live audio immediately.
                    loop {
                        match output_rx.try_recv() {
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }

                    let encoded = encoder.encode(&frame_buf);
                    let rtc_frame = RtcAudioFrame {
                        rtp_timestamp,
                        clock_rate: clock_rate,
                        data: encoded.into(),
                        sequence_number: Some(sequence_number),
                        payload_type: Some(payload_type),
                        marker: false,
                        header_extension: None,
                        raw_packet: None,
                        source_addr: None,
                    };
                    if let Err(e) = audio_sender.send(MediaSample::Audio(rtc_frame)).await {
                        warn!(
                            leg_id = %leg_id,
                            error = %e,
                            "Failed to send hold audio to media track"
                        );
                        return;
                    }
                    rtp_timestamp = rtp_timestamp.wrapping_add(rtp_ticks_per_frame);
                    sequence_number = sequence_number.wrapping_add(1);
                }
                Some(frame) = output_rx.recv(), if !is_held() => {
                    // Resample to the encoder rate with a stateful polyphase
                    // FIR: a plain decimation here folds 4-8 kHz energy back
                    // into the voice band (audible hiss on wideband rooms).
                    if !resample_fir_into(
                        &mut resampler,
                        &frame.samples,
                        frame.sample_rate,
                        sample_rate,
                        &mut pcm_buf,
                    ) {
                        warn!(
                            leg_id = %leg_id,
                            conf_id = %conf_id,
                            src_sample_rate = frame.sample_rate,
                            dst_sample_rate = sample_rate,
                            "Conference forward loop could not resample frame"
                        );
                        continue;
                    }

                    // Process in chunks of samples_per_frame
                    for chunk in pcm_buf.chunks(samples_per_frame) {
                        // Copy into the persistent frame buffer, padding the
                        // tail with silence for a partial final chunk.
                        frame_buf[..chunk.len()].copy_from_slice(chunk);
                        frame_buf[chunk.len()..].fill(0);

                        let encoded = encoder.encode(&frame_buf);
                        let rtc_frame = RtcAudioFrame {
                            rtp_timestamp,
                            clock_rate: clock_rate,
                            data: encoded.into(),
                            sequence_number: Some(sequence_number),
                            payload_type: Some(payload_type),
                            marker: false,
                            header_extension: None,
                            raw_packet: None,
                            source_addr: None,
                        };

                        let bytes_sent = chunk.len() * 2; // i16 = 2 bytes
                        if let Err(e) = audio_sender.send(MediaSample::Audio(rtc_frame)).await {
                            warn!(
                                leg_id = %leg_id,
                                error = %e,
                                "Failed to send conference audio to media track"
                            );
                            return;
                        }
                        crate::metrics::conference::media_injected_bytes(bytes_sent as u64);

                        rtp_timestamp = rtp_timestamp.wrapping_add(rtp_ticks_per_frame);
                        sequence_number = sequence_number.wrapping_add(1);
                    }

                    trace!(
                        leg_id = %leg_id,
                        samples = pcm_buf.len(),
                        original_sample_rate = frame.sample_rate,
                        "Encoded and sent mixed audio frame from conference"
                    );
                }
                else => {
                    warn!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference output_rx closed"
                    );
                    break;
                }
            }
        }

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge forward loop ended"
        );
    }

    /// Reverse loop: read decoded PCM from audio receiver, and send to conference mixer input.
    pub async fn reverse_loop(
        mut audio_receiver: Box<dyn AudioReceiver>,
        input_tx: mpsc::Sender<AudioFrame>,
        leg_id: LegId,
        conf_id: String,
        cancel_token: tokio_util::sync::CancellationToken,
        mixer_sample_rate: u32,
    ) {
        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            mixer_sample_rate,
            "Conference media bridge reverse loop started"
        );

        // Per-leg resampler state (never shared between legs). The mixer rate
        // is fixed for the room; only the ingress codec rate can change.
        let mut resampler: Option<(u32, u32, audio_codec::BoxedResampler)> = None;

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Conference media bridge reverse loop cancelled"
                    );
                    break;
                }
                Some(pcm_frame) = audio_receiver.recv() => {
                    let sample_count = pcm_frame.samples.len();
                    let sample_rate = pcm_frame.sample_rate;

                    // Anti-aliased, stateful resampling into the room's mix
                    // bus rate (G.722 16 kHz / Opus 48 kHz → 16 kHz room).
                    let mut samples = Vec::new();
                    if !resample_fir_into(
                        &mut resampler,
                        &pcm_frame.samples,
                        sample_rate,
                        mixer_sample_rate,
                        &mut samples,
                    ) {
                        warn!(
                            leg_id = %leg_id,
                            conf_id = %conf_id,
                            src_sample_rate = sample_rate,
                            dst_sample_rate = mixer_sample_rate,
                            "Conference reverse loop could not resample frame"
                        );
                        continue;
                    }
                    // Carry the ingress sample position onto the room timeline
                    // (codec PCM rate -> mixer rate). Untimestamped sources
                    // (timestamp 0) keep the mixer's synthetic pacing.
                    let output_timestamp = if pcm_frame.timestamp == 0 {
                        0
                    } else {
                        (pcm_frame.timestamp as u128 * mixer_sample_rate as u128
                            / sample_rate as u128) as u64
                    };
                    let mut audio_frame = AudioFrame::new(samples, mixer_sample_rate);
                    audio_frame.timestamp = output_timestamp;

                    if let Err(e) = input_tx.send(audio_frame).await {
                        warn!(
                            leg_id = %leg_id,
                            conf_id = %conf_id,
                            error = %e,
                            "Failed to send audio to conference mixer input"
                        );
                        break;
                    }

                    trace!(
                        leg_id = %leg_id,
                        samples = sample_count,
                        sample_rate = sample_rate,
                        "Sent participant audio to conference mixer"
                    );
                }
                else => {
                    info!(
                        leg_id = %leg_id,
                        conf_id = %conf_id,
                        "Audio receiver closed, stopping reverse loop"
                    );
                    break;
                }
            }
        }

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            "Conference media bridge reverse loop ended"
        );
    }
}

/// Resample audio into `out` with a stateful polyphase FIR, rebuilding the
/// filter only when the source or destination rate changes.
///
/// This is the conference path's only resampler: linear interpolation (or
/// bare decimation, which is what an integer ratio degrades to) has no
/// anti-alias filter, so 4-8 kHz wideband content folds back into the voice
/// band as continuous hiss. `BoxedResampler` keeps its 24-tap history across
/// calls, so frames must be fed to the same instance in order.
///
/// Returns `false` (leaving `out` empty) for invalid rates.
pub(crate) fn resample_fir_into(
    resampler: &mut Option<(u32, u32, audio_codec::BoxedResampler)>,
    samples: &[i16],
    src_rate: u32,
    dst_rate: u32,
    out: &mut Vec<i16>,
) -> bool {
    out.clear();
    if src_rate == dst_rate {
        out.extend_from_slice(samples);
        return true;
    }
    if src_rate == 0 || dst_rate == 0 {
        return false;
    }

    let needs_rebuild = resampler
        .as_ref()
        .map_or(true, |(src, dst, _)| *src != src_rate || *dst != dst_rate);
    if needs_rebuild {
        match audio_codec::BoxedResampler::new(src_rate as usize, dst_rate as usize) {
            Ok(new_resampler) => *resampler = Some((src_rate, dst_rate, new_resampler)),
            Err(error) => {
                warn!(
                    %src_rate,
                    %dst_rate,
                    %error,
                    "Failed to build conference resampler"
                );
                return false;
            }
        }
    }

    let resampled = resampler
        .as_mut()
        .expect("resampler built above")
        .2
        .resample(samples);
    out.extend_from_slice(&resampled);
    true
}

/// Resample audio using linear interpolation into `out` (reused across
/// frames to avoid per-frame allocation in audio hot paths).
///
/// Retained for the realtime bridge / transfer paths; the conference media
/// bridge uses [`resample_fir_into`] instead.
pub(crate) fn resample_linear_into(
    samples: &[i16],
    src_rate: u32,
    dst_rate: u32,
    out: &mut Vec<i16>,
) {
    if src_rate == dst_rate {
        out.clear();
        out.extend_from_slice(samples);
        return;
    }

    let ratio = src_rate as f32 / dst_rate as f32;
    let new_len = (samples.len() as f32 / ratio) as usize;
    out.clear();
    out.reserve(new_len);

    for i in 0..new_len {
        let src_idx = i as f32 * ratio;
        let src_idx_floor = src_idx.floor() as usize;
        let src_idx_ceil = (src_idx.ceil() as usize).min(samples.len().saturating_sub(1));
        let frac = src_idx - src_idx.floor();

        let sample = if src_idx_floor == src_idx_ceil {
            samples[src_idx_floor]
        } else {
            let s0 = samples[src_idx_floor] as f32;
            let s1 = samples[src_idx_ceil] as f32;
            (s0 + frac * (s1 - s0)) as i16
        };
        out.push(sample);
    }
}

/// Resample audio using linear interpolation.
pub(crate) fn resample_linear(samples: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    let mut out = Vec::new();
    resample_linear_into(samples, src_rate, dst_rate, &mut out);
    out
}

/// Handle to control a conference media bridge.
pub struct ConferenceBridgeHandle {
    pub(crate) _tasks: Vec<tokio::task::JoinHandle<()>>,
    pub cancel_token: tokio_util::sync::CancellationToken,
    pub(crate) hold: Option<HoldGateHandle>,
}

impl ConferenceBridgeHandle {
    /// Stop the bridge.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    /// Flip the hold gate: while held, the forward loop feeds hold music
    /// instead of the mixer output (the loops and the room membership stay
    /// intact — the room ends when a participant truly departs, not when a
    /// participant holds).
    pub fn set_hold(&self, held: bool) {
        if let Some(gate) = &self.hold {
            gate.set_held(held);
        }
    }
}

/// Hold music gate for a conference-attached leg: while held, the bridge's
/// forward loop feeds looped hold-music PCM instead of the mixer output.
/// The mixer channel keeps flowing (frames are dropped while held) so the
/// gate is independent of the room lifecycle.
pub struct HoldGate {
    held: std::sync::atomic::AtomicBool,
    pcm: std::sync::Mutex<MohPcm>,
}

struct MohPcm {
    /// Pre-resampled to the bridge codec rate.
    samples: Vec<i16>,
    pos: usize,
}

#[derive(Clone)]
pub struct HoldGateHandle(Arc<HoldGate>);

impl HoldGate {
    fn new(samples: Vec<i16>) -> Self {
        Self {
            held: std::sync::atomic::AtomicBool::new(false),
            pcm: std::sync::Mutex::new(MohPcm { samples, pos: 0 }),
        }
    }
}

impl HoldGateHandle {
    pub fn set_held(&self, held: bool) {
        self.0.held.store(held, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_held(&self) -> bool {
        self.0.held.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Fill `frame_buf` with the next hold-music frame (looping). The buffer
    /// is the encoder frame (samples_per_frame long).
    fn next_frame_into(&self, frame_buf: &mut [i16]) {
        let mut pcm = match self.0.pcm.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pcm.samples.is_empty() {
            frame_buf.fill(0);
            return;
        }
        for slot in frame_buf.iter_mut() {
            *slot = pcm.samples[pcm.pos % pcm.samples.len()];
            pcm.pos = (pcm.pos + 1) % pcm.samples.len();
        }
    }
}

/// Decode a PCM16 mono WAV into samples. Returns None for anything other
/// than a plain PCM16 RIFF file (the hold-music fixtures are all PCM16).
fn load_wav_pcm16(path: &str) -> Option<(Vec<i16>, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }
    // Walk chunks: fmt (audio_format=1/PCM16, channels, rate) then data.
    let mut off = 12usize;
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pcm: Option<Vec<i16>> = None;
    while off + 8 <= data.len() {
        let id = &data[off..off + 4];
        let size = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
        let body = data.get(off + 8..off + 8 + size)?;
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes(body.get(2..4)?.try_into().ok()?);
                rate = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
                bits = u16::from_le_bytes(body.get(14..16)?.try_into().ok()?);
            }
            b"data" => {
                if bits == 16 {
                    let usable = body.len() - body.len() % 2;
                    let step = channels.max(1) as usize;
                    pcm = Some(
                        body[..usable]
                            .chunks_exact(2 * step)
                            .map(|c| i16::from_le_bytes([c[0], c[step * 2 - 1]]))
                            .collect(),
                    );
                }
            }
            _ => {}
        }
        off += 8 + size + (size & 1);
    }
    let samples = pcm?;
    (channels == 1 || channels >= 1).then_some((samples, rate))
}

/// One-shot anti-aliased resample (hold-music fixtures at bridge start).
fn resample_to_target(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || from == 0 || to == 0 {
        return samples.to_vec();
    }
    match audio_codec::BoxedResampler::new(from as usize, to as usize) {
        Ok(mut resampler) => resampler.resample(samples),
        Err(error) => {
            warn!(%from, %to, %error, "Failed to resample hold music; using source rate");
            samples.to_vec()
        }
    }
}

/// Build a hold gate from a PCM16 WAV file (pre-resampled to `codec_rate`).
/// None when the file is missing or not plain PCM16.
pub(crate) fn hold_gate_from_wav(path: &str, codec_rate: u32) -> Option<HoldGateHandle> {
    let (samples, rate) = load_wav_pcm16(path)?;
    Some(HoldGateHandle(Arc::new(HoldGate::new(resample_to_target(
        &samples, rate, codec_rate,
    )))))
}


impl Drop for ConferenceBridgeHandle {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        for task in &self._tasks {
            task.abort();
        }
        // Only real bridges (created by `setup_full_duplex_bridge`) carry the
        // spawned forward/reverse tasks. Handles built elsewhere as pure
        // cancel-token markers (`_tasks: vec![]`) are not conference bridges
        // and must not count towards the destroyed metric.
        if !self._tasks.is_empty() {
            crate::metrics::conference::destroyed("bridge_closed");
        }
    }
}

/// Per-session conference bridge state.
pub struct SessionConferenceBridge {
    pub bridge_handle: Option<ConferenceBridgeHandle>,
    pub conf_id: Option<String>,
}

impl SessionConferenceBridge {
    pub fn new() -> Self {
        Self {
            bridge_handle: None,
            conf_id: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.bridge_handle.is_some()
    }

    pub fn stop_bridge(&mut self) {
        if let Some(ref handle) = self.bridge_handle {
            handle.stop();
        }
        self.bridge_handle = None;
        self.conf_id = None;
    }
}

impl Default for SessionConferenceBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::domain::LegId;
    use crate::call::runtime::test_utils::{MockAudioReceiver, MockAudioSender};
    use crate::media::conference_mixer::AudioFrame;
    use rustrtc::media::MediaSample;

    #[tokio::test]
    async fn test_forward_loop_audio_encoding() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Spawn forward loop
        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                None,
            )
            .await;
        });

        // Send an audio frame at 8kHz (matching the encoder)
        let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
        tx.send(AudioFrame {
            sample_rate: 8000,
            samples: samples.clone(),
            timestamp: 0,
        })
        .await
        .unwrap();

        // Give it time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Cancel and wait
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        // Verify audio was encoded and sent
        let sent = sender.get_samples().await;
        assert!(
            !sent.is_empty(),
            "Expected audio samples to be sent after encoding"
        );

        // Verify the first sample is an Audio variant
        match &sent[0] {
            MediaSample::Audio(frame) => {
                assert_eq!(frame.clock_rate, 8000);
                assert_eq!(frame.payload_type, Some(0)); // PCMU
                assert!(!frame.data.is_empty());
            }
            _ => panic!("Expected Audio sample, got {:?}", sent[0]),
        }
    }

    #[tokio::test]
    async fn test_forward_loop_resampling() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                None,
            )
            .await;
        });

        // Send audio at 16kHz (needs decimation to 8kHz)
        let samples: Vec<i16> = (0..320).map(|i| (i as i16 * 50) % 32767).collect();
        tx.send(AudioFrame {
            sample_rate: 16000,
            samples,
            timestamp: 0,
        })
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "Expected resampled audio to be sent");
    }

    #[tokio::test]
    async fn test_forward_loop_sequence_increment() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_clone = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_clone,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                None,
            )
            .await;
        });

        // Send two frames to verify sequence numbers increment
        for _ in 0..2 {
            let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
            tx.send(AudioFrame {
                sample_rate: 8000,
                samples,
                timestamp: 0,
            })
            .await
            .unwrap();
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(sent.len() >= 2, "Expected at least 2 audio packets");

        // Verify sequence numbers increment
        let seq1 = match &sent[0] {
            MediaSample::Audio(f) => f.sequence_number.unwrap(),
            _ => panic!("Expected Audio"),
        };
        let seq2 = match &sent[1] {
            MediaSample::Audio(f) => f.sequence_number.unwrap(),
            _ => panic!("Expected Audio"),
        };
        assert_eq!(
            seq2,
            seq1.wrapping_add(1),
            "Sequence numbers should increment by 1"
        );
    }

    #[tokio::test]
    async fn test_reverse_loop_sends_to_mixer() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(10);
        let pcm_frames = vec![
            PcmAudioFrame::new(vec![1000i16; 160], 8000),
            PcmAudioFrame::new(vec![2000i16; 160], 8000),
        ];
        let receiver = MockAudioReceiver::new(pcm_frames);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                Box::new(receiver),
                input_tx,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                8000,
            )
            .await;
        });

        // Wait for frames to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        // Verify frames were sent to mixer input
        let mut received_count = 0;
        while let Ok(frame) = input_rx.try_recv() {
            received_count += 1;
            assert_eq!(frame.samples.len(), 160);
            assert_eq!(frame.sample_rate, 8000);
        }
        assert_eq!(
            received_count, 2,
            "Expected 2 frames to be sent to mixer input"
        );
    }

    #[tokio::test]
    async fn test_full_duplex_bridge() {
        let conf_mgr = Arc::new(ConferenceManager::new());
        let bridge = ConferenceMediaBridge::new(conf_mgr.clone());

        // Create conference first
        conf_mgr
            .create_conference("conf-1".into(), None)
            .await
            .unwrap();

        let leg_id = LegId::new("test-leg");
        let sender = MockAudioSender::new();
        let receiver = MockAudioReceiver::new(vec![PcmAudioFrame::new(vec![1000i16; 160], 8000)]);

        let handle = bridge
            .start_bridge_full_duplex(
                "conf-1",
                &leg_id,
                sender,
                Box::new(receiver),
                audio_codec::CodecType::PCMU,
                None,
            )
            .await;
        assert!(
            handle.is_ok(),
            "Full-duplex bridge should start successfully"
        );

        let handle = handle.unwrap();
        handle.stop();
    }

    #[tokio::test]
    async fn test_session_conference_bridge_lifecycle() {
        let mut session_bridge = SessionConferenceBridge::new();
        assert!(!session_bridge.is_active());

        session_bridge.conf_id = Some("conf-1".to_string());
        // Note: we can't set bridge_handle without a real JoinHandle,
        // but we can test the stop logic
        session_bridge.stop_bridge();
        assert!(!session_bridge.is_active());
        assert!(session_bridge.conf_id.is_none());
    }

    #[tokio::test]
    async fn test_forward_loop_cancel_immediately() {
        let (_tx, rx) = tokio::sync::mpsc::channel::<AudioFrame>(10);
        let sender = MockAudioSender::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                None
            )
            .await;
        });

        // Cancel immediately
        cancel.cancel();
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        assert!(result.is_ok(), "Forward loop should exit cleanly on cancel");
    }

    #[tokio::test]
    async fn test_reverse_loop_resamples_opus_48khz_to_mixer_8khz() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(10);
        // Simulate Opus decoder output: 960 samples at 48kHz
        let opus_frame = PcmAudioFrame::new(vec![1000i16; 960], 48000);
        let receiver = MockAudioReceiver::new(vec![opus_frame]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                Box::new(receiver),
                input_tx,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                8000,
            )
            .await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        // Verify the frame was resampled to 8kHz
        let frame = input_rx
            .try_recv()
            .expect("Should receive resampled frame from reverse loop");
        assert_eq!(
            frame.sample_rate, 8000,
            "Opus 48kHz PCM should be resampled to mixer rate 8000"
        );
        // 960 samples at 48kHz = 20ms → 160 samples at 8kHz = 20ms
        assert_eq!(
            frame.samples.len(),
            160,
            "960 samples at 48kHz should become 160 at 8kHz (20ms)"
        );
    }

    #[tokio::test]
    async fn test_reverse_loop_passthrough_when_rate_matches() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(10);
        // PCM at 8kHz should pass through without resampling
        let pcm_frame = PcmAudioFrame::new(vec![500i16; 160], 8000);
        let receiver = MockAudioReceiver::new(vec![pcm_frame]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                Box::new(receiver),
                input_tx,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                8000,
            )
            .await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let frame = input_rx
            .try_recv()
            .expect("Should receive passthrough frame");
        assert_eq!(frame.sample_rate, 8000);
        assert_eq!(frame.samples.len(), 160);
        // Samples should be unchanged
        assert_eq!(frame.samples[0], 500);
    }

    #[tokio::test]
    async fn test_forward_loop_g722_uses_encoder_sample_rate() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_for_loop = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Spawn forward loop with G.722 codec
        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_for_loop,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::G722,
                None
            )
            .await;
        });

        // Send mixer audio at 8kHz (160 samples = 20ms).
        // Bug scenario: forward_loop must NOT treat this as 16kHz PCM,
        // but instead resample 8kHz→16kHz for the G.722 encoder.
        let samples: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
        tx.send(AudioFrame {
            sample_rate: 8000,
            samples: samples.clone(),
            timestamp: 0,
        })
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "Expected G.722 audio to be sent");

        match &sent[0] {
            MediaSample::Audio(frame) => {
                // RTP clock rate must be 8000 (G.722 convention)
                assert_eq!(
                    frame.clock_rate, 8000,
                    "G.722 RTP clock rate should be 8000"
                );
                assert_eq!(frame.payload_type, Some(9)); // G.722 static PT
                assert!(!frame.data.is_empty(), "G.722 payload should not be empty");
            }
            _ => panic!("Expected Audio sample"),
        }
    }

    #[tokio::test]
    async fn test_forward_loop_g722_resamples_from_mixer_8khz() {
        let (tx, rx) = tokio::sync::mpsc::channel(10);
        let sender = MockAudioSender::new();
        let sender_for_loop = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_for_loop,
                LegId::new("test-leg"),
                "conf-1".to_string(),
                cancel_clone,
                audio_codec::CodecType::G722,
                None
            )
            .await;
        });

        // Send mixer audio: 160 samples at 8kHz (20ms).
        // With fix, this should be resampled to 320 samples at 16kHz,
        // then fed to G.722 encoder in one 320-sample chunk (20ms).
        let samples: Vec<i16> = vec![1000i16; 160];
        tx.send(AudioFrame {
            sample_rate: 8000,
            samples,
            timestamp: 0,
        })
        .await
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "Expected G.722 encoded audio");

        match &sent[0] {
            MediaSample::Audio(frame) => {
                assert!(!frame.data.is_empty(), "G.722 payload should not be empty");
            }
            _ => panic!("Expected Audio sample"),
        }
    }

    /// Verify that `set_active_bridge` on `SessionConferenceBridge` stops the
    /// old bridge before installing the new one.
    #[tokio::test]
    async fn test_session_conference_bridge_stop_on_replace() {
        use tokio_util::sync::CancellationToken;

        let mut bridge = SessionConferenceBridge::new();

        let cancel1 = CancellationToken::new();
        let cancel1_clone = cancel1.clone();
        let task1 = crate::utils::spawn(async move {
            cancel1_clone.cancelled().await;
        });
        bridge.bridge_handle = Some(ConferenceBridgeHandle {
            hold: None,
            _tasks: vec![task1],
            cancel_token: cancel1,
        });

        // stop_bridge should cancel the first handle
        bridge.stop_bridge();
        assert!(bridge.bridge_handle.is_none());

        // Install a second bridge
        let cancel2 = CancellationToken::new();
        let task2 = crate::utils::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        bridge.bridge_handle = Some(ConferenceBridgeHandle {
            hold: None,
            _tasks: vec![task2],
            cancel_token: cancel2,
        });

        // Drop the bridge entirely — second handle should be cleaned up
        drop(bridge);
    }

    /// Reproduction for the three-way conference hiss: a 5 kHz tone at 16 kHz
    /// (valid G.722 wideband content) must not be folded into the 8 kHz mix.
    /// Plain decimation (`samples[2i]`) aliases it to 3 kHz at full amplitude;
    /// an anti-alias FIR pushes it below the noise floor.
    #[tokio::test]
    async fn test_reverse_loop_decimation_suppresses_aliasing() {
        use std::f32::consts::PI;

        const SRC_RATE: u32 = 16_000;
        const MIX_RATE: u32 = 8_000;
        const FRAMES: usize = 40;
        let samples_per_frame = SRC_RATE as usize / 50;
        let total = samples_per_frame * FRAMES;
        let tone: Vec<i16> = (0..total)
            .map(|i| {
                let phase = i as f32 * 5000.0 * 2.0 * PI / SRC_RATE as f32;
                (phase.sin() * 10_000.0) as i16
            })
            .collect();
        let frames: Vec<PcmAudioFrame> = tone
            .chunks(samples_per_frame)
            .map(|chunk| PcmAudioFrame::new(chunk.to_vec(), SRC_RATE))
            .collect();

        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(128);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::reverse_loop(
                Box::new(MockAudioReceiver::new(frames)),
                input_tx,
                LegId::new("alias-leg"),
                "conf-alias".to_string(),
                cancel_clone,
                MIX_RATE,
            )
            .await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let mut out = Vec::new();
        while let Ok(frame) = input_rx.try_recv() {
            assert_eq!(frame.sample_rate, MIX_RATE);
            out.extend_from_slice(&frame.samples);
        }
        assert!(out.len() >= FRAMES * 80, "expected 40 x 80 resampled samples");

        // Skip the FIR warm-up (history starts zeroed), then measure the
        // surviving energy: the 3 kHz alias would sit here at full amplitude.
        let tail = &out[320..];
        let rms = (tail.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>()
            / tail.len() as f64)
            .sqrt();
        assert!(
            rms < 500.0,
            "5 kHz @16 kHz aliased into the 8 kHz mix: residual rms {rms:.0} \
             (input rms ~7071) — decimation needs an anti-alias filter"
        );
    }

    /// Same defect on the egress side: the mixer runs wideband, but a PCMU
    /// leg must be decimated to 8 kHz without folding 4-8 kHz energy back in.
    #[tokio::test]
    async fn test_forward_loop_decimation_suppresses_aliasing() {
        use std::f32::consts::PI;

        const MIX_RATE: u32 = 16_000;
        const ENC_RATE: u32 = 8_000;
        const FRAMES: usize = 40;
        let samples_per_frame = MIX_RATE as usize / 50;
        let total = samples_per_frame * FRAMES;
        let tone: Vec<i16> = (0..total)
            .map(|i| {
                let phase = i as f32 * 5000.0 * 2.0 * PI / MIX_RATE as f32;
                (phase.sin() * 10_000.0) as i16
            })
            .collect();

        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let sender = MockAudioSender::new();
        let sender_for_loop = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                rx,
                sender_for_loop,
                LegId::new("alias-leg"),
                "conf-alias".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                None,
            )
            .await;
        });

        for chunk in tone.chunks(samples_per_frame) {
            tx.send(AudioFrame::new(chunk.to_vec(), MIX_RATE))
                .await
                .unwrap();
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "expected PCMU frames");
        let mut decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
        let mut out = Vec::new();
        for sample in sent {
            if let MediaSample::Audio(frame) = sample {
                out.extend_from_slice(&decoder.decode(&frame.data));
            }
        }
        // Skip warm-up + codec settling frames.
        let tail = &out[ENC_RATE as usize * 40 / 1000 * 3..];
        let rms = (tail.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>()
            / tail.len() as f64)
            .sqrt();
        assert!(
            rms < 800.0,
            "5 kHz @16 kHz aliased into the 8 kHz PCMU egress: residual rms \
             {rms:.0} (input rms ~7071)"
        );
    }

    /// Regression (production 2026-11-13): a transfer-room hold must NOT
    /// replay stale mixer frames after release. While held, the room mixer
    /// keeps producing frames; the channel (capacity 100) accumulates them.
    /// The hold gate now drains the channel every pacer tick, so the release
    /// resumes at live audio: frames pushed while held must never reach the
    /// wire, and frames pushed after release must be the first ones heard.
    #[tokio::test]
    async fn test_forward_loop_hold_release_does_not_replay_stale_frames() {
        use tokio::sync::mpsc;

        let gate = HoldGateHandle(Arc::new(HoldGate::new(vec![500i16; 160])));
        let gate_handle = gate.clone();
        let (output_tx, output_rx) = mpsc::channel::<AudioFrame>(100);
        let sender = MockAudioSender::new();
        let sender_for_loop = MockAudioSender {
            samples: sender.samples.clone(),
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = crate::utils::spawn(async move {
            ConferenceMediaBridge::forward_loop(
                output_rx,
                sender_for_loop,
                LegId::new("hold-leg"),
                "conf-hold".to_string(),
                cancel_clone,
                audio_codec::CodecType::PCMU,
                Some(gate_handle),
            )
            .await;
        });

        let push = |value: i16| {
            let samples: Vec<i16> = vec![value; 160];
            let tx = output_tx.clone();
            async move {
                tx.send(AudioFrame {
                    sample_rate: 8000,
                    samples,
                    timestamp: 0,
                })
                .await
                .unwrap();
            }
        };

        // Pre-hold live audio: value 1200 must be forwarded while not held.
        push(1200).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;

        // Hold. From here on, mixer frames (2400) keep flowing into the
        // channel and the loop must discard them while emitting gate audio.
        gate.set_held(true);
        for _ in 0..8 {
            push(2400).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
        // More stale frames queued right before the release.
        push(2400).await;
        // Give the pacer one more tick to run its drain (the real flow's
        // release is driven by signaling, seconds after the last hold pacer
        // tick — the channel is empty by then).
        tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;

        // Release. The very next audible frames must be LIVE audio (3600),
        // not the stale hold-window frames (2400).
        gate.set_held(false);
        push(3600).await;
        for _ in 0..6 {
            push(3600).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(
            !sent.is_empty(),
            "forward loop must emit frames across hold and release"
        );
        if std::env::var("HOLD_TEST_DEBUG").is_ok() {
            let mut counts: std::collections::HashMap<i16, usize> = Default::default();
            for sample in &sent {
                if let MediaSample::Audio(frame) = sample {
                    let mut decoder =
                        audio_codec::create_decoder(audio_codec::CodecType::PCMU);
                    let pcm = decoder.decode(&frame.data);
                    let peak = pcm.iter().map(|v| v.abs()).max().unwrap_or(0);
                    if peak < 100 {
                        *counts.entry(0).or_default() += 1;
                    } else {
                        // bucket to nearest 200
                        let first = pcm.first().copied().unwrap_or(0);
                        let bucket = ((first as f64 / 200.0).round() * 200.0) as i16;
                        *counts.entry(bucket).or_default() += 1;
                    }
                }
            }
            eprintln!("HOLD-TEST buckets: {:?}", counts);
        }

        // Classify emitted frames by decoding PCMU payloads. The release edge
        // races the last hold pacer tick, so at most a couple of boundary
        // frames may slip through — what must NEVER happen is the old
        // behavior: replaying the whole hold tail (up to the channel capacity
        // of 100 frames / 2 s) before live audio.
        let mut saw_hold_music = false;
        let mut stale_count = 0usize;
        let mut saw_live_after_release = false;
        let mut released = false;
        for sample in &sent {
            if let MediaSample::Audio(frame) = sample {
                // Decode PCMU payload.
                let mut decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
                let pcm = decoder.decode(&frame.data);
                let first = pcm.first().copied().unwrap_or(0);
                let peak = pcm.iter().map(|v| v.abs()).max().unwrap_or(0);
                if peak < 100 {
                    continue; // silence frame, ignore
                }
                let close = |target: i16| (first - target).abs() <= 150;
                if close(500) {
                    saw_hold_music = true;
                    released = false;
                } else if close(1200) {
                    // Pre-hold live audio: legal only before the hold.
                    if released {
                        stale_count += 1;
                    }
                } else if close(2400) {
                    // Stale hold-window frame reached the wire.
                    stale_count += 1;
                } else if close(3600) {
                    saw_live_after_release = true;
                    released = true;
                }
            }
        }
        assert!(
            saw_hold_music,
            "hold window must emit gate (hold music) frames"
        );
        assert!(
            saw_live_after_release,
            "after release the live audio (3600) must reach the wire"
        );
        assert!(
            stale_count <= 2,
            "{stale_count} stale hold-window frames (2400) reached the wire after release — \
             the gate's drain must keep the replay below a couple of boundary \
             frames (the old behavior replayed the whole 2 s hold tail)"
        );
    }

}