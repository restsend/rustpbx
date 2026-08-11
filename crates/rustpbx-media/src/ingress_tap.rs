//! Plaintext transport observer (ingress + egress) for a single leg.
//!
//! [`IngressTap`] implements rustrtc's [`RtpObserver`] trait and is installed
//! via `PeerConnection::add_observer` / `RtpTransport::add_observer`. Because
//! `RtpObserver` fires at the plaintext boundary (post-SRTP-unprotect on
//! ingress, pre-SRTP-protect on egress) and covers the relay fast-path too,
//! a single tap observes BOTH directions of a leg in ALL forwarding modes —
//! exactly what per-leg bidirectional recording needs.
//!
//! The hot path is lock-free: 2 atomics for stats, 1 PT compare for DTMF,
//! 1 `try_send` for recording. The DTMF detector and DTMF payload-type list
//! live behind a `parking_lot::Mutex` acquired only on the (rare) telephone-
//! event payload type, never on audio packets.
//!
//! This replaces the old `RecorderTap` (which implemented the post-bridge
//! `RtpReceiverInterceptor` and missed relay packets). For NACK / RTCP
//! feedback the existing `RtpReceiverInterceptor` is unaffected.
//!
//! DTMF telephone-event packets are detected (for the DTMF event bus) but
//! still forwarded to the recorder's `write_sample` as raw RTP — this lets
//! SipflowRecorder store them for WAV DTMF-tone synthesis and FileRecorder
//! decode them via `Recorder::write_dtmf_payload`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::broadcast;
use tracing::trace;

use crate::dtmf::DtmfDetector;

/// Which direction of a leg's transport a packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketDirection {
    /// Inbound: received from the remote peer (post-SRTP-unprotect).
    Ingress,
    /// Outbound: sent to the remote peer (pre-SRTP-protect / pre-relay-push).
    Egress,
}

/// A DTMF digit detected on a leg, tagged with its direction and the RTP
/// timestamp at which it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtmfEvent {
    pub direction: PacketDirection,
    pub digit: char,
    pub timestamp: u32,
}

/// Recording / capture backend — the implementation owns its own threading,
/// files, sipflow connection, and codec policy. The tap calls these methods
/// on the hot path, so implementations MUST be non-blocking (queue internally
/// and drain from a background task).
///
/// This is the abstraction the host passes in: `MediaBridge::set_recorder`.
/// `FileRecorder` (WAV) and `SipflowRecorder` are the typical backends; a
/// `TeeRecorder` fans out to several at once.
pub trait MediaRecorder: Send + Sync {
    /// A plaintext RTP sample traversed the leg. `packet` carries its
    /// payload type in `packet.header.payload_type`; the backend resolves the
    /// codec from its negotiated profiles.
    fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket);

    /// A DTMF telephone-event was detected and deduplicated.
    fn write_dtmf(&self, event: DtmfEvent);

    /// Pause / resume capture. When paused the tap short-circuits before
    /// calling `write_sample` / `write_dtmf`, so this is informational.
    fn set_paused(&self, paused: bool);

    /// Flush and close all outputs. Called on stop and on Drop by the host.
    fn finalize(&self);
}

/// Snapshot of the per-direction counters.
#[derive(Debug, Clone, Default)]
pub struct TapStats {
    pub ingress_packets: u64,
    pub ingress_bytes: u64,
    pub egress_packets: u64,
    pub egress_bytes: u64,
}

/// Plaintext observer for one leg: stats + DTMF detection + recording.
///
/// Install via `PeerConnection::add_observer(Arc::clone(&tap))`. The same
/// `Arc<IngressTap>` may be shared by multiple transports of a leg (primary
/// + muxed media) — the `add_observer` helper already does this.
pub struct IngressTap {
    // ── stats (lock-free) ────────────────────────────────────────────────
    ingress_packets: AtomicU64,
    ingress_bytes: AtomicU64,
    egress_packets: AtomicU64,
    egress_bytes: AtomicU64,
    /// Ingress SSRC → payload types seen (post-SRTP-unprotect, pre-rewrite).
    /// Lets the RTCP relay map a receiver's PLI/NACK (targeting the relayed
    /// SSRC) back onto the peer's real sender SSRC so the peer browser's
    /// encoder actually responds. Written on the hot path (per new (ssrc,pt)),
    /// read occasionally by the RTCP relay — a sharded concurrent map keeps
    /// both non-blocking (no try_lock-skip semantics).
    ingress_ssrc_pts: DashMap<u32, std::collections::HashSet<u8>>,

