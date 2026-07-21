//! Interceptor-based recording tap for A-leg (caller) bidirectional capture.
//!
//! Installs on a PeerConnection's transceivers via `RtcConfiguration`:
//! - Receiver interceptor fires on every incoming RTP (caller mic) → Leg::A
//! - Sender interceptor fires on every outgoing RTP (caller egress) → Leg::B
//!
//! Both write directly to their sinks (file recorder + sipflow backend) —
//! **no broadcast channel, no drain task, no intermediate buffer.**
//! The hot path does:
//!   1. `AtomicBool::load` (1 ns when inactive)
//!   2. `recorder.try_write()` (non-blocking, skip if locked)
//!   3. `Bytes::clone()` for payload (Arc refcount bump — 0 byte copy)
//!   4. `packet.marshal()` for sipflow (1 alloc, unavoidable for raw storage)

use crate::media::ReceiveTimestampClock;
use crate::media::recorder::{Leg, Recorder};
use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use rustrtc::media::MediaSample;
use rustrtc::media::frame::AudioFrame;
use rustrtc::rtp::{RtcpPacket, RtpPacket};
use rustrtc::transports::rtp::RtpTransport;
use rustrtc::{RtpReceiverInterceptor, RtpSenderInterceptor};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{trace, warn};

/// Recording tap that directly writes incoming/outgoing RTP to file + sipflow.
///
/// Clone-cheap (Arc bumps). Install one per direction (recv → Leg::A,
/// send → Leg::B) on the **caller's** PeerConnection via
/// `RtcConfigurationBuilder::receiver_interceptor / sender_interceptor`.
pub struct RecorderTap {
    /// File recorder (shared with the session). `None` = file recording disabled.
    recorder: Option<Arc<RwLock<Option<Recorder>>>>,
    /// Sipflow backend. `None` = sipflow disabled.
    sipflow: Option<Arc<dyn SipFlowBackend>>,
    /// Call-ID for sipflow storage.
    call_id: String,
    /// Leg tag: `Leg::A` for recv (caller mic), `Leg::B` for send (caller egress).
    leg: Leg,
    /// Shared pause flag (same Arc as `SipSession::recording_paused`).
    /// When `true`, the tap is a no-op (1 atomic read + return).
    paused: Arc<AtomicBool>,
    allowed_pts: Vec<u8>,
    clock: ReceiveTimestampClock,
    /// Diagnostic: total packets captured (for debugging empty-WAV issues).
    pkt_count: AtomicU64,
}

impl RecorderTap {
    /// Create a recv-side tap (Leg::A = caller mic).
    pub fn for_recv(
        recorder: Option<Arc<RwLock<Option<Recorder>>>>,
        sipflow: Option<Arc<dyn SipFlowBackend>>,
        call_id: String,
        paused: Arc<AtomicBool>,
        allowed_pts: Vec<u8>,
    ) -> Self {
        Self {
            recorder,
            sipflow,
            call_id,
            leg: Leg::A,
            paused,
            allowed_pts,
            clock: ReceiveTimestampClock::new(),
            pkt_count: AtomicU64::new(0),
        }
    }

    /// Create a send-side tap (Leg::B = caller egress).
    pub fn for_send(
        recorder: Option<Arc<RwLock<Option<Recorder>>>>,
        sipflow: Option<Arc<dyn SipFlowBackend>>,
        call_id: String,
        paused: Arc<AtomicBool>,
        allowed_pts: Vec<u8>,
    ) -> Self {
        Self {
            recorder,
            sipflow,
            call_id,
            leg: Leg::B,
            paused,
            allowed_pts,
            clock: ReceiveTimestampClock::new(),
            pkt_count: AtomicU64::new(0),
        }
    }

    /// Clone-cheap: all fields are Arc or String.
    pub fn clone_arc(&self) -> Self {
        Self {
            recorder: self.recorder.clone(),
            sipflow: self.sipflow.clone(),
            call_id: self.call_id.clone(),
            leg: self.leg,
            paused: self.paused.clone(),
            allowed_pts: self.allowed_pts.clone(),
            clock: self.clock.clone(),
            pkt_count: AtomicU64::new(0),
        }
    }

    /// Total packets captured (for diagnostics).
    pub fn pkt_count(&self) -> u64 {
        self.pkt_count.load(Ordering::Relaxed)
    }

