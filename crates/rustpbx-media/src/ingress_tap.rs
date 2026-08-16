//! Plaintext transport observer (ingress + egress) for a single leg.
//!
//! [`IngressTap`] implements rustrtc's [`RtpObserver`] trait and is installed
//! via `PeerConnection::add_observer` / `RtpTransport::add_observer`. Because
//! `RtpObserver` fires at the plaintext boundary (post-SRTP-unprotect on
//! ingress, pre-SRTP-protect on egress) and covers the relay fast-path too,
//! a single tap observes BOTH directions of a leg in ALL forwarding modes —
//! exactly what per-leg bidirectional recording needs.
//!
//! The recorder hot path is lock-free: an immutable sender performs one
//! non-blocking channel enqueue. Stats and steady-state SSRC/PT tracking use
//! atomics, and DTMF payload matching uses a lock-free bitmask. Only the
//! stateful DTMF detector has a `parking_lot::Mutex`, acquired for the rare
//! telephone-event packets rather than normal audio.
//!
//! This replaces the old `RecorderTap` (which implemented the post-bridge
//! `RtpReceiverInterceptor` and missed relay packets). For NACK / RTCP
//! feedback the existing `RtpReceiverInterceptor` is unaffected.
//!
//! Ingress DTMF telephone-event packets are detected for the DTMF event bus.
//! Raw telephone-event RTP in both directions is still forwarded to the
//! recording task — this lets the unified task store it in Sipflow and decode
//! it into the local WAV via `Recorder::write_dtmf_payload`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::broadcast;
use tracing::trace;

use crate::dtmf::DtmfDetector;
use crate::media_recorder::RecorderSender;

/// Which direction of a leg's transport a packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketDirection {
    /// Inbound: received from the remote peer (post-SRTP-unprotect).
    Ingress,
    /// Outbound: sent to the remote peer (pre-SRTP-protect / pre-relay-push).
    Egress,
}

/// A DTMF digit detected on a leg, tagged with its direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtmfEvent {
    pub direction: PacketDirection,
    pub digit: char,
}

/// Snapshot of the per-direction counters.
#[derive(Debug, Clone, Default)]
pub(crate) struct TapStats {
    pub ingress_packets: u64,
    pub egress_packets: u64,
}

/// Plaintext observer for one leg: stats + DTMF detection + recording.
///
/// Install via `PeerConnection::add_observer(Arc::clone(&tap))`. The same
/// `Arc<IngressTap>` may be shared by multiple transports of a leg (primary
/// + muxed media) — the `add_observer` helper already does this.
pub struct IngressTap {
    // ── stats (lock-free) ────────────────────────────────────────────────
    ingress_packets: AtomicU64,
    egress_packets: AtomicU64,
    /// Whether to keep populating `ingress_ssrc_pts` on the hot path. The map
    /// exists only to let the RTCP relay rewrite a WebRTC receiver's PLI/NACK
    /// back onto the peer's real sender SSRC; for plain RTP↔RTP bridges (no
    /// WebRTC anywhere) no feedback relay is needed, so the leg can skip the
    /// per-packet DashMap write entirely. Toggled by `MediaBridge::bridge`
    /// once both legs' transports are known.
    track_ingress_ssrc_pts: AtomicBool,
    /// Last-seen packed `(ssrc, pt)` on ingress. SSRC + payload type are fixed
    /// for the lifetime of a stream, so after the first packet of a given
    /// (ssrc, pt) pair this cache lets `process` skip the DashMap lookup/insert
    /// (previously a per-packet shard lock + SipHash + HashSet insert).
    last_ingress_ssrc_pt: AtomicU64,
    /// Ingress SSRC → payload types seen (post-SRTP-unprotect, pre-rewrite).
    /// Lets the RTCP relay map a receiver's PLI/NACK (targeting the relayed
    /// SSRC) back onto the peer's real sender SSRC so the peer browser's
    /// encoder actually responds. Written once per new (ssrc,pt) pair, read
    /// occasionally by the RTCP relay — a sharded concurrent map keeps both
    /// non-blocking (no try_lock-skip semantics).
    ingress_ssrc_pts: DashMap<u32, std::collections::HashSet<u8>>,

