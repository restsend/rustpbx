//! Single-leg decoded PCM ingress.
//!
//! [`LegPcmStream`] attaches a dedicated decode task to one leg's incoming
//! RTP: a packet-forwarding observer feeds every ingress packet into a
//! channel, and the decode task emits one PCM frame per ptime tick. Silence
//! frames keep the cadence when the leg is starved, so downstream consumers
//! (conference / supervisor mixer input, live transcription) always see a
//! continuous frame stream.
//!
//! Dropping the stream cancels the decode task and deactivates the
//! forwarding observer, so consumers that start/stop the tap repeatedly
//! (live transcription, supervisor) never accumulate observers on the
//! PeerConnection.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Decode-buffer ceiling. A producer clock running fast (or a slow consumer)
/// otherwise grows playback latency without bound over long calls.
const MAX_DECODE_BUFFER_MS: u64 = 400;
/// Jitter window for the reorder buffer, in ticks (~40 ms). While inside it
/// we wait for a late packet rather than skipping; past it we jump ahead so
/// a loss costs silence instead of decoder-state corruption.
const MAX_REORDER_WAIT_TICKS: u8 = 2;
/// Hard cap on packets held for reordering.
const MAX_PENDING_PACKETS: usize = 64;

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
        let audio_payload_type = audio.payload_type;
        let sample_rate = audio.codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(256);
        let (frame_tx, frame_rx) = mpsc::channel::<PcmFrame>(16);
        let cancel = parent_token.child_token();
        let observer_active = Arc::new(AtomicBool::new(true));

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
            audio_payload_type,
            sample_rate,
            pcm_per_frame,
            cancel.clone(),
        ));

        Ok(Self {
            rx: frame_rx,
            sample_rate,
            cancel,
            observer_active,
        })
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

/// Per-leg RTP reorder buffer feeding the stateful audio decoder.
///
/// The previous path decoded strictly in arrival order: a reordered or
/// duplicated packet smeared the decoder's predictor state (G.722 ADPCM /
/// Opus are both stateful) across several frames — audible as bursts that
/// the conference then mixes into every participant. This buffer drops
/// duplicates and stale packets, holds a small jitter window for genuine
/// reordering, and skips ahead past losses.
struct PacketReorderer {
    pending: BTreeMap<u16, RtpPacket>,
    /// Sequence number expected next (anchored to the first packet seen).
    next_seq: u16,
    /// `next_seq` is anchored to a real packet (first accepted push).
    primed: bool,
    /// Consecutive ticks where `next_seq` was missing while newer packets
    /// waited; past [`MAX_REORDER_WAIT_TICKS`] we skip over the loss.
    missing_streak: u8,
    /// Duplicates and packets far behind the decode point (defensive stats).
    stale_dropped: u64,
    skipped_losses: u64,
}

impl PacketReorderer {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next_seq: 0,
            primed: false,
            missing_streak: 0,
            stale_dropped: 0,
            skipped_losses: 0,
        }
    }

    /// Buffer a packet for ordered decode. Duplicates and stale packets are
    /// dropped here. `audio_payload_type` is the negotiated voice PT —
    /// anything else (telephone-event / video / RTCP mux) must never reach
    /// the stateful decoder.
    fn push(&mut self, packet: RtpPacket, audio_payload_type: u8) {
        if packet.header.payload_type != audio_payload_type {
            // RFC 4733 DTMF and other non-voice payloads: not audio, and
            // decoding them as audio corrupts the decoder for frames after.
            return;
        }
        let seq = packet.header.sequence_number;
        if !self.primed {
            // First packet anchors the decode point: accept unconditionally.
            self.next_seq = seq;
            self.primed = true;
            self.pending.insert(seq, packet);
            return;
        }
        if self.pending.contains_key(&seq) {
            return; // duplicate
        }
        if self.pending.len() >= MAX_PENDING_PACKETS {
            return;
        }
        let forward = seq.wrapping_sub(self.next_seq);
        if forward >= 32768 {
            // More than half a sequence ring behind the decode point: stale.
            self.stale_dropped += 1;
            return;
        }
        self.pending.insert(seq, packet);
    }

    /// Pop the next in-order packet, if one is available.
    ///
    /// Losses inside the jitter window hold decode for up to
    /// [`MAX_REORDER_WAIT_TICKS`] ticks; beyond that the loss is skipped so
    /// the decoder state stays valid (the missing audio becomes silence).
    fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(packet) = self.pending.remove(&self.next_seq) {
            self.next_seq = self.next_seq.wrapping_add(1);
            self.missing_streak = 0;
            return Some(packet);
        }
        if self.pending.is_empty() {
            self.missing_streak = 0;
            return None;
        }
        // The wanted seq is absent but newer packets wait: first wait out
        // the jitter window, then skip ahead past the loss.
        self.missing_streak += 1;
        if self.missing_streak <= MAX_REORDER_WAIT_TICKS {
            return None;
        }
        let seq = *self.pending.keys().next().expect("non-empty map");
        let packet = self.pending.remove(&seq).expect("key from same map");
        self.skipped_losses += 1;
        self.missing_streak = 0;
        self.next_seq = seq.wrapping_add(1);
        Some(packet)
    }
}

