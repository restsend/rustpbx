//! One call-scoped recording task for caller-facing RTP capture.
//!
//! The caller leg owns a lightweight [`RecorderSender`]. [`RecordingSession`]
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
use crate::recorder::{Leg, Recorder, RecorderOption};

// 256 slots ≈ 2.5 s of audio at 100 pkt/s per call. Large pre-allocated
// queues (2048) cost ~150 KB per recording call and add malloc churn at
// high concurrency; overflow only drops recording packets, never media.
const DEFAULT_CAPTURE_QUEUE_CAPACITY: usize = 256;

/// Dedicated runtime for recorder tasks, set once at startup by the app
/// (rustpbx wires it to the media runtime). Recording tasks wake once per
/// captured packet (~100/s per call); at thousands of concurrent recordings
/// that load must not compete with latency-sensitive SIP transaction
/// processing on the SIP runtime. Falls back to the ambient runtime when
/// unset (tests, embedders).
static RECORDER_RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Set the runtime on which recorder tasks are spawned. Must be called
/// before the first recording starts.
pub fn set_recorder_runtime(handle: tokio::runtime::Handle) {
    let _ = RECORDER_RUNTIME.set(handle);
}

fn recorder_runtime_or_current() -> tokio::runtime::Handle {
    RECORDER_RUNTIME
        .get()
        .cloned()
        .unwrap_or_else(|| tokio::runtime::Handle::current())
}

pub(crate) struct CapturedRtp {
    pub(crate) profile: Option<Arc<NegotiatedLegProfile>>,
    pub(crate) channel: CaptureChannel,
    pub(crate) packet: RtpPacket,
    pub(crate) received_at_micros: u64,
    pub(crate) peer_addr: Option<std::net::SocketAddr>,
}

/// Which recording channel a captured packet belongs to.
///
/// The tap that observes a leg decides this at capture time: the caller leg
/// maps ingress→[`CaptureChannel::Caller`] / egress→[`CaptureChannel::Callee`]
/// (the classic bridged-call layout), while the callee leg contributes its
/// ingress (the agent mic — the live audio the bridge no longer relays while
/// the call is held) as [`CaptureChannel::Callee`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureChannel {
    /// Caller-side (leg A) audio.
    Caller,
    /// Callee-side (leg B) audio.
    Callee,
}

impl CaptureChannel {
    /// Channel recorded by a leg-observed packet travelling in `direction`
    /// (the classic caller-tap mapping).
    pub fn for_direction(direction: PacketDirection) -> Self {
        match direction {
            PacketDirection::Ingress => Self::Caller,
            PacketDirection::Egress => Self::Callee,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Caller),
            2 => Some(Self::Callee),
            _ => None,
        }
    }

    pub(crate) fn code(&self) -> u8 {
        match self {
            Self::Caller => 1,
            Self::Callee => 2,
        }
    }
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
    /// Local output path when this recorder produces a file.
    fn file_path(&self) -> Option<&str> {
        None
    }

    /// Perform backend setup inside the recording task.
    async fn initialize(&mut self) -> Result<()> {
        Ok(())
    }

    /// Resume the file timeline without counting deliberately paused time.
    fn resume(&mut self) {}

    async fn write_rtp(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<()>;

    /// Captures from another leg retain that leg's negotiated payload mapping.
    /// Backends that preserve raw RTP can keep the original entry point.
    async fn write_rtp_with_profile(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
        _profile: Option<&NegotiatedLegProfile>,
    ) -> Result<()> {
        self.write_rtp(channel, packet, received_at_micros, peer_addr)
            .await
    }

    /// Finalize the backend. File recorders return metadata; streaming
    /// recorders return `None` after flushing.
    async fn finalize(self: Box<Self>) -> Result<Option<RecordingResult>>;
}


/// Synchronous file-recorder configuration. File creation and WAV header
/// initialization happen later in [`MediaRecorder::initialize`] on the task.
pub struct FileRecorder {
    option: RecorderOption,
    caller_profile: NegotiatedLegProfile,
    channels: u16,
    mono_caller_only: bool,
    recorder: Option<Recorder>,
}

impl FileRecorder {
    pub fn new(
        option: RecorderOption,
        caller_profile: NegotiatedLegProfile,
        channels: u16,
        mono_caller_only: bool,
    ) -> Self {
        Self {
            option,
            caller_profile,
            channels,
            mono_caller_only,
            recorder: None,
        }
    }
}

#[async_trait]
impl MediaRecorder for FileRecorder {
    fn file_path(&self) -> Option<&str> {
        Some(&self.option.recorder_file)
    }