    // ── DTMF ────────────────────────────────────────────────────────────
    /// Telephone-event payload types as a 128-bit bitmask (PT 0..=127), split
    /// across two u64 words. Set once after SDP negotiation; read lock-free on
    /// every packet so audio packets pay zero lock cost.
    dtmf_pt_mask: [AtomicU64; 2],
    dtmf_detector: Mutex<DtmfDetector>,
    dtmf_tx: broadcast::Sender<DtmfEvent>,

    // ── recording ───────────────────────────────────────────────────────
    /// Immutable sender for the call-scoped recording task. The caller leg
    /// receives it when this tap is constructed; both RTP directions can use
    /// the sender concurrently without synchronization here.
    recorder_sender: Option<RecorderSender>,
}

impl IngressTap {
    /// Create a new tap. `dtmf_bus_capacity` bounds the DTMF broadcast
    /// channel (subscribers that lag are dropped, never blocking the tap).
    /// The optional recording sender is fixed for the tap's lifetime.
    pub fn new(dtmf_bus_capacity: usize, recorder_sender: Option<RecorderSender>) -> Arc<Self> {
        let (dtmf_tx, _) = broadcast::channel(dtmf_bus_capacity.max(1));
        Arc::new(Self {
            ingress_packets: AtomicU64::new(0),
            egress_packets: AtomicU64::new(0),
            track_ingress_ssrc_pts: AtomicBool::new(true),
            last_ingress_ssrc_pt: AtomicU64::new(u64::MAX),
            ingress_ssrc_pts: DashMap::new(),
            dtmf_pt_mask: [AtomicU64::new(0), AtomicU64::new(0)],
            dtmf_detector: Mutex::new(DtmfDetector::default()),
            dtmf_tx,
            recorder_sender,
        })
    }

    /// Enable/disable hot-path SSRC→PT tracking (see `track_ingress_ssrc_pts`).
    /// Call once the leg's peer transport is known; plain RTP↔RTP bridges can
    /// disable it to skip the per-packet DashMap work entirely.
    pub fn set_track_ingress_ssrc_pts(&self, track: bool) {
        self.track_ingress_ssrc_pts.store(track, Ordering::Relaxed);
    }

    /// Set the telephone-event payload type(s) negotiated for this leg.
    /// Called once after SDP negotiation (e.g. from the negotiated leg profile).
    pub fn set_dtmf_payload_types(&self, pts: Vec<u8>) {
        let mut lo = 0u64;
        let mut hi = 0u64;
        for p in pts {
            if p < 64 {
                lo |= 1u64 << p;
            } else {
                hi |= 1u64 << (p - 64);
            }
        }
        self.dtmf_pt_mask[0].store(lo, Ordering::Relaxed);
        self.dtmf_pt_mask[1].store(hi, Ordering::Relaxed);
    }

    /// Lock-free telephone-event check: is `pt` one of the negotiated DTMF
    /// payload types?
    #[inline]
    fn is_dtmf_payload_type(&self, pt: u8) -> bool {
        let word = (pt as usize) >> 6;
        let bit = 1u64 << (pt & 63);
        self.dtmf_pt_mask[word].load(Ordering::Relaxed) & bit != 0
    }

    /// Subscribe to deduplicated ingress DTMF events.
    pub(crate) fn subscribe_dtmf(&self) -> broadcast::Receiver<DtmfEvent> {
        self.dtmf_tx.subscribe()
    }

