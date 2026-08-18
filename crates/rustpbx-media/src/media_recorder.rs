//! One call-scoped recording task for caller-facing RTP capture.
//!
//! The caller leg owns a lightweight [`RecorderSender`]. [`MediaBridge`]
//! owns the task control handle and installs one [`MediaRecorder`] backend at
//! a time. File and Sipflow recording therefore share the same RTP queue and
//! task lifecycle without exposing recorder mutation on the RTP hot path.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use rustpbx_sipflow::{SipFlowBackend, SipFlowItem as BackendSipFlowItem, SipFlowMsgType};
use rustrtc::media::frame::{AudioFrame, MediaSample};
use rustrtc::rtp::RtpPacket;
use tokio::sync::{mpsc, oneshot};
use tracing::{trace, warn};

use crate::ingress_tap::PacketDirection;
use crate::negotiate::NegotiatedLegProfile;
use crate::recorder::{Leg, Recorder};

const DEFAULT_CAPTURE_QUEUE_CAPACITY: usize = 2048;

pub(crate) struct CapturedRtp {
    pub(crate) direction: PacketDirection,
    pub(crate) packet: RtpPacket,
    pub(crate) received_at_micros: u64,
}

/// Result produced after a file recorder has drained queued RTP and rewritten
/// the WAV header with its final data size.
#[derive(Debug, Clone)]
pub struct RecordingResult {
    pub path: String,
    pub duration_secs: f64,
    pub file_size: u64,
}

/// A backend exclusively owned and driven by the call's recording task.
///
/// It does not need `Sync`: no RTP transport calls it directly. File setup,
/// packet processing, and finalization all execute serially inside the task.
#[async_trait]
pub trait MediaRecorder: Send {
    /// Perform backend setup inside the recording task.
    async fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    async fn write_rtp(
        &mut self,
        direction: PacketDirection,
        packet: &RtpPacket,
        received_at_micros: u64,
    ) -> Result<()>;

    /// Finalize the backend. File recorders return metadata; streaming
    /// recorders return `None` after flushing.
    async fn finalize(self: Box<Self>) -> Result<Option<RecordingResult>>;
}

/// Synchronous file-recorder configuration. File creation and WAV header
/// initialization happen later in [`MediaRecorder::initialize`] on the task.
pub struct FileRecorder {
    path: String,
    caller_profile: NegotiatedLegProfile,
    channels: u16,
    mono_caller_only: bool,
    recorder: Option<Recorder>,
}

impl FileRecorder {
    pub fn new(
        path: impl Into<String>,
        caller_profile: NegotiatedLegProfile,
        channels: u16,
        mono_caller_only: bool,
    ) -> Self {
        Self {
            path: path.into(),
            caller_profile,
            channels,
            mono_caller_only,
            recorder: None,
        }
    }
}

#[async_trait]
impl MediaRecorder for FileRecorder {
    async fn initialize(&mut self) -> Result<()> {
        let output_codec = self
            .caller_profile
            .audio
            .as_ref()
            .map(|codec| codec.codec)
            .unwrap_or(audio_codec::CodecType::PCMU);
        let mut recorder = Recorder::new_with_channels(
            &self.path,
            output_codec,
            self.channels,
            self.mono_caller_only,
        )
        .await?;
        recorder.set_profile(self.caller_profile.clone());
        self.recorder = Some(recorder);
        Ok(())
    }

    async fn write_rtp(
        &mut self,
        direction: PacketDirection,
        packet: &RtpPacket,
        _received_at_micros: u64,
    ) -> Result<()> {
        let recorder = self
            .recorder
            .as_mut()
            .ok_or_else(|| anyhow!("file recorder is not initialized"))?;
        let frame = AudioFrame {
            rtp_timestamp: packet.header.timestamp,
            clock_rate: 0,
            data: packet.payload.clone(),
            sequence_number: Some(packet.header.sequence_number),
            payload_type: Some(packet.header.payload_type),
            marker: packet.header.marker,
            header_extension: None,
            source_addr: None,
            raw_packet: Some(packet.clone()),
        };
        recorder
            .write_sample(
                direction_to_leg(direction),
                &MediaSample::Audio(frame),
                None,
                None,
                None,
            )
            .await
    }

