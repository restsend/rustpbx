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
//! Audio and telephone-event RTP in both directions reach the recording task,
//! allowing DTMF tones to be synthesized into WAV recordings. Video and unknown
//! RTP payload types are excluded.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use rustrtc::peer_connection::RtpObserver;
use rustrtc::rtp::RtpPacket;
use tokio::sync::broadcast;
use tracing::trace;

use crate::dtmf::DtmfDetector;
use crate::media_recorder::{CaptureChannel, RecorderSender};

/// Lock-free 128-bit payload-type bitmask (PT 0..=127), shared by the DTMF
/// and recording-audio allowlists.
struct PtMask([AtomicU64; 2]);

impl PtMask {
    fn new() -> Self {
        Self([AtomicU64::new(0), AtomicU64::new(0)])
    }

    /// Replace the mask contents with `pts`.
    fn set(&self, pts: &[u8]) {
        let mut words = [0u64; 2];
        for &p in pts {
            words[(p as usize) >> 6] |= 1u64 << (p & 63);
        }
        self.0[0].store(words[0], Ordering::Relaxed);
        self.0[1].store(words[1], Ordering::Relaxed);
    }

    /// Lock-free membership check.
    #[inline]
    fn contains(&self, pt: u8) -> bool {
        let word = (pt as usize) >> 6;
        let bit = 1u64 << (pt & 63);
        self.0[word].load(Ordering::Relaxed) & bit != 0
    }
}

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
    pub recording_queue_drops: u64,
    pub audio_packet_samples: Option<AudioPacketSamples>,
}

const AUDIO_PACKET_SAMPLES: usize = 4;

/// Bounded plaintext packet evidence, without retaining audio bytes.
/// Payload hashes are comparable only within the same process/build.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AudioPacketSample {
    ssrc: u32,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    payload_bytes: usize,
    payload_hash: String,
    peer_addr: std::net::SocketAddr,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AudioPacketSamples {
    ingress: VecDeque<AudioPacketSample>,
    // The transport observer runs before SRTP protection and socket sending.
    // These samples cannot establish wire delivery, even when hashes match.
    egress_attempts: VecDeque<AudioPacketSample>,
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
    audio_packet_samples_enabled: AtomicBool,
    audio_packet_samples: Mutex<Option<AudioPacketSamples>>,
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
    dtmf_pt_mask: PtMask,
    dtmf_detector: Mutex<DtmfDetector>,
    dtmf_tx: broadcast::Sender<DtmfEvent>,

    // ── recording ───────────────────────────────────────────────────────
    /// Audio payload types configured on this leg, as a 128-bit lock-free
    /// bitmask (RTP payload types are 7 bits). Initialized from the leg's
    /// codec list and re-synced by `set_audio_payload_types` whenever a
    /// re-INVITE renegotiates the codec — a stale allowlist would silently
    /// drop every post-renegotiation audio packet from the recording.
    audio_pt_mask: PtMask,
    // Shared snapshots retain the source PT mapping without cloning it per packet.
    recording_profile: parking_lot::RwLock<Option<Arc<crate::negotiate::NegotiatedLegProfile>>>,
    /// Capture routing: which recording channel (if any) each direction of
    /// this leg feeds. Stored as the `CaptureChannel` code (0 = off) in
    /// atomics so route selection stays lock-free. The caller leg
    /// uses the classic layout (ingress->Caller, egress->Callee); the callee
    /// leg starts fully off and switches to ingress->Callee only while the
    /// call is held (see `set_capture_route`).
    capture_ingress_channel: AtomicU8,
    capture_egress_channel: AtomicU8,
    /// Sender for the call-scoped recording task. Shared by every tapped leg
    /// of the call; it remains immutable for the tap's lifetime.
    recorder_sender: Option<RecorderSender>,
}

