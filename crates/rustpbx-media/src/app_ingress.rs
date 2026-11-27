//! Single-leg decoded PCM ingress.
//!
//! [`LegPcmStream`] attaches a dedicated decode task to one leg's incoming
//! RTP: a packet-forwarding observer feeds every ingress packet into a
//! channel, and the decode task emits one PCM frame per ptime tick. Silence
//! frames keep the cadence when the leg is starved, so downstream consumers
//! (conference / supervisor mixer input, live transcription) always see a
//! continuous frame stream.
//!
//! Ingress RTP is reordered by sequence number into a small jitter buffer
//! ([`JITTER_BUFFER_PACKETS`] ≈ 40 ms at 20 ms ptime) before decoding —
//! the tap bypasses the receiver's own reordering path, and stateful codecs
//! corrupt when fed out-of-order packets. Added latency is therefore ~40 ms
//! plus the FIR group delay when a downstream bridge resamples.
//!
//! Dropping the stream cancels the decode task and deactivates the
//! forwarding observer, so consumers that start/stop the tap repeatedly
//! (live transcription, supervisor) never accumulate observers on the
//! PeerConnection.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use audio_codec::{CodecType, create_decoder};
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::AudioFrame;
use crate::leg_id::LegId;
use crate::negotiate::NegotiatedLegProfile;

const DEFAULT_PTIME_MS: u64 = 20;

/// Packets held before playback starts (2 x 20 ms): absorbs network
/// reordering and short scheduler jitter without adding audible delay.
const JITTER_BUFFER_PACKETS: usize = 2;

/// Hard bound on the reorder map (32 packets ≈ 640 ms at 20 ms ptime).
const JITTER_BUFFER_MAX_PACKETS: usize = 32;

/// Idle ticks after which a buffered packet is released even if the target
/// depth was never reached (a lone packet must not wait forever).
const JITTER_FORCE_TICKS: u8 = 2;

/// Decoded PCM bound (400 ms). A producer whose clock runs fast must not grow
/// latency without limit; the oldest audio is trimmed instead.
const MAX_PCM_BUFFER_MS: u64 = 400;

#[derive(Default)]
struct IngressCounters {
    late_packets: AtomicU64,
    duplicate_packets: AtomicU64,
    lost_packets: AtomicU64,
    non_audio_packets: AtomicU64,
    dropped_samples: AtomicU64,
}

/// Snapshot of the ingress tap's loss diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngressStats {
    /// Packets that arrived after their slot had already been played.
    pub late_packets: u64,
    /// Replayed sequence numbers discarded.
    pub duplicate_packets: u64,
    /// Sequence gaps that the jitter buffer gave up waiting for.
    pub lost_packets: u64,
    /// Packets whose payload type was not the negotiated audio codec
    /// (telephone-event / comfort-noise / video).
    pub non_audio_packets: u64,
    /// Decoded PCM samples trimmed from the bounded playback buffer.
    pub dropped_samples: u64,
}

/// Reorders ingress RTP by sequence number before decoding.
///
/// The tap consumes packets directly from `RtpObserver::on_ingress`, i.e. in
/// arrival order — bypassing the jitter/reorder path used for normal
/// playback. Stateful codecs (G.722 ADPCM, Opus) corrupt their predictor on
/// out-of-order input, so ordering has to happen here.
#[derive(Default)]
struct ReorderBuffer {
    packets: BTreeMap<u64, RtpPacket>,
    /// Extended (wrap-aware) sequence number of the next packet to decode.
    next_seq: Option<u64>,
    highest_seq: Option<u64>,
    late: u64,
    duplicates: u64,
    lost: u64,
}