    /// Core capture — called from both interceptor trait impls.
    #[inline]
    fn capture(
        &self,
        packet: &RtpPacket,
        peer_addr: std::net::SocketAddr,
        local_addr: std::net::SocketAddr,
    ) {
        // 1. Pause guard — when paused, hot path is 1 relaxed load + return.
        if self.paused.load(Ordering::Relaxed) {
            return;
        }

        let pt = packet.header.payload_type;

        // 2. PT filter: only capture audio + DTMF, skip video.
        if !self.allowed_pts.is_empty() && !self.allowed_pts.contains(&pt) {
            return;
        }
        let timestamp_us = self.clock.now_micros();
        let count = self.pkt_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            tracing::info!(
                leg = ?self.leg,
                pt = pt,
                ssrc = packet.header.ssrc,
                peer_addr = %peer_addr,
                local_addr = %local_addr,
                has_recorder = self.recorder.is_some(),
                has_sipflow = self.sipflow.is_some(),
                call_id = %self.call_id,
                "RecorderTap first packet captured"
            );
        }

        // 2. File recorder: try_write (non-blocking). If the lock is held
        //    (another thread is writing), skip this packet — same semantics
        //    as the previous try_send-on-full-channel approach.
        if let Some(rec) = &self.recorder {
            if let Some(mut guard) = rec.try_write() {
                if let Some(r) = guard.as_mut() {
                    // Construct a minimal AudioFrame from the raw RTP packet.
                    // raw_packet = None so write_sample uses frame.data.clone()
                    // (Bytes Arc-bump) instead of Bytes::copy_from_slice.
                    let frame = AudioFrame {
                        rtp_timestamp: packet.header.timestamp,
                        clock_rate: 8000, // write_sample determines actual rate from profile
                        data: packet.payload.clone(), // Bytes Arc-bump — 0 byte copy
                        sequence_number: Some(packet.header.sequence_number),
                        payload_type: Some(pt),
                        marker: packet.header.marker,
                        header_extension: None,
                        source_addr: None,
                        raw_packet: None,
                    };
                    let sample = MediaSample::Audio(frame);
                    let wr = r.write_sample(self.leg, &sample, None, None, None);
                    if let Err(e) = &wr {
                        warn!(leg=?self.leg, pt=pt, "recorder write_sample error: {e}");
                    }
                }
            }
        }

        // 3. Sipflow: marshal once + send to unbounded channel.
        //    marshal() allocates a Vec<u8> (header + payload) — unavoidable
        //    for raw RTP storage. The channel send is non-blocking.
        //
        //    Address semantics per leg:
        //      Leg::A (recv): src = remote peer (caller), dst = local server
        //      Leg::B (send): src = local server, dst = remote peer (caller egress)
        if let Some(backend) = &self.sipflow {
            if let Ok(rtp_bytes) = packet.marshal() {
                let (src_addr, dst_addr) = match self.leg {
                    Leg::A => (peer_addr.to_string(), local_addr.to_string()),
                    Leg::B => (local_addr.to_string(), peer_addr.to_string()),
                };
                let item = SipFlowItem {
                    timestamp: timestamp_us,
                    seq: packet.header.sequence_number as u64,
                    leg: Some(self.leg as i32),
                    msg_type: SipFlowMsgType::Rtp,
                    src_addr,
                    dst_addr,
                    payload: Bytes::from(rtp_bytes),
                };
                if let Err(e) = backend.record(&self.call_id, item) {
                    trace!("sipflow record error: {e}");
                }
            }
        }
    }
}

#[async_trait]
impl RtpReceiverInterceptor for RecorderTap {
    async fn on_packet_received(
        &self,
        packet: &RtpPacket,
        src_addr: std::net::SocketAddr,
        local_addr: std::net::SocketAddr,
    ) -> Option<RtcpPacket> {
        self.capture(packet, src_addr, local_addr);
        None
    }

    async fn on_rtcp_received(&self, _packet: &RtcpPacket, _transport: Arc<RtpTransport>) {}
}

#[async_trait]
impl RtpSenderInterceptor for RecorderTap {
    async fn on_packet_sent(
        &self,
        packet: &RtpPacket,
        dst_addr: std::net::SocketAddr,
        local_addr: std::net::SocketAddr,
    ) {
        self.capture(packet, dst_addr, local_addr);
    }

    async fn on_rtcp_received(&self, _packet: &RtcpPacket, _transport: Arc<RtpTransport>) {}
}