    // ── DTMF ────────────────────────────────────────────────────────────
    /// Telephone-event payload types for this leg (e.g. `[101]`). Only
    /// packets whose PT matches are passed to the detector, so audio packets
    /// pay zero DTMF cost.
    dtmf_payload_types: Mutex<Vec<u8>>,
    dtmf_detector: Mutex<DtmfDetector>,
    dtmf_tx: broadcast::Sender<DtmfEvent>,

    // ── recording ───────────────────────────────────────────────────────
    /// Optional recorder backend. Read on every packet, written only when
    /// the host attaches/detaches — a `RwLock` (uncontended, ~5ns read) is
    /// cheaper than the trait-object complications of `ArcSwapOption<dyn>`.
    recorder: RwLock<Option<Arc<dyn MediaRecorder>>>,
    paused: AtomicBool,
}

impl IngressTap {
    /// Create a new tap. `dtmf_bus_capacity` bounds the DTMF broadcast
    /// channel (subscribers that lag are dropped, never blocking the tap).
    pub fn new(dtmf_bus_capacity: usize) -> Arc<Self> {
        let (dtmf_tx, _) = broadcast::channel(dtmf_bus_capacity.max(1));
        Arc::new(Self {
            ingress_packets: AtomicU64::new(0),
            ingress_bytes: AtomicU64::new(0),
            egress_packets: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            ingress_ssrc_pts: DashMap::new(),
            dtmf_payload_types: Mutex::new(Vec::new()),
            dtmf_detector: Mutex::new(DtmfDetector::default()),
            dtmf_tx,
            recorder: RwLock::new(None),
            paused: AtomicBool::new(false),
        })
    }

    /// Set the telephone-event payload type(s) negotiated for this leg.
    /// Called once after SDP negotiation (e.g. from the negotiated leg profile).
    pub fn set_dtmf_payload_types(&self, pts: Vec<u8>) {
        *self.dtmf_payload_types.lock() = pts;
    }

    /// Subscribe to deduplicated DTMF events (both directions).
    pub fn subscribe_dtmf(&self) -> broadcast::Receiver<DtmfEvent> {
        self.dtmf_tx.subscribe()
    }

    /// Attach a recording / capture backend (replaces any previous one).
    pub fn set_recorder(&self, recorder: Option<Arc<dyn MediaRecorder>>) {
        *self.recorder.write() = recorder;
    }

    /// Pause / resume capture. When paused, `write_sample` / `write_dtmf` are
    /// not called and stats still advance (so RTP-timeout detection works).
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        if let Some(rec) = self.recorder.read().as_ref() {
            rec.set_paused(paused);
        }
    }

    /// Snapshot the per-direction counters.
    pub fn stats(&self) -> TapStats {
        TapStats {
            ingress_packets: self.ingress_packets.load(Ordering::Relaxed),
            ingress_bytes: self.ingress_bytes.load(Ordering::Relaxed),
            egress_packets: self.egress_packets.load(Ordering::Relaxed),
            egress_bytes: self.egress_bytes.load(Ordering::Relaxed),
        }
    }

    /// Total inbound packets (convenience for RTP-timeout checks).
    pub fn ingress_packet_count(&self) -> u64 {
        self.ingress_packets.load(Ordering::Relaxed)
    }

    /// The most-recently-seen ingress SSRC that carries any of `pts`. Used by
    /// the RTCP relay to rewrite a receiver's PLI/NACK (whose media_ssrc is
    /// the *relayed* SSRC) back onto the peer browser's real sender SSRC, so
    /// the peer's encoder responds. `None` while the peer hasn't sent that
    /// media type yet.
    pub fn ingress_ssrc_for_pts(&self, pts: &[u8]) -> Option<u32> {
        self.ingress_ssrc_pts
            .iter()
            .filter(|e| pts.iter().any(|p| e.value().contains(p)))
            .map(|e| *e.key())
            .max()
    }

    /// Finalize the recorder backend, if any.
    pub fn finalize_recorder(&self) {
        if let Some(rec) = self.recorder.read().as_ref() {
            rec.finalize();
        }
    }

    /// Shared processing for both directions: stats + DTMF + record.
    #[inline]
    fn process(&self, direction: PacketDirection, packet: &RtpPacket) {
        let payload_len = packet.payload.len() as u64;
        match direction {
            PacketDirection::Ingress => {
                self.ingress_packets.fetch_add(1, Ordering::Relaxed);
                self.ingress_bytes.fetch_add(payload_len, Ordering::Relaxed);
                // Remember which SSRC carries which payload type (needed by the
                // RTCP relay to rewrite PLI/NACK back onto the peer's sender
                // SSRC). DashMap: shard-locked, non-blocking; a tiny set per
                // SSRC, so an existing key just gets a PT inserted.
                self.ingress_ssrc_pts
                    .entry(packet.header.ssrc)
                    .or_default()
                    .insert(packet.header.payload_type);
            }
            PacketDirection::Egress => {
                self.egress_packets.fetch_add(1, Ordering::Relaxed);
                self.egress_bytes.fetch_add(payload_len, Ordering::Relaxed);
            }
        }

        // DTMF: only telephone-event payloads are inspected. The PT list is
        // behind a Mutex but is only locked to read the (tiny) Vec; for audio
        // packets the lock+iter is a few ns and uncontended.
        let pt = packet.header.payload_type;
        let is_dtmf_pt = {
            let pts = self.dtmf_payload_types.lock();
            pts.iter().any(|&p| p == pt)
        };
        if is_dtmf_pt {
            trace!(pt, len = payload_len, "tap: telephone-event packet");
            let digit = self
                .dtmf_detector
                .lock()
                .observe(&packet.payload, packet.header.timestamp);
            if let Some(digit) = digit {
                let event = DtmfEvent {
                    direction,
                    digit,
                    timestamp: packet.header.timestamp,
                };
                // Broadcast (lagged subscribers dropped, never blocks).
                let _ = self.dtmf_tx.send(event);
            }
            // Fall through to write_sample: the raw telephone-event RTP packet
            // must reach the recorder so it can be stored (SipflowRecorder →
            // wav_utils synthesizes the tone during export) or decoded
            // (FileRecorder → Recorder::write_sample detects DTMF by PT →
            // write_dtmf_payload). We do NOT call write_dtmf separately to
            // avoid double DTMF in the FileRecorder path.
        }

        // All packets (audio + telephone-event) → recorder (skip when paused).
        if self.paused.load(Ordering::Acquire) {
            return;
        }
        if let Some(rec) = self.recorder.read().as_ref() {
            rec.write_sample(direction, packet);
        }
    }
}