    /// Snapshot the per-direction counters.
    pub(crate) fn stats(&self) -> TapStats {
        TapStats {
            ingress_packets: self.ingress_packets.load(Ordering::Relaxed),
            egress_packets: self.egress_packets.load(Ordering::Relaxed),
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

    /// Shared processing for both directions: stats + DTMF + record.
    #[inline]
    fn process(&self, direction: PacketDirection, packet: &RtpPacket) {
        match direction {
            PacketDirection::Ingress => {
                self.ingress_packets.fetch_add(1, Ordering::Relaxed);
                // Remember which SSRC carries which payload type (needed by the
                // RTCP relay to rewrite PLI/NACK back onto the peer's sender
                // SSRC). SSRC+PT are stable per stream, so a packed atomic
                // cache lets us skip the DashMap entirely for the (overwhelming)
                // steady-state packets; the map is only touched when a new
                // (ssrc,pt) pair first appears.
                if self.track_ingress_ssrc_pts.load(Ordering::Relaxed) {
                    let key =
                        ((packet.header.ssrc as u64) << 8) | packet.header.payload_type as u64;
                    if self.last_ingress_ssrc_pt.load(Ordering::Relaxed) != key {
                        self.last_ingress_ssrc_pt.store(key, Ordering::Relaxed);
                        self.ingress_ssrc_pts
                            .entry(packet.header.ssrc)
                            .or_default()
                            .insert(packet.header.payload_type);
                    }
                }
            }
            PacketDirection::Egress => {
                self.egress_packets.fetch_add(1, Ordering::Relaxed);
            }
        }

        // DTMF events are decoded only where they enter their originating leg.
        // Decoding egress too would see the same bridged digit a second time on
        // the destination leg and could poison this tap's deduplication state.
        // Raw egress telephone-event RTP still falls through to the recorder.
        let pt = packet.header.payload_type;
        if direction == PacketDirection::Ingress && self.is_dtmf_payload_type(pt) {
            let payload_len = packet.payload.len();
            trace!(pt, len = payload_len, "tap: telephone-event packet");
            let digit = self
                .dtmf_detector
                .lock()
                .observe(&packet.payload, packet.header.timestamp);
            if let Some(digit) = digit {
                let event = DtmfEvent { direction, digit };
                // Broadcast (lagged subscribers dropped, never blocks).
                let _ = self.dtmf_tx.send(event);
            }
            // Fall through to write_sample: the raw telephone-event RTP packet
            // must reach the recording task so it can be stored in Sipflow
            // (wav_utils synthesizes the tone during export) and decoded into
            // a local WAV (Recorder::write_sample detects DTMF by PT). We do
            // NOT call write_dtmf separately, which would duplicate the tone.
        }

        // All packets (audio + telephone-event) enter the one call-scoped
        // queue. File pause/stop is owned by the task; Sipflow remains live.
        if let Some(sender) = self.recorder_sender.as_ref() {
            sender.capture(direction, packet);
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
        let tap = IngressTap::new(8, None);
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
        assert_eq!(s.egress_packets, 2);
    }

    /// The RTCP relay looks up the peer's real sender SSRC via the ingress
    /// tap's (ssrc → PT) map so it can rewrite a PLI/NACK's media_ssrc. This
    /// guards that lookup: audio and video SSRCs are tracked per payload type.
    #[test]
    fn ingress_ssrc_for_pts_resolves_peer_sender_ssrc() {
        let tap = IngressTap::new(8, None);
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
        let tap = IngressTap::new(8, None);
        tap.set_dtmf_payload_types(vec![101]);
        let mut rx = tap.subscribe_dtmf();

        // Audio packet (PT 0) — no DTMF.
        let audio = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&audio, test_addr());
        assert!(rx.try_recv().is_err(), "audio PT must not raise DTMF");

        // Egress DTMF is not an input event and must not affect the detector.
        // Feed the exact same packet on ingress afterward to prove egress did
        // not poison the deduplication state.
        let dtmf_payload = vec![1u8, 0x80, 10, 0xA0];
        let dtmf = make_packet(101, 1, 0, 1, dtmf_payload);
        tap.on_egress(&dtmf, test_addr());
        assert!(
            rx.try_recv().is_err(),
            "egress DTMF must not raise an event"
        );

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

    #[test]
    fn recorder_receives_audio_and_dtmf() {
        let (tx, mut captured) = tokio::sync::mpsc::channel(16);
        let sender = RecorderSender::new(tx);
        let tap = IngressTap::new(8, Some(sender));
        tap.set_dtmf_payload_types(vec![101]);

        // 3 audio packets → 3 sample writes.
        for seq in 1..=3u16 {
            let p = make_packet(0, seq, 160, 1, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        // Raw DTMF in both directions still reaches the recorder even though
        // only ingress is decoded into a DTMF event.
        let dtmf = make_packet(101, 1, 0, 1, vec![1u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&dtmf, test_addr());
        tap.on_egress(&dtmf, test_addr());

        let packets: Vec<_> = std::iter::from_fn(|| captured.try_recv().ok()).collect();
        assert_eq!(packets.len(), 5, "3 audio + 2 DTMF packets");
        assert_eq!(packets[3].direction, PacketDirection::Ingress);
        assert_eq!(packets[4].direction, PacketDirection::Egress);
        assert_eq!(packets.last().unwrap().packet.header.payload_type, 101);
    }

    #[test]
    fn no_recorder_no_panic() {
        let tap = IngressTap::new(8, None);
        let p = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&p, test_addr()); // must not panic
        assert_eq!(tap.ingress_packet_count(), 1);
    }

    /// The packed (ssrc, pt) cache must still record a *new* pair even after
    /// many packets of an existing pair — a mid-call SSRC/PT change must be
    /// visible to the RTCP relay lookup.
    #[test]
    fn ingress_ssrc_pt_cache_captures_new_pairs() {
        let tap = IngressTap::new(8, None);
        // Steady-state audio: 100 packets on (1001, 0).
        for seq in 1..=100u16 {
            let p = make_packet(0, seq, 160, 1001, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        // Same SSRC, different PT (e.g. telephone-event 101 on the audio SSRC).
        tap.on_ingress(
            &make_packet(101, 1, 0, 1001, vec![1u8, 0x80, 10, 0xA0]),
            test_addr(),
        );
        // New SSRC entirely (e.g. video).
        tap.on_ingress(&make_packet(96, 1, 3000, 2002, vec![1u8; 200]), test_addr());

        // All three (ssrc, pt) pairs must resolve.
        assert_eq!(tap.ingress_ssrc_for_pts(&[0]), Some(1001));
        assert_eq!(tap.ingress_ssrc_for_pts(&[101]), Some(1001));
        assert_eq!(tap.ingress_ssrc_for_pts(&[96]), Some(2002));
    }

    /// When SSRC→PT tracking is disabled (plain RTP↔RTP bridge, no RTCP relay
    /// needed), ingress packets must not populate the map but stats still
    /// advance.
    #[test]
    fn tracking_disabled_skips_ssrc_pt_map() {
        let tap = IngressTap::new(8, None);
        tap.set_track_ingress_ssrc_pts(false);
        for seq in 1..=3u16 {
            let p = make_packet(0, seq, 160, 1001, vec![1u8; 160]);
            tap.on_ingress(&p, test_addr());
        }
        assert_eq!(tap.ingress_packet_count(), 3, "stats must still advance");
        assert_eq!(
            tap.ingress_ssrc_for_pts(&[0]),
            None,
            "disabled tracking must not record (ssrc, pt)"
        );
    }

    /// Malformed / random telephone-event payloads must never panic the DTMF
    /// detector or the recording path. Feeds 1000 pseudo-random payloads (varying
    /// length and contents) on the DTMF PT and on the audio PT.
    #[test]
    fn malformed_rtp_packets_do_not_panic() {
        let tap = IngressTap::new(64, None);
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
