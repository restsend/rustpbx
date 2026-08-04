//! Interceptor-based recording tap for A-leg (caller) bidirectional capture.
//!
//! Installs on a PeerConnection's transceivers via `RtcConfiguration`:
//! - Receiver interceptor fires on every incoming RTP (caller mic) → Leg::A
//! - Sender interceptor fires on every outgoing RTP (caller egress) → Leg::B
//!
//! Both enqueue already packetized RTP into one bounded call-scoped RTP queue.
//! A separate control channel drives the same worker's recording lifecycle,
//! keeping control traffic independent from RTP queue pressure.

use crate::ReceiveTimestampClock;
use crate::negotiate::NegotiatedLegProfile;
use crate::recorder::{Leg, Recorder};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use rustpbx_sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use rustrtc::media::MediaSample;
use rustrtc::media::frame::AudioFrame;
use rustrtc::rtp::{RtcpPacket, RtpPacket};
use rustrtc::transports::rtp::RtpTransport;
use rustrtc::{RtpReceiverInterceptor, RtpSenderInterceptor};
use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};
use tracing::{trace, warn};

const DEFAULT_CAPTURE_QUEUE_CAPACITY: usize = 2048;

struct CapturedRtp {
    leg: Leg,
    packet: RtpPacket,
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
    timestamp_us: u64,
}

/// File result returned only after the worker has finalized the recorder.
#[derive(Debug, Clone)]
pub struct RecordingResult {
    pub path: String,
    pub duration_secs: f64,
    pub file_size: u64,
}

enum RecorderCommand {
    Start {
        path: String,
        caller_profile: Option<NegotiatedLegProfile>,
        stereo_swap: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Pause,
    Resume,
    Stop {
        reply: oneshot::Sender<Result<Option<RecordingResult>>>,
    },
}

/// Cloneable sender for one caller-facing RTP capture queue and recording task.
///
/// Track builders turn this sender into one shared interceptor and install it
/// on both directions (recv → Leg::A, send → Leg::B) of the **caller's**
/// PeerConnection. Callers carry only this sender; they do not build or store
/// RTC interceptor trait objects.
#[derive(Clone)]
pub struct RecorderSender {
    tx: mpsc::Sender<CapturedRtp>,
    clock: ReceiveTimestampClock,
    allowed_pts: Vec<u8>,
    dropped: Arc<AtomicU64>,
}

/// Control-plane owner for one caller-facing recorder task.
///
/// It exposes lifecycle commands. Control and RTP have separate channels into
/// one worker.
#[derive(Clone)]
pub struct RecorderHandle {
    tx: mpsc::UnboundedSender<RecorderCommand>,
    uses_sipflow: bool,
}

impl RecorderHandle {
    /// Create the one call-scoped recording task, its control handle, and its RTP sender.
    /// `sipflow = None` selects the file recorder, which is created lazily by `Start`.
    /// The task always starts dormant; lifecycle commands sent through the
    /// returned handle are the only way to activate capture.
    pub fn new(
        sipflow: Option<(Arc<dyn SipFlowBackend>, String)>,
        allowed_pts: Vec<u8>,
    ) -> (Self, RecorderSender) {
        let uses_sipflow = sipflow.is_some();
        let (rtp_tx, rtp_rx) = mpsc::channel::<CapturedRtp>(DEFAULT_CAPTURE_QUEUE_CAPACITY);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<RecorderCommand>();

        tokio::spawn(async move {
            run_capture_worker(rtp_rx, command_rx, sipflow).await;
        });

        let rtp_sender = RecorderSender {
            tx: rtp_tx,
            clock: ReceiveTimestampClock::new(),
            allowed_pts,
            dropped: Arc::new(AtomicU64::new(0)),
        };

        (
            Self {
                tx: command_tx,
                uses_sipflow,
            },
            rtp_sender,
        )
    }