    fn resume(&mut self) {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.resume_capture_clock();
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        let output_codec = self
            .caller_profile
            .audio
            .as_ref()
            .map(|codec| codec.codec)
            .unwrap_or(audio_codec::CodecType::PCMU);
        let mut recorder = Recorder::new_with_channels(
            &self.option,
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
        channel: CaptureChannel,
        packet: &RtpPacket,
        _received_at_micros: u64,
        _peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<()> {
        self.write_rtp_with_profile(channel, packet, _received_at_micros, _peer_addr, None)
            .await
    }

    async fn write_rtp_with_profile(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        _peer_addr: Option<std::net::SocketAddr>,
        profile: Option<&NegotiatedLegProfile>,
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
            .write_sample_with_profile(
                match channel {
                    CaptureChannel::Caller => Leg::A,
                    CaptureChannel::Callee => Leg::B,
                },
                &MediaSample::Audio(frame),
                None,
                None,
                None,
                profile,
                received_at_micros,
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
    /// Swap leg ids at capture: `true` writes the callee (egress) side as
    /// leg 0 and the caller (ingress) side as leg 1. Exported stereo WAVs
    /// map leg 0 → left, so this is how `stereo_mode = "external_left"` /
    /// `"external_right"` is honoured for RTP-captured (sipflow) recordings
    /// without touching the export path.
    swap_legs: bool,
}

impl SipflowRecorder {
    pub fn new(backend: Arc<dyn SipFlowBackend>, call_id: impl Into<String>) -> Self {
        Self {
            backend,
            call_id: call_id.into(),
            swap_legs: false,
        }
    }

    /// Flip leg ids so the callee side lands on leg 0 (the left channel).
    pub fn with_swapped_legs(mut self, swap: bool) -> Self {
        self.swap_legs = swap;
        self
    }
}

#[async_trait]
impl MediaRecorder for SipflowRecorder {
    async fn write_rtp(
        &mut self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        received_at_micros: u64,
        peer_addr: Option<std::net::SocketAddr>,
    ) -> Result<()> {
        let raw = packet.marshal()?;
        let leg = match channel {
            CaptureChannel::Caller => 0,
            CaptureChannel::Callee => 1,
        };
        let leg = if self.swap_legs { 1 - leg } else { leg };
        self.backend.record(
            Cow::Borrowed(self.call_id.as_str()),
            BackendSipFlowItem {
                timestamp: received_at_micros,
                seq: packet.header.sequence_number as u64,
                leg: Some(leg),
                msg_type: SipFlowMsgType::Rtp,
                // Media storage uses `src` as the per-direction peer key;
                // leg 0/1 identifies ingress/egress, including relay traffic.
                src_addr: peer_addr.map(|addr| addr.to_string()).unwrap_or_default(),
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
    QueryStatus {
        reply: oneshot::Sender<RecorderStatus>,
    },
    Pause,
    Resume,
    StopRecorder {
        reply: oneshot::Sender<RecordingCompletion>,
    },
}

/// Control handle owned by [`RecordingSession`].
pub(crate) struct RecorderHandle {
    tx: mpsc::UnboundedSender<RecorderCommand>,
}

/// Non-blocking media-path sender installed on the tapped legs' RTP
/// transports. Cloneable: every tap of the call (caller + callee legs) pushes
/// into the same recording task.
pub struct RecorderSender {
    tx: mpsc::Sender<CapturedRtp>,
    dropped: Arc<AtomicU64>,
}

pub type RecordingCompletion = Result<Option<RecordingResult>>;

/// Recorder state exposed to session-control adapters such as RWI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecorderStatus {
    /// Whether the task currently owns an initialized recorder.
    pub active: bool,
    /// Whether RTP writes to the current recorder are paused.
    pub paused: bool,
    /// Local file path owned by the active recorder, when it writes a file.
    pub file_path: Option<String>,
}

impl RecorderHandle {
    pub(crate) fn new() -> (
        Self,
        RecorderSender,
        mpsc::UnboundedReceiver<RecordingCompletion>,
    ) {
        let (rtp_tx, rtp_rx) = mpsc::channel(DEFAULT_CAPTURE_QUEUE_CAPACITY);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (recorder_finished_tx, recorder_finished_rx) = mpsc::unbounded_channel();
        let runtime = recorder_runtime_or_current();
        runtime.spawn(async move {
            RecorderTask::new(rtp_rx, command_rx, recorder_finished_tx)
                .run()
                .await;
        });
        let sender = RecorderSender::new(rtp_tx);
        (Self { tx: command_tx }, sender, recorder_finished_rx)
    }

    pub(crate) async fn status(&self) -> Result<RecorderStatus> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RecorderCommand::QueryStatus { reply })
            .map_err(|_| anyhow!("recording task stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("recording task stopped"))
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

    pub(crate) async fn stop_recorder(&self) -> RecordingCompletion {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(RecorderCommand::StopRecorder { reply })
            .map_err(|_| anyhow!("recording task stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("recording task stopped"))?
    }
}

impl RecorderSender {
    pub(crate) fn new(tx: mpsc::Sender<CapturedRtp>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    #[inline]
    pub(crate) fn capture(
        &self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        peer_addr: Option<std::net::SocketAddr>,
    ) {
        self.capture_with_profile(channel, packet, peer_addr, None);
    }

    pub(crate) fn capture_with_profile(
        &self,
        channel: CaptureChannel,
        packet: &RtpPacket,
        peer_addr: Option<std::net::SocketAddr>,
        profile: Option<Arc<NegotiatedLegProfile>>,
    ) {
        let received_at_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_micros() as u64)
            .unwrap_or_default();
        let captured = CapturedRtp {
            profile,
            channel,
            packet: packet.clone(),
            received_at_micros,
            peer_addr,
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

    pub(crate) fn dropped_packets(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Compatibility helper for direct injection: maps the classic caller-tap
    /// direction onto its channel (ingress→Caller, egress→Callee).
    pub fn write_sample(&self, direction: PacketDirection, packet: &RtpPacket) {
        self.capture(CaptureChannel::for_direction(direction), packet, None);
    }
}

impl Clone for RecorderSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }
}

struct RecorderTask {
    rtp_rx: mpsc::Receiver<CapturedRtp>,
    command_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    recorder_finished_tx: mpsc::UnboundedSender<RecordingCompletion>,
    recorder: Option<Box<dyn MediaRecorder>>,
    paused: bool,
    deadline: Option<tokio::time::Instant>,
}

impl RecorderTask {
    fn new(
        rtp_rx: mpsc::Receiver<CapturedRtp>,
        command_rx: mpsc::UnboundedReceiver<RecorderCommand>,
        recorder_finished_tx: mpsc::UnboundedSender<RecordingCompletion>,
    ) -> Self {
        Self {
            rtp_rx,
            command_rx,
            recorder_finished_tx,
            recorder: None,
            paused: false,
            deadline: None,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        self.finish_recorder().await;
                        return;
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
                        RecorderCommand::QueryStatus { reply } => {
                            let _ = reply.send(self.status());
                        }
                        RecorderCommand::Pause => self.paused = true,
                        RecorderCommand::Resume => {
                            if self.paused {
                                // Commands take priority over RTP. Discard the
                                // paused backlog before reanchoring on fresh media.
                                let queued = self.rtp_rx.len();
                                for _ in 0..queued {
                                    if self.rtp_rx.try_recv().is_err() {
                                        break;
                                    }
                                }
                                if let Some(recorder) = self.recorder.as_mut() {
                                    recorder.resume();
                                }
                                self.paused = false;
                            }
                        }
                        RecorderCommand::StopRecorder { reply } => {
                            let _ = reply.send(self.finalize_recorder().await);
                        }
                    }
                }
                _ = wait_for_deadline(self.deadline), if self.deadline.is_some() => {
                    self.finish_recorder().await;
                }
                captured = self.rtp_rx.recv() => {
                    match captured {
                        Some(captured) => self.write_rtp(&captured).await,
                        None => {
                            self.finish_recorder().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn finish_recorder(&mut self) {
        let result = self.finalize_recorder().await;
        if let Err(error) = &result {
            warn!(%error, "recording task failed to finalize recorder");
        }
        let _ = self.recorder_finished_tx.send(result);
    }

    async fn set_recorder(
        &mut self,
        mut recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        if self.recorder.is_some() {
            return Err(anyhow!("recording_already_active"));
        }
        recorder.initialize().await?;
        self.recorder = Some(recorder);
        self.paused = false;
        self.deadline = max_duration.map(|duration| tokio::time::Instant::now() + duration);
        Ok(())
    }

    fn status(&self) -> RecorderStatus {
        let active = self.recorder.is_some();
        RecorderStatus {
            active,
            paused: active && self.paused,
            file_path: self
                .recorder
                .as_ref()
                .and_then(|recorder| recorder.file_path())
                .map(ToOwned::to_owned),
        }
    }

    async fn write_rtp(&mut self, captured: &CapturedRtp) {
        if self.paused {
            return;
        }
        if let Some(recorder) = self.recorder.as_mut()
            && let Err(error) = recorder
                .write_rtp_with_profile(
                    captured.channel,
                    &captured.packet,
                    captured.received_at_micros,
                    captured.peer_addr,
                    captured.profile.as_deref(),
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

    async fn recv_recorder_finished(
        rx: &mut mpsc::UnboundedReceiver<RecordingCompletion>,
    ) -> Option<RecordingResult> {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("recorder did not finish")
            .expect("recorder-finished channel closed without a result")
            .expect("recorder finalization failed")
    }

    async fn assert_recorder_task_stopped(rx: &mut mpsc::UnboundedReceiver<RecordingCompletion>) {
        let result = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("recorder task did not stop");
        assert!(result.is_none(), "recorder-finished channel remains open");
    }

    struct CountingBackend {
        recorded: AtomicUsize,
        flushed: AtomicBool,
        items: parking_lot::Mutex<Vec<(String, BackendSipFlowItem)>>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self {
                recorded: AtomicUsize::new(0),
                flushed: AtomicBool::new(false),
                items: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SipFlowBackend for CountingBackend {
        fn record(&self, call_id: Cow<'_, str>, item: BackendSipFlowItem) -> Result<()> {
            self.items.lock().push((call_id.into_owned(), item));
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
    async fn file_output_follows_each_legs_rtp_timeline_when_one_audio_clock_runs_slow() {
        use audio_codec::{CodecType, create_encoder, create_decoder};
        let dir = tempfile::tempdir().unwrap();
        for (codec, rate) in [(CodecType::PCMU, Some(8000)), (CodecType::G729, Some(8000)), (CodecType::G729, None)] {
            let path = dir.path().join(format!("drift-{codec:?}-{rate:?}.wav"));
            let mut negotiated = profile();
            let audio = negotiated.audio.as_mut().unwrap();
            audio.codec = codec;
            audio.payload_type = codec.payload_type();
            let mut recorder = FileRecorder::new(
                RecorderOption {
                    recorder_file: path.to_string_lossy().into_owned(),
                    samplerate: rate,
                    ..Default::default()
                },
                negotiated, 2, false,
            );
            recorder.initialize().await.unwrap();
            let mut encoders = [create_encoder(codec), create_encoder(codec)];
            // Both streams have contiguous sequence numbers and 20 ms RTP
            // increments. Only capture time exposes the slower sender clock.
            for wall_ms in 0..=12_000u64 {
                for (side, interval_ms) in [20, 21].into_iter().enumerate() {
                    if wall_ms % interval_ms != 0 {
                        continue;
                    }
                    let sequence = (wall_ms / interval_ms) as u16;
                    let marked = (2000..2200).contains(&wall_ms)
                        || (10_000..10_200).contains(&wall_ms);
                    let pcm: Vec<i16> = (0..160).map(|i| {
                        if marked {
                            (10_000.0 * (i as f64 * 440.0 * std::f64::consts::TAU / 8000.0).sin()) as i16
                        } else {
                            0
                        }
                    }).collect();
                    let packet = RtpPacket::new(
                        RtpHeader::new(codec.payload_type(), sequence, sequence as u32 * 160, 1234 + side as u32),
                        encoders[side].encode(&pcm).to_vec(),
                    );
                    recorder.write_rtp(
                        if side == 0 { CaptureChannel::Caller } else { CaptureChannel::Callee },
                        &packet, 1_000_000 + wall_ms * 1000, None,
                    ).await.unwrap();
                }
                if wall_ms % 500 == 0 {
                    recorder.recorder.as_mut().unwrap().flush().await.unwrap();
                }
            }
            let result = Box::new(recorder).finalize().await.unwrap().unwrap();
            let wav = std::fs::read(path).unwrap();
            let samples: Vec<i16> = if rate.is_some() {
                wav[44..].chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
            } else {
                assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 7);
                create_decoder(CodecType::PCMU).decode(&wav[44..])
            };
            assert!((result.duration_secs - 12.02).abs() < 0.03);
            // Each leg follows its own RTP timeline from its wall-anchored
            // first packet: the 21 ms sender's RTP clock runs ~5% slow, so its
            // markers land proportionally earlier than the 20 ms sender's —
            // audio stays contiguous instead of being diluted with silence.
            for (side, marker_expectations) in [
                (0usize, [2.0f64, 10.0]),
                (1, [1.90, 9.52]),
            ] {
                for expected_onset in marker_expectations {
                    let from = ((expected_onset - 1.0) * 8000.0) as usize;
                    let to = ((expected_onset + 0.6) * 8000.0) as usize;
                    let first = (from..to).find(|i| samples[i * 2 + side].abs() > 2000)
                        .expect("recorded tone marker is missing");
                    let onset = first as f64 / 8000.0;
                    assert!((onset - expected_onset).abs() <= 0.12,
                        "{codec:?} channel={side} marker@{expected_onset}s recorded at {onset}s");
                    let mut quiet_run = 0;
                    let mut longest_quiet = 0;
                    for i in first + 320..first + 1120 {
                        if samples[i * 2 + side].abs() < 500 {
                            quiet_run += 1;
                            longest_quiet = longest_quiet.max(quiet_run);
                        } else {
                            quiet_run = 0;
                        }
                    }
                    assert!(longest_quiet <= 160,
                        "{codec:?} channel={side}: drift correction interrupted a tone for {longest_quiet} samples");
                }
            }
        }
    }

    #[tokio::test]
    async fn file_output_keeps_first_speaker_timing_after_peer_startup_burst() {
        use audio_codec::{CodecType, create_decoder, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("startup-burst.wav");
        let mut recorder = FileRecorder::new(
            RecorderOption::new(path.to_string_lossy().into_owned()), profile(), 2, false,
        );
        recorder.initialize().await.unwrap();
        let mut encoder = create_encoder(CodecType::PCMU);
        let quiet = encoder.encode(&vec![0; 160]);
        let tone = encoder.encode(&vec![8000; 160]);
        // A transport handshake can release already queued microphone audio
        // before the caller sends its first RTP. It must not advance that
        // caller's file position beyond the caller's actual capture time.
        for sequence in 0..25 {
            let packet = RtpPacket::new(RtpHeader::new(0, sequence, sequence as u32 * 160, 1234), tone.clone());
            recorder.write_rtp(CaptureChannel::Callee, &packet, 1_000_000, None).await.unwrap();
        }
        for wall_ms in 0..=12_000u64 {
            if wall_ms % 20 == 0 {
                let sequence = (25 + wall_ms / 20) as u16;
                let packet = RtpPacket::new(RtpHeader::new(0, sequence, sequence as u32 * 160, 1234), tone.clone());
                recorder.write_rtp(CaptureChannel::Callee, &packet, 1_000_000 + wall_ms * 1000, None).await.unwrap();
            }
            if wall_ms >= 600 && (wall_ms - 600) % 22 == 0 {
                let sequence = ((wall_ms - 600) / 22) as u16;
                let marked = (50..75).contains(&sequence) || (250..275).contains(&sequence);
                let packet = RtpPacket::new(RtpHeader::new(0, sequence, sequence as u32 * 160, 5678),
                    if marked { tone.clone() } else { quiet.clone() });
                recorder.write_rtp(CaptureChannel::Caller, &packet, 1_000_000 + wall_ms * 1000, None).await.unwrap();
            }
            if wall_ms % 500 == 0 {
                recorder.recorder.as_mut().unwrap().flush().await.unwrap();
            }
        }
        Box::new(recorder).finalize().await.unwrap();
        let wav = std::fs::read(path).unwrap();
        let samples = create_decoder(CodecType::PCMU).decode(&wav[44..]);
        let mut onsets = Vec::new();
        let mut previous = false;
        for (i, frame) in samples.chunks_exact(2).enumerate() {
            let audible = frame[0] > 6000;
            if audible && !previous && onsets.last().is_none_or(|last| i - last > 8000) {
                onsets.push(i);
            }
            previous = audible;
        }
        assert_eq!(onsets.len(), 2, "both caller markers must survive recording");
        // The caller sends every 22 ms with 160-sample RTP increments (a ~10%
        // slow RTP clock) anchored at its wall start (600 ms): its markers
        // land on its own RTP timeline — 0.6 s + n×160 samples — contiguous,
        // not diluted against the wall clock.
        for (onset, expected) in onsets.into_iter().zip([1.6, 5.6]) {
            let actual = onset as f64 / 8000.0;
            assert!((actual - expected).abs() <= 0.12,
                "caller marker captured at {expected}s was recorded at {actual}s");
        }
    }

    #[tokio::test]
    async fn file_output_keeps_a_late_peer_at_its_capture_position() {
        use audio_codec::{CodecType, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        for (peer_start_ms, expected_ms) in [(2500u64, 4020usize), (12500, 14020)] {
            let path = dir.path().join(format!("late-peer-{peer_start_ms}.wav"));
            let mut recorder = FileRecorder::new(
                RecorderOption {
                    recorder_file: path.to_string_lossy().into_owned(),
                    samplerate: Some(8000),
                    ..Default::default()
                }, profile(), 2, false,
            );
            recorder.initialize().await.unwrap();
            let mut encoder = create_encoder(CodecType::PCMU);
            for wall_ms in (0..=peer_start_ms + 1500).step_by(20) {
                let packet = RtpPacket::new(
                    RtpHeader::new(0, (wall_ms / 20) as u16, wall_ms as u32 * 8, 1234),
                    encoder.encode(&vec![0; 160]).to_vec(),
                );
                recorder.write_rtp(CaptureChannel::Caller, &packet, 1_000_000 + wall_ms * 1000, None).await.unwrap();
                if wall_ms >= peer_start_ms {
                    let packet = RtpPacket::new(packet.header.clone(), encoder.encode(&vec![8000; 160]).to_vec());
                    recorder.write_rtp(CaptureChannel::Callee, &packet, 1_000_000 + wall_ms * 1000, None).await.unwrap();
                }
            }
            Box::new(recorder).finalize().await.unwrap();
            let wav = std::fs::read(path).unwrap();
            let samples: Vec<i16> = wav[44..].chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            assert_eq!(samples.len(), expected_ms * 8 * 2, "a peer's leading silence must not shorten either channel");
            let first = samples.chunks_exact(2).position(|s| s[1] > 6000).unwrap();
            assert_eq!(first, peer_start_ms as usize * 8, "late peer must retain its capture position");
        }
    }

    #[tokio::test]
    async fn file_output_does_not_turn_capture_jitter_into_audio_gaps() {
        use audio_codec::{CodecType, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jitter.wav");
        let mut recorder = FileRecorder::new(
            RecorderOption {
                recorder_file: path.to_string_lossy().into_owned(),
                samplerate: Some(8000),
                ..Default::default()
            }, profile(), 2, false,
        );
        recorder.initialize().await.unwrap();
        let data = create_encoder(CodecType::PCMU).encode(&vec![8000; 160]).to_vec();
        for sequence in 0..100u16 {
            let packet = RtpPacket::new(RtpHeader::new(0, sequence, sequence as u32 * 160, 1234), data.clone());
            for channel in [CaptureChannel::Caller, CaptureChannel::Callee] {
                let jitter_ms = [0, 15, 5, 25][sequence as usize % 4];
                recorder.write_rtp(channel, &packet, 1_000_000 + sequence as u64 * 20_000 + jitter_ms * 1000, None).await.unwrap();
            }
        }
        let result = Box::new(recorder).finalize().await.unwrap().unwrap();
        let wav = std::fs::read(path).unwrap();
        let samples: Vec<i16> = wav[44..].chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(samples.len(), 2 * 8000 * 2);
        assert!(samples.iter().all(|s| *s > 6000), "capture jitter must not add silence to continuous audio");
        assert_eq!(result.duration_secs, 2.0);
    }

    #[tokio::test]
    async fn file_output_pause_excludes_paused_capture_time() {
        use audio_codec::{CodecType, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        for drain_before_resume in [true, false] {
            let path = dir.path().join(format!("pause-{drain_before_resume}.wav"));
            let (tx, rx) = mpsc::channel(8);
            let (command_tx, command_rx) = mpsc::unbounded_channel();
            let (finished_tx, _finished_rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(RecorderTask::new(rx, command_rx, finished_tx).run());
            let handle = RecorderHandle { tx: command_tx };
            handle.set_recorder(Box::new(FileRecorder::new(
                RecorderOption {
                    recorder_file: path.to_string_lossy().into_owned(),
                    samplerate: Some(8000),
                    ..Default::default()
                }, profile(), 2, false,
            )), None).await.unwrap();
            let data = create_encoder(CodecType::PCMU).encode(&vec![8000; 160]).to_vec();
            for (sequence, capture_micros) in [(0, 1_000_000), (1, 3_000_000), (2, 7_000_000)] {
                for channel in [CaptureChannel::Caller, CaptureChannel::Callee] {
                    tx.send(CapturedRtp {
                        profile: None, channel,
                        packet: RtpPacket::new(RtpHeader::new(0, sequence, sequence as u32 * 160, 1234), data.clone()),
                        received_at_micros: capture_micros,
                        peer_addr: None,
                    }).await.unwrap();
                }
                if sequence != 1 || drain_before_resume {
                    // Exercise both already-discarded and still-queued paused RTP.
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while tx.capacity() != 8 {
                            tokio::task::yield_now().await;
                        }
                    }).await.unwrap();
                }
                if sequence == 0 {
                    handle.pause().unwrap();
                    assert!(handle.status().await.unwrap().paused);
                } else if sequence == 1 {
                    handle.resume().unwrap();
                    assert!(!handle.status().await.unwrap().paused);
                }
            }
            let result = handle.stop_recorder().await.unwrap().unwrap();
            drop(handle);
            task.await.unwrap();
            let wav = std::fs::read(path).unwrap();
            let samples: Vec<i16> = wav[44..].chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            assert_eq!(result.duration_secs, 0.04, "paused capture time must be excluded from the file");
            assert_eq!(samples.len(), 320 * 2);
            assert!(samples.iter().all(|s| *s > 6000));
        }
    }

    #[tokio::test]
    async fn file_output_places_dtmf_once_at_capture_time() {
        use crate::negotiate::NegotiatedCodec;
        use audio_codec::CodecType;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dtmf-capture.wav");
        let mut negotiated = profile();
        negotiated.dtmf = Some(NegotiatedCodec {
            codec: CodecType::TelephoneEvent, payload_type: 101, clock_rate: 8000, channels: 1,
        });
        let mut recorder = FileRecorder::new(RecorderOption {
            recorder_file: path.to_string_lossy().into_owned(), samplerate: Some(8000),
            ..Default::default()
        }, negotiated, 2, false);
        recorder.initialize().await.unwrap();
        recorder.write_rtp(CaptureChannel::Caller, &packet(0, 0, 0), 1_000_000, None).await.unwrap();
        for (seq, elapsed_ms, end, duration) in [
            (1, 1000, false, 160u16), (2, 1040, false, 800),
            (3, 1060, true, 800), (4, 1100, true, 800),
        ] {
            let event = RtpPacket::new(RtpHeader::new(101, seq, 64000, 1234),
                vec![5, if end { 0x80 } else { 0 }, (duration >> 8) as u8, duration as u8]);
            recorder.write_rtp(CaptureChannel::Callee, &event, 1_000_000 + elapsed_ms * 1000, None).await.unwrap();
        }
        recorder.write_rtp(CaptureChannel::Caller, &packet(0, 5, 160), 3_000_000, None).await.unwrap();
        Box::new(recorder).finalize().await.unwrap();
        let wav = std::fs::read(path).unwrap();
        let samples: Vec<i16> = wav[44..].chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let callee: Vec<i16> = samples.chunks_exact(2).map(|s| s[1]).collect();
        assert!(callee[8000..8800].iter().any(|s| s.abs() > 2000));
        assert!(callee[..8000].iter().all(|s| s.abs() < 100));
        assert!(callee[8800..].iter().all(|s| s.abs() < 100), "terminal repeats must not extend the tone");
    }

    #[tokio::test]
    async fn file_output_sample_rate_resamples_audio_and_preserves_duration() {
        use audio_codec::{CodecType, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        for source_codec in [CodecType::PCMU, CodecType::G722, CodecType::Opus] {
            for rate in [8000, 16000, 48000] {
                for channels in [1, 2] {
                    let path = dir.path().join(format!("{rate}-{channels}.wav"));
                    let mut source_profile = profile();
                    source_profile.audio.as_mut().unwrap().codec = source_codec;
                    source_profile.audio.as_mut().unwrap().payload_type = source_codec.payload_type();
                    let rtp_clock_rate = if source_codec == CodecType::Opus { 48000 } else { 8000 };
                    source_profile.audio.as_mut().unwrap().clock_rate = rtp_clock_rate;
                    let mut recorder = FileRecorder::new(
                        RecorderOption {
                            recorder_file: path.to_string_lossy().into_owned(),
                            samplerate: Some(rate),
                            ..Default::default()
                        },
                        source_profile, channels, false,
                    );
                    recorder.initialize().await.unwrap();
                    let mut encoder = create_encoder(source_codec);
                    let source_rate = encoder.sample_rate() as usize;
                    let frame_samples = source_rate / 50;
                    for seq in 0..50u16 {
                        let pcm: Vec<i16> = (0..frame_samples).map(|i| {
                            let t = (seq as usize * frame_samples + i) as f64 / source_rate as f64;
                            (8000.0 * (t * 440.0 * std::f64::consts::TAU).sin()) as i16
                        }).collect();
                        let packet = RtpPacket::new(
                            RtpHeader::new(source_codec.payload_type(), seq, seq as u32 * (rtp_clock_rate / 50), 1234),
                            encoder.encode(&pcm).to_vec(),
                        );
                        recorder
                            .write_rtp(CaptureChannel::Caller, &packet, 0, None)
                            .await
                            .unwrap();
                        recorder
                            .write_rtp(CaptureChannel::Callee, &packet, 0, None)
                            .await
                            .unwrap();
                    }
                    let result = Box::new(recorder).finalize().await.unwrap().unwrap();
                    let wav = std::fs::read(path).unwrap();
                    assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
                    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), rate);
                    assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
                    let samples: Vec<i16> = wav[44..].chunks_exact(2)
                        .map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                    let duration = samples.len() as f64 / (rate as f64 * channels as f64);
                    assert!((duration - 1.0).abs() < 0.03, "duration: {duration}, rate: {rate}");
                    assert!((result.duration_secs - 1.0).abs() < 0.03);
                    let mono: Vec<i16> = samples.iter().step_by(channels as usize).copied().collect();
                    // Hysteresis avoids counting resampler ringing around zero twice.
                    let mut below = false;
                    let mut crossings = 0;
                    for sample in &mono {
                        if *sample < -1000 {
                            below = true;
                        } else if *sample > 1000 && below {
                            crossings += 1;
                            below = false;
                        }
                    }
                    assert!((crossings as f64 / duration - 440.0).abs() < 5.0, "rate={rate} channels={channels} crossings={crossings} duration={duration}");
                    assert!(mono.iter().any(|s| *s > 6000));
                    assert!(mono.iter().any(|s| *s < -6000));
                    if channels == 2 {
                        assert!(samples.chunks_exact(2).all(|s| s[0] == s[1]));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn file_output_mixed_rtp_clocks_keep_default_recording_synchronized() {
        use audio_codec::{CodecType, create_decoder, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        for caller_codec in [CodecType::Opus, CodecType::PCMU] {
            for switch_codecs in [false, true] {
                let path = dir
                    .path()
                    .join(format!("{caller_codec:?}-{switch_codecs}.wav"));
                let mut caller_profile = profile();
                caller_profile.audio.as_mut().unwrap().codec = caller_codec;
                caller_profile.audio.as_mut().unwrap().payload_type = caller_codec.payload_type();
                caller_profile.audio.as_mut().unwrap().clock_rate = if caller_codec == CodecType::Opus {
                    48000
                } else {
                    8000
                };
                let mut recorder = FileRecorder::new(
                    RecorderOption::new(path.to_string_lossy().into_owned()),
                    caller_profile,
                    2,
                    false,
                );
                recorder.initialize().await.unwrap();
                let mut encoders = [
                    create_encoder(CodecType::Opus),
                    create_encoder(CodecType::PCMU),
                ];
                for seq in 0..50u16 {
                    for (side, channel) in [CaptureChannel::Caller, CaptureChannel::Callee]
                        .into_iter()
                        .enumerate()
                    {
                        let codec_index = side
                            ^ usize::from(caller_codec == CodecType::PCMU)
                            ^ usize::from(switch_codecs && seq >= 25);
                        let codec = [CodecType::Opus, CodecType::PCMU][codec_index];
                        let clock_rate = if codec == CodecType::Opus {
                            48000
                        } else {
                            8000
                        };
                        let mut leg_profile = profile();
                        let audio = leg_profile.audio.as_mut().unwrap();
                        audio.codec = codec;
                        audio.payload_type = codec.payload_type();
                        audio.clock_rate = clock_rate;
                        let encoder = &mut encoders[codec_index];
                        let rate = encoder.sample_rate() as usize;
                        let pcm: Vec<i16> = (0..rate / 50)
                            .map(|i| {
                                if (10..20).contains(&seq) || (35..45).contains(&seq) {
                                    (8000.0
                                        * (i as f64 * 440.0 * std::f64::consts::TAU / rate as f64)
                                            .sin()) as i16
                                } else {
                                    0
                                }
                            })
                            .collect();
                        // The new codec has its own forward-jumping RTP origin.
                        let origin = if switch_codecs && seq >= 25 {
                            200_000
                        } else {
                            100_000
                        };
                        let packet = RtpPacket::new(
                            RtpHeader::new(
                                codec.payload_type(),
                                seq,
                                origin + seq as u32 * (clock_rate / 50),
                                1234 + side as u32,
                            ),
                            encoder.encode(&pcm).to_vec(),
                        );
                        recorder
                            .write_rtp_with_profile(
                                channel,
                                &packet,
                                1_000_000 + seq as u64 * 20_000,
                                None,
                                Some(&leg_profile),
                            )
                            .await
                            .unwrap();
                    }
                }
                let result = Box::new(recorder).finalize().await.unwrap().unwrap();
                let wav = std::fs::read(path).unwrap();
                assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 7);
                assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8000);
                let samples = create_decoder(CodecType::PCMU).decode(&wav[44..]);
                let duration = samples.len() as f64 / 16000.0;
                assert!(
                    (duration - 1.0).abs() < 0.03,
                    "{caller_codec:?} switch={switch_codecs}: WAV duration={duration}"
                );
                assert!((result.duration_secs - 1.0).abs() < 0.03);
                for start in [1600usize, 5600] {
                    let mut onsets = Vec::new();
                    for side in 0..2 {
                        let onset = (start..start + 240)
                            .find(|i| samples[i * 2 + side].abs() > 2000)
                            .expect("both legs must retain their tone markers");
                        onsets.push(onset);
                        for frame in 1..9 {
                            assert!(
                                (start + frame * 160..start + (frame + 1) * 160)
                                    .any(|i| samples[i * 2 + side].abs() > 2000),
                                "recording must not insert silent frames into a tone"
                            );
                        }
                    }
                    assert!(
                        onsets[0].abs_diff(onsets[1]) < 240,
                        "leg markers must stay within 30 ms: {onsets:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn file_output_stereo_swap_preserves_mono_and_legacy_format() {
        use audio_codec::{CodecType, create_encoder, create_decoder};
        let dir = tempfile::tempdir().unwrap();
        for rate in [None, Some(16000)] {
            for channels in [1, 2] {
                for swap in [false, true] {
                    let path = dir.path().join(format!("{rate:?}-{channels}-{swap}.wav"));
                    let mut recorder = FileRecorder::new(
                        RecorderOption {
                            recorder_file: path.to_string_lossy().into_owned(),
                            samplerate: rate,
                            stereo_swap: Some(swap),
                            ..Default::default()
                        },
                        profile(), channels, true,
                    );
                    recorder.initialize().await.unwrap();
                    let mut encoder = create_encoder(CodecType::PCMU);
                    for seq in 0..10u16 {
                        for (channel, level) in [
                            (CaptureChannel::Caller, 8000i16),
                            (CaptureChannel::Callee, -8000i16),
                        ] {
                            let packet = RtpPacket::new(
                                RtpHeader::new(0, seq, seq as u32 * 160, 1234),
                                encoder.encode(&vec![level; 160]).to_vec(),
                            );
                            recorder
                                .write_rtp(channel, &packet, 0, None)
                                .await
                                .unwrap();
                        }
                    }
                    Box::new(recorder).finalize().await.unwrap();
                    let wav = std::fs::read(path).unwrap();
                    let samples = if rate.is_some() {
                        wav[44..].chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>()
                    } else {
                        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 7);
                        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8000);
                        create_decoder(CodecType::PCMU).decode(&wav[44..])
                    };
                    let middle = samples.len() / channels as usize / 2 * channels as usize;
                    if channels == 1 {
                        assert!(samples[middle] > 6000, "caller-only mono must not swap");
                    } else if swap {
                        assert!(samples[middle] < -6000 && samples[middle + 1] > 6000);
                    } else {
                        assert!(samples[middle] > 6000 && samples[middle + 1] < -6000);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn file_output_rejects_invalid_settings() {
        let dir = tempfile::tempdir().unwrap();
        for mut option in [
            RecorderOption { samplerate: Some(0), ..Default::default() },
            RecorderOption { samplerate: Some(192001), ..Default::default() },
            RecorderOption { ptime: Some(0), ..Default::default() },
        ] {
            let path = dir.path().join("invalid.wav");
            option.recorder_file = path.to_string_lossy().into_owned();
            let mut recorder = FileRecorder::new(option, profile(), 2, false);
            assert!(recorder.initialize().await.is_err());
            assert!(!path.exists());
        }
    }

    #[tokio::test]
    async fn file_stop_returns_recorder_result() {
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
        assert!(!handle.status().await.unwrap().active);
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();
        let status = handle.status().await.unwrap();
        assert!(status.active);
        assert!(!status.paused);
        assert_eq!(status.file_path.as_deref(), Some(path.as_str()));
        handle.pause().unwrap();
        assert!(handle.status().await.unwrap().paused);
        handle.resume().unwrap();
        assert!(!handle.status().await.unwrap().paused);
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));
        let result = handle.stop_recorder().await.unwrap().unwrap();
        let status = handle.status().await.unwrap();
        assert!(!status.active);
        assert!(status.file_path.is_none());
        assert!(
            handle.pause().is_ok(),
            "stopping a recorder must keep its task alive"
        );
        assert_eq!(result.path, path);
        assert!(result.file_size > 44);

        drop(handle);
        assert!(
            recv_recorder_finished(&mut recorder_finished_rx)
                .await
                .is_none()
        );
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn max_duration_returns_recorder_result() {
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                Some(Duration::from_millis(20)),
            )
            .await
            .unwrap();
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));

        let result = recv_recorder_finished(&mut recorder_finished_rx)
            .await
            .unwrap();
        assert!(!handle.status().await.unwrap().active);
        assert!(
            handle.pause().is_ok(),
            "max duration must not stop the capture task"
        );
        assert_eq!(result.path, path);
        assert!(result.file_size > 44);

        drop(handle);
        assert!(
            recv_recorder_finished(&mut recorder_finished_rx)
                .await
                .is_none()
        );
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn dropping_command_handle_stops_task_and_finalizes_recorder() {
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);
        handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();

        drop(handle);
        let result = recv_recorder_finished(&mut recorder_finished_rx)
            .await
            .unwrap();
        assert_eq!(result.path, path);
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        drop(sender);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn dropping_rtp_sender_stops_task_and_finalizes_recorder() {
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);
        handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();

        drop(sender);
        let result = recv_recorder_finished(&mut recorder_finished_rx)
            .await
            .unwrap();
        assert_eq!(result.path, path);
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        drop(handle);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn sipflow_does_not_flush_shared_backend_when_handle_is_dropped() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();
        use rustrtc::peer_connection::RtpObserver;
        let tap = crate::ingress_tap::IngressTap::new(8, vec![0], Some(sender));
        tap.on_ingress(&packet(0, 1, 160), "127.0.0.1:4000".parse().unwrap());
        tap.on_egress(&packet(0, 2, 320), "127.0.0.1:5000".parse().unwrap());
        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.recorded.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            recv_recorder_finished(&mut recorder_finished_rx)
                .await
                .is_none()
        );
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        assert_eq!(backend.recorded.load(Ordering::SeqCst), 2);
        let items = backend.items.lock();
        assert_eq!(items[0].0, "call-1");
        assert_eq!(items[0].1.src_addr, "127.0.0.1:4000");
        assert_eq!(items[0].1.dst_addr, "");
        assert_eq!(items[0].1.leg, Some(0));
        assert_eq!(items[1].1.src_addr, "127.0.0.1:5000");
        assert_eq!(items[1].1.dst_addr, "");
        assert_eq!(items[1].1.leg, Some(1));
        assert_eq!(
            items[0].1.payload.as_ref(),
            packet(0, 1, 160).marshal().unwrap().as_slice()
        );
        assert!(items[0].1.timestamp > 0);
        assert!(!backend.flushed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn sipflow_swapped_legs_put_egress_on_leg0() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1").with_swapped_legs(true);
        let (handle, sender, _recorder_finished_rx) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();
        use rustrtc::peer_connection::RtpObserver;
        let tap = crate::ingress_tap::IngressTap::new(8, vec![0], Some(sender));
        tap.on_ingress(&packet(0, 1, 160), "127.0.0.1:4000".parse().unwrap());
        tap.on_egress(&packet(0, 2, 320), "127.0.0.1:5000".parse().unwrap());
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.recorded.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let items = backend.items.lock();
        // Swapped: egress (callee side) becomes leg 0 — the left channel of
        // exported WAVs — and ingress (caller) becomes leg 1.
        assert_eq!(items[0].1.leg, Some(1), "ingress must flip to leg 1");
        assert_eq!(items[1].1.leg, Some(0), "egress must flip to leg 0");
        assert_eq!(items[0].1.src_addr, "127.0.0.1:4000");
        assert_eq!(items[1].1.src_addr, "127.0.0.1:5000");
    }

    #[tokio::test]
    async fn stop_without_file_recorder_finishes_with_no_file_result() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, sender, mut recorder_finished_rx) = RecorderHandle::new();
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
        assert!(handle.stop_recorder().await.unwrap().is_none());
        assert!(handle.pause().is_ok());
        drop(handle);
        assert!(
            recv_recorder_finished(&mut recorder_finished_rx)
                .await
                .is_none()
        );
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        assert!(!backend.flushed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn stop_then_set_another_recorder_keeps_task_alive() {
        let backend = Arc::new(CountingBackend::new());
        let initial = SipflowRecorder::new(backend.clone(), "call-1");
        let (handle, _sender, mut recorder_finished_rx) = RecorderHandle::new();
        handle.set_recorder(Box::new(initial), None).await.unwrap();
        let temp = tempfile::NamedTempFile::new().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        drop(temp);

        let error = handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("recording_already_active"));
        assert!(handle.stop_recorder().await.unwrap().is_none());

        handle
            .set_recorder(
                Box::new(FileRecorder::new(RecorderOption::new(path.clone()), profile(), 2, false)),
                None,
            )
            .await
            .unwrap();
        assert!(!backend.flushed.load(Ordering::SeqCst));

        let result = handle.stop_recorder().await.unwrap().unwrap();
        assert_eq!(result.path, path);
        drop(handle);
        assert!(
            recv_recorder_finished(&mut recorder_finished_rx)
                .await
                .is_none()
        );
        assert_recorder_task_stopped(&mut recorder_finished_rx).await;
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn channel_mapping_is_stable() {
        // Classic caller-tap layout: ingress (received) is the caller channel,
        // egress (sent) is the callee channel; sipflow leg ids follow.
        assert_eq!(
            CaptureChannel::for_direction(PacketDirection::Ingress),
            CaptureChannel::Caller
        );
        assert_eq!(
            CaptureChannel::for_direction(PacketDirection::Egress),
            CaptureChannel::Callee
        );
        assert_eq!(CaptureChannel::Caller.code(), 1);
        assert_eq!(CaptureChannel::Callee.code(), 2);
        assert_eq!(CaptureChannel::from_code(1), Some(CaptureChannel::Caller));
        assert_eq!(CaptureChannel::from_code(2), Some(CaptureChannel::Callee));
        assert_eq!(CaptureChannel::from_code(0), None);
    }
}

/// Session-owned recording control, independent of a selected media pair.
#[derive(Default)]
pub struct RecordingSession {
    recorder_handle: Option<RecorderHandle>,
    recorder_finished_rx: Option<mpsc::UnboundedReceiver<RecordingCompletion>>,
    /// Cloneable sender retained when the capture task is created so the
    /// callee leg's tap can feed the same recording task.
    capture_sender: Option<RecorderSender>,
}

impl RecordingSession {
    pub fn setup_recorder_task(&mut self) -> Result<RecorderSender> {
        if self.recorder_handle.is_some() {
            return Err(anyhow!("recording task is already started"));
        }
        let (handle, sender, recorder_finished_rx) = RecorderHandle::new();
        self.recorder_handle = Some(handle);
        self.recorder_finished_rx = Some(recorder_finished_rx);
        self.capture_sender = Some(sender.clone());
        Ok(sender)
    }

    /// Sender for an additional tapped leg (the callee leg's ingress feeds the
    /// same recording task — its mic audio is the B-channel source while the
    /// call is held). `None` until [`Self::setup_recorder_task`] ran.
    pub fn capture_sender(&self) -> Option<RecorderSender> {
        self.capture_sender.clone()
    }

    /// Start file recording from caller leg A. The task initializes the file
    /// backend asynchronously before this resolves.
    pub async fn start_recording(
        &mut self,
        caller_profile: NegotiatedLegProfile,
        option: RecorderOption,
        channels: u16,
        mono_caller_only: bool,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        let recorder = FileRecorder::new(option, caller_profile, channels, mono_caller_only);
        self.set_recorder(Box::new(recorder), max_duration).await
    }

    /// Install and initialize the selected recorder implementation in the
    /// capture task that was prepared before caller-leg construction.
    pub async fn set_recorder(
        &mut self,
        recorder: Box<dyn MediaRecorder>,
        max_duration: Option<Duration>,
    ) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .set_recorder(recorder, max_duration)
            .await
    }

    pub fn has_recorder_task(&self) -> bool {
        self.recorder_handle.is_some()
    }

    /// Whether the recording task currently owns an initialized recorder
    /// implementation.
    pub async fn has_recorder(&self) -> bool {
        self.recorder_status()
            .await
            .is_ok_and(|status| status.active)
    }

    pub async fn recorder_status(&self) -> Result<RecorderStatus> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .status()
            .await
    }

    pub fn pause_recording(&self) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .pause()
    }

    pub fn resume_recording(&self) -> Result<()> {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .resume()
    }

    /// Finalize only the current recorder. The call-scoped capture task stays
    /// alive and can accept another recorder later.
    pub async fn stop_recording(&mut self) -> RecordingCompletion {
        self.recorder_handle
            .as_ref()
            .ok_or_else(|| anyhow!("recording task is unavailable"))?
            .stop_recorder()
            .await
    }

    /// Wait for a recorder completion reported independently of a control
    /// command, such as max-duration expiry.
    pub async fn recv_recorder_finished(&mut self) -> Option<RecordingCompletion> {
        self.recorder_finished_rx.as_mut()?.recv().await
    }
}
