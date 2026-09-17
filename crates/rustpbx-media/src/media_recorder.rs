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
    /// Local output path when this recorder produces a file.
    fn file_path(&self) -> Option<&str> {
        None
    }

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

/// Non-blocking media-path sender installed on the caller leg's RTP tap.
pub struct RecorderSender {
    tx: mpsc::Sender<CapturedRtp>,
    dropped: AtomicU64,
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
                        RecorderCommand::Resume => self.paused = false,
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
    async fn file_output_sample_rate_resamples_audio_and_preserves_duration() {
        use audio_codec::{CodecType, create_encoder};
        let dir = tempfile::tempdir().unwrap();
        for source_codec in [CodecType::PCMU, CodecType::G722] {
            for rate in [8000, 16000, 48000] {
                for channels in [1, 2] {
                    let path = dir.path().join(format!("{rate}-{channels}.wav"));
                    let mut source_profile = profile();
                    source_profile.audio.as_mut().unwrap().codec = source_codec;
                    source_profile.audio.as_mut().unwrap().payload_type = source_codec.payload_type();
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
                            RtpHeader::new(source_codec.payload_type(), seq, seq as u32 * 160, 1234),
                            encoder.encode(&pcm).to_vec(),
                        );
                        recorder.write_rtp(PacketDirection::Ingress, &packet, 0).await.unwrap();
                        recorder.write_rtp(PacketDirection::Egress, &packet, 0).await.unwrap();
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
                        for (direction, level) in [
                            (PacketDirection::Ingress, 8000i16),
                            (PacketDirection::Egress, -8000i16),
                        ] {
                            let packet = RtpPacket::new(
                                RtpHeader::new(0, seq, seq as u32 * 160, 1234),
                                encoder.encode(&vec![level; 160]).to_vec(),
                            );
                            recorder.write_rtp(direction, &packet, 0).await.unwrap();
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
        sender.write_sample(PacketDirection::Ingress, &packet(0, 1, 160));
        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.recorded.load(Ordering::SeqCst) == 0 {
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
        assert_eq!(backend.recorded.load(Ordering::SeqCst), 1);
        assert!(!backend.flushed.load(Ordering::SeqCst));
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
    fn direction_mapping_is_stable() {
        assert_eq!(direction_to_leg(PacketDirection::Ingress), Leg::A);
        assert_eq!(direction_to_leg(PacketDirection::Egress), Leg::B);
        assert_eq!(direction_to_leg_id(PacketDirection::Ingress), 0);
        assert_eq!(direction_to_leg_id(PacketDirection::Egress), 1);
    }
}

/// Session-owned recording control, independent of a selected media pair.
#[derive(Default)]
pub struct RecordingSession {
    recorder_handle: Option<RecorderHandle>,
    recorder_finished_rx: Option<mpsc::UnboundedReceiver<RecordingCompletion>>,
}

impl RecordingSession {
    pub fn setup_recorder_task(&mut self) -> Result<RecorderSender> {
        if self.recorder_handle.is_some() {
            return Err(anyhow!("recording task is already started"));
        }
        let (handle, sender, recorder_finished_rx) = RecorderHandle::new();
        self.recorder_handle = Some(handle);
        self.recorder_finished_rx = Some(recorder_finished_rx);
        Ok(sender)
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