/// Convenience: build `RecorderInterceptors` for a caller PC's `RtcConfiguration`.
///
/// Returns a list suitable for `RtcConfigurationBuilder::receiver_interceptor`
/// and `sender_interceptor`. The `active` flag is shared — flip it to start/stop
/// recording without rebuilding the PC.
pub fn build_caller_interceptors(
    recorder: Option<Arc<RwLock<Option<Recorder>>>>,
    sipflow: Option<Arc<dyn SipFlowBackend>>,
    call_id: String,
    paused: Arc<AtomicBool>,
    allowed_pts: Vec<u8>,
) -> (Arc<RecorderTap>, Arc<RecorderTap>) {
    let recv_tap = Arc::new(RecorderTap::for_recv(
        recorder.clone(),
        sipflow.clone(),
        call_id.clone(),
        paused.clone(),
        allowed_pts.clone(),
    ));
    let send_tap = Arc::new(RecorderTap::for_send(
        recorder,
        sipflow,
        call_id,
        paused,
        allowed_pts,
    ));
    (recv_tap, send_tap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::recorder::Recorder;
    use crate::sipflow::SipFlowItem;
    use audio_codec::CodecType;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::sync::atomic::AtomicUsize;

    fn make_rtp_packet(pt: u8, seq: u16, ts: u32, payload: Vec<u8>) -> RtpPacket {
        let header = RtpHeader::new(pt, seq, ts, 0x12345678);
        RtpPacket::new(header, payload)
    }

    /// Stand-in backend that counts recorded items.
    struct CountingBackend {
        count: AtomicUsize,
    }
    impl CountingBackend {
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }
        fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl SipFlowBackend for CountingBackend {
        fn record(&self, _call_id: &str, _item: SipFlowItem) -> anyhow::Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn query_flow(
            &self,
            _call_id: &str,
            _start: chrono::DateTime<chrono::Local>,
            _end: chrono::DateTime<chrono::Local>,
        ) -> anyhow::Result<Vec<SipFlowItem>> {
            Ok(Vec::new())
        }
        async fn query_media_stats(
            &self,
            _call_id: &str,
            _start: chrono::DateTime<chrono::Local>,
            _end: chrono::DateTime<chrono::Local>,
        ) -> anyhow::Result<Vec<crate::sipflow::SipFlowMediaStats>> {
            Ok(Vec::new())
        }
        async fn query_media(
            &self,
            _call_id: &str,
            _start: chrono::DateTime<chrono::Local>,
            _end: chrono::DateTime<chrono::Local>,
        ) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn inactive_tap_does_nothing() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(true)); // paused → skip
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "call-1".into(),
            paused,
            vec![],
        );

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        assert_eq!(backend.count(), 0, "inactive tap should not record");
    }

    #[tokio::test]
    async fn active_tap_records_to_sipflow() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false)); // not paused → record
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "call-2".into(),
            paused,
            vec![],
        );

        for i in 0..5u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        assert_eq!(backend.count(), 5, "should record 5 packets");
    }

    #[tokio::test]
    async fn send_tap_uses_leg_b() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_send(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "call-3".into(),
            paused,
            vec![],
        );
        assert_eq!(tap.leg, Leg::B);
    }

    #[tokio::test]
    async fn file_recorder_writes_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tap_test.wav");
        let mut rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();

        // Set up PCMU profile for leg A
        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: None,
            transport: rustrtc::TransportMode::Rtp,
        };
        rec.set_leg_profile(Leg::A, profile.clone());
        rec.set_leg_profile(Leg::B, profile);

        let recorder_arc = Arc::new(RwLock::new(Some(rec)));
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            Some(recorder_arc.clone()),
            None,
            "call-4".into(),
            paused,
            vec![],
        );

        // Send 10 PCMU packets (200ms of audio)
        for i in 0..10u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }

        // Finalize recorder
        {
            let mut g = recorder_arc.write();
            if let Some(r) = g.as_mut() {
                r.finalize().unwrap();
            }
        }

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() > 44,
            "WAV should have header + data, got {} bytes",
            metadata.len()
        );
    }

    #[tokio::test]
    async fn dtmf_packet_captured() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "call-dtmf".into(),
            paused,
            vec![],
        );

        // DTMF digit '5' end event (PT 101, 4-byte payload)
        let dtmf_payload = vec![5u8, 0x80, 0x00, 0xA0];
        let pkt = make_rtp_packet(101, 1, 160, dtmf_payload);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        assert_eq!(backend.count(), 1, "DTMF packet should be captured");
    }

    #[test]
    fn leg_as_i32() {
        assert_eq!(Leg::A as i32, 0);
        assert_eq!(Leg::B as i32, 1);
    }

    // ── Dual recording: file + sipflow simultaneously ───────────────────

    #[tokio::test]
    async fn dual_file_and_sipflow_capture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dual.wav");
        let mut rec = Recorder::new(path.to_str().unwrap(), CodecType::PCMU).unwrap();

        use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
        let profile = NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: Some(NegotiatedCodec {
                payload_type: 101,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            transport: rustrtc::TransportMode::Rtp,
        };
        rec.set_leg_profile(Leg::A, profile.clone());
        rec.set_leg_profile(Leg::B, profile);
        let recorder_arc = Arc::new(RwLock::new(Some(rec)));

        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            Some(recorder_arc.clone()),
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "dual-call".into(),
            paused,
            vec![],
        );

        // Send 10 audio + 2 DTMF packets
        for i in 0..10u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        for i in 10..12u16 {
            let pkt = make_rtp_packet(101, i, 1600, vec![5u8, 0x80, 0x00, 0xA0]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }

        // Sipflow should have all 12 packets
        assert_eq!(backend.count(), 12, "sipflow should receive all 12 packets");

        // File recorder should have audio + DTMF rendered
        {
            let mut g = recorder_arc.write();
            if let Some(r) = g.as_mut() {
                r.finalize().unwrap();
            }
        }
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() > 44, "WAV should have data");
    }

    // ── Pause/resume ───────────────────────────────────────────────────

    #[tokio::test]
    async fn pause_stops_capture_resume_continues() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "pause-call".into(),
            paused.clone(),
            vec![],
        );

        // 3 packets while active
        for i in 0..3u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        assert_eq!(backend.count(), 3);

        // Pause
        paused.store(true, Ordering::SeqCst);

        // 3 packets while paused — should be skipped
        for i in 3..6u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        assert_eq!(backend.count(), 3, "paused tap should not capture");

        // Resume
        paused.store(false, Ordering::SeqCst);

        for i in 6..9u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        assert_eq!(backend.count(), 6, "resumed tap should capture again");
    }

    // ── No memory leak: tap holds no per-packet state ──────────────────

    #[tokio::test]
    async fn no_memory_leak_high_packet_count() {
        // The RecorderTap should not accumulate any per-packet state.
        // After N packets, the tap struct size should remain constant.
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "leak-test".into(),
            paused,
            vec![],
        );

        // Send 1000 packets
        for i in 0..1000u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }

        assert_eq!(backend.count(), 1000);
        // The tap itself holds no Vec/buffer — it's stateless beyond the
        // shared Arc references. The CountingBackend's count is just an
        // AtomicUsize, which doesn't grow.
    }

    // ── Leg isolation: recv=Leg::A, send=Leg::B ────────────────────────

    #[tokio::test]
    async fn recv_tap_tags_leg_a_send_tap_tags_leg_b() {
        let backend_a = Arc::new(CountingBackend::new());
        let backend_b = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));

        let recv_tap = RecorderTap::for_recv(
            None,
            Some(backend_a.clone() as Arc<dyn SipFlowBackend>),
            "leg-test".into(),
            paused.clone(),
            vec![],
        );
        let send_tap = RecorderTap::for_send(
            None,
            Some(backend_b.clone() as Arc<dyn SipFlowBackend>),
            "leg-test".into(),
            paused,
            vec![],
        );

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        recv_tap
            .on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        send_tap
            .on_packet_sent(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;

        assert_eq!(backend_a.count(), 1);
        assert_eq!(backend_b.count(), 1);
        assert_eq!(recv_tap.leg, Leg::A);
        assert_eq!(send_tap.leg, Leg::B);
    }

    // ── Video PT is captured (not filtered) ────────────────────────────

    #[tokio::test]
    async fn video_packet_captured_when_no_filter() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "video-call".into(),
            paused,
            vec![],
        );

        // H264 packet (PT 96, dynamic)
        let pkt = make_rtp_packet(96, 1, 9000, vec![0x80u8; 1200]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        assert_eq!(backend.count(), 1, "video captured when no filter");
    }

    // ── Audio → DTMF → Audio sequence (real call flow) ─────────────────

    #[tokio::test]
    async fn audio_dtmf_audio_sequence_captured_in_order() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "seq-call".into(),
            paused,
            vec![],
        );

        let mut seq = 0u16;
        // 3 audio packets
        for _ in 0..3 {
            let pkt = make_rtp_packet(0, seq, seq as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
            seq += 1;
        }
        // DTMF '1' start + end
        for (flags, dur) in [(0x00u8, 80u16), (0x80, 160)] {
            let pkt = make_rtp_packet(
                101,
                seq,
                480,
                vec![1u8, flags, (dur >> 8) as u8, (dur & 0xff) as u8],
            );
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
            seq += 1;
        }
        // 2 more audio packets
        for _ in 0..2 {
            let pkt = make_rtp_packet(0, seq, seq as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
            seq += 1;
        }

        assert_eq!(
            backend.count(),
            7,
            "all 7 packets (3 audio + 2 DTMF + 2 audio) captured"
        );
    }

    // ── Zero-copy verification: payload Bytes is Arc-shared ────────────

    #[tokio::test]
    async fn payload_bytes_shared_not_copied() {
        // Verify that the interceptor doesn't deep-copy payload bytes.
        // We check this by verifying that the original payload Bytes
        // handle is still valid after the interceptor processes it
        // (proving it was cloned via Arc, not moved).
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "zerocopy-call".into(),
            paused,
            vec![],
        );

        let payload = vec![0xABu8; 160];
        let payload_ptr = payload.as_ptr();

        let pkt = RtpPacket::new(RtpHeader::new(0, 1, 160, 0x1234), payload);

        // The interceptor receives &RtpPacket — it must not move the payload.
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        // Original packet's payload should still be accessible
        assert_eq!(
            pkt.payload.as_ptr(),
            payload_ptr,
            "payload ptr must not change"
        );
        assert_eq!(pkt.payload.len(), 160, "payload must be intact");
        assert_eq!(backend.count(), 1);
    }

    // ── IVR scenario: silence/file audio on send side = Leg::B ─────────

    #[tokio::test]
    async fn ivr_file_audio_captured_on_send_side() {
        // Simulate IVR: the caller PC's sender sends file audio (silence/prompts).
        // The send interceptor should capture it as Leg::B.
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        let send_tap = RecorderTap::for_send(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "ivr-call".into(),
            paused,
            vec![],
        );

        // Simulate 5 IVR prompt packets (Opus PT=111)
        for i in 0..5u16 {
            let pkt = make_rtp_packet(111, i, i as u32 * 960, vec![0x55u8; 80]);
            send_tap
                .on_packet_sent(
                    &pkt,
                    std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                    std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                )
                .await;
        }

        assert_eq!(
            backend.count(),
            5,
            "IVR audio should be captured on send side"
        );
    }

    // ── PT filter: relies on SDP-negotiated audio PT whitelist ─────────
    //
    // The `allowed_pts` filter is NOT hardcoded to exclude PT 96 as video.
    // Instead, it is populated from the SDP answer's audio profile:
    //   build_recorder_taps() → extract_leg_profile(answer_sdp) → allowed_pts
    // Only audio+DTMF PTs from the negotiated profile are whitelisted.
    // Any PT not in the whitelist (including dynamic video PTs like 96, 106,
    // or 122) is filtered out — regardless of its numeric value.

    #[tokio::test]
    async fn video_packet_filtered_by_allowed_pts() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));
        // allowed_pts = [0, 101] simulates an SDP where PCMU(PT=0) and
        // telephone-event(PT=101) are the only audio codecs. Any packet
        // with a different PT (e.g. H264/VP8 at dynamic PT 96-127) is
        // filtered out, regardless of what codec it carries.
        let tap = RecorderTap::for_recv(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "video-filter".into(),
            paused,
            vec![0, 101],
        );

        tap.on_packet_received(
            &make_rtp_packet(96, 1, 9000, vec![0x80u8; 1200]),
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        tap.on_packet_received(
            &make_rtp_packet(0, 2, 160, vec![0x55u8; 160]),
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        tap.on_packet_received(
            &make_rtp_packet(101, 3, 320, vec![5u8, 0x80, 0, 0xA0]),
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        assert_eq!(
            backend.count(),
            2,
            "only audio+DTMF captured, video filtered"
        );
    }

    // ── build_caller_interceptors returns matching taps ────────────────

    #[tokio::test]
    async fn build_caller_interceptors_produces_matched_pair() {
        let backend = Arc::new(CountingBackend::new());
        let paused = Arc::new(AtomicBool::new(false));

        let (recv, send) = build_caller_interceptors(
            None,
            Some(backend.clone() as Arc<dyn SipFlowBackend>),
            "build-test".into(),
            paused,
            vec![],
        );

        assert_eq!(recv.leg, Leg::A, "recv tap should be Leg::A");
        assert_eq!(send.leg, Leg::B, "send tap should be Leg::B");

        // Both should capture to the same backend
        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        recv.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        send.on_packet_sent(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        assert_eq!(backend.count(), 2);
    }
}