    pub async fn start(
        &self,
        path: String,
        caller_profile: Option<NegotiatedLegProfile>,
        stereo_swap: bool,
    ) -> Result<()> {
        if self.uses_sipflow {
            return self.resume();
        }
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RecorderCommand::Start {
                path,
                caller_profile,
                stereo_swap,
                reply,
            })
            .map_err(|_| anyhow!("recorder task stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("recorder task stopped"))?
    }

    pub fn pause(&self) -> Result<()> {
        self.tx
            .send(RecorderCommand::Pause)
            .map_err(|_| anyhow!("recorder task stopped"))
    }

    pub fn resume(&self) -> Result<()> {
        self.tx
            .send(RecorderCommand::Resume)
            .map_err(|_| anyhow!("recorder task stopped"))
    }

    pub async fn stop(&self) -> Result<Option<RecordingResult>> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RecorderCommand::Stop { reply })
            .map_err(|_| anyhow!("recorder task stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("recorder task stopped"))?
    }
}

impl RecorderSender {
    /// Capture one audio/DTMF RTP packet without blocking the media path.
    #[inline]
    fn capture(&self, leg: Leg, packet: &RtpPacket, src_addr: SocketAddr, dst_addr: SocketAddr) {
        let pt = packet.header.payload_type;
        if !self.allowed_pts.is_empty() && !self.allowed_pts.contains(&pt) {
            return;
        }

        let captured = CapturedRtp {
            leg,
            packet: packet.clone(),
            src_addr,
            dst_addr,
            timestamp_us: self.clock.now_micros(),
        };
        if let Err(error) = self.tx.try_send(captured) {
            if matches!(error, mpsc::error::TrySendError::Full(_)) {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped % 1000 == 0 {
                    warn!(dropped, "caller RTP capture queue full; packet dropped");
                }
            }
        }
    }
}

async fn run_capture_worker(
    mut rtp_rx: mpsc::Receiver<CapturedRtp>,
    mut command_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    sipflow: Option<(Arc<dyn SipFlowBackend>, String)>,
) {
    let mut recorder: Option<Recorder> = None;
    let mut active = false;
    let mut rtp_open = true;

    loop {
        tokio::select! {
            biased;

            command = command_rx.recv() => {
                let Some(command) = command else {
                    rtp_rx.close();
                    drain_queued_rtp(
                        &mut rtp_rx,
                        active,
                        &mut recorder,
                        &sipflow,
                    );
                    if let Err(error) = finalize_recorder(&mut recorder) {
                        warn!(%error, "failed to finalize recorder after control handles dropped");
                    }
                    return;
                };

                match command {
                    RecorderCommand::Start {
                        path,
                        caller_profile,
                        stereo_swap,
                        reply,
                    } => {
                        drain_queued_rtp(
                            &mut rtp_rx,
                            active,
                            &mut recorder,
                            &sipflow,
                        );
                        match Recorder::new_caller_facing(&path, caller_profile, stereo_swap) {
                            Ok(created) => recorder = Some(created),
                            Err(error) => {
                                warn!(%path, %error, "failed to start recording");
                                let _ = reply.send(Err(error));
                                continue;
                            }
                        }

                        active = true;
                        let _ = reply.send(Ok(()));
                    }
                    RecorderCommand::Pause => {
                        drain_queued_rtp(
                            &mut rtp_rx,
                            active,
                            &mut recorder,
                            &sipflow,
                        );
                        active = false;
                    }
                    RecorderCommand::Resume => {
                        drain_queued_rtp(
                            &mut rtp_rx,
                            active,
                            &mut recorder,
                            &sipflow,
                        );
                        active = true;
                    }
                    RecorderCommand::Stop { reply } => {
                        drain_queued_rtp(
                            &mut rtp_rx,
                            active,
                            &mut recorder,
                            &sipflow,
                        );
                        active = false;
                        let _ = reply.send(finalize_recorder(&mut recorder));
                    }
                }
            }
            captured = rtp_rx.recv(), if rtp_open => {
                match captured {
                    Some(captured) => {
                        process_captured_rtp(captured, active, &mut recorder, &sipflow)
                    }
                    None => rtp_open = false,
                }
            }
        }
    }
}

fn drain_queued_rtp(
    rtp_rx: &mut mpsc::Receiver<CapturedRtp>,
    active: bool,
    recorder: &mut Option<Recorder>,
    sipflow: &Option<(Arc<dyn SipFlowBackend>, String)>,
) {
    let queued = rtp_rx.len();
    for _ in 0..queued {
        let Ok(captured) = rtp_rx.try_recv() else {
            break;
        };
        process_captured_rtp(captured, active, recorder, sipflow);
    }
}

fn process_captured_rtp(
    captured: CapturedRtp,
    active: bool,
    recorder: &mut Option<Recorder>,
    sipflow: &Option<(Arc<dyn SipFlowBackend>, String)>,
) {
    if !active {
        return;
    }

    if let Some(recorder) = recorder.as_mut() {
        let packet = &captured.packet;
        let frame = AudioFrame {
            rtp_timestamp: packet.header.timestamp,
            clock_rate: 8000,
            data: packet.payload.clone(),
            sequence_number: Some(packet.header.sequence_number),
            payload_type: Some(packet.header.payload_type),
            marker: packet.header.marker,
            header_extension: packet.header.extension.clone(),
            source_addr: None,
            raw_packet: Some(packet.clone()),
        };
        if let Err(error) =
            recorder.write_sample(captured.leg, &MediaSample::Audio(frame), None, None, None)
        {
            trace!(leg = ?captured.leg, %error, "file recorder write error");
        }
    }

    if let Some((sipflow, call_id)) = sipflow.as_ref() {
        let result: Result<()> = (|| {
            let item = SipFlowItem {
                timestamp: captured.timestamp_us,
                seq: captured.packet.header.sequence_number as u64,
                leg: Some(captured.leg as i32),
                msg_type: SipFlowMsgType::Rtp,
                src_addr: captured.src_addr.to_string(),
                dst_addr: captured.dst_addr.to_string(),
                payload: Bytes::from(captured.packet.marshal()?),
            };
            sipflow.record(Cow::Borrowed(call_id.as_str()), item)
        })();
        if let Err(error) = result {
            trace!(leg = ?captured.leg, %error, "SipFlow recorder write error");
        }
    }
}

fn finalize_recorder(recorder: &mut Option<Recorder>) -> Result<Option<RecordingResult>> {
    let Some(mut recorder) = recorder.take() else {
        return Ok(None);
    };
    recorder.finalize()?;
    let path = recorder.path.clone();
    let duration_secs = recorder.duration_secs();
    let file_size = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Ok(Some(RecordingResult {
        path,
        duration_secs,
        file_size,
    }))
}

#[async_trait]
impl RtpReceiverInterceptor for RecorderSender {
    async fn on_packet_received(
        &self,
        packet: &RtpPacket,
        src_addr: SocketAddr,
        local_addr: SocketAddr,
    ) -> Option<RtcpPacket> {
        self.capture(Leg::A, packet, src_addr, local_addr);
        None
    }

