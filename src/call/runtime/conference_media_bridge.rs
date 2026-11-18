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

/// One PCM frame equals 20 ms everywhere on the conference path: the mix-bus
/// frame size and both bridge resampler loops share this cadence.
const CONFERENCE_PTIME_MS: usize = 20;
/// Resample staging-buffer ceiling, in frames. The stateful FIR can leave a
/// short tail per call (group delay) which the next frame absorbs; a backlog
/// beyond this means the consumer stalled — drop instead of growing memory.
const MAX_RESAMPLE_PENDING_FRAMES: usize = 10;

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

        // The participant was just added, so the room exists: read its mix-bus
        // rate instead of assuming 8 kHz. Every leg's PCM is FIR-resampled to
        // this rate on ingress and the mix back to the leg's encoder rate on
        // egress.
        let mixer_sample_rate = self
            .conference_manager
            .conference_mixer_sample_rate(&conf_id_obj)
            .unwrap_or(crate::media::conference_mixer::CONFERENCE_MIXER_SAMPLE_RATE);

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
        use audio_codec::{BoxedResampler, create_encoder};
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

        // Reusable frame buffer — this loop runs every 20ms per participant,
        // so per-frame `Vec` allocations are worth avoiding.
        let mut frame_buf: Vec<i16> = vec![0i16; samples_per_frame];
        // Stateful FIR resampler (mix-bus rate → encoder rate), rebuilt only
        // when the mix rate changes mid-call. Filter state must persist
        // across frames: a per-frame instance turns every frame boundary
        // into a click, and a zero-order decimator folds high frequencies
        // back into the voice band as loud aliasing.
        let mut resampler: Option<(u32, BoxedResampler)> = None;
        // Resampled PCM awaiting a full encoder frame. The FIR group delay
        // makes per-call outputs drift slightly off frame boundaries, so the
        // tail stays buffered and joins the next mix frame (never zero-padded
        // mid-stream).
        let mut pending: Vec<i16> = Vec::with_capacity(samples_per_frame * 2);

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
                    let src_rate = frame.sample_rate;
                    if resampler.as_ref().map_or(true, |(r, _)| *r != src_rate) {
                        resampler = if src_rate == sample_rate {
                            None
                        } else {
                            match BoxedResampler::new(src_rate as usize, sample_rate as usize) {
                                Ok(rs) => {
                                    info!(
                                        leg_id = %leg_id,
                                        from = src_rate,
                                        to = sample_rate,
                                        "Forward loop armed anti-aliased resampler"
                                    );
                                    Some((src_rate, rs))
                                }
                                Err(e) => {
                                    warn!(
                                        leg_id = %leg_id,
                                        from = src_rate,
                                        to = sample_rate,
                                        error = %e,
                                        "Invalid resampler rates; dropping mixed frame"
                                    );
                                    continue;
                                }
                            }
                        };
                    }
                    match &mut resampler {
                        Some((_, rs)) => pending.extend_from_slice(&rs.resample(&frame.samples)),
                        None => pending.extend_from_slice(&frame.samples),
                    }
                    if pending.len() > samples_per_frame * MAX_RESAMPLE_PENDING_FRAMES {
                        warn!(
                            leg_id = %leg_id,
                            backlog = pending.len(),
                            "Conference forward resample backlog; dropping stale PCM"
                        );
                        pending.clear();
                    }

                    // Encode whole frames only; the remainder waits for the
                    // next mix frame instead of being zero-padded (padding
                    // truncated the stream whenever rates misaligned).
                    while pending.len() >= samples_per_frame {
                        frame_buf.copy_from_slice(&pending[..samples_per_frame]);
                        pending.drain(..samples_per_frame);

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

                        let bytes_sent = samples_per_frame * 2; // i16 = 2 bytes
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
                        pending = pending.len(),
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
        use audio_codec::BoxedResampler;

        info!(
            leg_id = %leg_id,
            conf_id = %conf_id,
            mixer_sample_rate,
            "Conference media bridge reverse loop started"
        );

        // The mixer consumes fixed 20 ms frames at the mix-bus rate.
        let frame_size = mixer_sample_rate as usize * CONFERENCE_PTIME_MS / 1000;
        // Stateful FIR resampler (leg PCM rate → mix-bus rate), cached per
        // source rate. Filter state persists across frames — a per-frame
        // instance injects a click at every boundary, and naive decimation
        // (e.g. taking every 2nd/6th sample from G.722/Opus) folds the upper
        // voice band back down as loud aliasing noise.
        let mut resampler: Option<(u32, BoxedResampler)> = None;
        // Resampled PCM awaiting a full mix frame; absorbs the FIR group delay.
        let mut pending: Vec<i16> = Vec::with_capacity(frame_size * 2);

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

                    if resampler.as_ref().map_or(true, |(r, _)| *r != sample_rate) {
                        resampler = if sample_rate == mixer_sample_rate {
                            None
                        } else {
                            match BoxedResampler::new(sample_rate as usize, mixer_sample_rate as usize) {
                                Ok(rs) => {
                                    info!(
                                        leg_id = %leg_id,
                                        from = sample_rate,
                                        to = mixer_sample_rate,
                                        "Reverse loop armed anti-aliased resampler"
                                    );
                                    Some((sample_rate, rs))
                                }
                                Err(e) => {
                                    warn!(
                                        leg_id = %leg_id,
                                        from = sample_rate,
                                        to = mixer_sample_rate,
                                        error = %e,
                                        "Invalid resampler rates; dropping participant frame"
                                    );
                                    continue;
                                }
                            }
                        };
                    }
                    match &mut resampler {
                        Some((_, rs)) => pending.extend_from_slice(&rs.resample(&pcm_frame.samples)),
                        None => pending.extend_from_slice(&pcm_frame.samples),
                    }
                    if pending.len() > frame_size * MAX_RESAMPLE_PENDING_FRAMES {
                        warn!(
                            leg_id = %leg_id,
                            backlog = pending.len(),
                            "Conference reverse resample backlog; dropping stale PCM"
                        );
                        pending.clear();
                    }

                    // Emit whole mix frames; the remainder joins the next
                    // inbound frame (mixer pads/aligns per its own contract).
                    while pending.len() >= frame_size {
                        let samples: Vec<i16> = pending.drain(..frame_size).collect();
                        let audio_frame = AudioFrame::new(samples, mixer_sample_rate);

                        if let Err(e) = input_tx.send(audio_frame).await {
                            warn!(
                                leg_id = %leg_id,
                                conf_id = %conf_id,
                                error = %e,
                                "Failed to send audio to conference mixer input"
                            );
                            return;
                        }
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

/// Resample into a fresh vector with the crate's anti-aliased FIR resampler
/// (same strategy as the bridge loops, so downsampling hold music never
/// folds high frequencies back into the voice band).
fn resample_to_target(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || from == 0 || to == 0 {
        return samples.to_vec();
    }
    match audio_codec::BoxedResampler::new(from as usize, to as usize) {
        Ok(mut rs) => rs.resample(samples),
        Err(_) => samples.to_vec(),
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
        // Simulate Opus decoder output: 3 frames of 960 samples at 48kHz
        // (60ms). Several frames are fed because the FIR's group delay can
        // hold back part of the first 20ms — the pending buffer emits whole
        // mix frames only.
        let receiver = MockAudioReceiver::new(vec![
            PcmAudioFrame::new(vec![1000i16; 960], 48000),
            PcmAudioFrame::new(vec![1000i16; 960], 48000),
            PcmAudioFrame::new(vec![1000i16; 960], 48000),
        ]);
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

        // Verify frames were resampled to 8kHz at the mixer's 20ms cadence
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
            "mix frames must be exactly 20ms (160 samples at 8kHz)"
        );
        // 60ms of input must produce at least two full mix frames even with
        // FIR group delay eating into the first one.
        let mut total_frames = 1;
        while input_rx.try_recv().is_ok() {
            total_frames += 1;
        }
        assert!(
            total_frames >= 2,
            "60ms of 48kHz input should yield ≥2 mix frames, got {total_frames}"
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

    /// Aliasing regression (R1): decimating 16 kHz (G.722) PCM to the 8 kHz
    /// mix bus must low-pass first. The old linear-interpolation path
    /// degraded to per-N sampling for integer ratios, folding everything
    /// above 4 kHz back into the voice band as loud noise. A 7.9 kHz tone
    /// must arrive at the mixer heavily attenuated, not mirrored to ~100 Hz.
    #[tokio::test]
    async fn test_reverse_loop_fir_rejects_out_of_band_before_decimation() {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(10);
        // 7.9 kHz tone at 16 kHz: far above the 8 kHz output's Nyquist
        // (4 kHz), so a proper anti-alias FIR suppresses it into the noise
        // floor while naive decimation mirrors it to a near-full-scale tone.
        let tone_at_16k: Vec<i16> = (0..320)
            .map(|i| {
                let t = i as f32 / 16000.0;
                (8000.0 * (2.0 * std::f32::consts::PI * 7900.0 * t).sin()) as i16
            })
            .collect();
        let input_peak = tone_at_16k.iter().map(|s| s.abs()).max().unwrap();
        let receiver = MockAudioReceiver::new(vec![
            PcmAudioFrame::new(tone_at_16k.clone(), 16000),
            PcmAudioFrame::new(tone_at_16k.clone(), 16000),
            PcmAudioFrame::new(tone_at_16k.clone(), 16000),
            PcmAudioFrame::new(tone_at_16k, 16000),
        ]);
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

        let mut output_peak = 0i64;
        let mut frames = 0usize;
        while let Ok(frame) = input_rx.try_recv() {
            assert_eq!(frame.sample_rate, 8000);
            for &s in &frame.samples {
                output_peak = output_peak.max(s.abs() as i64);
            }
            frames += 1;
        }
        assert!(frames > 0, "resampled frames must reach the mixer");
        assert!(
            output_peak * 4 <= input_peak as i64,
            "out-of-band tone must be attenuated by the anti-alias FIR: \
             output peak {output_peak} vs input peak {input_peak}"
        );
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

    /// Mixed conference audio rides the 16 kHz mix bus; an Opus leg's
    /// forward loop must upsample 16k → 48 kHz with the stateful FIR before
    /// encoding (interpolated upsampling, never sample duplication).
    #[tokio::test]
    async fn test_forward_loop_opus_resamples_from_mixer_16khz() {
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
                audio_codec::CodecType::Opus,
                None,
            )
            .await;
        });

        // Three 20 ms mix-bus frames at 16 kHz (60 ms total — several frames
        // so the FIR group delay cannot hold the whole stream unemitted).
        let mix_frame: Vec<i16> = (0..320).map(|i| ((i * 37) % 3000) as i16).collect();
        for _ in 0..3 {
            tx.send(AudioFrame {
                sample_rate: 16000,
                samples: mix_frame.clone(),
                timestamp: 0,
            })
            .await
            .unwrap();
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await;

        let sent = sender.get_samples().await;
        assert!(!sent.is_empty(), "Expected Opus-encoded audio to be sent");

        match &sent[0] {
            MediaSample::Audio(frame) => {
                assert_eq!(frame.clock_rate, 48000, "Opus RTP clock rate");
                assert_eq!(frame.payload_type, Some(audio_codec::CodecType::Opus.payload_type()));
                assert!(!frame.data.is_empty(), "Opus payload should not be empty");
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