/// Decode-task: buffers ingress RTP in order, decodes to PCM, emits one PCM
/// frame per ptime tick. Silence frames keep the cadence while starved.
///
/// When the consumer is slow the frame channel (capacity 16) drops frames
/// (`try_send`) rather than letting the decoder fall behind real time —
/// matching the previous broadcast lagged-drop behavior.
async fn decode_task(
    mut pkt_rx: mpsc::Receiver<RtpPacket>,
    frame_tx: mpsc::Sender<PcmFrame>,
    leg: LegId,
    codec: CodecType,
    audio_payload_type: u8,
    sample_rate: u32,
    pcm_per_frame: usize,
    cancel: CancellationToken,
) {
    let mut decoder = create_decoder(codec);
    let mut reorder = PacketReorderer::new();
    let mut buffer: Vec<i16> = Vec::with_capacity(pcm_per_frame * 4);
    let max_buffer = pcm_per_frame * (MAX_DECODE_BUFFER_MS / DEFAULT_PTIME_MS).max(1) as usize;
    let mut overflow_reports: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_millis(DEFAULT_PTIME_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            pkt = pkt_rx.recv() => match pkt {
                Some(p) => reorder.push(p, audio_payload_type),
                None => break,
            },
            _ = interval.tick() => {
                // Drain the reorder buffer in sequence order. Steady state is
                // one packet per tick; post-reorder bursts decode up to the
                // drain cap so the pipeline never falls behind real time.
                while let Some(pkt) = reorder.pop() {
                    let pcm = decoder.decode(&pkt.payload);
                    buffer.extend_from_slice(&pcm);
                    if buffer.len() >= max_buffer * 2 {
                        break;
                    }
                }
                // A producer clock running fast must not grow latency without
                // bound: trim the oldest PCM past the ceiling.
                if buffer.len() > max_buffer {
                    let excess = buffer.len() - max_buffer;
                    buffer.drain(..excess);
                    overflow_reports += 1;
                    if overflow_reports == 1 || overflow_reports % 100 == 0 {
                        debug!(
                            leg = %leg, dropped_samples = excess,
                            reports = overflow_reports,
                            "app-ingress decode buffer over ceiling, trimmed oldest PCM"
                        );
                    }
                }
                let (samples, silence) = if buffer.len() >= pcm_per_frame {
                    let frame: Vec<i16> = buffer.drain(..pcm_per_frame).collect();
                    (frame, false)
                } else {
                    // Not enough decoded PCM yet → pad with silence to keep
                    // cadence (buffer continues accumulating).
                    (vec![0i16; pcm_per_frame], true)
                };
                let _ = frame_tx.try_send(PcmFrame {
                    frame: AudioFrame::new(samples, sample_rate),
                    silence,
                });
            }
        }
    }
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

    /// Reordered arrival must decode in sequence order, never in arrival
    /// order (a stateful decoder fed out of order smears its predictor
    /// across frames — the "burst of noise" symptom in conferences).
    #[test]
    fn reorder_buffer_decodes_in_sequence_order() {
        let pkt = |pt: u8, seq: u16| {
            RtpPacket::new(RtpHeader::new(pt, seq, (seq as u32) * 160, 1), vec![0xAAu8; 8])
        };
        let mut r = PacketReorderer::new();

        r.push(pkt(0, 102), 0);
        r.push(pkt(0, 101), 0);
        r.push(pkt(0, 100), 0);

        for expected in [100u16, 101, 102] {
            assert_eq!(r.pop().expect("packet available").header.sequence_number, expected);
        }
        assert!(r.pop().is_none());
    }

    /// Duplicates and stale packets must never reach the decoder, and a loss
    /// costs at most the jitter window before decode skips ahead.
    #[test]
    fn reorder_buffer_drops_duplicates_and_skips_losses() {
        let pkt = |pt: u8, seq: u16| {
            RtpPacket::new(RtpHeader::new(pt, seq, (seq as u32) * 160, 1), vec![0xAAu8; 8])
        };
        let mut r = PacketReorderer::new();

        // Duplicates (same seq twice) collapse to one.
        r.push(pkt(0, 200), 0);
        r.push(pkt(0, 200), 0);
        assert_eq!(r.pop().unwrap().header.sequence_number, 200);
        assert!(r.pop().is_none());

        // Stale packet (half the sequence ring behind the decode point).
        r.push(pkt(0, 201), 0);
        r.push(pkt(0, 201u16.wrapping_sub(20000)), 0);
        assert_eq!(
            r.pop().expect("in-order packet decodes despite stale neighbor").header.sequence_number,
            201
        );
        // Loss: 202 never arrives, 203 does — wait the jitter window, skip.
        r.push(pkt(0, 203), 0);
        assert!(r.pop().is_none(), "loss must hold decode inside the window");
        assert!(r.pop().is_none(), "still inside the window (tick 2)");
        assert_eq!(
            r.pop().expect("skip past loss after the window").header.sequence_number,
            203
        );
        assert_eq!(r.skipped_losses, 1);

        // Non-voice payload types (RFC 4733 DTMF PT 101) never decode.
        r.push(pkt(101, 204), 0);
        assert!(r.pop().is_none(), "telephone-event packets must be filtered");
    }
}