impl IngressTap {
    /// Create a new tap. `dtmf_bus_capacity` bounds the DTMF broadcast
    /// channel (subscribers that lag are dropped, never blocking the tap).
    /// The optional recording sender is fixed for the tap's lifetime.
    pub fn new(
        dtmf_bus_capacity: usize,
        audio_payload_types: Vec<u8>,
        recorder_sender: Option<RecorderSender>,
    ) -> Arc<Self> {
        let (dtmf_tx, _) = broadcast::channel(dtmf_bus_capacity.max(1));
        // Classic caller-tap layout. Legs without a recorder sender never
        // capture; legs with one can retarget via `set_capture_route`.
        let default_route = if recorder_sender.is_some() {
            (
                CaptureChannel::Caller.code(),
                CaptureChannel::Callee.code(),
            )
        } else {
            (0, 0)
        };
        let tap = Arc::new(Self {
            ingress_packets: AtomicU64::new(0),
            egress_packets: AtomicU64::new(0),
            audio_packet_samples_enabled: AtomicBool::new(false),
            audio_packet_samples: Mutex::new(None),
            track_ingress_ssrc_pts: AtomicBool::new(true),
            last_ingress_ssrc_pt: AtomicU64::new(u64::MAX),
            ingress_ssrc_pts: DashMap::new(),
            dtmf_pt_mask: PtMask::new(),
            dtmf_detector: Mutex::new(DtmfDetector::default()),
            dtmf_tx,
            audio_pt_mask: PtMask::new(),
            recording_profile: parking_lot::RwLock::new(None),
            capture_ingress_channel: AtomicU8::new(default_route.0),
            capture_egress_channel: AtomicU8::new(default_route.1),
            recorder_sender,
        });
        tap.set_audio_payload_types(audio_payload_types);
        tap
    }

    pub(crate) fn enable_audio_packet_samples(&self) {
        *self.audio_packet_samples.lock() = Some(AudioPacketSamples {
            ingress: VecDeque::with_capacity(AUDIO_PACKET_SAMPLES),
            egress_attempts: VecDeque::with_capacity(AUDIO_PACKET_SAMPLES),
        });
        self.audio_packet_samples_enabled
            .store(true, Ordering::Release);
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
        self.dtmf_pt_mask.set(&pts);
    }

    /// Lock-free telephone-event check: is `pt` one of the negotiated DTMF
    /// payload types?
    #[inline]
    fn is_dtmf_payload_type(&self, pt: u8) -> bool {
        self.dtmf_pt_mask.contains(pt)
    }

    /// Re-sync the recording audio allowlist. Called when a re-INVITE
    /// renegotiates the leg's codec set: without this the tap keeps only the
    /// pre-renegotiation payload types and every packet sent/received under
    /// the new codec is excluded from the call recording.
    pub fn set_audio_payload_types(&self, pts: Vec<u8>) {
        self.audio_pt_mask.set(&pts);
    }

    pub(crate) fn set_recording_profile(&self, profile: crate::negotiate::NegotiatedLegProfile) {
        *self.recording_profile.write() = Some(Arc::new(profile));
    }

    /// Retarget this leg's recording capture (lock-free, safe mid-call).
    ///
    /// Used by the session to switch the recorder's data sources when a leg
    /// is held: the callee leg's ingress becomes the B channel (the agent's
    /// live mic -- the audio the bridge no longer relays), and the caller
    /// leg's egress capture is dropped (it only carries comfort noise / hold
    /// music, which would interleave with the mic stream on the same
    /// sequential channel). `None` disables capture for that direction.
    pub fn set_capture_route(
        &self,
        ingress: Option<CaptureChannel>,
        egress: Option<CaptureChannel>,
    ) {
        self.capture_ingress_channel
            .store(ingress.map(|c| c.code()).unwrap_or(0), Ordering::Relaxed);
        self.capture_egress_channel
            .store(egress.map(|c| c.code()).unwrap_or(0), Ordering::Relaxed);
    }

    /// Read back the current capture route (tests / diagnostics).
    pub fn capture_route(&self) -> (Option<CaptureChannel>, Option<CaptureChannel>) {
        (
            CaptureChannel::from_code(self.capture_ingress_channel.load(Ordering::Relaxed)),
            CaptureChannel::from_code(self.capture_egress_channel.load(Ordering::Relaxed)),
        )
    }