impl RtpObserver for IngressTap {
    fn on_ingress(&self, packet: &RtpPacket, _src_addr: std::net::SocketAddr) {
        self.process(PacketDirection::Ingress, packet);
    }

    fn on_egress(&self, packet: &RtpPacket, _dst_addr: std::net::SocketAddr) {
        self.process(PacketDirection::Egress, packet);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::net::SocketAddr;

    fn make_packet(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: Vec<u8>) -> RtpPacket {
        RtpPacket::new(RtpHeader::new(pt, seq, ts, ssrc), payload)
    }

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    #[test]
    fn stats_advance_on_ingress_and_egress() {
        let tap = IngressTap::new(8);
        for seq in 1..=3u16 {
            let p = make_packet(0, seq, 160, 1234, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        for seq in 1..=2u16 {
            let p = make_packet(0, seq, 160, 1234, vec![2u8; 160]);
            tap.on_egress(&p, test_addr());
        }
        let s = tap.stats();
        assert_eq!(s.ingress_packets, 3);
        assert_eq!(s.ingress_bytes, 3 * 160);
        assert_eq!(s.egress_packets, 2);
        assert_eq!(s.egress_bytes, 2 * 160);
    }

    /// The RTCP relay looks up the peer's real sender SSRC via the ingress
    /// tap's (ssrc → PT) map so it can rewrite a PLI/NACK's media_ssrc. This
    /// guards that lookup: audio and video SSRCs are tracked per payload type.
    #[test]
    fn ingress_ssrc_for_pts_resolves_peer_sender_ssrc() {
        let tap = IngressTap::new(8);
        // Bob's browser sends audio on SSRC 1001 (PT 111) and video on
        // SSRC 2002 (PT 96). Both ingress.
        tap.on_ingress(&make_packet(111, 1, 160, 1001, vec![1u8; 160]), test_addr());
        tap.on_ingress(&make_packet(96, 1, 3000, 2002, vec![1u8; 200]), test_addr());

        // Video lookup must return bob's video SSRC, not the audio one.
        assert_eq!(tap.ingress_ssrc_for_pts(&[96]), Some(2002));
        assert_eq!(tap.ingress_ssrc_for_pts(&[96, 97]), Some(2002));
        // Audio lookup returns the audio SSRC.
        assert_eq!(tap.ingress_ssrc_for_pts(&[111]), Some(1001));
        // An SSRC that has only been seen on egress (relayed) must NOT resolve.
        let relayed = make_packet(96, 1, 3000, 9999, vec![1u8; 200]);
        tap.on_egress(&relayed, test_addr());
        assert_ne!(tap.ingress_ssrc_for_pts(&[96]), Some(9999));
        // Unknown PT → None.
        assert_eq!(tap.ingress_ssrc_for_pts(&[110]), None);
    }

    #[test]
    fn dtmf_detected_only_for_telephone_event_pt() {
        let tap = IngressTap::new(8);
        tap.set_dtmf_payload_types(vec![101]);
        let mut rx = tap.subscribe_dtmf();

        // Audio packet (PT 0) — no DTMF.
        let audio = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&audio, test_addr());
        assert!(rx.try_recv().is_err(), "audio PT must not raise DTMF");

        // DTMF event "1" = digit_code 0x01. Minimal 4-byte payload the
        // detector accepts: [code, flags, volume, duration...].
        let dtmf_payload = vec![1u8, 0x80, 10, 0xA0];
        let dtmf = make_packet(101, 1, 0, 1, dtmf_payload);
        tap.on_ingress(&dtmf, test_addr());
        let ev = rx.try_recv().expect("telephone-event PT must raise DTMF");
        assert_eq!(ev.digit, '1');
        assert_eq!(ev.direction, PacketDirection::Ingress);

        // Duplicate (same code+timestamp) — deduplicated, no second event.
        tap.on_ingress(&dtmf, test_addr());
        assert!(
            rx.try_recv().is_err(),
            "duplicate DTMF event must be deduped"
        );
    }

    use crate::test_utils::CountingRecorder;

    /// A counting recorder backend used to verify the recording hook fires.

    #[test]
    fn recorder_receives_audio_and_dtmf() {
        let tap = IngressTap::new(8);
        tap.set_dtmf_payload_types(vec![101]);
        let rec = Arc::new(CountingRecorder::new());
        tap.set_recorder(Some(rec.clone()));

        // 3 audio packets → 3 sample writes.
        for seq in 1..=3u16 {
            let p = make_packet(0, seq, 160, 1, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        // 1 DTMF packet → forwarded via write_sample (raw RTP for later tone
        // synthesis), NOT via write_dtmf. This ensures SipflowRecorder stores
        // the telephone-event packet for WAV DTMF synthesis.
        let dtmf = make_packet(101, 1, 0, 1, vec![1u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&dtmf, test_addr());

        assert_eq!(
            rec.samples(),
            4,
            "3 audio + 1 DTMF packet → 4 write_sample calls"
        );
        assert_eq!(
            rec.dtmfs.load(Ordering::Relaxed),
            0,
            "write_dtmf is no longer called; DTMF goes through write_sample"
        );
    }

    #[test]
    fn paused_stops_recording_but_stats_advance() {
        let tap = IngressTap::new(8);
        let rec = Arc::new(CountingRecorder::new());
        tap.set_recorder(Some(rec.clone()));
        tap.set_paused(true);

        for seq in 1..=5u16 {
            let p = make_packet(0, seq, 160, 1, vec![1u8; 160]);
            tap.on_egress(&p, test_addr());
        }
        // Stats still advance (RTP-timeout detection relies on this).
        assert_eq!(tap.stats().egress_packets, 5);
        // Recorder not called while paused.
        assert_eq!(rec.samples(), 0);

        tap.set_paused(false);
        let p = make_packet(0, 9, 160, 1, vec![1u8; 160]);
        tap.on_egress(&p, test_addr());
        assert_eq!(rec.samples(), 1);
    }

    #[test]
    fn no_recorder_no_panic() {
        let tap = IngressTap::new(8);
        let p = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&p, test_addr()); // must not panic
        tap.finalize_recorder(); // no-op, must not panic
        assert_eq!(tap.ingress_packet_count(), 1);
    }

    /// Malformed / random telephone-event payloads must never panic the DTMF
    /// detector or the recording path. Feeds 1000 pseudo-random payloads (varying
    /// length and contents) on the DTMF PT and on the audio PT.
    #[test]
    fn malformed_rtp_packets_do_not_panic() {
        let tap = IngressTap::new(64);
        tap.set_dtmf_payload_types(vec![101]);
        let mut rx = tap.subscribe_dtmf();
        let mut seed = 0x1234_5678u32;
        let mut next_u8 = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as u8
        };
        for i in 0..1000u32 {
            let pt = if i % 2 == 0 { 101 } else { 0 };
            let len = (next_u8() as usize) % 32;
            let payload: Vec<u8> = (0..len).map(|_| next_u8()).collect();
            let pkt = make_packet(
                pt,
                (i as u16).wrapping_add(1),
                i.wrapping_mul(160),
                1,
                payload,
            );
            tap.on_ingress(&pkt, test_addr());
            tap.on_egress(&pkt, test_addr());
            // The DTMF detector dedups; drain whatever fired to keep the
            // broadcast buffer from filling on long runs.
            while rx.try_recv().is_ok() {}
        }
        // Truncated RFC 4733 payloads (< 4 bytes) on the DTMF PT specifically.
        for len in 0..4usize {
            let payload: Vec<u8> = (0..len).map(|_| next_u8()).collect();
            let pkt = make_packet(101, 1, 0, 1, payload);
            tap.on_ingress(&pkt, test_addr());
        }
        // No panic reached == pass.
    }
}
