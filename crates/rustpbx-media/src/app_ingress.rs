//! MCU external-app mixer ingress: aggregates each leg's incoming audio as
//! per-leg PCM streams for an external application to mix.
//!
//! When [`crate::media_bridge::MediaBridge::mcu_mode`] is entered, an
//! [`AppIngressAggregator`] is attached to the participating legs. Each leg
//! gets a dedicated decode task that:
//!  1. observes every inbound plaintext RTP packet (via its own
//!     [`RtpObserver`], independent of the recording tap),
//!  2. decodes it to PCM (codec from the negotiated profile),
//!  3. emits one PCM frame per ptime tick to a shared broadcast bus.
//!
//! Muted / held legs emit silence frames so the app mixer always sees a
//! continuous frame cadence per leg. The app subscribes via
//! [`AppIngressAggregator::subscribe_pcm`] and does its own mixing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::Result;
use audio_codec::{CodecType, create_decoder};
use parking_lot::Mutex;
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::leg_id::LegId;
use crate::negotiate::NegotiatedLegProfile;

const DEFAULT_PTIME_MS: u64 = 20;

/// Per-leg ingest state.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestState {
    Active = 0,
    Muted = 1,
    Hold = 2,
}

impl IngestState {
    fn from_atomic(v: u8) -> Self {
        match v {
            1 => IngestState::Muted,
            2 => IngestState::Hold,
            _ => IngestState::Active,
        }
    }
}

/// One PCM frame tagged with its origin leg.
#[derive(Debug, Clone)]
pub struct LegPcmFrame {
    pub leg: LegId,
    pub sample_rate: u32,
    pub samples: Vec<i16>,
    /// `true` when this frame is silence (muted/held/no-data). Lets the app
    /// mixer skip decode work or mark gaps.
    pub silence: bool,
}

/// Aggregates per-leg PCM for an external app mixer.
pub struct AppIngressAggregator {
    pcm_bus: broadcast::Sender<LegPcmFrame>,
    legs: Mutex<HashMap<LegId, AggLeg>>,
}

struct AggLeg {
    state: Arc<AtomicU8>,
    cancel: CancellationToken,
}

impl AppIngressAggregator {
    pub fn new(bus_capacity: usize) -> Arc<Self> {
        let (pcm_bus, _) = broadcast::channel(bus_capacity.max(1));
        Arc::new(Self {
            pcm_bus,
            legs: Mutex::new(HashMap::new()),
        })
    }

    /// Subscribe to the per-leg PCM stream (each frame tagged with its LegId).
    pub fn subscribe_pcm(&self) -> broadcast::Receiver<LegPcmFrame> {
        self.pcm_bus.subscribe()
    }

    /// Attach a leg: installs a packet-forwarding observer on the leg's PC and
    /// spawns the decode task. Requires the leg's negotiated audio profile.
    /// The `parent_token` is the CancellationToken that this decode task
    /// should be a child of (e.g. from `MediaBridge::root_cancel` or
    /// `PcmObserver`). When the parent cancels, the decode task stops.
    pub fn attach_leg(
        self: &Arc<Self>,
        leg_id: LegId,
        pc: rustrtc::PeerConnection,
        profile: NegotiatedLegProfile,
        parent_token: tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let audio = profile
            .audio
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no negotiated audio codec for leg {leg_id}"))?;
        let codec = audio.codec;
        let sample_rate = audio.codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(256);
        let state = Arc::new(AtomicU8::new(IngestState::Active as u8));
        let cancel = parent_token.child_token();

        // Install a forwarding observer on the PC (fires on every ingress
        // plaintext packet, including relay fast-path).
        pc.add_observer(Arc::new(PacketForwarder { tx: pkt_tx }));

        let bus = self.pcm_bus.clone();
        let state_task = state.clone();
        let leg_id_task = leg_id.clone();
        let cancel_task = cancel.clone();
        tokio::spawn(decode_task(
            pkt_rx,
            bus,
            leg_id_task,
            codec,
            sample_rate,
            pcm_per_frame,
            state_task,
            cancel_task,
        ));

        self.legs.lock().insert(
            leg_id,
            AggLeg {
                state,
                cancel,
            },
        );
        Ok(())
    }

    /// Detach a leg (cancels its decode task).
    pub fn detach_leg(&self, leg_id: &LegId) {
        if let Some(agg) = self.legs.lock().remove(leg_id) {
            agg.cancel.cancel();
        }
    }

    pub fn set_state(&self, leg_id: &LegId, state: IngestState) {
        if let Some(agg) = self.legs.lock().get(leg_id) {
            agg.state.store(state as u8, Ordering::Release);
        }
    }

    pub fn set_muted(&self, leg_id: &LegId, muted: bool) {
        self.set_state(leg_id, if muted { IngestState::Muted } else { IngestState::Active });
    }

    pub fn set_hold(&self, leg_id: &LegId, hold: bool) {
        self.set_state(leg_id, if hold { IngestState::Hold } else { IngestState::Active });
    }

    /// Snapshot of currently-attached leg ids.
    pub fn legs_snapshot(&self) -> Vec<LegId> {
        self.legs.lock().keys().cloned().collect()
    }
}