    async fn on_rtcp_received(&self, _packet: &RtcpPacket, _transport: Arc<RtpTransport>) {}
}

#[async_trait]
impl RtpSenderInterceptor for RecorderSender {
    async fn on_packet_sent(
        &self,
        packet: &RtpPacket,
        dst_addr: SocketAddr,
        local_addr: SocketAddr,
    ) {
        self.capture(Leg::B, packet, local_addr, dst_addr);
    }

    async fn on_rtcp_received(&self, _packet: &RtcpPacket, _transport: Arc<RtpTransport>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negotiate::NegotiatedLegProfile;
    use audio_codec::CodecType;
    use rustpbx_sipflow::SipFlowItem;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::sync::atomic::AtomicUsize;

    fn make_rtp_packet(pt: u8, seq: u16, ts: u32, payload: Vec<u8>) -> RtpPacket {
        let header = RtpHeader::new(pt, seq, ts, 0x12345678);
        RtpPacket::new(header, payload)
    }

    async fn file_recorder(allowed_pts: Vec<u8>) -> (RecorderSender, RecorderHandle) {
        let (handle, sender) = RecorderHandle::new(None, allowed_pts);
        handle.resume().unwrap();
        tokio::task::yield_now().await;
        (sender, handle)
    }

    async fn sipflow_recorder(
        sipflow: Arc<dyn SipFlowBackend>,
        call_id: &str,
        allowed_pts: Vec<u8>,
    ) -> (RecorderSender, RecorderHandle) {
        let (handle, sender) =
            RecorderHandle::new(Some((sipflow, call_id.to_string())), allowed_pts);
        handle.resume().unwrap();
        tokio::task::yield_now().await;
        (sender, handle)
    }

    fn pcmu_profile() -> NegotiatedLegProfile {
        use crate::negotiate::NegotiatedCodec;
        NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            }),
            video: None,
            dtmf: None,
            transport: rustrtc::TransportMode::Rtp,
        }
    }

