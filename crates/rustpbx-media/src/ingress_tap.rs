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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::broadcast;

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
                if !self.paused.load(Ordering::Acquire) {
                    if let Some(rec) = self.recorder.read().as_ref() {
                        rec.write_dtmf(event);
                    }
                }
            }
            return;
        }

        // Audio sample → recorder (skip entirely when paused).
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
    use std::sync::atomic::AtomicUsize;

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
        assert!(rx.try_recv().is_err(), "duplicate DTMF event must be deduped");
    }

    /// A counting recorder backend used to verify the recording hook fires.
    struct CountingRecorder {
        samples: AtomicUsize,
        dtmfs: AtomicUsize,
    }

    impl MediaRecorder for CountingRecorder {
        fn write_sample(&self, _d: PacketDirection, _p: &RtpPacket) {
            self.samples.fetch_add(1, Ordering::Relaxed);
        }
        fn write_dtmf(&self, _e: DtmfEvent) {
            self.dtmfs.fetch_add(1, Ordering::Relaxed);
        }
        fn set_paused(&self, _paused: bool) {}
        fn finalize(&self) {}
    }

    #[test]
    fn recorder_receives_audio_and_dtmf() {
        let tap = IngressTap::new(8);
        tap.set_dtmf_payload_types(vec![101]);
        let rec = Arc::new(CountingRecorder {
            samples: AtomicUsize::new(0),
            dtmfs: AtomicUsize::new(0),
        });
        tap.set_recorder(Some(rec.clone()));

        // 3 audio packets → 3 sample writes.
        for seq in 1..=3u16 {
            let p = make_packet(0, seq, 160, 1, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        // 1 DTMF packet → 1 dtmf write.
        let dtmf = make_packet(101, 1, 0, 1, vec![1u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&dtmf, test_addr());

        assert_eq!(rec.samples.load(Ordering::Relaxed), 3);
        assert_eq!(rec.dtmfs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn paused_stops_recording_but_stats_advance() {
        let tap = IngressTap::new(8);
        let rec = Arc::new(CountingRecorder {
            samples: AtomicUsize::new(0),
            dtmfs: AtomicUsize::new(0),
        });
        tap.set_recorder(Some(rec.clone()));
        tap.set_paused(true);

        for seq in 1..=5u16 {
            let p = make_packet(0, seq, 160, 1, vec![1u8; 160]);
            tap.on_egress(&p, test_addr());
        }
        // Stats still advance (RTP-timeout detection relies on this).
        assert_eq!(tap.stats().egress_packets, 5);
        // Recorder not called while paused.
        assert_eq!(rec.samples.load(Ordering::Relaxed), 0);

        tap.set_paused(false);
        let p = make_packet(0, 9, 160, 1, vec![1u8; 160]);
        tap.on_egress(&p, test_addr());
        assert_eq!(rec.samples.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn no_recorder_no_panic() {
        let tap = IngressTap::new(8);
        let p = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&p, test_addr()); // must not panic
        tap.finalize_recorder(); // no-op, must not panic
        assert_eq!(tap.ingress_packet_count(), 1);
    }
}