impl ReorderBuffer {
    fn push(&mut self, packet: RtpPacket) {
        let seq = packet.header.sequence_number;
        let extended = match self.highest_seq {
            Some(highest) => extend_sequence(highest, seq),
            None => seq as u64,
        };

        if let Some(next) = self.next_seq
            && extended < next
        {
            self.late += 1;
            return;
        }
        if self.packets.contains_key(&extended) {
            self.duplicates += 1;
            return;
        }

        self.highest_seq = Some(self.highest_seq.map_or(extended, |h| h.max(extended)));
        self.packets.insert(extended, packet);

        while self.packets.len() > JITTER_BUFFER_MAX_PACKETS {
            let Some((&lowest, _)) = self.packets.iter().next() else {
                break;
            };
            self.packets.remove(&lowest);
            self.lost += 1;
            if self.next_seq.map_or(true, |next| next <= lowest) {
                self.next_seq = Some(lowest + 1);
            }
        }
    }

    /// Packets that are safe to decode, in sequence order.
    ///
    /// Normally returns nothing until the buffer holds more than the target
    /// depth, which is what gives reordering room. `force` (idle timeout or a
    /// gap) releases the next packet anyway, skipping any sequence gap.
    fn drain_ready(&mut self, force: bool) -> Vec<RtpPacket> {
        let mut ready = Vec::new();
        if self.packets.is_empty() {
            return ready;
        }

        let mut force = force;
        while !self.packets.is_empty() {
            if !force && self.packets.len() <= JITTER_BUFFER_PACKETS {
                break;
            }
            // Pin the decode position only when actually decoding: while the
            // buffer is still filling, a lower sequence number may still
            // arrive, and pinning early would discard it as "late".
            let next = match self.next_seq {
                Some(next) => next,
                None => *self.packets.keys().next().expect("non-empty above"),
            };
            if let Some(packet) = self.packets.remove(&next) {
                ready.push(packet);
                self.next_seq = Some(next + 1);
                force = false;
                continue;
            }

            // Missing `next`: wait unless forced past the gap.
            if !force {
                break;
            }
            let Some((&lowest, _)) = self.packets.iter().next() else {
                break;
            };
            self.lost += lowest.saturating_sub(next);
            self.next_seq = Some(lowest);
            if let Some(packet) = self.packets.remove(&lowest) {
                ready.push(packet);
                self.next_seq = Some(lowest + 1);
            }
            force = false;
        }
        ready
    }
}

/// Publish the reorder buffer's deltas to the shared counters. Called every
/// tick so diagnostics are live (a long call must not show all-zero stats),
/// and once more when the task exits.
fn flush_ingress_counters(reorder: &mut ReorderBuffer, counters: &IngressCounters) {
    if reorder.late != 0 {
        counters
            .late_packets
            .fetch_add(std::mem::take(&mut reorder.late), Ordering::Relaxed);
    }
    if reorder.duplicates != 0 {
        counters
            .duplicate_packets
            .fetch_add(std::mem::take(&mut reorder.duplicates), Ordering::Relaxed);
    }
    if reorder.lost != 0 {
        counters
            .lost_packets
            .fetch_add(std::mem::take(&mut reorder.lost), Ordering::Relaxed);
    }
}

/// Wrap-aware extension of a 16-bit RTP sequence number.
fn extend_sequence(highest: u64, seq: u16) -> u64 {
    let base = highest & !0xFFFF;
    let candidate = base | seq as u64;
    if candidate + 0x8000 < highest {
        candidate + 0x1_0000
    } else if candidate > highest + 0x8000 {
        candidate.checked_sub(0x1_0000).unwrap_or(candidate)
    } else {
        candidate
    }
}

/// A decoded PCM frame.
#[derive(Debug, Clone)]
pub struct PcmFrame {
    pub frame: AudioFrame,
    /// `true` when this frame is silence (no buffered data this tick). Lets
    /// consumers skip processing or mark gaps.
    pub silence: bool,
}

/// Single-leg decoded PCM stream.
///
/// Attach a leg's `PeerConnection` + negotiated profile, then `recv()`
/// returns one PCM frame per ptime tick (silence frames keep cadence when
/// the leg is starved).
pub struct LegPcmStream {
    rx: mpsc::Receiver<PcmFrame>,
    sample_rate: u32,
    cancel: CancellationToken,
    counters: Arc<IngressCounters>,
    /// Shared with the installed `PacketForwarder` observer; set to `false`
    /// on drop so the observer stops forwarding packets even though rustrtc
    /// does not expose a "remove observer" API. Prevents observer
    /// accumulation from pushing into a dead channel forever.
    observer_active: Arc<AtomicBool>,
}