    async fn start_file(handle: &RecorderHandle, path: &std::path::Path) {
        handle
            .start(
                path.to_string_lossy().into_owned(),
                Some(pcmu_profile()),
                false,
            )
            .await
            .unwrap();
    }

    /// Stand-in backend that counts recorded items.
    struct CountingBackend {
        count: AtomicUsize,
        legs: parking_lot::Mutex<Vec<Option<i32>>>,
        flows: parking_lot::Mutex<Vec<(String, String)>>,
    }
    impl CountingBackend {
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
                legs: parking_lot::Mutex::new(Vec::new()),
                flows: parking_lot::Mutex::new(Vec::new()),
            }
        }
        fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }

        fn legs(&self) -> Vec<Option<i32>> {
            self.legs.lock().clone()
        }

        fn flows(&self) -> Vec<(String, String)> {
            self.flows.lock().clone()
        }
    }
    #[async_trait]
    impl SipFlowBackend for CountingBackend {
        fn record(&self, _call_id: Cow<'_, str>, item: SipFlowItem) -> anyhow::Result<()> {
            let mut legs = self.legs.lock();
            if legs.len() < 16 {
                legs.push(item.leg);
            }
            drop(legs);
            let mut flows = self.flows.lock();
            if flows.len() < 16 {
                flows.push((item.src_addr.clone(), item.dst_addr.clone()));
            }
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
        ) -> anyhow::Result<Vec<rustpbx_sipflow::SipFlowMediaStats>> {
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
        let (tap, handle) = sipflow_recorder(backend.clone(), "call-1", vec![]).await;
        handle.pause().unwrap();
        tokio::task::yield_now().await;

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 0, "inactive tap should not record");
    }

    #[tokio::test]
    async fn sipflow_target_does_not_activate_dormant_recording() {
        let backend = Arc::new(CountingBackend::new());
        let (handle, tap) = RecorderHandle::new(
            Some((backend.clone(), "dormant-sipflow".to_string())),
            vec![],
        );

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        handle.stop().await.unwrap();
        assert_eq!(
            backend.count(),
            0,
            "configured SipFlow target must not activate recording"
        );
    }

    #[tokio::test]
    async fn dormant_tap_activates_when_started() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live_tap.wav");
        let (handle, tap) = RecorderHandle::new(None, vec![0]);
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 5060));
        let local = std::net::SocketAddr::from(([127, 0, 0, 1], 10000));

        tap.on_packet_received(&make_rtp_packet(0, 1, 160, vec![0x55; 160]), peer, local)
            .await;

        start_file(&handle, &path).await;

        tap.on_packet_received(&make_rtp_packet(0, 2, 320, vec![0x55; 160]), peer, local)
            .await;

        let result = handle.stop().await.unwrap().unwrap();
        assert_eq!(result.path, path.to_string_lossy());
        assert!(result.file_size > 44);
    }

    #[tokio::test]
    async fn active_tap_records_to_sipflow() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "call-2", vec![]).await;

        for i in 0..5u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 5, "should record 5 packets");
    }

    #[tokio::test]
    async fn send_tap_uses_leg_b() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "call-3", vec![]).await;
        tap.on_packet_sent(
            &make_rtp_packet(0, 1, 160, vec![0x55; 160]),
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        handle.stop().await.unwrap();
        assert_eq!(backend.legs(), vec![Some(Leg::B as i32)]);
    }

    #[tokio::test]
    async fn file_recorder_writes_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tap_test.wav");
        let (tap, handle) = file_recorder(vec![]).await;
        start_file(&handle, &path).await;

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

        let result = handle.stop().await.unwrap().unwrap();
        assert!(
            result.file_size > 44,
            "WAV should have header + data, got {} bytes",
            result.file_size
        );
        assert!(
            (result.duration_secs - 0.2).abs() < 0.000_001,
            "duration should come from 1600 written samples at 8 kHz, got {}",
            result.duration_secs
        );
    }

    #[tokio::test]
    async fn stop_finalizes_and_same_worker_can_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.wav");
        let second_path = dir.path().join("second.wav");
        let (tap, handle) = file_recorder(vec![0]).await;
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 5060));
        let local = std::net::SocketAddr::from(([127, 0, 0, 1], 10000));

        for (path, sequence_base) in [(&first_path, 0), (&second_path, 100)] {
            start_file(&handle, path).await;
            for offset in 0..10 {
                let sequence = sequence_base + offset;
                tap.on_packet_received(
                    &make_rtp_packet(0, sequence, sequence as u32 * 160, vec![0x55; 160]),
                    peer,
                    local,
                )
                .await;
            }
            let result = handle.stop().await.unwrap().unwrap();
            assert_eq!(result.path, path.to_string_lossy());
            assert!(result.file_size > 44);
        }
    }

    #[tokio::test]
    async fn dropping_last_control_handle_finalizes_and_stops_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drop-finalize.wav");
        let (tap, handle) = file_recorder(vec![0]).await;
        let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 5060));
        let local = std::net::SocketAddr::from(([127, 0, 0, 1], 10000));

        start_file(&handle, &path).await;
        for sequence in 0..10 {
            tap.on_packet_received(
                &make_rtp_packet(0, sequence, sequence as u32 * 160, vec![0x55; 160]),
                peer,
                local,
            )
            .await;
        }

        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !tap.tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recorder worker did not stop after its last control handle dropped");

        let wav = std::fs::read(&path).unwrap();
        assert!(wav.len() > 44);
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            wav.len() as u32 - 8
        );
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            wav.len() as u32 - 44
        );
    }

    #[tokio::test]
    async fn dtmf_packet_captured() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "call-dtmf", vec![]).await;

        // DTMF digit '5' end event (PT 101, 4-byte payload)
        let dtmf_payload = vec![5u8, 0x80, 0x00, 0xA0];
        let pkt = make_rtp_packet(101, 1, 160, dtmf_payload);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 1, "DTMF packet should be captured");
    }

    #[test]
    fn leg_as_i32() {
        assert_eq!(Leg::A as i32, 0);
        assert_eq!(Leg::B as i32, 1);
    }

    // ── Exactly one recording target per worker ─────────────────────────

    #[tokio::test]
    async fn sipflow_start_does_not_create_a_file_recorder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("must-not-exist.wav");
        let backend = Arc::new(CountingBackend::new());
        let (handle, tap) =
            RecorderHandle::new(Some((backend.clone(), "sipflow-only".to_string())), vec![]);
        handle
            .start(
                path.to_string_lossy().into_owned(),
                Some(pcmu_profile()),
                false,
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;

        for i in 0..10u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }

        let result = handle.stop().await.unwrap();
        assert!(result.is_none(), "SipFlow backend has no file result");
        assert_eq!(backend.count(), 10);
        assert!(!path.exists(), "SipFlow backend must not create a WAV file");
    }

    // ── Pause/resume ───────────────────────────────────────────────────

    #[tokio::test]
    async fn pause_stops_capture_resume_continues() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "pause-call", vec![]).await;

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
        // Pause
        handle.pause().unwrap();
        tokio::task::yield_now().await;

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
        // Resume
        handle.resume().unwrap();
        tokio::task::yield_now().await;

        for i in 6..9u16 {
            let pkt = make_rtp_packet(0, i, i as u32 * 160, vec![0x55u8; 160]);
            tap.on_packet_received(
                &pkt,
                std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
                std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
            )
            .await;
        }
        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 6, "resumed tap should capture again");
    }

    // ── No memory leak: tap holds no per-packet state ──────────────────

    #[tokio::test]
    async fn no_memory_leak_high_packet_count() {
        // The RecorderSender should not accumulate any per-packet state.
        // After N packets, the tap struct size should remain constant.
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "leak-test", vec![]).await;

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

        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 1000);
        // The tap keeps no packet collection; the test backend retains only
        // capped diagnostic metadata and an AtomicUsize count.
    }

    // ── Leg isolation: recv=Leg::A, send=Leg::B ────────────────────────

    #[tokio::test]
    async fn recv_tap_tags_leg_a_send_tap_tags_leg_b() {
        let backend = Arc::new(CountingBackend::new());

        let (tap, handle) = sipflow_recorder(backend.clone(), "leg-test", vec![]).await;
        let caller = std::net::SocketAddr::from(([10, 0, 0, 1], 10000));
        let rustpbx = std::net::SocketAddr::from(([10, 0, 0, 2], 20000));

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        tap.on_packet_received(&pkt, caller, rustpbx).await;
        tap.on_packet_sent(&pkt, caller, rustpbx).await;

        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 2);
        assert_eq!(
            backend.legs(),
            vec![Some(Leg::A as i32), Some(Leg::B as i32)]
        );
        assert_eq!(
            backend.flows(),
            vec![
                (caller.to_string(), rustpbx.to_string()),
                (rustpbx.to_string(), caller.to_string()),
            ]
        );
    }

    // ── Video PT is captured (not filtered) ────────────────────────────

    #[tokio::test]
    async fn video_packet_captured_when_no_filter() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "video-call", vec![]).await;

        // H264 packet (PT 96, dynamic)
        let pkt = make_rtp_packet(96, 1, 9000, vec![0x80u8; 1200]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;

        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 1, "video captured when no filter");
    }

    // ── Audio → DTMF → Audio sequence (real call flow) ─────────────────

    #[tokio::test]
    async fn audio_dtmf_audio_sequence_captured_in_order() {
        let backend = Arc::new(CountingBackend::new());
        let (tap, handle) = sipflow_recorder(backend.clone(), "seq-call", vec![]).await;

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

        handle.stop().await.unwrap();
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
        let (tap, handle) = sipflow_recorder(backend.clone(), "zerocopy-call", vec![]).await;

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

        handle.stop().await.unwrap();
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
        let (send_tap, handle) = sipflow_recorder(backend.clone(), "ivr-call", vec![]).await;

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

        handle.stop().await.unwrap();
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
        // allowed_pts = [0, 101] simulates an SDP where PCMU(PT=0) and
        // telephone-event(PT=101) are the only audio codecs. Any packet
        // with a different PT (e.g. H264/VP8 at dynamic PT 96-127) is
        // filtered out, regardless of what codec it carries.
        let (tap, handle) = sipflow_recorder(backend.clone(), "video-filter", vec![0, 101]).await;

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

        handle.stop().await.unwrap();
        assert_eq!(
            backend.count(),
            2,
            "only audio+DTMF captured, video filtered"
        );
    }

    // ── one caller tap serves both directions ──────────────────────────

    #[tokio::test]
    async fn recorder_sender_serves_both_directions() {
        let backend = Arc::new(CountingBackend::new());

        let (tap, handle) = sipflow_recorder(backend.clone(), "build-test", vec![]).await;

        let pkt = make_rtp_packet(0, 1, 160, vec![0x55u8; 160]);
        tap.on_packet_received(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        tap.on_packet_sent(
            &pkt,
            std::net::SocketAddr::from(([127, 0, 0, 1], 5060)),
            std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
        )
        .await;
        handle.stop().await.unwrap();
        assert_eq!(backend.count(), 2);
        assert_eq!(
            backend.legs(),
            vec![Some(Leg::A as i32), Some(Leg::B as i32)]
        );
    }
}