    /// Lock-free audio-codec check: is `pt` one of the allowlisted audio
    /// payload types?
    #[inline]
    fn is_audio_payload_type(&self, pt: u8) -> bool {
        self.audio_pt_mask.contains(pt)
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
            recording_queue_drops: self
                .recorder_sender
                .as_ref()
                .map(|sender| sender.dropped_packets())
                .unwrap_or(0),
            audio_packet_samples: self.audio_packet_samples.lock().clone(),
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
    fn process(
        &self,
        direction: PacketDirection,
        packet: &RtpPacket,
        peer_addr: std::net::SocketAddr,
    ) {
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
        // Raw telephone-event RTP in both directions still reaches the recorder.
        let pt = packet.header.payload_type;
        if self.audio_packet_samples_enabled.load(Ordering::Acquire)
            && self.is_audio_payload_type(pt)
        {
            // Compare payloads across this instance's ingress/egress boundaries;
            // RTP headers legitimately change during relay. No decode or raw audio log.
            let mut hash = DefaultHasher::new();
            hash.write(&packet.payload);
            let sample = AudioPacketSample {
                ssrc: packet.header.ssrc,
                payload_type: pt,
                sequence: packet.header.sequence_number,
                timestamp: packet.header.timestamp,
                payload_bytes: packet.payload.len(),
                payload_hash: format!("{:016x}", hash.finish()),
                peer_addr,
            };
            if let Some(samples) = self.audio_packet_samples.lock().as_mut() {
                let samples = match direction {
                    PacketDirection::Ingress => &mut samples.ingress,
                    PacketDirection::Egress => &mut samples.egress_attempts,
                };
                if samples.len() == AUDIO_PACKET_SAMPLES {
                    samples.pop_front();
                }
                samples.push_back(sample);
            }
        }
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
        }

        // Preserve telephone-event packets for recording, including duration
        // updates and terminal repeats suppressed by the digit event detector.
        // The leg's capture route (see `set_capture_route`) decides whether --
        // and into which channel -- each direction is recorded.
        if (self.is_audio_payload_type(pt) || self.is_dtmf_payload_type(pt))
            && let Some(sender) = self.recorder_sender.as_ref()
        {
            let code = match direction {
                PacketDirection::Ingress => self.capture_ingress_channel.load(Ordering::Relaxed),
                PacketDirection::Egress => self.capture_egress_channel.load(Ordering::Relaxed),
            };
            if let Some(channel) = CaptureChannel::from_code(code) {
                sender.capture_with_profile(
                    channel,
                    packet,
                    Some(peer_addr),
                    self.recording_profile.read().clone(),
                );
            }
        }
    }
}

impl RtpObserver for IngressTap {
    fn on_ingress(&self, packet: &RtpPacket, src_addr: std::net::SocketAddr) {
        self.process(PacketDirection::Ingress, packet, src_addr);
    }