impl LegPcmStream {
    /// Attach to `pc` (the leg's PeerConnection) using its negotiated
    /// profile. Installs a packet-forwarding observer that decodes ingress
    /// RTP to PCM. `parent_token` (e.g. the MediaBridge root token) cancels
    /// the decode task independently of this stream's lifetime.
    pub fn attach(
        pc: &rustrtc::PeerConnection,
        profile: NegotiatedLegProfile,
        leg_id: LegId,
        parent_token: CancellationToken,
    ) -> Result<Self> {
        let audio = profile
            .audio
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no negotiated audio codec for leg {leg_id}"))?;
        let codec = audio.codec;
        let payload_type = audio.payload_type;
        let sample_rate = audio.codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(256);
        let (frame_tx, frame_rx) = mpsc::channel::<PcmFrame>(16);
        let cancel = parent_token.child_token();
        let observer_active = Arc::new(AtomicBool::new(true));
        let counters = Arc::new(IngressCounters::default());

        // Install a forwarding observer on the PC (fires on every ingress
        // plaintext packet, including relay fast-path).
        pc.add_observer(Arc::new(PacketForwarder {
            tx: pkt_tx,
            active: observer_active.clone(),
        }));

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            leg_id,
            codec,
            payload_type,
            sample_rate,
            pcm_per_frame,
            cancel.clone(),
            counters.clone(),
        ));

        Ok(Self {
            rx: frame_rx,
            sample_rate,
            cancel,
            counters,
            observer_active,
        })
    }

    /// Loss diagnostics for this tap (reordering / duplicates / PT filtering).
    pub fn stats(&self) -> IngressStats {
        IngressStats {
            late_packets: self.counters.late_packets.load(Ordering::Relaxed),
            duplicate_packets: self.counters.duplicate_packets.load(Ordering::Relaxed),
            lost_packets: self.counters.lost_packets.load(Ordering::Relaxed),
            non_audio_packets: self.counters.non_audio_packets.load(Ordering::Relaxed),
            dropped_samples: self.counters.dropped_samples.load(Ordering::Relaxed),
        }
    }

    /// Fixed decoded PCM rate for this ingress tap, available before the first frame.
    /// Uses the same value as the decode task; this is not the RTP clock rate
    /// (e.g. G.722 decodes at 16 kHz despite its 8 kHz RTP clock). This tap reads
    /// ingress RTP directly, before bridge/mixer/egress resampling.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Receive the next PCM frame. Returns `None` when the stream is closed
    /// (decode task stopped).
    pub async fn recv(&mut self) -> Option<PcmFrame> {
        self.rx.recv().await
    }
}

impl Drop for LegPcmStream {
    fn drop(&mut self) {
        // Deactivate the observer BEFORE cancelling so the observer stops
        // forwarding immediately (rustrtc has no observer removal API).
        self.observer_active.store(false, Ordering::Release);
        self.cancel.cancel();
    }
}

/// Forwards ingress RTP packets into a channel for the decode task.
struct PacketForwarder {
    tx: mpsc::Sender<RtpPacket>,
    /// Set to `false` on detach so the observer stops forwarding.
    active: Arc<AtomicBool>,
}

impl RtpObserver for PacketForwarder {
    fn on_ingress(&self, packet: &RtpPacket, _src_addr: std::net::SocketAddr) {
        if self.active.load(Ordering::Acquire) {
            let _ = self.tx.try_send(packet.clone());
        }
    }
    // Egress not needed for ingress aggregation.
}