    async fn finalize(mut self: Box<Self>) -> Result<Option<RecordingResult>> {
        let mut recorder = self
            .recorder
            .take()
            .ok_or_else(|| anyhow!("file recorder is not initialized"))?;
        recorder.finalize().await?;
        let path = recorder.path.clone();
        let file_size = tokio::fs::metadata(&path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(Some(RecordingResult {
            path,
            duration_secs: recorder.duration_secs(),
            file_size,
        }))
    }
}

/// Sipflow backend driven by the same serialized recording task as files.
pub struct SipflowRecorder {
    backend: Arc<dyn SipFlowBackend>,
    call_id: String,
}

impl SipflowRecorder {
    pub fn new(backend: Arc<dyn SipFlowBackend>, call_id: impl Into<String>) -> Self {
        Self {
            backend,
            call_id: call_id.into(),
        }
    }
}

#[async_trait]
impl MediaRecorder for SipflowRecorder {
    async fn write_rtp(
        &mut self,
        direction: PacketDirection,
        packet: &RtpPacket,
        received_at_micros: u64,
    ) -> Result<()> {
        let raw = packet.marshal()?;
        self.backend.record(
            Cow::Borrowed(self.call_id.as_str()),
            BackendSipFlowItem {
                timestamp: received_at_micros,
                seq: packet.header.sequence_number as u64,
                leg: Some(direction_to_leg_id(direction)),
                msg_type: SipFlowMsgType::Rtp,
                src_addr: "synth".to_string(),
                dst_addr: String::new(),
                payload: Bytes::from(raw),
            },
        )
    }

    async fn finalize(self: Box<Self>) -> Result<Option<RecordingResult>> {
        Ok(None)
    }
}

enum RecorderCommand {
    SetRecorder {
        recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
        reply: oneshot::Sender<Result<()>>,
    },
    HasRecorder {
        reply: oneshot::Sender<bool>,
    },
    Pause,
    Resume,
    Stop,
}

/// Control handle owned by [`crate::media_bridge::MediaBridge`].
pub(crate) struct RecorderHandle {
    tx: mpsc::UnboundedSender<RecorderCommand>,
}

/// Non-blocking media-path sender installed on the caller leg's RTP tap.
pub struct RecorderSender {
    tx: mpsc::Sender<CapturedRtp>,
    dropped: AtomicU64,
}

pub type RecordingCompletion = Result<Option<RecordingResult>>;

impl RecorderHandle {
    pub(crate) fn new() -> (
        Self,
        RecorderSender,
        tokio::task::JoinHandle<RecordingCompletion>,
    ) {
        let (rtp_tx, rtp_rx) = mpsc::channel(DEFAULT_CAPTURE_QUEUE_CAPACITY);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let join = tokio::spawn(async move {
            let result = RecorderTask::new(rtp_rx, command_rx).run().await;
            if let Err(error) = &result {
                warn!(%error, "recording task failed");
            }
            result
        });
        let sender = RecorderSender::new(rtp_tx);
        (Self { tx: command_tx }, sender, join)
    }

    pub(crate) async fn has_recorder(&self) -> bool {
        let (reply, response) = oneshot::channel();
        if self
            .tx
            .send(RecorderCommand::HasRecorder { reply })
            .is_err()
        {
            return false;
        }
        response.await.unwrap_or(false)
    }

    pub(crate) async fn set_recorder(
        &self,
        recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RecorderCommand::SetRecorder {
                recorder,
                max_duration,
                reply,
            })
            .map_err(|_| anyhow!("recording task stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("recording task stopped"))?
    }

    pub(crate) fn pause(&self) -> Result<()> {
        self.tx
            .send(RecorderCommand::Pause)
            .map_err(|_| anyhow!("recording task stopped"))
    }

    pub(crate) fn resume(&self) -> Result<()> {
        self.tx
            .send(RecorderCommand::Resume)
            .map_err(|_| anyhow!("recording task stopped"))
    }

    pub(crate) fn stop(&self) -> Result<()> {
        self.tx
            .send(RecorderCommand::Stop)
            .map_err(|_| anyhow!("recording task stopped"))
    }
}

impl RecorderSender {
    pub(crate) fn new(tx: mpsc::Sender<CapturedRtp>) -> Self {
        Self {
            tx,
            dropped: AtomicU64::new(0),
        }
    }