    fn on_egress(&self, packet: &RtpPacket, dst_addr: std::net::SocketAddr) {
        self.process(PacketDirection::Egress, packet, dst_addr);
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
        let tap = IngressTap::new(8, Vec::new(), None);
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
        let tap = IngressTap::new(8, Vec::new(), None);
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
        let tap = IngressTap::new(8, Vec::new(), None);
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
    fn recorder_receives_audio_and_dtmf_payload_types() {
        let (tx, mut captured) = tokio::sync::mpsc::channel(16);
        let sender = RecorderSender::new(tx);
        let tap = IngressTap::new(8, vec![0, 8], Some(sender));
        tap.set_dtmf_payload_types(vec![101]);

        let pcmu = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        let pcma = make_packet(8, 2, 320, 1, vec![2u8; 160]);
        tap.on_ingress(&pcmu, test_addr());
        tap.on_egress(&pcma, test_addr());

        // Telephone-event RTP must be captured even outside the audio allowlist.
        // Video remains excluded.
        let video = make_packet(96, 3, 3000, 2, vec![3u8; 200]);
        let dtmf = make_packet(101, 1, 0, 1, vec![1u8, 0x80, 10, 0xA0]);
        tap.on_ingress(&video, test_addr());
        tap.on_egress(&video, test_addr());
        tap.on_ingress(&dtmf, test_addr());
        tap.on_egress(&dtmf, test_addr());
        tap.on_ingress(&dtmf, test_addr());

        let packets: Vec<_> = std::iter::from_fn(|| captured.try_recv().ok()).collect();
        assert_eq!(packets.len(), 5);
        assert_eq!(packets[0].channel, CaptureChannel::Caller);
        assert_eq!(packets[0].packet.header.payload_type, 0);
        assert_eq!(packets[1].channel, CaptureChannel::Callee);
        assert_eq!(packets[1].packet.header.payload_type, 8);
        for (packet, channel) in packets[2..].iter().zip([
            CaptureChannel::Caller,
            CaptureChannel::Callee,
            CaptureChannel::Caller,
        ]) {
            assert_eq!(packet.channel, channel);
            assert_eq!(packet.packet.header.payload_type, 101);
            assert_eq!(packet.packet.payload, dtmf.payload);
        }
    }

    /// Capture routing: a tap can be retargeted mid-call -- the callee leg's
    /// ingress becomes the B channel during hold, and disabling a direction
    /// stops its capture entirely.
    #[test]
    fn capture_route_switches_channels_and_disables_directions() {
        let (tx, mut captured) = tokio::sync::mpsc::channel(16);
        let sender = RecorderSender::new(tx);
        let tap = IngressTap::new(8, vec![0], Some(sender));

        // Default (caller layout): ingress->Caller, egress->Callee.
        tap.on_ingress(&make_packet(0, 1, 160, 1, vec![1u8; 160]), test_addr());
        tap.on_egress(&make_packet(0, 2, 320, 1, vec![1u8; 160]), test_addr());

        // Callee-leg hold layout: ingress->Callee, egress off.
        tap.set_capture_route(Some(CaptureChannel::Callee), None);
        tap.on_ingress(&make_packet(0, 3, 480, 1, vec![1u8; 160]), test_addr());
        tap.on_egress(&make_packet(0, 4, 640, 1, vec![1u8; 160]), test_addr());

        let packets: Vec<_> = std::iter::from_fn(|| captured.try_recv().ok()).collect();
        assert_eq!(packets.len(), 3, "disabled egress must not capture");
        assert_eq!(packets[0].channel, CaptureChannel::Caller);
        assert_eq!(packets[0].packet.header.sequence_number, 1);
        assert_eq!(packets[1].channel, CaptureChannel::Callee);
        assert_eq!(packets[1].packet.header.sequence_number, 2);
        assert_eq!(packets[2].channel, CaptureChannel::Callee);
        assert_eq!(packets[2].packet.header.sequence_number, 3);
    }

    /// A tap created without a recorder sender captures nothing regardless of
    /// route.
    #[test]
    fn capture_route_without_sender_is_inert() {
        let tap = IngressTap::new(8, vec![0], None);
        tap.set_capture_route(Some(CaptureChannel::Callee), Some(CaptureChannel::Callee));
        tap.on_ingress(&make_packet(0, 1, 160, 1, vec![1u8; 160]), test_addr());
        tap.on_egress(&make_packet(0, 2, 320, 1, vec![1u8; 160]), test_addr());
        // No panic, no captures (nothing to observe without a channel).
    }

    #[test]
    fn no_recorder_no_panic() {
        let tap = IngressTap::new(8, Vec::new(), None);
        let p = make_packet(0, 1, 160, 1, vec![1u8; 160]);
        tap.on_ingress(&p, test_addr()); // must not panic
        assert_eq!(tap.ingress_packet_count(), 1);
    }

    /// The packed (ssrc, pt) cache must still record a *new* pair even after
    /// many packets of an existing pair — a mid-call SSRC/PT change must be
    /// visible to the RTCP relay lookup.
    #[test]
    fn ingress_ssrc_pt_cache_captures_new_pairs() {
        let tap = IngressTap::new(8, Vec::new(), None);
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
        let tap = IngressTap::new(8, Vec::new(), None);
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
        let tap = IngressTap::new(64, Vec::new(), None);
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