/// Decode-task: reorders ingress RTP, decodes to PCM, and emits one PCM frame
/// per ptime with a continuous sample timestamp.
///
/// Packets are held for [`JITTER_BUFFER_PACKETS`] so out-of-order arrivals are
/// decoded in sequence order (stateful codecs corrupt on reordering).
/// Non-audio payload types (telephone-event, CNG) are never fed to the
/// decoder. Decoded PCM is bounded to [`MAX_PCM_BUFFER_MS`], and each emitted
/// frame carries its position on the tap's timeline so downstream mixers can
/// place it by time instead of by arrival.
///
/// When the consumer is slow the frame channel (capacity 16) drops frames
/// (`try_send`) rather than letting the decoder fall behind real time —
/// matching the previous broadcast lagged-drop behavior.
#[allow(clippy::too_many_arguments)]
async fn decode_task(
    mut pkt_rx: mpsc::Receiver<RtpPacket>,
    frame_tx: mpsc::Sender<PcmFrame>,
    leg: LegId,
    codec: CodecType,
    payload_type: u8,
    sample_rate: u32,
    pcm_per_frame: usize,
    cancel: CancellationToken,
    counters: Arc<IngressCounters>,
) {
    let mut decoder = create_decoder(codec);
    let mut reorder = ReorderBuffer::default();
    let mut buffer: Vec<i16> = Vec::with_capacity(pcm_per_frame * 4);
    let mut interval = tokio::time::interval(Duration::from_millis(DEFAULT_PTIME_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let max_buffer_samples = (sample_rate as u64 * MAX_PCM_BUFFER_MS / 1000) as usize;
    // Sample position of the next emitted frame on this tap's timeline.
    let mut next_timestamp: u64 = 0;
    let mut idle_ticks: u8 = 0;
    let mut non_audio_logged = false;
    let mut in_dtx = false;
    let mut sid_silence_pending = false;
    let sid_samples = (sample_rate as usize / 100).max(1);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            pkt = pkt_rx.recv() => match pkt {
                Some(p) => {
                    if p.header.payload_type != payload_type {
                        // RFC 4733 telephone-event / CNG / video: decoding
                        // these as audio poisons stateful codecs.
                        counters.non_audio_packets.fetch_add(1, Ordering::Relaxed);
                        if p.header.payload_type == crate::dtx::COMFORT_NOISE_PT {
                            in_dtx = true;
                            sid_silence_pending = false;
                        }
                        if !non_audio_logged {
                            non_audio_logged = true;
                            debug!(
                                leg = %leg,
                                payload_type = p.header.payload_type,
                                negotiated = payload_type,
                                "app-ingress skipping non-audio RTP payload type"
                            );
                        }
                        continue;
                    }
                    reorder.push(p);
                }
                None => break,
            },
            _ = interval.tick() => {
                let force = idle_ticks >= JITTER_FORCE_TICKS && !reorder.packets.is_empty();
                let ready = reorder.drain_ready(force);
                flush_ingress_counters(&mut reorder, &counters);
                // Count ticks where a buffered packet is still waiting. A
                // lost sequence number keeps later packets arriving, so this
                // must not be reset by pushes: after JITTER_FORCE_TICKS the
                // gap is skipped instead of stalling the buffer.
                if ready.is_empty() && !reorder.packets.is_empty() {
                    idle_ticks = idle_ticks.saturating_add(1);
                } else {
                    idle_ticks = 0;
                }
                for packet in ready {
                    match crate::dtx::decode_audio_packet(
                        codec,
                        decoder.as_mut(),
                        &packet.payload,
                    ) {
                        crate::dtx::DecodedAudio::Speech { pcm, enter_dtx } => {
                            if sid_silence_pending {
                                // Preserve the 10 ms G.729B SID duration when
                                // speech resumes before the trailing silence flushes.
                                buffer.resize(buffer.len() + sid_samples, 0);
                            }
                            buffer.extend_from_slice(&pcm);
                            in_dtx = enter_dtx;
                            sid_silence_pending = enter_dtx;
                        }
                        crate::dtx::DecodedAudio::Silence => {
                            in_dtx = true;
                            sid_silence_pending = false;
                        }
                        crate::dtx::DecodedAudio::Ignore => {}
                    }
                }

                if buffer.len() > max_buffer_samples {
                    let excess = buffer.len() - max_buffer_samples;
                    buffer.drain(..excess);
                    counters.dropped_samples.fetch_add(excess as u64, Ordering::Relaxed);
                }

                let (samples, silence) = if buffer.len() >= pcm_per_frame {
                    let frame: Vec<i16> = buffer.drain(..pcm_per_frame).collect();
                    (frame, false)
                } else if in_dtx && !buffer.is_empty() {
                    // Explicit DTX ended on a partial frame (e.g. 12 B G.729
                    // 10 ms speech + 2 B SID): flush it padded with silence
                    // rather than stranding trailing speech until the next talkspurt.
                    let mut frame: Vec<i16> = buffer.drain(..).collect();
                    frame.resize(pcm_per_frame, 0);
                    sid_silence_pending = false;
                    (frame, false)
                } else {
                    // Not enough decoded PCM yet → pad with silence to keep
                    // cadence (buffer continues accumulating).
                    sid_silence_pending = false;
                    (vec![0i16; pcm_per_frame], true)
                };
                let mut frame = AudioFrame::new(samples, sample_rate);
                frame.timestamp = next_timestamp;
                next_timestamp = next_timestamp.saturating_add(pcm_per_frame as u64);
                let _ = frame_tx.try_send(PcmFrame { frame, silence });
            }
        }
    }

    flush_ingress_counters(&mut reorder, &counters);
    trace!(leg = %leg, "app-ingress decode task stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::rtp::{RtpHeader, RtpPacket};

    #[tokio::test]
    async fn per_leg_pcm_rate_is_available_before_audio_and_matches_frames() {
        let mut taps = Vec::new();
        for (id, codec, rate) in [("caller", CodecType::PCMU, 8000), ("callee", CodecType::G722, 16000), ("webrtc", CodecType::Opus, 48000)] {
            let pc = rustrtc::PeerConnection::new(rustrtc::RtcConfiguration::sip());
            let profile = NegotiatedLegProfile {
                audio: Some(crate::negotiate::NegotiatedCodec {
                    codec, payload_type: codec.payload_type(),
                    clock_rate: if codec == CodecType::G722 { 8000 } else { rate }, channels: 1,
                    fmtp: None,
                }), ..Default::default()
            };
            let stream = LegPcmStream::attach(&pc, profile, LegId::from(id), CancellationToken::new()).unwrap();
            // No recv, packet, or task yield is required to learn each leg's PCM rate.
            assert_eq!(stream.sample_rate(), rate);
            assert_eq!(create_decoder(codec).sample_rate(), rate);
            taps.push((pc, stream, rate));
        }
        for (_pc, mut stream, rate) in taps {
            let frame = tokio::time::timeout(Duration::from_millis(200), stream.recv()).await.unwrap().unwrap();
            assert!(frame.silence);
            assert_eq!(frame.frame.sample_rate, stream.sample_rate());
            assert_eq!(frame.frame.samples.len(), rate as usize / 50);
        }
    }

    /// Drive the decode task directly (no PeerConnection) and verify it emits
    /// silence frames when starved, and PCM frames when fed.
    #[tokio::test]
    async fn decode_task_emits_silence_then_pcm() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();

        // PCMU @ 8kHz, 20ms → 160 samples/frame.
        let codec = CodecType::PCMU;
        let sample_rate = codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("a"),
            codec,
            codec.payload_type(),
            sample_rate,
            pcm_per_frame,
            cancel.clone(),
            Arc::new(IngressCounters::default()),
        ));

        // First tick(s) with no data → silence frames.
        let f1 = rx.recv().await.unwrap();
        assert!(f1.silence, "starved task must emit silence");
        assert_eq!(f1.frame.samples.len(), pcm_per_frame);

        // Feed enough PCMU data to fill a frame. PCMU decode of N bytes → N samples.
        // Feed 3 frames worth of payload so at least one non-silence frame lands.
        for i in 0..3 {
            let p = RtpPacket::new(RtpHeader::new(0, i, (i as u32) * 160, 1), vec![0xFFu8; 160]);
            let _ = pkt_tx.try_send(p);
        }
        // Allow the task to decode + a couple ticks.
        let mut saw_pcm = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(f)) => {
                    if !f.silence {
                        saw_pcm = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            saw_pcm,
            "must emit at least one non-silence PCM frame after feeding data"
        );

        cancel.cancel();
    }

    /// Out-of-order packets must be decoded in sequence order (stateful
    /// codecs corrupt otherwise), duplicates dropped, and non-audio payload
    /// types (telephone-event) never fed to the decoder.
    #[tokio::test]
    async fn decode_task_reorders_deduplicates_and_skips_non_audio() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let counters = Arc::new(IngressCounters::default());
        let codec = CodecType::PCMU;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("reorder"),
            codec,
            codec.payload_type(),
            sr,
            pf,
            cancel.clone(),
            counters.clone(),
        ));

        // Three distinguishable payloads; seq 2 arrives first, then 0, a
        // duplicate of 2, seq 1, plus a telephone-event packet.
        let payloads = [vec![0x00u8; 160], vec![0x80u8; 160], vec![0xFFu8; 160]];
        let mut reference = create_decoder(codec);
        let expected: Vec<i16> = payloads
            .iter()
            .flat_map(|payload| reference.decode(payload))
            .collect();

        for (seq, payload) in [(2u16, 2usize), (0, 0), (2, 2), (1, 1)] {
            let packet = RtpPacket::new(
                RtpHeader::new(codec.payload_type(), seq, seq as u32 * 160, 1),
                payloads[payload].clone(),
            );
            pkt_tx.try_send(packet).unwrap();
        }
        // RFC 4733 telephone-event: dynamic PT, must be filtered.
        pkt_tx
            .try_send(RtpPacket::new(RtpHeader::new(101, 9, 0, 1), vec![0xABu8; 4]))
            .unwrap();

        let mut got = Vec::new();
        for _ in 0..20 {
            let Ok(Some(frame)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            else {
                break;
            };
            if !frame.silence {
                got.extend_from_slice(&frame.frame.samples);
            }
            if got.len() >= expected.len() {
                break;
            }
        }

        assert_eq!(
            &got[..expected.len()],
            &expected[..],
            "packets must be decoded in sequence order with the duplicate dropped"
        );
        assert_eq!(
            counters.duplicate_packets.load(Ordering::Relaxed),
            1,
            "duplicate sequence number must be ignored"
        );
        assert_eq!(
            counters.non_audio_packets.load(Ordering::Relaxed),
            1,
            "telephone-event payload must not reach the audio decoder"
        );

        cancel.cancel();
    }

    /// Reordering across a tick boundary (the common case): seq 1 arrives
    /// first and survives a mixer tick before seq 0 shows up. The decode
    /// position must not be pinned while the buffer is still filling.
    #[tokio::test(start_paused = true)]
    async fn decode_task_orders_reorder_across_tick_boundary() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let counters = Arc::new(IngressCounters::default());
        let codec = CodecType::PCMU;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("reorder-boundary"),
            codec,
            codec.payload_type(),
            sr,
            pf,
            cancel.clone(),
            counters.clone(),
        ));

        let payloads = [vec![0x00u8; 160], vec![0x80u8; 160]];
        let mut reference = create_decoder(codec);
        let expected: Vec<i16> = payloads
            .iter()
            .flat_map(|payload| reference.decode(payload))
            .collect();

        // seq 1 first; it is the only buffered packet when a tick fires.
        pkt_tx
            .try_send(RtpPacket::new(
                RtpHeader::new(codec.payload_type(), 1, 160, 1),
                payloads[1].clone(),
            ))
            .unwrap();
        tokio::time::advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        // seq 0 arrives after that tick — must still be decoded first.
        pkt_tx
            .try_send(RtpPacket::new(
                RtpHeader::new(codec.payload_type(), 0, 0, 1),
                payloads[0].clone(),
            ))
            .unwrap();
        for _ in 0..8 {
            tokio::time::advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
        }

        let mut got = Vec::new();
        let mut frames_seen = 0usize;
        let mut silence_seen = 0usize;
        while let Ok(frame) = rx.try_recv() {
            frames_seen += 1;
            if frame.silence {
                silence_seen += 1;
                continue;
            }
            got.extend_from_slice(&frame.frame.samples);
        }
        if std::env::var("REORDER_DEBUG").is_ok() {
            eprintln!(
                "REORDER-DEBUG frames={frames_seen} silence={silence_seen} got={} late={} lost={}",
                got.len(),
                counters.late_packets.load(Ordering::Relaxed),
                counters.lost_packets.load(Ordering::Relaxed),
            );
        }
        assert_eq!(
            &got[..expected.len()],
            &expected[..],
            "packets reordered across a tick boundary must decode in sequence order"
        );
        assert_eq!(
            counters.late_packets.load(Ordering::Relaxed),
            0,
            "a late-but-in-order packet must not be discarded as too old"
        );

        cancel.cancel();
    }

    #[test]
    fn sequence_extension_handles_wraparound() {
        assert_eq!(extend_sequence(100, 101), 101);
        assert_eq!(extend_sequence(100, 99), 99);
        // 65534 -> 65535 -> 0 -> 1
        assert_eq!(extend_sequence(65535, 0), 65536);
        assert_eq!(extend_sequence(65536, 1), 65537);
        // Out-of-order packet from before the wrap maps back below it.
        assert_eq!(extend_sequence(65536, 65535), 65535);
    }

    /// A slow consumer drops frames (bounded channel) but the stream stays
    /// open — `recv` keeps returning after the channel has been full.
    #[tokio::test]
    async fn slow_consumer_does_not_close_stream() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(2);
        let (_pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let codec = CodecType::PCMU;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("s"),
            codec,
            codec.payload_type(),
            sr,
            pf,
            cancel.clone(),
            Arc::new(IngressCounters::default()),
        ));

        // Don't read for a while → channel overflows and frames drop...
        tokio::time::sleep(Duration::from_millis(150)).await;
        // ...but recv still works afterwards (task alive, cadence preserved).
        let f = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv after overflow")
            .expect("stream still open");
        assert_eq!(f.frame.samples.len(), pf);

        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn decode_task_g729_dtx_suppresses_cng_and_flushes_12b_tail() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let codec = CodecType::G729;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("g729-dtx"),
            codec,
            18,
            sr,
            pf,
            cancel.clone(),
            Arc::new(IngressCounters::default()),
        ));

        let mut enc = audio_codec::create_encoder(CodecType::G729);
        let sine: Vec<i16> = (0..160)
            .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 8000.0).sin() * 12_000.0) as i16)
            .collect();
        let _ = enc.encode(&sine);
        let speech1 = enc.encode(&sine);
        let speech2 = enc.encode(&sine);
        let speech3 = enc.encode(&sine);
        let mut mixed_12 = speech3[..10].to_vec();
        mixed_12.extend_from_slice(&[0x12, 0x34]);

        // Send 20 ms speech, 20 ms speech, 12 B (10 ms speech + 2 B SID), then 4 standalone SIDs.
        for (seq, ts, payload) in [
            (1u16, 0u32, speech1),
            (2, 160, speech2),
            (3, 320, mixed_12),
            (4, 480, vec![0x12, 0x34]),
            (5, 640, vec![0x12, 0x34]),
            (6, 800, vec![0x12, 0x34]),
            (7, 960, vec![0x12, 0x34]),
        ] {
            pkt_tx
                .send(RtpPacket::new(RtpHeader::new(18, seq, ts, 0x11), payload))
                .await
                .unwrap();
        }

        let mut active_frames = Vec::new();
        let mut trailing_silence = 0usize;
        for _ in 0..12 {
            tokio::time::advance(Duration::from_millis(DEFAULT_PTIME_MS)).await;
            if let Some(frame) = rx.recv().await {
                if frame.silence {
                    if !active_frames.is_empty() {
                        trailing_silence += 1;
                    }
                } else {
                    assert_eq!(
                        trailing_silence, 0,
                        "standalone G.729B SIDs must not emit non-silent CNG frames after DTX"
                    );
                    active_frames.push(frame.frame.samples);
                }
            }
        }

        // Expect 3 active frames: speech1 (20 ms), speech2 (20 ms), and flushed
        // 12 B tail (10 ms speech + 10 ms zero-pad).
        assert_eq!(active_frames.len(), 3, "12 B speech tail must flush without CNG extras");
        assert!(
            active_frames[2][..80].iter().any(|s| s.unsigned_abs() > 100),
            "flushed 12 B tail must carry the 10 ms speech"
        );
        assert!(
            active_frames[2][80..].iter().all(|&s| s == 0),
            "second half of flushed 12 B tail must be digital silence"
        );
        assert!(trailing_silence >= 4, "DTX ticks must emit silence frames");

        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn decode_task_opus_dtx_emits_silence_without_plc() {
        let (frame_tx, mut rx) = mpsc::channel::<PcmFrame>(64);
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let cancel = CancellationToken::new();
        let codec = CodecType::Opus;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            frame_tx,
            LegId::from("opus-dtx"),
            codec,
            111,
            sr,
            pf,
            cancel.clone(),
            Arc::new(IngressCounters::default()),
        ));

        let mut enc = audio_codec::create_encoder(CodecType::Opus);
        let sine: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 10_000.0) as i16)
            .collect();
        let speech1 = enc.encode(&sine);
        let speech2 = enc.encode(&sine);
        let speech3 = enc.encode(&sine);
        let flipped_dtx = vec![(speech3[0] & !3) ^ 0x04];

        // 3 speech frames + 3 Opus DTX markers (1 B flipped-stereo TOC, 2 B code-2, 2 B code-3).
        for (seq, ts, payload) in [
            (1u16, 0u32, speech1),
            (2, 960, speech2),
            (3, 1920, speech3),
            (4, 2880, flipped_dtx),
            (5, 3840, vec![0xFA, 0]),
            (6, 4800, vec![0xFB, 2]),
        ] {
            pkt_tx
                .send(RtpPacket::new(RtpHeader::new(111, seq, ts, 0x22), payload))
                .await
                .unwrap();
        }

        let mut active_before_dtx = 0usize;
        let mut silence_during_dtx = 0usize;
        for _ in 0..12 {
            tokio::time::advance(Duration::from_millis(DEFAULT_PTIME_MS)).await;
            if let Some(frame) = rx.recv().await {
                if frame.silence {
                    if active_before_dtx > 0 {
                        silence_during_dtx += 1;
                    }
                } else {
                    assert_eq!(
                        silence_during_dtx, 0,
                        "Opus DTX markers must never emit non-silent PLC frames"
                    );
                    active_before_dtx += 1;
                }
            }
        }
        assert_eq!(active_before_dtx, 3, "exact 3 active Opus speech frames expected");
        assert!(silence_during_dtx >= 3, "Opus DTX must emit silence frames");

        // Resumed speech after DTX decodes cleanly.
        for (seq, ts) in [(7u16, 19_200u32), (8, 20_160), (9, 21_120)] {
            pkt_tx
                .send(RtpPacket::new(
                    RtpHeader::new(111, seq, ts, 0x22),
                    enc.encode(&sine),
                ))
                .await
                .unwrap();
        }
        let mut resumed_speech = false;
        for _ in 0..8 {
            tokio::time::advance(Duration::from_millis(DEFAULT_PTIME_MS)).await;
            if let Some(frame) = rx.recv().await
                && !frame.silence
                && frame.frame.samples.iter().any(|s| s.unsigned_abs() > 100)
            {
                resumed_speech = true;
                break;
            }
        }
        assert!(resumed_speech, "resumed Opus speech must decode after DTX");

        cancel.cancel();
    }
}