    #[inline]
    pub(crate) fn capture(&self, direction: PacketDirection, packet: &RtpPacket) {
        let received_at_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_micros() as u64)
            .unwrap_or_default();
        let captured = CapturedRtp {
            direction,
            packet: packet.clone(),
            received_at_micros,
        };
        if let Err(error) = self.tx.try_send(captured)
            && matches!(error, mpsc::error::TrySendError::Full(_))
        {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped % 1000 == 0 {
                warn!(dropped, "recording RTP queue full; packet dropped");
            }
        }
    }

    pub fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket) {
        self.capture(direction, packet);
    }
}

struct RecorderTask {
    rtp_rx: mpsc::Receiver<CapturedRtp>,
    command_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    recorder: Option<Box<dyn MediaRecorder>>,
    paused: bool,
    deadline: Option<tokio::time::Instant>,
    rtp_open: bool,
}

impl RecorderTask {
    fn new(
        rtp_rx: mpsc::Receiver<CapturedRtp>,
        command_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    ) -> Self {
        Self {
            rtp_rx,
            command_rx,
            recorder: None,
            paused: false,
            deadline: None,
            rtp_open: true,
        }
    }

    async fn run(mut self) -> RecordingCompletion {
        loop {
            tokio::select! {
                biased;
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        return self.finalize_recorder().await;
                    };
                    match command {
                        RecorderCommand::SetRecorder {
                            recorder,
                            max_duration,
                            reply,
                        } => {
                            let result = self.set_recorder(recorder, max_duration).await;
                            let _ = reply.send(result);
                        }
                        RecorderCommand::HasRecorder { reply } => {
                            let _ = reply.send(self.recorder.is_some());
                        }
                        RecorderCommand::Pause => self.paused = true,
                        RecorderCommand::Resume => self.paused = false,
                        RecorderCommand::Stop => return self.finalize_recorder().await,
                    }
                }
                _ = wait_for_deadline(self.deadline), if self.deadline.is_some() => {
                    return self.finalize_recorder().await;
                }
                captured = self.rtp_rx.recv(), if self.rtp_open => {
                    match captured {
                        Some(captured) => self.write_rtp(&captured).await,
                        None => self.rtp_open = false,
                    }
                }
            }
        }
    }

    async fn set_recorder(
        &mut self,
        mut recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        recorder.initialize().await?;
        if let Some(previous) = self.recorder.take() {
            previous.finalize().await?;
        }
        self.recorder = Some(recorder);
        self.paused = false;
        self.deadline = max_duration.map(|duration| tokio::time::Instant::now() + duration);
        Ok(())
    }

    async fn write_rtp(&mut self, captured: &CapturedRtp) {
        if self.paused {
            return;
        }
        if let Some(recorder) = self.recorder.as_mut()
            && let Err(error) = recorder
                .write_rtp(
                    captured.direction,
                    &captured.packet,
                    captured.received_at_micros,
                )
                .await
        {
            trace!(%error, "recorder write error");
        }
    }

    async fn finalize_recorder(&mut self) -> RecordingCompletion {
        self.deadline = None;
        let queued = self.rtp_rx.len();
        for _ in 0..queued {
            let Ok(captured) = self.rtp_rx.try_recv() else {
                break;
            };
            self.write_rtp(&captured).await;
        }
        self.paused = false;
        match self.recorder.take() {
            Some(recorder) => recorder.finalize().await,
            None => Ok(None),
        }
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn direction_to_leg(direction: PacketDirection) -> Leg {
    match direction {
        PacketDirection::Ingress => Leg::A,
        PacketDirection::Egress => Leg::B,
    }
}

fn direction_to_leg_id(direction: PacketDirection) -> i32 {
    match direction {
        PacketDirection::Ingress => 0,
        PacketDirection::Egress => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Local};
    use rustpbx_sipflow::SipFlowMediaStats;
    use rustrtc::rtp::{RtpHeader, RtpPacket};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn packet(pt: u8, sequence: u16, timestamp: u32) -> RtpPacket {
        RtpPacket::new(
            RtpHeader::new(pt, sequence, timestamp, 1234),
            vec![0xFF; 80],
        )
    }

    fn profile() -> NegotiatedLegProfile {
        use crate::negotiate::NegotiatedCodec;
        NegotiatedLegProfile {
            audio: Some(NegotiatedCodec {
                codec: audio_codec::CodecType::PCMU,
                payload_type: 0,
                clock_rate: 8000,
                channels: 1,
            }),
            ..Default::default()
        }
    }

    struct CountingBackend {
        recorded: AtomicUsize,
        flushed: AtomicBool,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                recorded: AtomicUsize::new(0),
                flushed: AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl SipFlowBackend for CountingBackend {
        fn record(&self, _call_id: Cow<'_, str>, _item: BackendSipFlowItem) -> Result<()> {
            self.recorded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn flush(&self) -> Result<()> {
            self.flushed.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn query_flow(
            &self,
            _call_id: &str,
            _start_time: DateTime<Local>,
            _end_time: DateTime<Local>,
        ) -> Result<Vec<BackendSipFlowItem>> {
            Ok(Vec::new())
        }

        async fn query_media_stats(
            &self,
            _call_id: &str,
            _start_time: DateTime<Local>,
            _end_time: DateTime<Local>,
        ) -> Result<Vec<SipFlowMediaStats>> {
            Ok(Vec::new())
        }

        async fn query_media(
            &self,
            _call_id: &str,
            _start_time: DateTime<Local>,
            _end_time: DateTime<Local>,
        ) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn file_stop_returns_task_result() {
        let (handle, sender, join) = RecorderHandle::new();
        assert!(!handle.has_recorder().await);
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        handle
            .set_recorder(
                Box::new(FileRecorder::new(path.clone(), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();
        assert!(handle.has_recorder().await);
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));
        handle.stop().unwrap();
        let result = join.await.unwrap().unwrap().unwrap();
        assert!(!handle.has_recorder().await);
        assert_eq!(result.path, path);
        assert!(result.file_size > 44);

        drop(handle);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn max_duration_returns_task_result() {
        let (handle, sender, join) = RecorderHandle::new();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        handle
            .set_recorder(
                Box::new(FileRecorder::new(path.clone(), profile(), 2, false)),
                Some(Duration::from_millis(20)),
            )
            .await
            .unwrap();
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));

        let result = tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!handle.has_recorder().await);
        assert_eq!(result.path, path);
        assert!(result.file_size > 44);

        drop(handle);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn sipflow_does_not_flush_shared_backend_when_handle_is_dropped() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, sender, _join) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));
        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.recorded.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(backend.recorded.load(Ordering::SeqCst), 1);
        assert!(!backend.flushed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stop_without_file_recorder_finishes_with_no_file_result() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, sender, join) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();

        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.recorded.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(backend.recorded.load(Ordering::SeqCst), 1);
        handle.stop().unwrap();
        assert!(join.await.unwrap().unwrap().is_none());
        assert!(!backend.flushed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn setting_file_recorder_finalizes_previous_sipflow_recorder() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, _sender, join) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        handle
            .set_recorder(
                Box::new(FileRecorder::new(path.clone(), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();
        assert!(!backend.flushed.load(Ordering::SeqCst));

        handle.stop().unwrap();
        let result = join.await.unwrap().unwrap().unwrap();
        assert_eq!(result.path, path);
        drop(handle);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn direction_mapping_is_stable() {
        assert_eq!(direction_to_leg(PacketDirection::Ingress), Leg::A);
        assert_eq!(direction_to_leg(PacketDirection::Egress), Leg::B);
        assert_eq!(direction_to_leg_id(PacketDirection::Ingress), 0);
        assert_eq!(direction_to_leg_id(PacketDirection::Egress), 1);
    }
}