/// Forwards ingress RTP packets into a channel for the decode task.
struct PacketForwarder {
    tx: mpsc::Sender<RtpPacket>,
}

impl RtpObserver for PacketForwarder {
    fn on_ingress(&self, packet: &RtpPacket, _src_addr: std::net::SocketAddr) {
        let _ = self.tx.try_send(packet.clone());
    }
    // Egress not needed for ingress aggregation.
}

/// Decode-task: drains packets, decodes to PCM, emits one PCM frame per ptime.
async fn decode_task(
    mut pkt_rx: mpsc::Receiver<RtpPacket>,
    bus: broadcast::Sender<LegPcmFrame>,
    leg: LegId,
    codec: CodecType,
    sample_rate: u32,
    pcm_per_frame: usize,
    state: Arc<AtomicU8>,
    cancel: CancellationToken,
) {
    let mut decoder = create_decoder(codec);
    let mut buffer: Vec<i16> = Vec::with_capacity(pcm_per_frame * 4);
    let mut interval = tokio::time::interval(Duration::from_millis(DEFAULT_PTIME_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let silence_frame = || {
        let v = vec![0i16; pcm_per_frame];
        v
    };

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            pkt = pkt_rx.recv() => match pkt {
                Some(p) => {
                    // Telephone-event payloads would decode to garbage; skip
                    // non-audio by length heuristics is unreliable, so we rely
                    // on the negotiated codec: only audio PTs reach here in
                    // practice (DTMF is filtered upstream).
                    let pcm = decoder.decode(&p.payload);
                    buffer.extend_from_slice(&pcm);
                }
                None => break,
            },
            _ = interval.tick() => {
                let st = IngestState::from_atomic(state.load(Ordering::Acquire));
                let (samples, silence) = match st {
                    IngestState::Active => {
                        if buffer.len() >= pcm_per_frame {
                            let frame: Vec<i16> = buffer.drain(..pcm_per_frame).collect();
                            (frame, false)
                        } else {
                            // Not enough decoded PCM yet → pad with silence to
                            // keep cadence (buffer continues accumulating).
                            (silence_frame(), true)
                        }
                    }
                    _ => {
                        // Muted / Hold: drain any buffered audio and emit silence.
                        buffer.clear();
                        (silence_frame(), true)
                    }
                };
                let _ = bus.send(LegPcmFrame {
                    leg: leg.clone(),
                    sample_rate,
                    samples,
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

    /// Drive the decode task directly (no PeerConnection) and verify it emits
    /// silence frames when starved, and PCM frames when fed.
    #[tokio::test]
    async fn decode_task_emits_silence_then_pcm() {
        let (pcm_bus, _) = broadcast::channel(64);
        let mut rx = pcm_bus.subscribe();
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let state = Arc::new(AtomicU8::new(IngestState::Active as u8));
        let cancel = CancellationToken::new();

        // PCMU @ 8kHz, 20ms → 160 samples/frame.
        let codec = CodecType::PCMU;
        let sample_rate = codec.samplerate();
        let pcm_per_frame = (sample_rate as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(
            pkt_rx,
            pcm_bus,
            LegId::from("a"),
            codec,
            sample_rate,
            pcm_per_frame,
            state,
            cancel.clone(),
        ));

        // First tick(s) with no data → silence frames.
        let f1 = rx.recv().await.unwrap();
        assert_eq!(f1.leg, LegId::from("a"));
        assert!(f1.silence, "starved task must emit silence");
        assert_eq!(f1.samples.len(), pcm_per_frame);

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
                Ok(Ok(f)) => {
                    if !f.silence {
                        saw_pcm = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_pcm, "must emit at least one non-silence PCM frame after feeding data");

        cancel.cancel();
    }

    #[tokio::test]
    async fn muted_state_emits_only_silence() {
        let (pcm_bus, _) = broadcast::channel(64);
        let mut rx = pcm_bus.subscribe();
        let (pkt_tx, pkt_rx) = mpsc::channel::<RtpPacket>(64);
        let state = Arc::new(AtomicU8::new(IngestState::Muted as u8));
        let cancel = CancellationToken::new();
        let codec = CodecType::PCMU;
        let sr = codec.samplerate();
        let pf = (sr as u64 * DEFAULT_PTIME_MS / 1000) as usize;

        tokio::spawn(decode_task(pkt_rx, pcm_bus, LegId::from("m"), codec, sr, pf, state, cancel.clone()));
        // Feed data while muted.
        for i in 0..5 {
            let _ = pkt_tx.try_send(RtpPacket::new(RtpHeader::new(0, i, i as u32 * 160, 1), vec![0xFFu8; 160]));
        }
        for _ in 0..3 {
            let f = rx.recv().await.unwrap();
            assert!(f.silence, "muted leg must emit only silence");
        }
        cancel.cancel();
    }

    #[test]
    fn ingest_state_roundtrip() {
        assert_eq!(IngestState::from_atomic(0), IngestState::Active);
        assert_eq!(IngestState::from_atomic(1), IngestState::Muted);
        assert_eq!(IngestState::from_atomic(2), IngestState::Hold);
        assert_eq!(IngestState::from_atomic(99), IngestState::Active);
    }
}
