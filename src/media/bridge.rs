//! WebRTC ↔ RTP Media Bridge
//!
//! This module provides media-layer bridging between WebRTC (SRTP) and plain RTP peers.
//! It creates two PeerConnections:
//! - One in WebRTC mode (with DTLS/SRTP encryption)
//! - One in RTP mode (plain RTP)
//!
//! Media packets are received from one side, decrypted if necessary, and forwarded
//! to the other side where they are re-encrypted if needed. rustrtc handles the
//! encryption/decryption automatically based on transport mode.
//!
//! # Usage Example
//!
//! ```rust,ignore
//! // Create a bridge
//! let bridge = BridgePeerBuilder::new("bridge-1".to_string())
//!     .with_rtp_port_range(20000, 30000)
//!     .build();
//!
//! // Setup bridge with sample tracks for forwarding
//! bridge.setup_bridge().await.unwrap();
//!
//! // Start bridge forwarding
//! bridge.start_bridge().await;
//!
//! // Access PeerConnections for SDP negotiation
//! let caller_pc = bridge.caller_pc();
//! let callee_pc = bridge.callee_pc();
//!
//! // ... perform SDP negotiation on both sides ...
//!
//! // Stop bridge when done
//! bridge.stop().await;
//! ```
//!
//! # Integration with SipSession
//!
//! The bridge is designed to work with `SdpBridge` for SDP format conversion:
//! 1. Use `SdpBridge::webrtc_to_rtp()` or `SdpBridge::rtp_to_webrtc()` for SDP conversion
//! 2. Create `BridgePeer` to handle media forwarding
//! 3. The bridge's WebRTC side connects to the WebRTC client
//! 4. The bridge's RTP side connects to the SIP/RTP endpoint

use crate::media::ReceiveTimestampClock;
use crate::media::engine::command::SharedMediaSample;
use crate::media::recorder::{Leg as RecLeg, Recorder};
use crate::media::transcoder::{RtpTiming, Transcoder, rewrite_dtmf_duration};
use anyhow::Result;
use audio_codec::CodecType as AudioCodecType;
use rustrtc::{
    IceServer, PeerConnection, PeerConnectionEvent, RtpCodecParameters, RtpSender, TransportMode,
    media::{
        AudioFrame, MediaError, MediaKind, MediaSample, MediaStreamTrack, SampleStreamSource,
        SampleStreamTrack,
    },
    rtp::RtcpPacket,
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

/// Type alias for the sender channel
pub type MediaSender = SampleStreamSource;

/// Peer identifier within a BridgePeer (maps to LegId in sip_session).
pub type PeerId = String;

const BRIDGE_OUTPUT_PEER: u8 = 0;
const BRIDGE_OUTPUT_FILE: u8 = 1;
const BRIDGE_OUTPUT_MUTED: u8 = 2;

/// Atomic state for one endpoint's output mode + file source.
/// Wrapped in a single Mutex to prevent TOCTOU between
/// replace_output_with_file and replace_output_with_peer.
struct OutputState {
    mode: u8,
    file_source: Option<crate::media::FileTrackPlaybackSource>,
    next_rtp_timestamp: Option<u32>,
    next_sequence_number: Option<u16>,
    active_rtp_offset: Option<u32>,
    active_seq_offset: Option<u16>,
    /// When true, the next audio frame sent on this output must carry the RTP
    /// marker bit (RFC 3550 §5.1) so the receiver resets its jitter buffer.
    /// Set by `replace_output_with_file` / `replace_output_with_silence`
    /// and cleared after the first frame is emitted.
    marker_pending: bool,
}

/// One peer's media endpoint within an N-peer bridge.
pub struct PeerEntry {
    /// PeerConnection for this peer
    pub pc: PeerConnection,
    /// Transport mode (WebRTC or RTP)
    pub transport: rustrtc::TransportMode,
    /// Audio sender channel (for sending media to this peer)
    pub audio_sender: Option<MediaSender>,
    /// Audio codec for this peer
    pub codec: AudioCodecType,
    /// DTMF detection config
    pub dtmf_payload_type: Option<u8>,
    pub dtmf_sink: Option<Arc<dyn Fn(char) + Send + Sync + 'static>>,
    /// Output state for file/peer/mute
    _output_state: Arc<parking_lot::Mutex<OutputState>>,
}

/// One media endpoint owned by a BridgePeer. Kept for backward compat;
/// new code should use PeerId-based APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeEndpoint {
    Caller,
    Callee,
}

/// Per-direction statistics for a media bridge leg.
/// All counters are cumulative since bridge start and updated atomically.
struct LegStats {
    /// RTP packets forwarded successfully
    packets: AtomicU64,
    /// Bytes forwarded (RTP payload)
    bytes: AtomicU64,
    /// Estimated lost packets via RTP sequence-number gaps
    lost: AtomicU64,
    /// Packets dropped due to send-channel errors
    dropped: AtomicU64,
}

impl LegStats {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }
}

struct VideoForwardingTrack {
    id: String,
    inner: Arc<dyn MediaStreamTrack>,
    payload_type: Arc<AtomicU8>,
    payload_map: Arc<parking_lot::RwLock<HashMap<u8, u8>>>,
}

#[async_trait::async_trait]
impl MediaStreamTrack for VideoForwardingTrack {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> MediaKind {
        MediaKind::Video
    }

    fn state(&self) -> rustrtc::media::track::TrackState {
        self.inner.state()
    }

    async fn recv(&self) -> rustrtc::media::error::MediaResult<MediaSample> {
        loop {
            let sample = self.inner.recv().await?;
            let MediaSample::Video(mut frame) = sample else {
                continue;
            };
            if matches!(frame.payload_type, Some(pt) if pt < 96) {
                continue;
            }
            let target_payload_type = frame
                .payload_type
                .and_then(|pt| self.payload_map.read().get(&pt).copied())
                .unwrap_or_else(|| self.payload_type.load(Ordering::Relaxed));
            frame.payload_type = Some(target_payload_type);
            frame.header_extension = None;
            frame.raw_packet = None;
            frame.csrcs.clear();
            return Ok(MediaSample::Video(frame));
        }
    }

    async fn request_key_frame(&self) -> rustrtc::media::error::MediaResult<()> {
        self.inner.request_key_frame().await
    }
}

/// Transport kind for one bridge leg endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegTransport {
    Caller,
    Callee,
}

impl LegTransport {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Caller => "Caller",
            Self::Callee => "Callee",
        }
    }

    const fn endpoint(self) -> BridgeEndpoint {
        match self {
            Self::Caller => BridgeEndpoint::Caller,
            Self::Callee => BridgeEndpoint::Callee,
        }
    }
}

/// Typed metadata describing source and destination leg transports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ForwardPath {
    from: LegTransport,
    to: LegTransport,
}

impl ForwardPath {
    const fn new(from: LegTransport, to: LegTransport) -> Self {
        Self { from, to }
    }

    const fn source_endpoint(self) -> BridgeEndpoint {
        self.from.endpoint()
    }

    /// Strip WebRTC-only header extensions when forwarding from caller (WebRTC) to callee (RTP) leg.
    const fn should_strip_caller_audio_metadata(self) -> bool {
        matches!(
            (self.from, self.to),
            (LegTransport::Caller, LegTransport::Callee)
        )
    }
}

impl std::fmt::Display for ForwardPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}→{}", self.from.as_str(), self.to.as_str())
    }
}

use crate::media::dtmf::{DtmfSink, PayloadMapping, DtmfDetector};

// ---------------------------------------------------------------------------
// ForwardLoopContext — builder for run_forward_loop / forward_track_to_sender
// ---------------------------------------------------------------------------

struct ForwardLoopContext {
    bridge_id: String,
    track: Arc<dyn MediaStreamTrack>,
    sender_weak: std::sync::Weak<parking_lot::Mutex<Option<MediaSender>>>,
    output_mode: Arc<AtomicU8>,
    cancel_token: CancellationToken,
    path: ForwardPath,
    leg_stats: Arc<LegStats>,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    recorder_leg: Option<RecLeg>,
    sipflow_tx: Option<mpsc::Sender<(RecLeg, SharedMediaSample, u64)>>,
    receive_clock: ReceiveTimestampClock,
    recording_paused: Arc<AtomicBool>,
    dtmf_sink: Arc<parking_lot::RwLock<Option<DtmfSink>>>,
    transcoder: Option<Arc<parking_lot::Mutex<Option<Transcoder>>>>,
    transcoder_timing: Option<Arc<parking_lot::Mutex<Option<RtpTiming>>>>,
    dtmf_mapping: Option<Arc<parking_lot::RwLock<Option<PayloadMapping>>>>,
    gate: Option<Arc<AtomicBool>>,
}

struct ForwardLoopContextBuilder {
    inner: ForwardLoopContext,
}

impl ForwardLoopContextBuilder {
    fn new(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<parking_lot::Mutex<Option<MediaSender>>>,
        output_mode: Arc<AtomicU8>,
        cancel_token: CancellationToken,
        path: ForwardPath,
        leg_stats: Arc<LegStats>,
    ) -> Self {
        Self {
            inner: ForwardLoopContext {
                bridge_id,
                track,
                sender_weak,
                output_mode,
                cancel_token,
                path,
                leg_stats,
                recorder: None,
                recorder_leg: None,
                sipflow_tx: None,
                receive_clock: ReceiveTimestampClock::new(),
                recording_paused: Arc::new(AtomicBool::new(false)),
                dtmf_sink: Arc::new(parking_lot::RwLock::new(None)),
                transcoder: None,
                transcoder_timing: None,
                dtmf_mapping: None,
                gate: None,
            },
        }
    }

    pub fn with_recorder(
        mut self,
        recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
        leg: Option<RecLeg>,
    ) -> Self {
        self.inner.recorder = recorder;
        self.inner.recorder_leg = leg;
        self
    }

    pub fn with_sipflow(
        mut self,
        tx: Option<mpsc::Sender<(RecLeg, SharedMediaSample, u64)>>,
        clock: ReceiveTimestampClock,
    ) -> Self {
        self.inner.sipflow_tx = tx;
        self.inner.receive_clock = clock;
        self
    }

    pub fn with_recording_paused(mut self, paused: Arc<AtomicBool>) -> Self {
        self.inner.recording_paused = paused;
        self
    }

    pub fn with_dtmf_sink(mut self, sink: Arc<parking_lot::RwLock<Option<DtmfSink>>>) -> Self {
        self.inner.dtmf_sink = sink;
        self
    }

    pub fn with_transcoder(
        mut self,
        transcoder: Option<Arc<parking_lot::Mutex<Option<Transcoder>>>>,
        timing: Option<Arc<parking_lot::Mutex<Option<RtpTiming>>>>,
    ) -> Self {
        self.inner.transcoder = transcoder;
        self.inner.transcoder_timing = timing;
        self
    }

    pub fn with_dtmf_mapping(
        mut self,
        mapping: Option<Arc<parking_lot::RwLock<Option<PayloadMapping>>>>,
    ) -> Self {
        self.inner.dtmf_mapping = mapping;
        self
    }

    pub fn with_gate(mut self, gate: Option<Arc<AtomicBool>>) -> Self {
        self.inner.gate = gate;
        self
    }

    pub fn build(self) -> ForwardLoopContext {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// FileOutputContext — builder for spawn_file_output_clock
// ---------------------------------------------------------------------------

struct FileOutputContext {
    bridge_id: String,
    endpoint: BridgeEndpoint,
    sender: Option<MediaSender>,
    output_state: Arc<parking_lot::Mutex<OutputState>>,
    output_mode_atomic: Arc<AtomicU8>,
    cancel_token: CancellationToken,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    recording_paused: Arc<AtomicBool>,
}

struct FileOutputContextBuilder {
    inner: FileOutputContext,
}

impl FileOutputContextBuilder {
    fn new(
        bridge_id: String,
        endpoint: BridgeEndpoint,
        sender: Option<MediaSender>,
        output_state: Arc<parking_lot::Mutex<OutputState>>,
        output_mode_atomic: Arc<AtomicU8>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            inner: FileOutputContext {
                bridge_id,
                endpoint,
                sender,
                output_state,
                output_mode_atomic,
                cancel_token,
                recorder: None,
                recording_paused: Arc::new(AtomicBool::new(false)),
            },
        }
    }

    pub fn with_recorder(
        mut self,
        recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
        paused: Arc<AtomicBool>,
    ) -> Self {
        self.inner.recorder = recorder;
        self.inner.recording_paused = paused;
        self
    }

    pub fn build(self) -> FileOutputContext {
        self.inner
    }
}

fn output_mode_name(mode: u8) -> &'static str {
    match mode {
        BRIDGE_OUTPUT_PEER => "peer",
        BRIDGE_OUTPUT_FILE => "file",
        BRIDGE_OUTPUT_MUTED => "muted",
        _ => "unknown",
    }
}

fn frame_ticks_20ms(clock_rate: u32) -> u32 {
    (clock_rate / 50).max(1)
}

fn scale_rtp_timestamp(rtp_timestamp: u32, source_rate: u32, target_rate: u32) -> u32 {
    if source_rate == target_rate {
        rtp_timestamp
    } else {
        (rtp_timestamp as u64 * target_rate as u64 / source_rate as u64) as u32
    }
}

/// Per-side state for one endpoint of the bridge (Caller or Callee).
struct BridgeSideState {
    pc: PeerConnection,
    send: Arc<parking_lot::Mutex<Option<MediaSender>>>,
    output_state: Arc<parking_lot::Mutex<OutputState>>,
    output_mode: Arc<AtomicU8>,
    video_send: Arc<parking_lot::Mutex<Option<MediaSender>>>,
    track: parking_lot::Mutex<Option<Arc<SampleStreamTrack>>>,
    sender_codec: Option<RtpCodecParameters>,
    video_codec: Option<RtpCodecParameters>,
    video_sender: parking_lot::Mutex<Option<Arc<RtpSender>>>,
    video_payload_type: Arc<AtomicU8>,
    video_payload_map: Arc<parking_lot::RwLock<HashMap<u8, u8>>>,
}

impl BridgeSideState {
    fn new(pc: PeerConnection) -> Self {
        Self {
            pc,
            send: Arc::new(parking_lot::Mutex::new(None)),
            output_state: Arc::new(parking_lot::Mutex::new(OutputState {
                mode: BRIDGE_OUTPUT_PEER,
                file_source: None,
                next_rtp_timestamp: None,
                next_sequence_number: None,
                active_rtp_offset: None,
                active_seq_offset: None,
                marker_pending: false,
            })),
            output_mode: Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
            video_send: Arc::new(parking_lot::Mutex::new(None)),
            track: parking_lot::Mutex::new(None),
            sender_codec: None,
            video_codec: None,
            video_sender: parking_lot::Mutex::new(None),
            video_payload_type: Arc::new(AtomicU8::new(96)),
            video_payload_map: Arc::new(parking_lot::RwLock::new(HashMap::new())),
        }
    }
}

/// Identifies which side of the bridge to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeSide {
    Caller,
    Callee,
}

/// Per-direction codec state: transcoder + timing + DTMF mapping.
struct DirectionCodecState {
    transcoder: Arc<parking_lot::Mutex<Option<Transcoder>>>,
    timing: Arc<parking_lot::Mutex<Option<RtpTiming>>>,
    dtmf_mapping: Arc<parking_lot::RwLock<Option<PayloadMapping>>>,
}

impl DirectionCodecState {
    fn new() -> Self {
        Self {
            transcoder: Arc::new(parking_lot::Mutex::new(None)),
            timing: Arc::new(parking_lot::Mutex::new(None)),
            dtmf_mapping: Arc::new(parking_lot::RwLock::new(None)),
        }
    }
}

/// Per-direction media forwarding stats.
struct BridgeStats {
    caller_to_callee: Arc<LegStats>,
    callee_to_caller: Arc<LegStats>,
}

/// N-peer routing state.
struct PeerRouteState {
    peers: Arc<parking_lot::Mutex<std::collections::HashMap<PeerId, PeerEntry>>>,
    routes: Arc<parking_lot::Mutex<std::collections::HashMap<PeerId, std::collections::HashSet<PeerId>>>>,
}

impl PeerRouteState {
    fn new() -> Self {
        Self {
            peers: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            routes: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

/// RTP timeout configuration.
struct RtpTimeoutConfig {
    duration: Option<std::time::Duration>,
    notify_tx: Option<mpsc::Sender<String>>,
}

impl RtpTimeoutConfig {
    const fn none() -> Self {
        Self { duration: None, notify_tx: None }
    }
}

pub struct BridgePeer {
    pub(crate) id: String,
    pub(crate) session_id: Option<String>,
    caller: BridgeSideState,
    callee: BridgeSideState,
    bridge_tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    sub_tasks: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    cancel_token: CancellationToken,
    forwarding_started: AtomicBool,
    caller_gate: Arc<AtomicBool>,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    recording_paused: Arc<AtomicBool>,
    sipflow_tx: Option<mpsc::Sender<(RecLeg, SharedMediaSample, u64)>>,
    receive_clock: ReceiveTimestampClock,
    dtmf_sink: Arc<parking_lot::RwLock<Option<DtmfSink>>>,
    stats: BridgeStats,
    callee_to_caller_codec: DirectionCodecState,
    caller_to_callee_codec: DirectionCodecState,
    peer_routes: PeerRouteState,
    rtp_timeout: RtpTimeoutConfig,
}

// ---------------------------------------------------------------------------
// ForwardTrackArgs / DirectionParams — helpers for spawn_bidirectional_forwarder
// ---------------------------------------------------------------------------

/// Common args shared by both bridge directions.
struct ForwardTrackArgs {
    cancel_token: CancellationToken,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    recording_paused: Arc<AtomicBool>,
    sipflow_tx: Option<mpsc::Sender<(RecLeg, SharedMediaSample, u64)>>,
    receive_clock: ReceiveTimestampClock,
    dtmf_sink: Arc<parking_lot::RwLock<Option<DtmfSink>>>,
}

/// Per-direction parameters for process_track_event.
struct DirectionParams {
    target_pc: PeerConnection,
    sender_weak: std::sync::Weak<parking_lot::Mutex<Option<MediaSender>>>,
    output_mode: Arc<AtomicU8>,
    direction: &'static str,
    path: ForwardPath,
    leg_stats: Arc<LegStats>,
    recorder_leg: Option<RecLeg>,
    video_payload_type: Arc<AtomicU8>,
    video_payload_map: Arc<parking_lot::RwLock<HashMap<u8, u8>>>,
    video_track_label: &'static str,
    pli_label: &'static str,
    transcoder: Option<Arc<parking_lot::Mutex<Option<Transcoder>>>>,
    transcoder_timing: Option<Arc<parking_lot::Mutex<Option<RtpTiming>>>>,
    dtmf_mapping: Option<Arc<parking_lot::RwLock<Option<PayloadMapping>>>>,
    gate: Option<Arc<AtomicBool>>,
}

enum DirectionEventResult {
    RePin,
    Break,
}

impl BridgePeer {
    /// Create a new bridge peer with given WebRTC and RTP PeerConnections
    pub fn new(id: String, caller_pc: PeerConnection, callee_pc: PeerConnection) -> Self {
        Self {
            session_id: None,
            id,
            caller: BridgeSideState::new(caller_pc),
            callee: BridgeSideState::new(callee_pc),
            bridge_tasks: parking_lot::Mutex::new(Vec::new()),
            sub_tasks: Arc::new(parking_lot::Mutex::new(Vec::new())),
            cancel_token: CancellationToken::new(),
            forwarding_started: AtomicBool::new(false),
            caller_gate: Arc::new(AtomicBool::new(false)),
            recorder: None,
            recording_paused: Arc::new(AtomicBool::new(false)),
            sipflow_tx: None,
            receive_clock: ReceiveTimestampClock::new(),
            dtmf_sink: Arc::new(parking_lot::RwLock::new(None)),
            stats: BridgeStats { caller_to_callee: LegStats::new(), callee_to_caller: LegStats::new() },
            callee_to_caller_codec: DirectionCodecState::new(),
            caller_to_callee_codec: DirectionCodecState::new(),
            peer_routes: PeerRouteState::new(),
            rtp_timeout: RtpTimeoutConfig::none(),
        }
    }

    /// Allow WebRTC→RTP forwarding. Call this when the callee answers (200 OK) or on
    /// re-INVITE, NOT on 183 early media. This prevents sending WebRTC audio to a SIP
    /// endpoint that may not be ready to receive it yet.
    pub fn open_caller_gate(&self) {
        self.caller_gate.store(true, Ordering::Release);
        debug!(
            bridge_id = %self.id,
            session_id = ?self.session_id,
            "Caller gate opened — WebRTC→RTP forwarding enabled"
        );
    }

    /// Returns whether the caller gate has been opened.
    pub fn is_caller_gate_open(&self) -> bool {
        self.caller_gate.load(Ordering::Acquire)
    }

    fn side_state(&self, side: BridgeSide) -> &BridgeSideState {
        match side {
            BridgeSide::Caller => &self.caller,
            BridgeSide::Callee => &self.callee,
        }
    }

    /// Register an audio sample track on one bridge side.
    fn setup_audio_side(&self, side: BridgeSide, params: RtpCodecParameters) {
        let (tx, track, _) = rustrtc::media::track::sample_track(MediaKind::Audio, 100);
        let st = self.side_state(side);
        *st.track.lock() = Some(track.clone());
        let pc = st.pc.clone();
        {
            let _g = crate::utils::media_enter();
            let _ = pc.add_track(track, params);
        }
        *st.send.lock() = Some(tx);
    }

    /// Register a video sample track on one bridge side (if not already present).
    fn setup_video_side(&self, side: BridgeSide, params: &RtpCodecParameters) -> Result<()> {
        let st = self.side_state(side);
        if st.video_send.lock().is_some() {
            return Ok(());
        }
        st.video_payload_type.store(params.payload_type, Ordering::Relaxed);
        let (tx, track, _) = rustrtc::media::track::sample_track(MediaKind::Video, 100);
        let pc = st.pc.clone();
        let sender = {
            let _g = crate::utils::media_enter();
            pc.add_track(track, params.clone())?
        };
        *st.video_sender.lock() = Some(sender);
        *st.video_send.lock() = Some(tx);
        Ok(())
    }

    pub async fn setup_bridge_with_codecs(
        &self,
        caller_params: RtpCodecParameters,
        callee_params: RtpCodecParameters,
    ) -> Result<()> {
        self.setup_audio_side(BridgeSide::Caller, caller_params);
        self.setup_audio_side(BridgeSide::Callee, callee_params);

        let mut tasks = self.bridge_tasks.lock();
        tasks.push(Self::spawn_file_output_clock(
            FileOutputContextBuilder::new(
                self.id.clone(),
                BridgeEndpoint::Caller,
                self.caller.send.lock().clone(),
                Arc::clone(&self.caller.output_state),
                Arc::clone(&self.caller.output_mode),
                self.cancel_token.clone(),
            )
            .with_recorder(self.recorder.clone(), self.recording_paused.clone())
            .build(),
        ));
        tasks.push(Self::spawn_file_output_clock(
            FileOutputContextBuilder::new(
                self.id.clone(),
                BridgeEndpoint::Callee,
                self.callee.send.lock().clone(),
                Arc::clone(&self.callee.output_state),
                Arc::clone(&self.callee.output_mode),
                self.cancel_token.clone(),
            )
            .with_recorder(self.recorder.clone(), self.recording_paused.clone())
            .build(),
        ));
        drop(tasks);

        for side in [BridgeSide::Caller, BridgeSide::Callee] {
            if let Some(ref p) = self.side_state(side).video_codec {
                self.setup_video_side(side, p)?;
                debug!(bridge_id = %self.id, pt = p.payload_type, clock_rate = p.clock_rate, "Side {side:?} video sender setup complete");
            }
        }
        Ok(())
    }

    /// Dynamically add video tracks to BOTH bridge sides mid-call.
    /// Used when a re-INVITE adds video to a previously audio-only call.
    /// Caller side determines the codec PT/clock-rate from its offer;
    /// the same params are used on the opposite side for symmetric video.
    pub async fn add_video_track(&self, pt: u8, clock_rate: u32) -> Result<()> {
        let params = RtpCodecParameters { payload_type: pt, clock_rate, channels: 0 };
        for side in [BridgeSide::Caller, BridgeSide::Callee] {
            self.setup_video_side(side, &params)?;
        }
        debug!(bridge_id = %self.id, pt = pt, clock_rate = clock_rate, "Video tracks added to both bridge sides");
        Ok(())
    }

    pub async fn has_video(&self) -> bool {
        self.caller.video_send.lock().is_some()
            && self.callee.video_send.lock().is_some()
    }

    /// Update target payload types stamped onto pass-through video frames.
    pub fn set_video_payload_types(
        &self,
        caller_params: Option<RtpCodecParameters>,
        callee_params: Option<RtpCodecParameters>,
    ) {
        if let Some(params) = caller_params.as_ref() {
            self.caller.video_payload_type
                .store(params.payload_type, Ordering::Relaxed);
            debug!(
                bridge_id = %self.id,
                side = "caller",
                pt = params.payload_type,
                clock_rate = params.clock_rate,
                "Video pass-through payload type updated"
            );
        }
        if let Some(params) = callee_params.as_ref() {
            self.callee.video_payload_type
                .store(params.payload_type, Ordering::Relaxed);
            debug!(
                bridge_id = %self.id,
                side = "callee",
                pt = params.payload_type,
                clock_rate = params.clock_rate,
                "Video pass-through payload type updated"
            );
        }
    }

    fn video_payload_map_by_codec(
        source_caps: &[rustrtc::VideoCapability],
        target_caps: &[rustrtc::VideoCapability],
    ) -> HashMap<u8, u8> {
        let mut map = HashMap::new();
        for source in source_caps {
            if let Some(target) = target_caps.iter().find(|target| {
                source.codec_name.eq_ignore_ascii_case(&target.codec_name)
                    && source.clock_rate == target.clock_rate
            }) {
                map.entry(source.payload_type)
                    .or_insert(target.payload_type);
            }
        }
        map
    }

    /// Update pass-through video payload mappings by codec identity.
    ///
    /// The bridge does not transcode video, so payload type numbers must be translated
    /// to the peer-local payload type for the same codec instead of copying a single
    /// "first" payload type across both legs.
    pub fn set_video_payload_maps(
        &self,
        webrtc_caps: &[rustrtc::VideoCapability],
        rtp_caps: &[rustrtc::VideoCapability],
    ) {
        let rtp_to_webrtc = Self::video_payload_map_by_codec(rtp_caps, webrtc_caps);
        let webrtc_to_rtp = Self::video_payload_map_by_codec(webrtc_caps, rtp_caps);

        let caller_fallback = rtp_caps
            .iter()
            .find_map(|cap| rtp_to_webrtc.get(&cap.payload_type).copied());
        let callee_fallback = webrtc_caps
            .iter()
            .find_map(|cap| webrtc_to_rtp.get(&cap.payload_type).copied());

        if let Some(pt) = caller_fallback {
            self.caller.video_payload_type.store(pt, Ordering::Relaxed);
        }
        if let Some(pt) = callee_fallback {
            self.callee.video_payload_type.store(pt, Ordering::Relaxed);
        }

        *self.caller.video_payload_map.write() = rtp_to_webrtc;
        *self.callee.video_payload_map.write() = webrtc_to_rtp;
        let caller_map = self.caller.video_payload_map.read().clone();
        let callee_map = self.callee.video_payload_map.read().clone();

        debug!(
            bridge_id = %self.id,
            rtp_to_webrtc = ?caller_map,
            webrtc_to_rtp = ?callee_map,
            caller_fallback_pt = ?caller_fallback,
            callee_fallback_pt = ?callee_fallback,
            "Video pass-through payload maps updated"
        );
    }

    /// Setup the bridge with codec parameters.
    /// Uses sender codecs from builder if set, otherwise falls back to defaults.
    pub async fn setup_bridge(&self) -> Result<()> {
        let caller_params = self
            .caller
            .sender_codec
            .clone()
            .unwrap_or(RtpCodecParameters {
                payload_type: 111,
                clock_rate: 48000,
                channels: 2,
            });
        let callee_params = self
            .callee
            .sender_codec
            .clone()
            .unwrap_or(RtpCodecParameters {
                payload_type: 0,
                clock_rate: 8000,
                channels: 1,
            });
        self.setup_bridge_with_codecs(caller_params, callee_params)
            .await
    }

    /// Start the bridge - begin forwarding media between sides
    /// Optimized: Merged bidirectional forwarding into a single task to reduce Tokio scheduling overhead
    pub async fn start_bridge(&self) {
        if self.forwarding_started.swap(true, Ordering::AcqRel) {
            debug!(bridge_id = %self.id, "Media bridge forwarding already started");
            return;
        }

        // Spawn a single bidirectional forwarding task instead of 2 separate tasks
        let bidirectional_task = self.spawn_bidirectional_forwarder();

        // Spawn a 5-second periodic stats logger for all bridge legs
        let stats_task = {
            let w2r = Arc::clone(&self.stats.caller_to_callee);
            let r2w = Arc::clone(&self.stats.callee_to_caller);
            let bridge_id = self.id.clone();
            let cancel = self.cancel_token.clone();
            let rtp_timeout = self.rtp_timeout.duration;
            let rtp_timeout_tx = self.rtp_timeout.notify_tx.clone();
            crate::utils::media_spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // skip initial immediate tick
                interval.tick().await;
                let (mut prev_w_pkts, mut prev_w_lost, mut prev_w_bytes) = (0u64, 0u64, 0u64);
                let (mut prev_r_pkts, mut prev_r_lost, mut prev_r_bytes) = (0u64, 0u64, 0u64);
                let mut caller_silence_start: Option<std::time::Instant> = None;
                let mut callee_silence_start: Option<std::time::Instant> = None;
                let mut rtp_timeout_fired = false;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = interval.tick() => {
                            let w_pkts  = w2r.packets.load(Ordering::Relaxed);
                            let w_bytes = w2r.bytes.load(Ordering::Relaxed);
                            let w_lost  = w2r.lost.load(Ordering::Relaxed);
                            let w_drop  = w2r.dropped.load(Ordering::Relaxed);
                            let r_pkts  = r2w.packets.load(Ordering::Relaxed);
                            let r_bytes = r2w.bytes.load(Ordering::Relaxed);
                            let r_lost  = r2w.lost.load(Ordering::Relaxed);
                            let r_drop  = r2w.dropped.load(Ordering::Relaxed);

                            let dw_pkts  = w_pkts.saturating_sub(prev_w_pkts);
                            let dw_bytes = w_bytes.saturating_sub(prev_w_bytes);
                            let dw_lost  = w_lost.saturating_sub(prev_w_lost);
                            let dr_pkts  = r_pkts.saturating_sub(prev_r_pkts);
                            let dr_bytes = r_bytes.saturating_sub(prev_r_bytes);
                            let dr_lost  = r_lost.saturating_sub(prev_r_lost);

                            let w_loss_pct = if dw_pkts + dw_lost > 0 {
                                dw_lost as f64 / (dw_pkts + dw_lost) as f64 * 100.0
                            } else { 0.0 };
                            let r_loss_pct = if dr_pkts + dr_lost > 0 {
                                dr_lost as f64 / (dr_pkts + dr_lost) as f64 * 100.0
                            } else { 0.0 };

                            debug!(
                                bridge_id = %bridge_id,
                                caller_to_callee_pkts_5s = dw_pkts,
                                caller_to_callee_kbps    = dw_bytes * 8 / 5 / 1000,
                                caller_to_callee_loss    = format!("{:.2}%", w_loss_pct),
                                caller_to_callee_drop    = w_drop,
                                callee_to_caller_pkts_5s = dr_pkts,
                                callee_to_caller_kbps    = dr_bytes * 8 / 5 / 1000,
                                callee_to_caller_loss    = format!("{:.2}%", r_loss_pct),
                                callee_to_caller_drop    = r_drop,
                                "Bridge leg stats [5s]"
                            );

                            if !rtp_timeout_fired
                                && let Some(timeout) = rtp_timeout
                            {
                                if dw_pkts == 0 {
                                    caller_silence_start.get_or_insert(std::time::Instant::now());
                                } else {
                                    caller_silence_start = None;
                                }
                                if dr_pkts == 0 {
                                    callee_silence_start.get_or_insert(std::time::Instant::now());
                                } else {
                                    callee_silence_start = None;
                                }

                                if caller_silence_start.is_some_and(|s| s.elapsed() >= timeout) {
                                    warn!(bridge_id = %bridge_id, ?timeout, "RTP timeout: caller side silent");
                                    if let Some(ref tx) = rtp_timeout_tx {
                                        let _ = tx.try_send("caller_silent".to_string());
                                    }
                                    rtp_timeout_fired = true;
                                } else if callee_silence_start.is_some_and(|s| s.elapsed() >= timeout) {
                                    warn!(bridge_id = %bridge_id, ?timeout, "RTP timeout: callee side silent");
                                    if let Some(ref tx) = rtp_timeout_tx {
                                        let _ = tx.try_send("callee_silent".to_string());
                                    }
                                    rtp_timeout_fired = true;
                                }
                            }

                            (prev_w_pkts, prev_w_lost, prev_w_bytes) = (w_pkts, w_lost, w_bytes);
                            (prev_r_pkts, prev_r_lost, prev_r_bytes) = (r_pkts, r_lost, r_bytes);
                        }
                    }
                }
            })
        };

        let mut tasks = self.bridge_tasks.lock();
        tasks.push(bidirectional_task);
        tasks.push(stats_task);
        tasks.push(self.spawn_peer_forward_loops());
    }

    /// Get the WebRTC-side sender (for test injection)
    pub async fn get_caller_sender(&self) -> Option<MediaSender> {
        self.caller.send.lock().clone()
    }

    /// Get the RTP-side sender (for test injection)
    pub async fn get_callee_sender(&self) -> Option<MediaSender> {
        self.callee.send.lock().clone()
    }

    pub fn set_dtmf_sink(
        &self,
        endpoint: BridgeEndpoint,
        mut payload_types: Vec<u8>,
        handler: Arc<dyn Fn(char) + Send + Sync + 'static>,
    ) {
        payload_types.sort_unstable();
        payload_types.dedup();
        if payload_types.is_empty() {
            warn!(
                bridge_id = %self.id,
                endpoint = ?endpoint,
                "Bridge DTMF sink install skipped: no payload types provided"
            );
            return;
        }

        let mut sink = self.dtmf_sink.write();
        *sink = Some(DtmfSink {
            endpoint,
            payload_types: payload_types.clone(),
            handler,
        });

        debug!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            payload_types = ?payload_types,
            "Bridge DTMF sink installed"
        );
    }

    /// Shared state transition for file/silence output: reserve the sender,
    /// install `FileTrackPlaybackSource`, set `BRIDGE_OUTPUT_FILE` mode.
    async fn set_output_to_file_source(
        &self,
        endpoint: BridgeEndpoint,
        source: crate::media::FileTrackPlaybackSource,
        log_msg: &str,
    ) -> Result<()> {
        match endpoint {
            BridgeEndpoint::Caller => self.get_caller_sender().await,
            BridgeEndpoint::Callee => self.get_callee_sender().await,
        }
        .ok_or_else(|| anyhow::anyhow!("bridge {:?} output sender is not ready", endpoint))?;

        let mut state = self.output_state(endpoint).lock();
        let old_source = state.file_source.replace(source);
        state.mode = BRIDGE_OUTPUT_FILE;
        state.active_rtp_offset = None;
        state.active_seq_offset = None;
        state.marker_pending = true;
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_FILE, Ordering::Release);
        drop(state);
        drop(old_source);

        info!(
            bridge_id = %self.id,
            session_id = ?self.session_id,
            endpoint = ?endpoint,
            "{}", log_msg,
        );
        Ok(())
    }

    pub async fn replace_output_with_file(
        &self,
        endpoint: BridgeEndpoint,
        track: &crate::media::FileTrack,
    ) -> Result<()> {
        let source = track.create_playback_source().await?;
        self.set_output_to_file_source(endpoint, source, "Bridge output replaced with file source")
            .await
    }

    pub async fn replace_output_with_silence(
        &self,
        endpoint: BridgeEndpoint,
        codec_info: crate::media::negotiate::CodecInfo,
    ) -> Result<()> {
        let track = crate::media::FileTrack::new(format!("{}-{:?}-silence", self.id, endpoint))
            .with_loop(true)
            .with_codec_info(codec_info);
        let source = track.create_playback_source().await?;
        self.set_output_to_file_source(endpoint, source, "Bridge output replaced with silence source")
            .await
    }

    pub async fn replace_output_with_peer(&self, endpoint: BridgeEndpoint) {
        let mut state = self.output_state(endpoint).lock();
        state.mode = BRIDGE_OUTPUT_PEER;
        let old_source = state.file_source.take();
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_PEER, Ordering::Release);
        drop(state);
        drop(old_source);
        debug!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            "Bridge output replaced with peer source"
        );
    }

    pub async fn mute_output(&self, endpoint: BridgeEndpoint) {
        let mut state = self.output_state(endpoint).lock();
        state.mode = BRIDGE_OUTPUT_MUTED;
        let old_source = state.file_source.take();
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_MUTED, Ordering::Release);
        drop(state);
        drop(old_source);
        info!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            "Bridge output muted"
        );
    }

    /// Expose the raw output-mode atomic for caller endpoint (tests only).
    #[cfg(test)]
    pub fn caller_output_mode_atomic(&self) -> &Arc<AtomicU8> {
        &self.caller.output_mode
    }

    #[cfg(test)]
    pub fn output_mode_atomic(&self, endpoint: BridgeEndpoint) -> &Arc<AtomicU8> {
        self.output_mode(endpoint)
    }

    /// Send RFC 4733 telephone-event DTMF packets to `endpoint`.
    ///
    /// For each digit in `digits` the method injects three telephone-event RTP
    /// frames into the endpoint's sender:
    ///   1. Begin packet  (marker=true,  E=0, duration=160)
    ///   2. Middle packet (marker=false, E=0, duration=320)
    ///   3. End packet    (marker=false, E=1, duration=480)
    ///
    /// The DTMF payload type is taken from the `caller_to_callee` or
    /// `callee_to_caller` mapping if available, falling back to the RFC
    /// conventional default of 101.
    pub async fn send_dtmf_to_endpoint(
        &self,
        endpoint: BridgeEndpoint,
        digits: &str,
    ) -> Result<()> {
        let sender = match endpoint {
            BridgeEndpoint::Caller => self.get_caller_sender().await,
            BridgeEndpoint::Callee => self.get_callee_sender().await,
        }
        .ok_or_else(|| {
            anyhow::anyhow!("bridge {:?} sender not ready for DTMF injection", endpoint)
        })?;

        // Resolve DTMF payload type from negotiated mapping.
        let dtmf_pt = {
            let mapping = match endpoint {
                BridgeEndpoint::Caller => self.callee_to_caller_codec.dtmf_mapping.read(),
                BridgeEndpoint::Callee => self.caller_to_callee_codec.dtmf_mapping.read(),
            };
            mapping.as_ref().map(|m| m.target_pt).unwrap_or(101u8)
        };

        // Audio clock rate used for DTMF timing (8000 Hz ≡ 160 samples/20ms).
        const CLOCK_RATE: u32 = 8000;
        const TICKS_20MS: u32 = 160;

        let mut rtp_timestamp: u32 = 0;
        let mut sequence_number: u16 = 0;

        for c in digits.chars() {
            let event_code = crate::media::telephone_event::dtmf_char_to_code(c)
                .ok_or_else(|| anyhow::anyhow!("Unknown DTMF digit: {:?}", c))?;

            // RFC 4733 §2.3 — three packets per event (begin, middle, end).
            for (packet_idx, (duration, is_end)) in [
                (TICKS_20MS, false),
                (TICKS_20MS * 2, false),
                (TICKS_20MS * 3, true),
            ]
            .iter()
            .enumerate()
            {
                // Telephone-event payload: [event, E|R|volume, duration_hi, duration_lo]
                let mut payload = [0u8; 4];
                payload[0] = event_code;
                payload[1] = if *is_end { 0x80 } else { 0x00 }; // E bit
                payload[2] = (duration >> 8) as u8;
                payload[3] = (*duration & 0xFF) as u8;

                let frame = AudioFrame {
                    rtp_timestamp,
                    clock_rate: CLOCK_RATE,
                    data: bytes::Bytes::copy_from_slice(&payload),
                    sequence_number: Some(sequence_number),
                    payload_type: Some(dtmf_pt),
                    marker: packet_idx == 0, // marker on first packet of each event
                    header_extension: None,
                    raw_packet: None,
                    source_addr: None,
                };

                if let Err(e) = sender.send(MediaSample::Audio(frame)).await {
                    warn!(
                        bridge_id = %self.id,
                        endpoint = ?endpoint,
                        digit = %c,
                        error = %e,
                        "DTMF telephone-event send failed"
                    );
                }

                sequence_number = sequence_number.wrapping_add(1);
            }

            // Advance RTP timestamp by one event duration (3 × 20ms = 60ms).
            rtp_timestamp = rtp_timestamp.wrapping_add(TICKS_20MS * 3);
        }

        debug!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            digits = %digits,
            dtmf_pt,
            "Sent RFC 4733 DTMF telephone-event packets"
        );
        Ok(())
    }

    /// Add a peer to the N-peer bridge.
    pub async fn add_peer(&self, peer_id: PeerId, entry: PeerEntry) -> Result<()> {
        let mut peers = self.peer_routes.peers.lock();
        if peers.contains_key(&peer_id) {
            return Err(anyhow::anyhow!(
                "Peer {} already exists in bridge {}",
                peer_id,
                self.id
            ));
        }
        peers.insert(peer_id, entry);
        Ok(())
    }

    /// Remove a peer from the N-peer bridge.
    pub async fn remove_peer(&self, peer_id: &PeerId) -> Option<PeerEntry> {
        let mut peers = self.peer_routes.peers.lock();
        let mut routes = self.peer_routes.routes.lock();
        routes.remove(peer_id);
        // Remove this peer from all other routes
        for dests in routes.values_mut() {
            dests.remove(peer_id);
        }
        peers.remove(peer_id)
    }

    /// Set forwarding route: audio from `from_peer` is sent to all peers in `to_peers`.
    pub async fn set_route(
        &self,
        from_peer: PeerId,
        to_peers: std::collections::HashSet<PeerId>,
    ) -> Result<()> {
        let peers = self.peer_routes.peers.lock();
        for dest in &to_peers {
            if !peers.contains_key(dest) {
                return Err(anyhow::anyhow!("Route destination peer {} not found", dest));
            }
        }
        drop(peers);
        let mut routes = self.peer_routes.routes.lock();
        routes.insert(from_peer, to_peers);
        Ok(())
    }

    /// Configure a transcoder for media flowing out of the given endpoint.
    ///
    /// `from_endpoint` is the source side of the direction to transcode:
    /// - `BridgeEndpoint::Callee` – transcode media received from the callee side before sending to caller
    /// - `BridgeEndpoint::Caller` – transcode media received from the caller side before sending to callee
    ///
    /// When the ingress and egress codecs differ (e.g. caller=G.729, agent=PCMU),
    /// this inserts a decode–resample–encode pipeline so both sides hear intelligible audio.
    pub fn set_transcoder(
        &self,
        from_endpoint: BridgeEndpoint,
        source: audio_codec::CodecType,
        target: audio_codec::CodecType,
        target_pt: u8,
    ) {
        let (transcoder_slot, timing_slot) = match from_endpoint {
            BridgeEndpoint::Callee => (
                &self.callee_to_caller_codec.transcoder,
                &self.callee_to_caller_codec.timing,
            ),
            BridgeEndpoint::Caller => (
                &self.caller_to_callee_codec.transcoder,
                &self.caller_to_callee_codec.timing,
            ),
        };
        let source_cr = source.clock_rate();
        let target_cr = target.clock_rate();
        *transcoder_slot.lock() = Some(Transcoder::new(source, target, target_pt));
        if source_cr != target_cr {
            *timing_slot.lock() = Some(RtpTiming::default());
        } else {
            timing_slot.lock().take();
        }
        info!(
            bridge_id = %self.id,
            from = ?from_endpoint,
            ?source,
            ?target,
            "Bridge transcoder configured"
        );
    }

    /// Remove the transcoder for a given direction.
    pub fn clear_transcoder(&self, from_endpoint: BridgeEndpoint) {
        let (transcoder_slot, timing_slot) = match from_endpoint {
            BridgeEndpoint::Callee => (
                &self.callee_to_caller_codec.transcoder,
                &self.callee_to_caller_codec.timing,
            ),
            BridgeEndpoint::Caller => (
                &self.caller_to_callee_codec.transcoder,
                &self.caller_to_callee_codec.timing,
            ),
        };
        *transcoder_slot.lock() = None;
        *timing_slot.lock() = None;
    }

    /// Configure telephone-event mapping for media received from one bridge endpoint.
    pub fn set_dtmf_mapping(
        &self,
        from_endpoint: BridgeEndpoint,
        source_pt: u8,
        source_clock_rate: u32,
        target_pt: u8,
        target_clock_rate: u32,
    ) {
        let mapping_slot = match from_endpoint {
            BridgeEndpoint::Callee => &self.callee_to_caller_codec.dtmf_mapping,
            BridgeEndpoint::Caller => &self.caller_to_callee_codec.dtmf_mapping,
        };

        *mapping_slot.write() = Some(PayloadMapping {
            source_pt,
            target_pt,
            source_clock_rate,
            target_clock_rate,
        });

        debug!(
            bridge_id = %self.id,
            from = ?from_endpoint,
            source_pt,
            source_clock_rate,
            target_pt,
            target_clock_rate,
            "Bridge DTMF mapping configured"
        );
    }

    /// Remove telephone-event mapping for media received from one bridge endpoint.
    pub fn clear_dtmf_mapping(&self, from_endpoint: BridgeEndpoint) {
        let mapping_slot = match from_endpoint {
            BridgeEndpoint::Callee => &self.callee_to_caller_codec.dtmf_mapping,
            BridgeEndpoint::Caller => &self.caller_to_callee_codec.dtmf_mapping,
        };
        mapping_slot.write().take();
    }

    fn output_state(&self, endpoint: BridgeEndpoint) -> &Arc<parking_lot::Mutex<OutputState>> {
        match endpoint {
            BridgeEndpoint::Caller => &self.caller.output_state,
            BridgeEndpoint::Callee => &self.callee.output_state,
        }
    }

    fn output_mode(&self, endpoint: BridgeEndpoint) -> &Arc<AtomicU8> {
        match endpoint {
            BridgeEndpoint::Caller => &self.caller.output_mode,
            BridgeEndpoint::Callee => &self.callee.output_mode,
        }
    }

    fn spawn_file_output_clock(ctx: FileOutputContext) -> tokio::task::JoinHandle<()> {
        let recorder_leg = match ctx.endpoint {
            BridgeEndpoint::Callee => Some(RecLeg::A),
            BridgeEndpoint::Caller => Some(RecLeg::B),
        };

        crate::utils::media_spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = ctx.cancel_token.cancelled() => {
                        break;
                    }
                    _ = interval.tick() => {
                            let sample = {
                            let mut guard = ctx.output_state.lock();
                            if guard.mode != BRIDGE_OUTPUT_FILE {
                                continue;
                            }
                            let Some(source) = guard.file_source.as_mut() else {
                                guard.mode = BRIDGE_OUTPUT_PEER;
                                ctx.output_mode_atomic.store(BRIDGE_OUTPUT_PEER, Ordering::Release);
                                continue;
                            };
                            source.next_audio_sample()
                        };

                        let Some(mut sample) = sample else {
                            let mut guard = ctx.output_state.lock();
                            guard.mode = BRIDGE_OUTPUT_PEER;
                            ctx.output_mode_atomic.store(BRIDGE_OUTPUT_PEER, Ordering::Release);
                            guard.file_source.take();
                            guard.active_rtp_offset = None;
                            guard.active_seq_offset = None;
                            debug!(
                                bridge_id = %ctx.bridge_id,
                                endpoint = ?ctx.endpoint,
                                "Bridge file output completed, restored peer output"
                            );
                            continue;
                        };

                        if let MediaSample::Audio(frame) = &mut sample {
                            let mut guard = ctx.output_state.lock();

                            if guard.marker_pending {
                                frame.marker = true;
                                guard.marker_pending = false;
                            }

                            let src_seq = frame.sequence_number.unwrap_or_default();
                            let src_ts = frame.rtp_timestamp;

                            let seq_offset = match guard.active_seq_offset {
                                Some(offset) => offset,
                                None => {
                                    let expected = guard.next_sequence_number.unwrap_or(src_seq);
                                    let offset = expected.wrapping_sub(src_seq);
                                    guard.active_seq_offset = Some(offset);
                                    offset
                                }
                            };

                            let ts_offset = match guard.active_rtp_offset {
                                Some(offset) => offset,
                                None => {
                                    let expected = guard.next_rtp_timestamp.unwrap_or(src_ts);
                                    let offset = expected.wrapping_sub(src_ts);
                                    guard.active_rtp_offset = Some(offset);
                                    offset
                                }
                            };

                            let mapped_seq = src_seq.wrapping_add(seq_offset);
                            let mapped_ts = src_ts.wrapping_add(ts_offset);
                            frame.sequence_number = Some(mapped_seq);
                            frame.rtp_timestamp = mapped_ts;

                            guard.next_sequence_number = Some(mapped_seq.wrapping_add(1));
                            guard.next_rtp_timestamp =
                                Some(mapped_ts.wrapping_add(frame_ticks_20ms(frame.clock_rate)));
                        }

                        if let (Some(rec), Some(leg)) = (&ctx.recorder, recorder_leg)
                            && !ctx.recording_paused.load(Ordering::Relaxed)
                            && let Some(mut guard) = rec.try_write()
                            && let Some(r) = guard.as_mut()
                        {
                            let _ = r.write_sample(
                                leg,
                                &sample,
                                None,
                                None::<u32>,
                                None::<AudioCodecType>,
                            );
                        }

                        let Some(sender) = ctx.sender.as_ref() else {
                            warn!(
                                bridge_id = %ctx.bridge_id,
                                endpoint = ?ctx.endpoint,
                                "Bridge file output sender is unavailable"
                            );
                            break;
                        };

                        match sender.try_send(sample.clone()) {
                            Ok(()) => {}
                            Err(MediaError::WouldBlock) => {
                                if let Err(error) = sender.send(sample).await {
                                    warn!(
                                        bridge_id = %ctx.bridge_id,
                                        endpoint = ?ctx.endpoint,
                                        error = %error,
                                        "Bridge file output send failed"
                                    );
                                    break;
                                }
                            }
                            Err(error) => {
                                warn!(
                                    bridge_id = %ctx.bridge_id,
                                    endpoint = ?ctx.endpoint,
                                    error = ?error,
                                    "Bridge file output send failed"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        })
    }

    /// Spawn per-peer receive loops for N>2 peers.
    /// Each peer's incoming tracks are forwarded to all route destinations.
    fn spawn_peer_forward_loops(&self) -> tokio::task::JoinHandle<()> {
        let peers_map = self.peer_routes.peers.clone();
        let routes_map = self.peer_routes.routes.clone();
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();

        crate::utils::media_spawn(async move {
            let extra_peers: Vec<(PeerId, PeerConnection)> = peers_map
                .lock()
                .iter()
                .filter(|(id, _)| *id != "webrtc" && *id != "rtp")
                .map(|(id, entry)| (id.clone(), entry.pc.clone()))
                .collect();

            for (peer_id, pc) in extra_peers {
                let routes = routes_map.clone();
                let cancel = cancel_token.child_token();
                let pid = peer_id.clone();
                let bid = bridge_id.clone();
                let peers_ref = peers_map.clone();

                crate::utils::media_spawn(async move {
                    let mut recv = Box::pin(pc.recv());
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            event = &mut recv => {
                                match event {
                                    Some(PeerConnectionEvent::Track(transceiver)) => {
                                        if let Some(receiver) = transceiver.receiver() {
                                            let track = receiver.track();
                                            Self::spawn_peer_track_forwarder(
                                                &bid, &pid, &routes, &peers_ref,
                                                &cancel, track,
                                            );
                                        }
                                        recv = Box::pin(pc.recv());
                                    }
                                    Some(_) => recv = Box::pin(pc.recv()),
                                    None => break,
                                }
                            }
                        }
                    }
                    debug!(bridge_id = %bid, peer_id = %pid, "N-peer recv loop ended");
                });
            }
        })
    }

    /// Spawn a per-track forwarder for an N>2 peer: reads from `track` and
    /// forwards samples to all route destinations' audio senders.
    fn spawn_peer_track_forwarder(
        bridge_id: &str,
        peer_id: &str,
        routes: &Arc<parking_lot::Mutex<std::collections::HashMap<PeerId, std::collections::HashSet<PeerId>>>>,
        peers_ref: &Arc<parking_lot::Mutex<std::collections::HashMap<PeerId, PeerEntry>>>,
        cancel: &CancellationToken,
        track: Arc<dyn MediaStreamTrack>,
    ) {
        let route_dests: Vec<PeerId> = routes
            .lock()
            .get(peer_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        if route_dests.is_empty() {
            return;
        }

        let track_id = track.id().to_string();
        let pid = peer_id.to_string();
        let bid = bridge_id.to_string();
        let dests = route_dests;
        let peers = peers_ref.clone();
        let fwd_cancel = cancel.child_token();

        crate::utils::media_spawn(async move {
            loop {
                let sample = tokio::select! {
                    biased;
                    _ = fwd_cancel.cancelled() => break,
                    res = track.recv() => match res {
                        Ok(s) => s,
                        Err(_) => break,
                    },
                };
                let guard = peers.lock();
                for dest in &dests {
                    if let Some(entry) = guard.get(dest) {
                        if let Some(ref sender) = entry.audio_sender {
                            let _ = sender.try_send(sample.clone());
                        }
                    }
                }
                drop(guard);
            }
            debug!(bridge_id = %bid, peer_id = %pid, track = %track_id, "N-peer forward task ended");
        });
    }

    pub async fn get_caller_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.caller.track.lock().clone()
    }

    /// Get the RTP-side track (for test consumption)
    pub async fn get_callee_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.callee.track.lock().clone()
    }

    /// Process a single PeerConnection event for one direction of the bridge.
    async fn handle_direction_event(
        bridge_id: &str,
        event: Option<PeerConnectionEvent>,
        source_pc: &PeerConnection,
        dir: &DirectionParams,
        common: &ForwardTrackArgs,
        sub_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    ) -> DirectionEventResult {
        match event {
            Some(rustrtc::PeerConnectionEvent::Track(transceiver)) => {
                Self::process_track_event(bridge_id, source_pc, dir, common, sub_tasks, transceiver).await;
                DirectionEventResult::RePin
            }
            Some(_) => DirectionEventResult::RePin,
            None => DirectionEventResult::Break,
        }
    }

    /// Handle a Track event for one direction.
    async fn process_track_event(
        bridge_id: &str,
        _source_pc: &PeerConnection,
        dir: &DirectionParams,
        common: &ForwardTrackArgs,
        sub_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        transceiver: Arc<rustrtc::RtpTransceiver>,
    ) {
        let Some(receiver) = transceiver.receiver() else {
            warn!(
                bridge_id = %bridge_id,
                direction = dir.direction,
                transceiver_id = transceiver.id(),
                kind = ?transceiver.kind(),
                mid = ?transceiver.mid(),
                "Bridge received track event without receiver"
            );
            return;
        };

        let track = receiver.track();
        let is_video = transceiver.kind() == rustrtc::MediaKind::Video;
        let payload_types: Vec<u8> = transceiver.get_payload_map().keys().copied().collect();

        debug!(
            bridge_id = %bridge_id,
            direction = dir.direction,
            transceiver_id = transceiver.id(),
            kind = ?transceiver.kind(),
            mid = ?transceiver.mid(),
            receiver_ssrc = receiver.ssrc(),
            track_id = %track.id(),
            payload_types = ?payload_types,
            "Bridge received track event"
        );

        if is_video {
            let Some(target_transceiver) = dir
                .target_pc
                .get_transceivers()
                .into_iter()
                .find(|t| t.kind() == rustrtc::MediaKind::Video)
            else {
                warn!(bridge_id = %bridge_id, direction = dir.direction, "Video transceiver not found on target PC");
                return;
            };
            let Some(existing_sender) = target_transceiver.sender() else {
                warn!(bridge_id = %bridge_id, "Target video transceiver has no sender for forwarding track");
                return;
            };

            let forwarding_track: Arc<dyn MediaStreamTrack> = Arc::new(VideoForwardingTrack {
                id: format!("{}-{}", bridge_id, dir.video_track_label),
                inner: track.clone(),
                payload_type: Arc::clone(&dir.video_payload_type),
                payload_map: Arc::clone(&dir.video_payload_map),
            });

            let mut sender_builder = rustrtc::RtpSender::builder(
                forwarding_track,
                existing_sender.ssrc(),
            )
            .stream_id(existing_sender.stream_id().to_string())
            .params(existing_sender.params());
            let cname_val = existing_sender.cname();
            if !cname_val.starts_with("rustrtc-cname-") {
                sender_builder = sender_builder.cname(cname_val.to_string());
            }
            let sender = sender_builder.build();
            target_transceiver.set_sender(Some(sender.clone()));

            let h = Self::spawn_pli_forwarder(
                bridge_id.to_string(),
                sender,
                track.clone(),
                common.cancel_token.clone(),
                dir.pli_label,
            );
            Self::prune_sub_tasks(sub_tasks).await;
            sub_tasks.lock().push(h);

            if let Err(e) = track.request_key_frame().await {
                debug!(
                    bridge_id = %bridge_id,
                    source_track = %track.id(),
                    error = %e,
                    "Initial video keyframe request failed"
                );
            }
            debug!(
                bridge_id = %bridge_id,
                source_track = %track.id(),
                target_ssrc = existing_sender.ssrc(),
                "Wired video forwarding track"
            );
            return;
        }

        // Audio track: spawn a forward_track_to_sender task.
        let h = Self::forward_track_to_sender(
            ForwardLoopContextBuilder::new(
                bridge_id.to_string(),
                track,
                dir.sender_weak.clone(),
                Arc::clone(&dir.output_mode),
                common.cancel_token.clone(),
                dir.path,
                Arc::clone(&dir.leg_stats),
            )
            .with_recorder(common.recorder.clone(), dir.recorder_leg)
            .with_sipflow(common.sipflow_tx.clone(), common.receive_clock.clone())
            .with_recording_paused(common.recording_paused.clone())
            .with_dtmf_sink(Arc::clone(&common.dtmf_sink))
            .with_transcoder(dir.transcoder.clone(), dir.transcoder_timing.clone())
            .with_dtmf_mapping(dir.dtmf_mapping.clone())
            .with_gate(dir.gate.clone())
            .build(),
        );
        Self::prune_sub_tasks(sub_tasks).await;
        sub_tasks.lock().push(h);
    }

    fn spawn_bidirectional_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let bridge_id = self.id.clone();

        let common = ForwardTrackArgs {
            cancel_token: self.cancel_token.clone(),
            recorder: self.recorder.clone(),
            recording_paused: self.recording_paused.clone(),
            sipflow_tx: self.sipflow_tx.clone(),
            receive_clock: self.receive_clock.clone(),
            dtmf_sink: Arc::clone(&self.dtmf_sink),
        };

        let c2c = DirectionParams {
            target_pc: self.callee.pc.clone(),
            sender_weak: Arc::downgrade(&self.callee.send),
            output_mode: Arc::clone(&self.callee.output_mode),
            direction: "Caller->Callee",
            path: ForwardPath::new(LegTransport::Caller, LegTransport::Callee),
            leg_stats: Arc::clone(&self.stats.caller_to_callee),
            recorder_leg: Some(RecLeg::A),
            video_payload_type: Arc::clone(&self.callee.video_payload_type),
            video_payload_map: Arc::clone(&self.callee.video_payload_map),
            video_track_label: "caller-to-callee-video",
            pli_label: "Callee PLI -> Caller source",
            transcoder: Some(Arc::clone(&self.caller_to_callee_codec.transcoder)),
            transcoder_timing: Some(Arc::clone(&self.caller_to_callee_codec.timing)),
            dtmf_mapping: Some(Arc::clone(&self.caller_to_callee_codec.dtmf_mapping)),
            gate: Some(Arc::clone(&self.caller_gate)),
        };

        let c2r = DirectionParams {
            target_pc: self.caller.pc.clone(),
            sender_weak: Arc::downgrade(&self.caller.send),
            output_mode: Arc::clone(&self.caller.output_mode),
            direction: "Callee->Caller",
            path: ForwardPath::new(LegTransport::Callee, LegTransport::Caller),
            leg_stats: Arc::clone(&self.stats.callee_to_caller),
            recorder_leg: Some(RecLeg::B),
            video_payload_type: Arc::clone(&self.caller.video_payload_type),
            video_payload_map: Arc::clone(&self.caller.video_payload_map),
            video_track_label: "callee-to-caller-video",
            pli_label: "Caller PLI -> Callee source",
            transcoder: Some(Arc::clone(&self.callee_to_caller_codec.transcoder)),
            transcoder_timing: Some(Arc::clone(&self.callee_to_caller_codec.timing)),
            dtmf_mapping: Some(Arc::clone(&self.callee_to_caller_codec.dtmf_mapping)),
            gate: None,
        };

        let caller_pc = self.caller.pc.clone();
        let callee_pc = self.callee.pc.clone();
        let sub_tasks = self.sub_tasks.clone();
        let cancel = self.cancel_token.clone();

        crate::utils::media_spawn(async move {
            let mut caller_recv = Box::pin(caller_pc.recv());
            let mut callee_recv = Box::pin(callee_pc.recv());

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        debug!(bridge_id = %bridge_id, "Bidirectional forwarder cancelled");
                        break;
                    }
                    event = &mut caller_recv => {
                        match Self::handle_direction_event(
                            &bridge_id, event, &caller_pc, &c2c, &common, &sub_tasks,
                        ).await {
                            DirectionEventResult::RePin => caller_recv = Box::pin(caller_pc.recv()),
                            DirectionEventResult::Break => break,
                        }
                    }
                    event = &mut callee_recv => {
                        match Self::handle_direction_event(
                            &bridge_id, event, &callee_pc, &c2r, &common, &sub_tasks,
                        ).await {
                            DirectionEventResult::RePin => callee_recv = Box::pin(callee_pc.recv()),
                            DirectionEventResult::Break => break,
                        }
                    }
                }
            }
        })
    }

    async fn run_forward_loop(ctx: ForwardLoopContext) {
        // Get the sender channel from weak pointer
        let sender = if let Some(strong) = ctx.sender_weak.upgrade() {
            let guard = strong.lock();
            guard.clone()
        } else {
            warn!(bridge_id = %ctx.bridge_id, direction = %ctx.path, "Sender channel no longer available");
            return;
        };

        if sender.is_none() {
            warn!(bridge_id = %ctx.bridge_id, direction = %ctx.path, "No sender channel available — video/audio sender was not configured");
            return;
        }
        let sender = sender.unwrap();

        let is_video = ctx.track.kind() == MediaKind::Video;
        let mut dtmf_detector = DtmfDetector::default();
        let mut packet_count: u64 = 0;
        let mut last_seq: Option<u16> = None;
        let mut stats_packets: u64 = 0;
        let mut stats_bytes: u64 = 0;
        let mut stats_last_pt: Option<u8> = None;
        let mut stats_last_port: Option<u16> = None;
        let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(1));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        'outer: loop {
            tokio::select! {
                _ = ctx.cancel_token.cancelled() => {
                    break;
                }
                _ = stats_interval.tick(), if is_video && packet_count > 0 => {
                    trace!(
                        bridge_id = %ctx.bridge_id,
                        direction = %ctx.path,
                        packets_per_sec = stats_packets,
                        bytes_per_sec = stats_bytes,
                        payload_type = ?stats_last_pt,
                        remote_port = ?stats_last_port,
                        total_packets = packet_count,
                        "Video stats"
                    );
                    stats_packets = 0;
                    stats_bytes = 0;
                }
                sample_result = ctx.track.recv() => {
                    match sample_result {
                        Ok(sample) => {
                            packet_count += 1;
                            if !is_video {
                                let (dtmf_pt, dtmf_clock_rate) = {
                                    let sink_guard = ctx.dtmf_sink.read();
                                    let dtmf_pt = sink_guard.as_ref()
                                        .filter(|s| s.endpoint == ctx.path.source_endpoint())
                                        .and_then(|s| s.payload_types.first().copied());
                                    let dtmf_clock_rate = dtmf_pt.and_then(|pt| {
                                        let mapping_guard = ctx.dtmf_mapping.as_ref()?.read();
                                        mapping_guard.as_ref()
                                            .filter(|m| m.source_pt == pt)
                                            .map(|m| m.source_clock_rate)
                                    }).unwrap_or(8000);
                                    (dtmf_pt, dtmf_clock_rate)
                                };
                                let record_peer_mic = ctx.output_mode.load(Ordering::Acquire) == BRIDGE_OUTPUT_PEER;
                                if record_peer_mic {
                                    if let (Some(rec), Some(leg)) = (&ctx.recorder, ctx.recorder_leg)
                                        && !ctx.recording_paused.load(std::sync::atomic::Ordering::Relaxed)
                                        && let Some(mut guard) = rec.try_write()
                                        && let Some(r) = guard.as_mut()
                                    {
                                        let _ = r.write_sample(
                                            leg,
                                            &sample,
                                            dtmf_pt,
                                            Some(dtmf_clock_rate),
                                            None::<AudioCodecType>,
                                        );
                                    }

                                    if let (Some(tx), Some(leg)) = (&ctx.sipflow_tx, ctx.recorder_leg) {
                                        let received_at_micros = ctx.receive_clock.now_micros();
                                        let _ = tx.try_send((
                                            leg,
                                            Arc::new(sample.clone()),
                                            received_at_micros,
                                        ));
                                    }
                                }
                            }
                            if !is_video {
                                Self::observe_dtmf_sample(
                                    &ctx.dtmf_sink,
                                    ctx.path.source_endpoint(),
                                    &sample,
                                    &mut dtmf_detector,
                                );
                            }
                            let mode = ctx.output_mode.load(Ordering::Acquire);
                            if !is_video && mode != BRIDGE_OUTPUT_PEER {
                                if packet_count == 1 {
                                    debug!(
                                        bridge_id = %ctx.bridge_id,
                                        direction = %ctx.path,
                                        output_source = output_mode_name(mode),
                                        "Peer media suppressed by bridge output source"
                                    );
                                }
                                continue;
                            }
                            if let Some(ref g) = ctx.gate {
                                if !g.load(Ordering::Acquire) {
                                    continue;
                                }
                            }
                            if packet_count == 1 {
                                debug!(bridge_id = %ctx.bridge_id, direction = %ctx.path, kind = ?sample.kind(), "First media sample forwarded");
                            }
                            let (sample_bytes, sample_seq) = match &sample {
                                MediaSample::Audio(a) => (a.data.len() as u64, a.sequence_number),
                                MediaSample::Video(v) => (v.data.len() as u64, v.sequence_number),
                            };
                            ctx.leg_stats.packets.fetch_add(1, Ordering::Relaxed);
                            ctx.leg_stats.bytes.fetch_add(sample_bytes, Ordering::Relaxed);
                            if let Some(seq) = sample_seq {
                                if let Some(prev) = last_seq {
                                    let gap = seq.wrapping_sub(prev.wrapping_add(1));
                                    if gap > 0 && gap < 512 {
                                        ctx.leg_stats.lost.fetch_add(gap as u64, Ordering::Relaxed);
                                    }
                                }
                                last_seq = Some(seq);
                            }
                            match sample {
                                MediaSample::Audio(mut a) => {
                                    if ctx.path.should_strip_caller_audio_metadata() {
                                        a.header_extension = None;
                                        a.raw_packet = None;
                                        a.marker = false;
                                    }
                                    let mapped_dtmf = Self::rewrite_dtmf_sample(
                                        &mut a,
                                        ctx.dtmf_mapping.as_deref(),
                                        ctx.transcoder_timing.as_deref(),
                                    );
                                    let transcoded = ctx.transcoder.as_ref().and_then(|tx_arc| {
                                        let mut guard = tx_arc.lock();
                                        let tx = guard.as_mut()?;
                                        let is_dtmf = ctx.dtmf_sink
                                            .read()
                                            .as_ref()
                                            .filter(|s| s.endpoint == ctx.path.source_endpoint())
                                            .map_or(false, |s| {
                                                a.payload_type
                                                    .is_some_and(|pt| s.payload_types.contains(&pt))
                                            });
                                        if mapped_dtmf || is_dtmf {
                                            return None;
                                        }
                                        let frame = AudioFrame {
                                            rtp_timestamp: a.rtp_timestamp,
                                            clock_rate: a.clock_rate,
                                            data: a.data.clone(),
                                            sequence_number: a.sequence_number,
                                            payload_type: a.payload_type,
                                            marker: a.marker,
                                            source_addr: a.source_addr,
                                            ..Default::default()
                                        };
                                        let mut output = tx.transcode(&frame);
                                        if let Some(ref timing_arc) = ctx.transcoder_timing {
                                            if let Some(ref mut timing) = *timing_arc.lock() {
                                                timing.rewrite(
                                                    &mut output,
                                                    tx.source_clock_rate(),
                                                    tx.target_clock_rate(),
                                                    tx.target_pt(),
                                                );
                                            }
                                        }
                                        Some(MediaSample::Audio(output))
                                    });
                                    let final_sample = transcoded
                                        .unwrap_or(MediaSample::Audio(a));
                                    if let Err(e) = sender.send(final_sample).await {
                                        ctx.leg_stats.dropped.fetch_add(1, Ordering::Relaxed);
                                        match e {
                                            MediaError::Closed | MediaError::KindMismatch { .. } => {
                                                warn!(bridge_id = %ctx.bridge_id, direction = %ctx.path, error = ?e, "Failed to forward media sample");
                                            }
                                            _ => {}
                                        }
                                        break 'outer;
                                    }
                                }
                                MediaSample::Video(mut v) => {
                                    let video_samples: Vec<MediaSample> = if is_video {
                                        if matches!(v.payload_type, Some(pt) if pt < 96) {
                                            debug!(
                                                bridge_id = %ctx.bridge_id,
                                                direction = %ctx.path,
                                                pt = ?v.payload_type,
                                                "Dropping non-video payload type on video track"
                                            );
                                            vec![]
                                        } else {
                                            stats_packets += 1;
                                            stats_bytes += v.data.len() as u64;
                                            stats_last_pt = v.payload_type;
                                            if let Some(addr) = v.source_addr {
                                                stats_last_port = Some(addr.port());
                                            }
                                            v.header_extension = None;
                                            v.raw_packet = None;
                                            v.csrcs.clear();
                                            if matches!(
                                                (ctx.path.from, ctx.path.to),
                                                (LegTransport::Caller, LegTransport::Callee)
                                            ) {
                                                v.sequence_number = None;
                                            }

                                            vec![MediaSample::Video(v)]
                                        }
                                    } else {
                                        v.sequence_number = None;
                                        v.payload_type = None;
                                        vec![MediaSample::Video(v)]
                                    };
                                    for sample in video_samples {
                                        if let Err(e) = sender.send(sample).await {
                                            ctx.leg_stats.dropped.fetch_add(1, Ordering::Relaxed);
                                            match e {
                                                MediaError::Closed | MediaError::KindMismatch { .. } => {
                                                    warn!(bridge_id = %ctx.bridge_id, direction = %ctx.path, error = ?e, "Failed to forward media sample");
                                                }
                                                _ => {}
                                            }
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            trace!(bridge_id = %ctx.bridge_id, direction = %ctx.path, error = %e, "Track recv ended");
                            break;
                        }
                    }
                }
            }
        }
    }

    fn rewrite_dtmf_sample(
        frame: &mut AudioFrame,
        mapping_slot: Option<&parking_lot::RwLock<Option<PayloadMapping>>>,
        timing_slot: Option<&parking_lot::Mutex<Option<RtpTiming>>>,
    ) -> bool {
        let Some(mapping_slot) = mapping_slot else {
            return false;
        };
        let Some(mapping) = mapping_slot.read().clone() else {
            return false;
        };

        if frame.payload_type != Some(mapping.source_pt) {
            return false;
        }

        if mapping.source_clock_rate != mapping.target_clock_rate {
            frame.data = rewrite_dtmf_duration(
                &frame.data,
                mapping.source_clock_rate,
                mapping.target_clock_rate,
            );
        }

        let used_shared_timing = if let Some(timing_slot) = timing_slot {
            let mut guard = timing_slot.lock();
            if let Some(timing) = guard.as_mut() {
                timing.rewrite(
                    frame,
                    mapping.source_clock_rate,
                    mapping.target_clock_rate,
                    mapping.target_pt,
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        if !used_shared_timing {
            frame.payload_type = Some(mapping.target_pt);
            frame.clock_rate = mapping.target_clock_rate;
            if mapping.source_clock_rate != mapping.target_clock_rate {
                frame.rtp_timestamp = scale_rtp_timestamp(
                    frame.rtp_timestamp,
                    mapping.source_clock_rate,
                    mapping.target_clock_rate,
                );
            }
        }

        true
    }

    fn observe_dtmf_sample(
        dtmf_sink: &Arc<parking_lot::RwLock<Option<DtmfSink>>>,
        endpoint: BridgeEndpoint,
        sample: &MediaSample,
        detector: &mut DtmfDetector,
    ) {
        let guard = dtmf_sink.read();
        let Some(sink) = guard.as_ref() else {
            return;
        };
        if sink.endpoint != endpoint {
            return;
        }

        let MediaSample::Audio(frame) = sample else {
            return;
        };

        let Some(frame_pt) = frame.payload_type else {
            return;
        };

        if !sink.payload_types.contains(&frame_pt) {
            return;
        }

        trace!(
            rtp_ts = frame.rtp_timestamp,
            data_len = frame.data.len(),
            first_byte = frame.data.first().copied().unwrap_or(0),
            "DTMF observe: PT matched, calling detector"
        );

        if let Some(digit) = detector.observe(&frame.data, frame.rtp_timestamp) {
            info!(digit = %digit, endpoint = ?sink.endpoint, "DTMF digit detected");
            (sink.handler)(digit);
        }
    }

    /// Spawn a task that subscribes to PLI/FIR RTCP on `sender` and forwards them as
    /// `request_key_frame()` calls on `source_track`.
    /// Remove finished sub-tasks and cap the total so that repeated
    /// renegotiations (each spawning fresh forwarder / PLI tasks) do not grow
    /// `sub_tasks` without bound. Stale tasks for superseded tracks are aborted.
    async fn prune_sub_tasks(sub_tasks: &Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>>) {
        let mut st = sub_tasks.lock();
        // Drop handles for tasks that have already finished.
        st.retain(|h| !h.is_finished());
        // If there are still many live tasks (typical of repeated re-INVITEs
        // where old forwarders keep draining superseded tracks), abort the
        // oldest ones. Each forwarder/PLI task holds strong Arc clones of the
        // source track and target sender, so retaining them wastes memory.
        const MAX_LIVE_SUB_TASKS: usize = 16;
        while st.len() > MAX_LIVE_SUB_TASKS {
            if let Some(h) = st.first() {
                h.abort();
            }
            st.remove(0);
        }
    }

    fn spawn_pli_forwarder(
        bridge_id: String,
        sender: Arc<RtpSender>,
        source_track: Arc<dyn MediaStreamTrack>,
        cancel_token: CancellationToken,
        label: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let mut rtcp_rx = sender.subscribe_rtcp();
        crate::utils::media_spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    result = rtcp_rx.recv() => {
                        match result {
                            Ok(RtcpPacket::PictureLossIndication(_)) | Ok(RtcpPacket::FullIntraRequest(_)) => {
                                if let Err(e) = source_track.request_key_frame().await {
                                    warn!(bridge_id = %bridge_id, label = %label, error = %e, "PLI forward: request_key_frame failed");
                                } else {
                                    debug!(bridge_id = %bridge_id, label = %label, "Forwarded PLI to source keyframe request");
                                }
                            }
                            Err(_) => break,
                            _ => {}
                        }
                    }
                }
            }
        })
    }

    fn forward_track_to_sender(ctx: ForwardLoopContext) -> tokio::task::JoinHandle<()> {
        crate::utils::media_spawn(async move {
            Self::run_forward_loop(ctx).await;
        })
    }

    /// Get the WebRTC PeerConnection
    pub fn caller_pc(&self) -> &PeerConnection {
        &self.caller.pc
    }

    /// Get the RTP PeerConnection
    pub fn callee_pc(&self) -> &PeerConnection {
        &self.callee.pc
    }

    /// Stop the bridge (async — waits for forwarding tasks to finish)
    pub async fn stop(&self) {
        debug!(bridge_id = %self.id, "Stopping bridge");
        self.cancel_token.cancel();
        self.caller.pc.close();
        self.callee.pc.close();

        // Wait for main tasks to complete (drain while holding lock, then drop guard before .await)
        let tasks = self.bridge_tasks.lock().drain(..).collect::<Vec<_>>();
        for task in tasks {
            let _ = task.await;
        }

        // Wait for sub-tasks (forward_track_to_sender, pli_forwarder, etc.)
        let sub = self.sub_tasks.lock().drain(..).collect::<Vec<_>>();
        for task in sub {
            let _ = task.await;
        }
    }

    /// Close both PeerConnections and abort all tasks (sync, no awaiting).
    pub fn close_sync(&self) {
        self.cancel_token.cancel();
        // Abort bridge tasks so they don't keep Arc references alive.
        for task in self.bridge_tasks.lock().iter() {
            task.abort();
        }
        let sub = self.sub_tasks.lock();
        for task in sub.iter() {
            task.abort();
        }
        self.caller.pc.close();
        self.callee.pc.close();
    }
}

impl Drop for BridgePeer {
    fn drop(&mut self) {
        trace!(bridge_id = %self.id, "BridgePeer dropping, cleaning up resources");
        self.cancel_token.cancel();
        // Drain and abort bridge tasks to release Arc references immediately.
        for task in self.bridge_tasks.get_mut().drain(..) {
            task.abort();
        }
        // Drain and abort sub-tasks.
        for task in self.sub_tasks.lock().drain(..) {
            task.abort();
        }
        self.caller.pc.close();
        self.callee.pc.close();
    }
}

/// Builder for creating BridgePeer instances
pub struct BridgePeerBuilder {
    bridge_id: String,
    session_id: Option<String>,
    caller_config: Option<rustrtc::RtcConfiguration>,
    callee_config: Option<rustrtc::RtcConfiguration>,
    rtp_port_range: (u16, u16),
    enable_latching: bool,
    probation_max_packets: Option<u8>,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    caller_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    callee_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    caller_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    callee_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    caller_sender_codec: Option<RtpCodecParameters>,
    callee_sender_codec: Option<RtpCodecParameters>,
    rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode,
    ice_servers: Vec<IceServer>,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    recording_paused: Arc<AtomicBool>,
    sipflow_tx: Option<mpsc::Sender<(RecLeg, SharedMediaSample, u64)>>,
    cname: Option<String>,
    rtp_timeout: Option<std::time::Duration>,
    rtp_timeout_tx: Option<mpsc::Sender<String>>,
}

impl BridgePeerBuilder {
    pub fn new(bridge_id: String) -> Self {
        Self {
            bridge_id,
            session_id: None,
            caller_config: None,
            callee_config: None,
            rtp_port_range: (20000, 30000),
            enable_latching: false,
            probation_max_packets: None,
            external_ip: None,
            bind_ip: None,
            caller_audio_capabilities: None,
            callee_audio_capabilities: None,
            caller_video_capabilities: None,
            callee_video_capabilities: None,
            caller_sender_codec: None,
            callee_sender_codec: None,
            rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode::LegacySip,
            ice_servers: Vec::new(),
            recorder: None,
            recording_paused: Arc::new(AtomicBool::new(false)),
            sipflow_tx: None,
            cname: None,
            rtp_timeout: None,
            rtp_timeout_tx: None,
        }
    }

    pub fn with_caller_config(mut self, config: rustrtc::RtcConfiguration) -> Self {
        self.caller_config = Some(config);
        self
    }

    pub fn with_callee_config(mut self, config: rustrtc::RtcConfiguration) -> Self {
        self.callee_config = Some(config);
        self
    }

    pub fn with_rtp_port_range(mut self, start: u16, end: u16) -> Self {
        self.rtp_port_range = (start, end);
        self
    }

    pub fn with_enable_latching(mut self, enable: bool) -> Self {
        self.enable_latching = enable;
        self
    }

    pub fn with_probation_max_packets(mut self, max: Option<u8>) -> Self {
        self.probation_max_packets = max;
        self
    }

    pub fn with_external_ip(mut self, ip: String) -> Self {
        self.external_ip = Some(ip);
        self
    }

    pub fn with_bind_ip(mut self, ip: String) -> Self {
        self.bind_ip = Some(ip);
        self
    }

    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Set audio capabilities for the WebRTC side PeerConnection.
    /// Controls which codecs appear in SDP offers/answers.
    pub fn with_caller_audio_capabilities(
        mut self,
        caps: Vec<rustrtc::config::AudioCapability>,
    ) -> Self {
        self.caller_audio_capabilities = Some(caps);
        self
    }

    /// Set audio capabilities for the callee-side PeerConnection.
    /// Controls which codecs appear in SDP offers/answers.
    pub fn with_callee_audio_capabilities(
        mut self,
        caps: Vec<rustrtc::config::AudioCapability>,
    ) -> Self {
        self.callee_audio_capabilities = Some(caps);
        self
    }

    /// Set video capabilities for the WebRTC side PeerConnection.
    /// Controls which video codecs appear in SDP offers/answers.
    pub fn with_caller_video_capabilities(
        mut self,
        caps: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        self.caller_video_capabilities = Some(caps);
        self
    }

    /// Set video capabilities for the callee-side PeerConnection.
    /// Controls which video codecs appear in SDP offers/answers.
    pub fn with_callee_video_capabilities(
        mut self,
        caps: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        self.callee_video_capabilities = Some(caps);
        self
    }

    /// Set sender codec parameters for both bridge sides.
    /// These are used by the sample track (RtpSender) on each side.
    pub fn with_sender_codecs(
        mut self,
        caller: RtpCodecParameters,
        callee: RtpCodecParameters,
    ) -> Self {
        self.caller_sender_codec = Some(caller);
        self.callee_sender_codec = Some(callee);
        self
    }

    /// Set SDP compatibility mode for the RTP-side PeerConnection.
    /// Use `SdpCompatibilityMode::Standard` when the remote caller offered BUNDLE,
    /// so the answer includes `a=group:BUNDLE` and `a=mid:` allowing proper
    /// multiplexing of audio and video on the same RTP port.
    pub fn with_rtp_sdp_compatibility(
        mut self,
        mode: rustrtc::config::SdpCompatibilityMode,
    ) -> Self {
        self.rtp_sdp_compatibility = mode;
        self
    }

    /// Attach a shared recorder so that both bridge directions write audio to it.
    /// The recorder is lazily activated: once `start_recording()` puts a `Recorder`
    /// inside the `Arc<RwLock<…>>`, the bridge forward loops will start writing.
    pub fn with_recorder(
        mut self,
        recorder: Arc<parking_lot::RwLock<Option<Recorder>>>,
        recording_paused: Arc<AtomicBool>,
    ) -> Self {
        self.recorder = Some(recorder);
        self.recording_paused = recording_paused;
        self
    }

    pub fn with_sipflow_capture(
        mut self,
        sipflow_tx: mpsc::Sender<(RecLeg, SharedMediaSample, u64)>,
    ) -> Self {
        self.sipflow_tx = Some(sipflow_tx);
        self
    }

    pub fn with_cname(mut self, cname: String) -> Self {
        self.cname = Some(cname);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_rtp_timeout_notify(
        mut self,
        tx: mpsc::Sender<String>,
        timeout: std::time::Duration,
    ) -> Self {
        self.rtp_timeout_tx = Some(tx);
        self.rtp_timeout = Some(timeout);
        self
    }

    fn default_video_capabilities() -> Vec<rustrtc::config::VideoCapability> {
        vec![
            rustrtc::config::VideoCapability {
                payload_type: 96,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some("packetization-mode=1".to_string()),
                rtcp_fbs: vec![],
            },
            rustrtc::config::VideoCapability {
                payload_type: 97,
                codec_name: "VP8".to_string(),
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec![],
            },
        ]
    }

    fn resolve_video_caps(
        configured: &Option<Vec<rustrtc::config::VideoCapability>>,
    ) -> Vec<rustrtc::config::VideoCapability> {
        configured
            .as_ref()
            .filter(|c| !c.is_empty())
            .cloned()
            .unwrap_or_else(Self::default_video_capabilities)
    }

    pub fn build(self) -> Arc<BridgePeer> {
        // Always include default video capabilities in bridge PCs so that
        // re-INVITE video renegotiation works. The video sender tracks are
        // created lazily by setup_bridge_with_codecs (when caller_video_codec
        // is Some) or dynamically by add_video_track().
        let caller_video = Self::resolve_video_caps(&self.caller_video_capabilities);
        let callee_video = Self::resolve_video_caps(&self.callee_video_capabilities);

        let caller_media_caps =
            self.caller_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: caller_video,
                    application: None,
                    image: vec![],
                });

        let caller_config = self
            .caller_config
            .unwrap_or_else(|| rustrtc::RtcConfiguration {
                transport_mode: TransportMode::WebRtc,
                external_ip: self.external_ip.clone(),
                media_capabilities: caller_media_caps,
                ssrc_start: rand::random::<u32>(),
                sdp_compatibility: rustrtc::config::SdpCompatibilityMode::Standard,
                ice_servers: self.ice_servers,
                cname: self.cname.clone(),
                ..Default::default()
            });

        let callee_media_caps =
            self.callee_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: callee_video,
                    application: None,
                    image: vec![],
                });

        let callee_config = self
            .callee_config
            .unwrap_or_else(|| rustrtc::RtcConfiguration {
                transport_mode: TransportMode::Rtp,
                rtp_start_port: Some(self.rtp_port_range.0),
                rtp_end_port: Some(self.rtp_port_range.1),
                enable_latching: self.enable_latching,
                probation_max_packets: self.probation_max_packets,
                external_ip: self.external_ip,
                bind_ip: self.bind_ip,
                media_capabilities: callee_media_caps,
                ssrc_start: rand::random::<u32>(),
                sdp_compatibility: self.rtp_sdp_compatibility,
                cname: self.cname.clone(),
                ..Default::default()
            });

        let mut caller_config = caller_config;
        caller_config.label = Some(format!("{}-caller", self.bridge_id));
        let mut callee_config = callee_config;
        callee_config.label = Some(format!("{}-callee", self.bridge_id));

        let (caller_pc, callee_pc) = {
            let _guard = crate::utils::media_enter();
            (PeerConnection::new(caller_config), PeerConnection::new(callee_config))
        };

        let mut bridge = BridgePeer::new(self.bridge_id, caller_pc, callee_pc);
        bridge.session_id = self.session_id;
        bridge.caller.sender_codec = self.caller_sender_codec;
        bridge.callee.sender_codec = self.callee_sender_codec;
        bridge.recorder = self.recorder;
        bridge.recording_paused = self.recording_paused;
        bridge.sipflow_tx = self.sipflow_tx;
        bridge.rtp_timeout = RtpTimeoutConfig {
            duration: self.rtp_timeout,
            notify_tx: self.rtp_timeout_tx,
        };

        // Store video codec params for setup_bridge to create video senders
        let vid_codec = |caps: &Option<Vec<rustrtc::VideoCapability>>| {
            caps.as_ref().and_then(|c| c.first()).map(|cap| RtpCodecParameters {
                payload_type: cap.payload_type, clock_rate: cap.clock_rate, channels: 0,
            })
        };
        bridge.caller.video_codec = vid_codec(&self.caller_video_capabilities);
        bridge.callee.video_codec = vid_codec(&self.callee_video_capabilities);

        Arc::new(bridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{CodecType, FileTrack, RtpTrackBuilder};
    use rustrtc::{
        TransportMode,
        sdp::{SdpType, SessionDescription},
    };

    fn create_test_wav_file(path: &str, num_samples: usize) -> Result<()> {
        let spec = crate::media::wav_reader::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: crate::media::wav_reader::SampleFormat::Int,
        };
        let mut writer = crate::media::wav_reader::WavWriter::create(path, spec)
            .map_err(|e| anyhow::anyhow!("WavWriter: {e}"))?;
        for i in 0..num_samples {
            let sample = ((i as f32 / 8.0).sin() * 1000.0) as i16;
            writer
                .write_sample(sample)
                .map_err(|e| anyhow::anyhow!("write_sample: {e}"))?;
        }
        writer
            .finalize()
            .map_err(|e| anyhow::anyhow!("finalize: {e}"))?;
        Ok(())
    }

    #[test]
    fn test_video_payload_map_matches_codec_identity_not_payload_number() {
        let rtp_caps = vec![
            rustrtc::config::VideoCapability {
                payload_type: 96,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some("profile-level-id=42801F".to_string()),
                rtcp_fbs: vec![],
            },
            rustrtc::config::VideoCapability {
                payload_type: 97,
                codec_name: "VP8".to_string(),
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec![],
            },
        ];
        let webrtc_caps = vec![
            rustrtc::config::VideoCapability {
                payload_type: 96,
                codec_name: "VP8".to_string(),
                clock_rate: 90000,
                fmtp: None,
                rtcp_fbs: vec![],
            },
            rustrtc::config::VideoCapability {
                payload_type: 103,
                codec_name: "H264".to_string(),
                clock_rate: 90000,
                fmtp: Some(
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"
                        .to_string(),
                ),
                rtcp_fbs: vec![],
            },
        ];

        let rtp_to_webrtc = BridgePeer::video_payload_map_by_codec(&rtp_caps, &webrtc_caps);
        let webrtc_to_rtp = BridgePeer::video_payload_map_by_codec(&webrtc_caps, &rtp_caps);

        assert_eq!(rtp_to_webrtc.get(&96), Some(&103));
        assert_eq!(rtp_to_webrtc.get(&97), Some(&96));
        assert_eq!(webrtc_to_rtp.get(&103), Some(&96));
        assert_eq!(webrtc_to_rtp.get(&96), Some(&97));
    }

    #[tokio::test]
    async fn test_bridge_peer_creation() {
        let bridge = BridgePeerBuilder::new("test-bridge".to_string())
            .with_rtp_port_range(25000, 25100)
            .build();

        // setup_bridge() creates the transceivers via add_track()
        bridge.setup_bridge().await.unwrap();

        // Generate offers for both PCs
        let webrtc_offer = bridge.caller_pc().create_offer().await;
        let rtp_offer = bridge.callee_pc().create_offer().await;

        assert!(webrtc_offer.is_ok(), "WebRTC should create offer");
        assert!(rtp_offer.is_ok(), "RTP should create offer");

        // Set local descriptions
        let _ = bridge
            .caller_pc()
            .set_local_description(webrtc_offer.unwrap());
        let _ = bridge.callee_pc().set_local_description(rtp_offer.unwrap());

        // Verify both PCs have local descriptions
        let webrtc_local = bridge.caller_pc().local_description();
        let rtp_local = bridge.callee_pc().local_description();

        assert!(
            webrtc_local.is_some(),
            "WebRTC PC should have local description"
        );
        assert!(rtp_local.is_some(), "RTP PC should have local description");

        // WebRTC should have DTLS/ICE attributes
        let webrtc_sdp = webrtc_local.unwrap().to_sdp_string();
        assert!(
            webrtc_sdp.contains("UDP/TLS/RTP/SAVPF"),
            "WebRTC should use SAVPF"
        );
        assert!(
            webrtc_sdp.contains("fingerprint"),
            "WebRTC should have DTLS fingerprint"
        );

        // RTP should be plain
        let rtp_sdp = rtp_local.unwrap().to_sdp_string();
        assert!(rtp_sdp.contains("RTP/AVP"), "RTP should use AVP");
        assert!(
            !rtp_sdp.contains("fingerprint"),
            "RTP should not have DTLS fingerprint"
        );
    }

    use crate::media::Track;

    /// Test bridge setup with external RTP track
    /// This simulates the scenario where:
    /// - Bridge RTP side connects to an RTP endpoint (SIP leg)
    #[tokio::test]
    async fn test_bridge_setup_with_external_tracks() {
        // Create bridge
        let bridge = BridgePeerBuilder::new("test-bridge-integration".to_string())
            .with_rtp_port_range(26000, 26100)
            .build();

        // setup_bridge() creates transceivers via add_track()
        bridge.setup_bridge().await.unwrap();

        // Create offer on RTP side
        let bridge_rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge
            .callee_pc()
            .set_local_description(bridge_rtp_offer)
            .unwrap();

        // Create external RTP endpoint (simulating SIP callee)
        let rtp_callee = RtpTrackBuilder::new("rtp-callee".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(26100, 26200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Get bridge's RTP offer SDP and have callee handshake with it
        let bridge_rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        let callee_answer = rtp_callee.handshake(bridge_rtp_sdp).await.unwrap();

        // Bridge RTP side sets remote description from callee
        let rtp_leg_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(rtp_leg_desc)
            .await
            .unwrap();

        // Setup WebRTC side for completeness
        let bridge_webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        bridge
            .caller_pc()
            .set_local_description(bridge_webrtc_offer)
            .unwrap();

        // Start bridge forwarding
        bridge.start_bridge().await;

        // Verify bridge is properly configured
        let caller_pc = bridge.caller_pc();
        let callee_pc = bridge.callee_pc();

        // Both should have local descriptions
        assert!(
            caller_pc.local_description().is_some(),
            "Caller should have local description"
        );
        assert!(
            callee_pc.local_description().is_some(),
            "Callee should have local description"
        );
        assert!(
            callee_pc.remote_description().is_some(),
            "Callee should have remote description"
        );

        // Clean up
        bridge.stop().await;
    }

    #[tokio::test]
    async fn test_bridge_file_output_polls_file_track_source() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_bridge_file_output.wav");
        create_test_wav_file(test_file.to_str().unwrap(), 800).unwrap();

        let bridge = BridgePeerBuilder::new("test-bridge-file-output".to_string())
            .with_rtp_port_range(25100, 25200)
            .build();
        bridge.setup_bridge().await.unwrap();

        let track = FileTrack::new("bridge-file-output".to_string())
            .with_path(test_file.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(crate::media::negotiate::CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            });

        bridge
            .replace_output_with_file(BridgeEndpoint::Callee, &track)
            .await
            .unwrap();
        drop(track);

        let rtp_track = bridge
            .get_callee_track()
            .await
            .expect("bridge RTP output track should exist");
        let mut frame_count = 0;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(400);
        while tokio::time::Instant::now() < deadline && frame_count < 3 {
            match tokio::time::timeout(tokio::time::Duration::from_millis(100), rtp_track.recv())
                .await
            {
                Ok(Ok(MediaSample::Audio(_))) => frame_count += 1,
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }

        bridge.stop().await;
        let _ = std::fs::remove_file(&test_file);

        assert!(
            frame_count >= 3,
            "bridge should poll the FileTrack source and send audio without a FileTrack-owned task"
        );
    }

    /// After a file finishes playing (natural EOF), the endpoint must return to
    /// `BRIDGE_OUTPUT_PEER` so the two parties can hear each other again.
    /// Regression: previously the clock set `BRIDGE_OUTPUT_MUTED` on EOF, leaving
    /// the leg silent until an explicit `media.stop` arrived.
    #[tokio::test]
    async fn test_bridge_file_output_restores_peer_on_eof() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_bridge_eof_restore.wav");
        // 160 samples @ 8 kHz = 20 ms = one frame; EOF on the second tick.
        create_test_wav_file(test_file.to_str().unwrap(), 160).unwrap();

        let bridge = BridgePeerBuilder::new("test-bridge-eof-restore".to_string())
            .with_rtp_port_range(25300, 25400)
            .build();
        bridge.setup_bridge().await.unwrap();

        let track = FileTrack::new("bridge-eof-restore".to_string())
            .with_path(test_file.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(crate::media::negotiate::CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            });

        bridge
            .replace_output_with_file(BridgeEndpoint::Callee, &track)
            .await
            .unwrap();
        drop(track);

        use std::sync::atomic::Ordering;
        let callee_mode = bridge.output_mode_atomic(BridgeEndpoint::Callee);
        assert_eq!(
            callee_mode.load(Ordering::Acquire),
            BRIDGE_OUTPUT_FILE,
            "callee must be in FILE mode right after replace_output_with_file"
        );

        // Wait for the 20 ms file-output clock to drain the short file and hit EOF.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(800);
        loop {
            if callee_mode.load(Ordering::Acquire) == BRIDGE_OUTPUT_PEER {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }

        bridge.stop().await;
        let _ = std::fs::remove_file(&test_file);

        assert_eq!(
            callee_mode.load(Ordering::Acquire),
            BRIDGE_OUTPUT_PEER,
            "callee output mode must restore to PEER after file EOF (regression: was left MUTED)"
        );
    }

    /// When both endpoints play a file (the `leg_id:"both"` announcement case),
    /// both must return to `BRIDGE_OUTPUT_PEER` after their files finish so the
    /// call resumes bidirectionally.
    #[tokio::test]
    async fn test_bridge_file_output_restores_peer_on_eof_both_legs() {
        let temp_dir = std::env::temp_dir();
        let caller_file = temp_dir.join("test_bridge_eof_both_caller.wav");
        let callee_file = temp_dir.join("test_bridge_eof_both_callee.wav");
        create_test_wav_file(caller_file.to_str().unwrap(), 160).unwrap();
        create_test_wav_file(callee_file.to_str().unwrap(), 160).unwrap();

        let bridge = BridgePeerBuilder::new("test-bridge-eof-both".to_string())
            .with_rtp_port_range(25500, 25600)
            .build();
        bridge.setup_bridge().await.unwrap();

        let codec_info = crate::media::negotiate::CodecInfo {
            payload_type: 0,
            codec: CodecType::PCMU,
            clock_rate: 8000,
            channels: 1,
        };
        let caller_track = FileTrack::new("bridge-eof-both-caller".to_string())
            .with_path(caller_file.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(codec_info.clone());
        let callee_track = FileTrack::new("bridge-eof-both-callee".to_string())
            .with_path(callee_file.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(codec_info.clone());

        bridge
            .replace_output_with_file(BridgeEndpoint::Caller, &caller_track)
            .await
            .unwrap();
        bridge
            .replace_output_with_file(BridgeEndpoint::Callee, &callee_track)
            .await
            .unwrap();
        drop(caller_track);
        drop(callee_track);

        use std::sync::atomic::Ordering;
        let caller_mode = bridge.output_mode_atomic(BridgeEndpoint::Caller);
        let callee_mode = bridge.output_mode_atomic(BridgeEndpoint::Callee);
        assert_eq!(caller_mode.load(Ordering::Acquire), BRIDGE_OUTPUT_FILE);
        assert_eq!(callee_mode.load(Ordering::Acquire), BRIDGE_OUTPUT_FILE);

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(800);
        loop {
            let caller_done = caller_mode.load(Ordering::Acquire) == BRIDGE_OUTPUT_PEER;
            let callee_done = callee_mode.load(Ordering::Acquire) == BRIDGE_OUTPUT_PEER;
            if (caller_done && callee_done) || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
        }

        bridge.stop().await;
        let _ = std::fs::remove_file(&caller_file);
        let _ = std::fs::remove_file(&callee_file);

        assert_eq!(
            caller_mode.load(Ordering::Acquire),
            BRIDGE_OUTPUT_PEER,
            "caller must restore to PEER after both-leg announcement EOF"
        );
        assert_eq!(
            callee_mode.load(Ordering::Acquire),
            BRIDGE_OUTPUT_PEER,
            "callee must restore to PEER after both-leg announcement EOF"
        );
    }

    /// Verify that the first audio frame after `replace_output_with_file`
    /// carries the RTP marker bit (RFC 3550 §5.1), and subsequent frames do not.
    #[tokio::test]
    async fn test_bridge_file_output_sets_marker_on_first_frame() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("test_bridge_marker.wav");
        create_test_wav_file(test_file.to_str().unwrap(), 800).unwrap();

        let bridge = BridgePeerBuilder::new("test-bridge-marker".to_string())
            .with_rtp_port_range(25300, 25400)
            .build();
        bridge.setup_bridge().await.unwrap();

        let track = FileTrack::new("bridge-marker-test".to_string())
            .with_path(test_file.to_string_lossy().to_string())
            .with_loop(true)
            .with_codec_info(crate::media::negotiate::CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            });

        bridge
            .replace_output_with_file(BridgeEndpoint::Callee, &track)
            .await
            .unwrap();
        drop(track);

        let rtp_track = bridge
            .get_callee_track()
            .await
            .expect("bridge RTP output track should exist");

        // Collect first two audio frames.
        let mut markers = Vec::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && markers.len() < 2 {
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                rtp_track.recv(),
            )
            .await
            {
                Ok(Ok(MediaSample::Audio(f))) => markers.push(f.marker),
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }

        bridge.stop().await;
        let _ = std::fs::remove_file(&test_file);

        assert!(
            markers.len() >= 2,
            "expected at least 2 frames, got {}",
            markers.len()
        );
        assert!(
            markers[0],
            "first frame after source switch must have marker=true (RFC 3550 §5.1)"
        );
        assert!(
            !markers[1],
            "second frame must have marker=false, only the first frame of a talkspurt is marked"
        );
    }

    /// Test that BridgePeer correctly handles SDP format differences
    #[tokio::test]
    async fn test_bridge_sdp_format_differences() {
        let bridge = BridgePeerBuilder::new("test-bridge-sdp".to_string())
            .with_rtp_port_range(27000, 27100)
            .build();

        // setup_bridge() creates transceivers via add_track()
        bridge.setup_bridge().await.unwrap();

        // Generate offers
        let webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();

        bridge
            .caller_pc()
            .set_local_description(webrtc_offer)
            .unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();

        let webrtc_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        let rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        // Verify format differences
        // WebRTC should have ICE/DTLS attributes
        assert!(
            webrtc_sdp.contains("ice-ufrag"),
            "WebRTC should have ICE ufrag"
        );
        assert!(webrtc_sdp.contains("ice-pwd"), "WebRTC should have ICE pwd");
        assert!(
            webrtc_sdp.contains("setup:"),
            "WebRTC should have DTLS setup"
        );

        // RTP should NOT have ICE/DTLS attributes
        assert!(
            !rtp_sdp.contains("ice-ufrag"),
            "RTP should NOT have ICE ufrag"
        );
        assert!(
            !rtp_sdp.contains("fingerprint:"),
            "RTP should NOT have DTLS fingerprint"
        );

        // Protocol should differ
        assert!(webrtc_sdp.contains("SAVPF"), "WebRTC should use SAVPF");
        assert!(rtp_sdp.contains("RTP/AVP"), "RTP should use AVP");
    }

    /// Test complete P2P WebRTC ↔ RTP bridging scenario
    /// Simulates: WebRTC caller -> Bridge -> RTP callee
    #[tokio::test]
    async fn test_p2p_webrtc_to_rtp_bridge() {
        // Step 1: Create bridge (represents PBX media bridge)
        let bridge = BridgePeerBuilder::new("p2p-bridge".to_string())
            .with_rtp_port_range(28000, 28100)
            .build();

        // Step 2: Create external RTP endpoint (simulates callee)
        let rtp_callee = RtpTrackBuilder::new("rtp-callee".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(28200, 28300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Step 3: Setup bridge
        bridge.setup_bridge().await.unwrap();

        // Step 4: Generate offers for bridge sides
        let bridge_webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        let bridge_rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge
            .caller_pc()
            .set_local_description(bridge_webrtc_offer.clone())
            .unwrap();
        bridge
            .callee_pc()
            .set_local_description(bridge_rtp_offer.clone())
            .unwrap();

        // Step 5: Bridge WebRTC side SDP (would be sent to WebRTC caller)
        let webrtc_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(webrtc_sdp.contains("SAVPF"), "WebRTC side should use SAVPF");
        assert!(
            webrtc_sdp.contains("fingerprint"),
            "WebRTC side should have DTLS fingerprint"
        );

        // Step 6: Bridge RTP side SDP (would be sent to RTP callee, possibly converted)
        let rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(rtp_sdp.contains("RTP/AVP"), "RTP side should use AVP");
        assert!(
            !rtp_sdp.contains("fingerprint"),
            "RTP side should NOT have DTLS fingerprint"
        );

        // Step 7: RTP callee receives bridge RTP offer and creates answer
        let rtp_callee_answer = rtp_callee.handshake(rtp_sdp).await.unwrap();

        // Step 8: Bridge RTP side sets remote description from callee
        let callee_desc = SessionDescription::parse(SdpType::Answer, &rtp_callee_answer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 9: Start bridge forwarding
        bridge.start_bridge().await;

        // Verify connections are established
        assert!(
            bridge.caller_pc().local_description().is_some(),
            "WebRTC should have local description"
        );
        assert!(
            bridge.callee_pc().local_description().is_some(),
            "RTP should have local description"
        );
        assert!(
            bridge.callee_pc().remote_description().is_some(),
            "RTP should have remote description from callee"
        );
        // Clean up
        bridge.stop().await;
    }

    /// Test that bridge's WebRTC PC correctly answers a WebRTC caller's offer.
    /// Verifies: setup:passive (not actpass), real fingerprint, real ICE credentials.
    #[tokio::test]
    async fn test_bridge_as_webrtc_answerer() {
        // Step 1: Create bridge (only callee-facing RTP side creates offer)
        let bridge = BridgePeerBuilder::new("answerer-test".to_string())
            .with_rtp_port_range(29000, 29100)
            .build();

        bridge.setup_bridge().await.unwrap();

        // Only create offer on RTP side (callee-facing)
        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();

        // Step 2: Create a WebRTC caller that generates an offer
        let webrtc_caller = RtpTrackBuilder::new("webrtc-caller".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(29100, 29200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        let caller_offer = webrtc_caller.local_description().await.unwrap();
        assert!(
            caller_offer.contains("SAVPF"),
            "Caller offer should use SAVPF"
        );
        assert!(
            caller_offer.contains("setup:actpass"),
            "Caller should offer actpass"
        );

        // Step 3: Bridge's WebRTC PC answers the caller's offer
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .caller_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.caller_pc().create_answer().await.unwrap();
        bridge.caller_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        // Step 4: Verify answer SDP correctness
        assert!(answer_sdp.contains("SAVPF"), "Answer should use SAVPF");
        assert!(
            answer_sdp.contains("fingerprint:sha-256"),
            "Answer must have real DTLS fingerprint"
        );
        assert!(
            answer_sdp.contains("ice-ufrag:"),
            "Answer must have real ICE ufrag"
        );
        assert!(
            answer_sdp.contains("ice-pwd:"),
            "Answer must have real ICE password"
        );
        assert!(
            answer_sdp.contains("rtcp-mux"),
            "Answer should have rtcp-mux"
        );

        // CRITICAL: answer must NOT have actpass — must be passive or active
        assert!(
            !answer_sdp.contains("setup:actpass"),
            "Answer MUST NOT use actpass"
        );
        assert!(
            answer_sdp.contains("setup:passive") || answer_sdp.contains("setup:active"),
            "Answer must choose a DTLS role (passive or active)"
        );

        // Clean up
        bridge.stop().await;
    }

    /// Test the complete WebRTC caller → Bridge → RTP callee SDP exchange flow.
    /// This mirrors the actual sip_session.rs flow after the fix.
    #[tokio::test]
    async fn test_full_webrtc_to_rtp_bridge_flow() {
        // Step 1: Create bridge — only callee-facing RTP side creates offer
        let bridge = BridgePeerBuilder::new("full-webrtc-rtp".to_string())
            .with_rtp_port_range(30000, 30100)
            .build();
        bridge.setup_bridge().await.unwrap();

        // Only create offer on RTP side (faces the callee)
        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();
        bridge.start_bridge().await;

        // Step 2: Create WebRTC caller (simulates JsSIP)
        let webrtc_caller = RtpTrackBuilder::new("caller".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(30100, 30200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let caller_offer = webrtc_caller.local_description().await.unwrap();

        // Step 3: Create RTP callee (simulates SIP phone)
        let rtp_callee = RtpTrackBuilder::new("callee".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(30200, 30300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Step 4: Send bridge's RTP offer to callee → get callee's answer
        let bridge_rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            bridge_rtp_sdp.contains("RTP/AVP"),
            "Bridge RTP offer should be plain RTP"
        );

        let callee_answer = rtp_callee.handshake(bridge_rtp_sdp).await.unwrap();

        // Step 5: Set callee's answer on bridge's RTP side
        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 6: Set caller's offer on bridge's WebRTC side and create answer
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .caller_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.caller_pc().create_answer().await.unwrap();
        bridge.caller_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        // Step 7: Verify the answer SDP that goes back to the WebRTC caller
        assert!(answer_sdp.contains("SAVPF"), "Answer must use SAVPF");
        assert!(
            answer_sdp.contains("fingerprint:sha-256"),
            "Must have real DTLS fingerprint"
        );
        assert!(
            answer_sdp.contains("ice-ufrag:"),
            "Must have real ICE ufrag"
        );
        assert!(
            answer_sdp.contains("ice-pwd:"),
            "Must have real ICE password"
        );
        assert!(
            !answer_sdp.contains("setup:actpass"),
            "Must NOT use actpass in answer"
        );
        assert!(
            answer_sdp.contains("setup:passive") || answer_sdp.contains("setup:active"),
            "Must choose DTLS role"
        );

        // Step 8: Set bridge's WebRTC answer on the caller (simulates 200 OK processing)
        webrtc_caller
            .set_remote_description(&answer_sdp)
            .await
            .unwrap();

        // Verify both sides are fully connected
        assert!(
            bridge.callee_pc().remote_description().is_some(),
            "RTP side should have remote"
        );
        assert!(
            bridge.caller_pc().local_description().is_some(),
            "WebRTC side should have local"
        );
        assert!(
            bridge.caller_pc().remote_description().is_some(),
            "WebRTC side should have remote"
        );

        // Clean up
        bridge.stop().await;
    }

    /// Test the reverse flow: RTP caller → Bridge → WebRTC callee
    #[tokio::test]
    async fn test_full_rtp_to_webrtc_bridge_flow() {
        // Step 1: Create bridge — only callee-facing WebRTC side creates offer
        let bridge = BridgePeerBuilder::new("full-rtp-webrtc".to_string())
            .with_rtp_port_range(31000, 31100)
            .build();
        bridge.setup_bridge().await.unwrap();

        // Only create offer on WebRTC side (faces the callee)
        let webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        bridge
            .caller_pc()
            .set_local_description(webrtc_offer)
            .unwrap();
        bridge.start_bridge().await;

        // Step 2: Create RTP caller (simulates SIP phone)
        let rtp_caller = RtpTrackBuilder::new("rtp-caller".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(31100, 31200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let caller_offer = rtp_caller.local_description().await.unwrap();

        // Step 3: Create WebRTC callee
        let webrtc_callee = RtpTrackBuilder::new("webrtc-callee".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(31200, 31300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Step 4: Send bridge's WebRTC offer to callee → get callee's answer
        let bridge_webrtc_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            bridge_webrtc_sdp.contains("SAVPF"),
            "Bridge WebRTC offer should use SAVPF"
        );

        let callee_answer = webrtc_callee.handshake(bridge_webrtc_sdp).await.unwrap();

        // Step 5: Set callee's answer on bridge's WebRTC side
        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .caller_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 6: Set caller's RTP offer on bridge's RTP side and create answer
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.callee_pc().create_answer().await.unwrap();
        bridge.callee_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        // Step 7: Verify the answer is plain RTP
        assert!(answer_sdp.contains("RTP/AVP"), "Answer must be plain RTP");
        assert!(
            !answer_sdp.contains("fingerprint"),
            "RTP answer must NOT have fingerprint"
        );
        assert!(
            !answer_sdp.contains("ice-ufrag"),
            "RTP answer must NOT have ICE"
        );

        // Step 8: Set bridge's RTP answer on the caller
        rtp_caller
            .set_remote_description(&answer_sdp)
            .await
            .unwrap();

        // Verify both sides are fully connected
        assert!(
            bridge.callee_pc().remote_description().is_some(),
            "RTP side should have remote"
        );
        assert!(
            bridge.callee_pc().local_description().is_some(),
            "RTP side should have local"
        );
        assert!(
            bridge.caller_pc().remote_description().is_some(),
            "WebRTC side should have remote"
        );

        // Clean up
        bridge.stop().await;
    }

    /// Test that bridge builder's audio capabilities produce correct SDP codecs.
    /// When allow_codecs=[PCMU,PCMA], the RTP side SDP should contain PCMU and PCMA,
    /// NOT Opus or other codecs.
    #[tokio::test]
    async fn test_bridge_rtp_side_respects_configured_audio_capabilities() {
        use rustrtc::config::AudioCapability;

        let bridge = BridgePeerBuilder::new("codec-test-rtp".to_string())
            .with_rtp_port_range(32000, 32100)
            .with_callee_audio_capabilities(vec![
                AudioCapability::pcmu(),
                AudioCapability::pcma(),
                AudioCapability::telephone_event(),
            ])
            .build();

        bridge.setup_bridge().await.unwrap();

        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();

        let rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        // Must have PCMU and PCMA
        assert!(rtp_sdp.contains("PCMU/8000"), "RTP SDP must contain PCMU");
        assert!(rtp_sdp.contains("PCMA/8000"), "RTP SDP must contain PCMA");
        assert!(
            rtp_sdp.contains("telephone-event"),
            "RTP SDP must contain telephone-event"
        );
        // Must be plain RTP
        assert!(rtp_sdp.contains("RTP/AVP"), "Must use RTP/AVP");

        bridge.stop().await;
    }

    /// Test WebRTC side with configured Opus+PCMU capabilities.
    #[tokio::test]
    async fn test_bridge_webrtc_side_respects_configured_audio_capabilities() {
        use rustrtc::config::AudioCapability;

        let bridge = BridgePeerBuilder::new("codec-test-webrtc".to_string())
            .with_rtp_port_range(33000, 33100)
            .with_caller_audio_capabilities(vec![
                AudioCapability::opus(),
                AudioCapability::pcmu(),
                AudioCapability::telephone_event(),
            ])
            .build();

        bridge.setup_bridge().await.unwrap();

        let webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        bridge
            .caller_pc()
            .set_local_description(webrtc_offer)
            .unwrap();

        let webrtc_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        assert!(
            webrtc_sdp.contains("opus/48000"),
            "WebRTC SDP must contain Opus"
        );
        assert!(
            webrtc_sdp.contains("PCMU/8000"),
            "WebRTC SDP must contain PCMU"
        );
        assert!(webrtc_sdp.contains("UDP/TLS/RTP/SAVPF"), "Must use SAVPF");
        assert!(
            webrtc_sdp.contains("fingerprint"),
            "Must have DTLS fingerprint"
        );

        bridge.stop().await;
    }

    /// Test bridge with sender codecs configured.
    /// Verifies that custom sender RtpCodecParameters are used (PCMA instead of default PCMU for RTP).
    #[tokio::test]
    async fn test_bridge_with_custom_sender_codecs() {
        use rustrtc::config::AudioCapability;

        let bridge = BridgePeerBuilder::new("sender-codec-test".to_string())
            .with_rtp_port_range(34000, 34100)
            .with_callee_audio_capabilities(vec![
                AudioCapability::pcmu(),
                AudioCapability::pcma(),
                AudioCapability::telephone_event(),
            ])
            .with_sender_codecs(
                RtpCodecParameters {
                    payload_type: 111,
                    clock_rate: 48000,
                    channels: 2,
                },
                RtpCodecParameters {
                    payload_type: 8,
                    clock_rate: 8000,
                    channels: 1,
                }, // PCMA
            )
            .build();

        // setup_bridge should succeed with custom sender codecs
        bridge.setup_bridge().await.unwrap();

        // Both sides should still create valid SDP
        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();
        let rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        assert!(rtp_sdp.contains("RTP/AVP"), "RTP SDP must be plain RTP");
        assert!(
            rtp_sdp.contains("PCMU/8000") || rtp_sdp.contains("PCMA/8000"),
            "RTP SDP must have audio codecs"
        );

        bridge.stop().await;
    }

    /// E2E: WebRTC caller (Opus+PCMU) → Bridge (allow PCMU only) → RTP callee
    /// Verifies that when allow_codecs restricts to PCMU, the bridge's RTP side
    /// offers only PCMU and the SDP negotiation completes successfully.
    #[tokio::test]
    async fn test_bridge_e2e_webrtc_to_rtp_pcmu_only() {
        use crate::media::negotiate::MediaNegotiator;
        // Create WebRTC caller offering Opus + PCMU
        let caller = RtpTrackBuilder::new("webrtc-caller-pcmu".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(35000, 35100)
            .with_codec_preference(vec![CodecType::Opus, CodecType::PCMU])
            .build();
        let caller_offer = caller.local_description().await.unwrap();
        assert!(caller_offer.contains("opus"), "Caller must offer Opus");

        // Build bridge side codec lists from caller SDP + allow_codecs=[PCMU]
        let caller_side_codecs = MediaNegotiator::build_codec_list_from_offer(
            &caller_offer,
            &[CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let callee_side_codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            &caller_offer,
            &[CodecType::PCMU, CodecType::TelephoneEvent],
        );

        // Build bridge with computed capabilities
        let webrtc_caps: Vec<_> = caller_side_codecs
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();
        let rtp_caps: Vec<_> = callee_side_codecs
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();

        let caller_sender = caller_side_codecs
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();
        let callee_sender = callee_side_codecs
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();

        let bridge = BridgePeerBuilder::new("e2e-pcmu-only".to_string())
            .with_rtp_port_range(35100, 35200)
            .with_caller_audio_capabilities(webrtc_caps)
            .with_callee_audio_capabilities(rtp_caps)
            .with_sender_codecs(caller_sender, callee_sender)
            .build();

        bridge.setup_bridge().await.unwrap();

        // RTP side offers to callee
        let rtp_offer = bridge.callee_pc().create_offer().await.unwrap();
        bridge.callee_pc().set_local_description(rtp_offer).unwrap();

        let bridge_rtp_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            bridge_rtp_sdp.contains("PCMU/8000"),
            "RTP side must offer PCMU"
        );
        // Opus should NOT be on the RTP side since allow_codecs=[PCMU]
        assert!(
            !bridge_rtp_sdp.contains("opus"),
            "RTP side should NOT offer Opus (not in allow_codecs)"
        );

        // Callee answers
        let callee = RtpTrackBuilder::new("rtp-callee-pcmu".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(35200, 35300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let callee_answer = callee.handshake(bridge_rtp_sdp).await.unwrap();

        // Set callee answer on bridge RTP side
        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // WebRTC side answers caller
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .caller_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.caller_pc().create_answer().await.unwrap();
        bridge.caller_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(answer_sdp.contains("SAVPF"), "Answer must be WebRTC");
        assert!(
            answer_sdp.contains("fingerprint:sha-256"),
            "Must have real DTLS fingerprint"
        );
        assert!(
            !answer_sdp.contains("setup:actpass"),
            "Must NOT use actpass"
        );

        // Caller processes answer
        caller.set_remote_description(&answer_sdp).await.unwrap();

        bridge.stop().await;
    }

    /// E2E: RTP caller (G729+PCMU) → Bridge → WebRTC callee.
    /// Generated WebRTC offers must not advertise G729.
    #[tokio::test]
    async fn test_bridge_e2e_rtp_to_webrtc_filters_g729_offer() {
        use crate::media::negotiate::MediaNegotiator;

        // Create RTP caller offering G729 + PCMU
        let caller = RtpTrackBuilder::new("rtp-caller-g729".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(36000, 36100)
            .with_codec_preference(vec![CodecType::G729, CodecType::PCMU])
            .build();
        let caller_offer = caller.local_description().await.unwrap();

        // Build bridge side codec lists.
        let caller_side_codecs = MediaNegotiator::build_codec_list_from_offer(
            &caller_offer,
            &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let callee_side_codecs = MediaNegotiator::build_callee_codec_offer_with_allow(
            &caller_offer,
            &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
        );
        let callee_side_codecs =
            MediaNegotiator::filter_webrtc_offer_codecs(&caller_offer, callee_side_codecs);

        // G729 stays on the RTP side, but is removed from the generated WebRTC offer.
        assert!(
            caller_side_codecs
                .iter()
                .any(|c| c.codec == CodecType::G729),
            "G729 on RTP caller side"
        );
        assert!(
            !callee_side_codecs
                .iter()
                .any(|c| c.codec == CodecType::G729),
            "G729 must be removed from WebRTC offer codecs"
        );
        assert!(
            callee_side_codecs
                .iter()
                .any(|c| c.codec == CodecType::PCMU)
        );

        let webrtc_caps: Vec<_> = callee_side_codecs
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();
        let rtp_caps: Vec<_> = caller_side_codecs
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();

        // Callee side sender: first non-DTMF from caller side
        let callee_sender = caller_side_codecs
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();
        let caller_sender = callee_side_codecs
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();

        let bridge = BridgePeerBuilder::new("e2e-g729-drop".to_string())
            .with_rtp_port_range(36100, 36200)
            .with_caller_audio_capabilities(webrtc_caps)
            .with_callee_audio_capabilities(rtp_caps)
            .with_sender_codecs(caller_sender, callee_sender)
            .build();

        bridge.setup_bridge().await.unwrap();

        // WebRTC callee side creates offer
        let webrtc_offer = bridge.caller_pc().create_offer().await.unwrap();
        bridge
            .caller_pc()
            .set_local_description(webrtc_offer)
            .unwrap();

        let bridge_webrtc_sdp = bridge
            .caller_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            bridge_webrtc_sdp.contains("SAVPF"),
            "WebRTC side must use SAVPF"
        );
        assert!(
            !bridge_webrtc_sdp.contains("G729"),
            "WebRTC-side SDP must not offer G729"
        );
        assert!(bridge_webrtc_sdp.contains("PCMU"));

        // WebRTC callee answers
        let callee = RtpTrackBuilder::new("webrtc-callee".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(36200, 36300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let callee_answer = callee.handshake(bridge_webrtc_sdp).await.unwrap();

        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .caller_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // RTP side answers caller
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .callee_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let rtp_answer = bridge.callee_pc().create_answer().await.unwrap();
        bridge
            .callee_pc()
            .set_local_description(rtp_answer)
            .unwrap();

        let rtp_answer_sdp = bridge
            .callee_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            rtp_answer_sdp.contains("RTP/AVP"),
            "RTP answer must be plain RTP"
        );

        caller
            .set_remote_description(&rtp_answer_sdp)
            .await
            .unwrap();

        bridge.stop().await;
    }

    use rustrtc::media::{MediaResult, TrackState};

    // ── Transcoding integration tests ────────────────────────────────────

    /// Mock audio track that yields one pre-defined sample then blocks forever.
    /// The test uses the CancellationToken to stop the forwarding loop.
    struct OneShotAudioTrack {
        sample: parking_lot::Mutex<Option<MediaSample>>,
    }

    #[async_trait::async_trait]
    impl MediaStreamTrack for OneShotAudioTrack {
        fn id(&self) -> &str {
            "one-shot-audio"
        }
        fn kind(&self) -> MediaKind {
            MediaKind::Audio
        }
        fn state(&self) -> TrackState {
            TrackState::Live
        }
        async fn recv(&self) -> MediaResult<MediaSample> {
            let s = self.sample.lock().take();
            if let Some(s) = s {
                Ok(s)
            } else {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(999)).await;
                }
            }
        }
        async fn request_key_frame(&self) -> MediaResult<()> {
            Ok(())
        }
    }

    /// Verify that set_transcoder / clear_transcoder store and clear correctly.
    #[tokio::test]
    async fn test_bridge_set_transcoder() {
        let bridge = BridgePeerBuilder::new("transcoder-test".to_string())
            .with_rtp_port_range(38000, 38100)
            .build();

        // Initially both directions are None
        assert!(bridge.callee_to_caller_codec.transcoder.lock().is_none());
        assert!(bridge.caller_to_callee_codec.transcoder.lock().is_none());

        // Set RTP→WebRTC transcoder (G.729→PCMU)
        bridge.set_transcoder(BridgeEndpoint::Callee, CodecType::G729, CodecType::PCMU, 0);
        assert!(
            bridge.callee_to_caller_codec.transcoder.lock().is_some(),
            "RTP→WebRTC transcoder should be set"
        );

        // Set WebRTC→RTP transcoder (PCMU→G.729)
        bridge.set_transcoder(BridgeEndpoint::Caller, CodecType::PCMU, CodecType::G729, 18);
        assert!(
            bridge.caller_to_callee_codec.transcoder.lock().is_some(),
            "WebRTC→RTP transcoder should be set"
        );

        // Clear RTP→WebRTC
        bridge.clear_transcoder(BridgeEndpoint::Callee);
        assert!(
            bridge.callee_to_caller_codec.transcoder.lock().is_none(),
            "RTP→WebRTC should be cleared"
        );
        assert!(
            bridge.caller_to_callee_codec.transcoder.lock().is_some(),
            "WebRTC→RTP should still be set"
        );

        // Clear all
        bridge.clear_transcoder(BridgeEndpoint::Caller);
        assert!(bridge.caller_to_callee_codec.transcoder.lock().is_none());
    }

    /// Verify that the bridge's forwarding loop applies a Transcoder when
    /// configured.  Injects a G.729 encoded frame into a mock track, lets
    /// run_forward_loop process it through the transcoder, and asserts the
    /// output is PCMU.
    #[tokio::test]
    async fn test_bridge_transcoding_in_forwarding_loop() {
        use audio_codec::create_encoder;
        use rustrtc::media::track::sample_track;

        // Encode 20 ms of silence as G.729 (20 bytes)
        let pcm = vec![0i16; 160];
        let mut g729_enc = create_encoder(CodecType::G729);
        let g729_data = g729_enc.encode(&pcm);

        let input_frame = AudioFrame {
            rtp_timestamp: 100,
            clock_rate: 8000,
            data: g729_data.into(),
            sequence_number: Some(10),
            payload_type: Some(18), // G.729 dynamic PT
            ..Default::default()
        };

        let mock_track: Arc<dyn MediaStreamTrack> = Arc::new(OneShotAudioTrack {
            sample: parking_lot::Mutex::new(Some(MediaSample::Audio(input_frame))),
        });

        // Output channel – run_forward_loop writes transcoded samples here
        // sample_track returns (source, track_a, feedback_rx).
        // track_a receives what source sends.
        let (output_tx, output_track, _) = sample_track(MediaKind::Audio, 10);
        let sender_arc = Arc::new(parking_lot::Mutex::new(Some(output_tx)));
        let sender_weak = Arc::downgrade(&sender_arc);

        // Configure G.729 → PCMU transcoder
        let transcoder = Arc::new(parking_lot::Mutex::new(Some(Transcoder::new(
            CodecType::G729,
            CodecType::PCMU,
            0, // PCMU PT
        ))));
        let timing: Arc<parking_lot::Mutex<Option<RtpTiming>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let cancel = CancellationToken::new();
        let stats: Arc<LegStats> = LegStats::new();
        let dtmf: Arc<parking_lot::RwLock<Option<DtmfSink>>> =
            Arc::new(parking_lot::RwLock::new(None));

        let task_handle = {
            let ctx = ForwardLoopContextBuilder::new(
                "test".to_string(),
                mock_track.clone(),
                sender_weak.clone(),
                Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
                cancel.clone(),
                ForwardPath::new(LegTransport::Callee, LegTransport::Caller),
                stats,
            )
            .with_transcoder(Some(transcoder), Some(timing))
            .with_dtmf_sink(dtmf)
            .build();
            crate::utils::media_spawn(async move {
                BridgePeer::run_forward_loop(ctx).await;
            })
        };

        // Give the loop a moment to process and then cancel
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let _ = task_handle.await;

        // Read from the output track – should receive ONE transcoded PCMU frame
        let captured =
            tokio::time::timeout(std::time::Duration::from_secs(1), output_track.recv()).await;

        match captured {
            Ok(Ok(MediaSample::Audio(frame))) => {
                assert_eq!(
                    frame.payload_type,
                    Some(0),
                    "Output should be PCMU (PT=0), got {:?}",
                    frame.payload_type
                );
                assert_eq!(frame.clock_rate, 8000, "PCMU clock rate should be 8000");
                assert_eq!(
                    frame.data.len(),
                    160,
                    "PCMU 20 ms frame is 160 bytes, got {}",
                    frame.data.len()
                );
                assert_ne!(
                    frame.data.len(),
                    20,
                    "Must NOT be G.729 (20 bytes) – transcoding should have expanded"
                );
            }
            other => panic!("Expected a transcoded PCMU Audio frame, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_bridge_dtmf_mapping_rewrites_payload_type_and_clock_rate() {
        use bytes::Bytes;
        use rustrtc::media::track::sample_track;

        let input_frame = AudioFrame {
            rtp_timestamp: 48_000,
            clock_rate: 48_000,
            data: Bytes::from_static(&[0x05, 0x0A, 0x12, 0xC0]), // digit 5, 4800 ticks
            sequence_number: Some(10),
            payload_type: Some(110),
            marker: true,
            ..Default::default()
        };

        let mock_track: Arc<dyn MediaStreamTrack> = Arc::new(OneShotAudioTrack {
            sample: parking_lot::Mutex::new(Some(MediaSample::Audio(input_frame))),
        });

        let (output_tx, output_track, _) = sample_track(MediaKind::Audio, 10);
        let sender_arc = Arc::new(parking_lot::Mutex::new(Some(output_tx)));
        let sender_weak = Arc::downgrade(&sender_arc);

        let transcoder = Arc::new(parking_lot::Mutex::new(Some(Transcoder::new(
            CodecType::PCMU,
            CodecType::PCMA,
            8,
        ))));
        let dtmf_mapping = Arc::new(parking_lot::RwLock::new(Some(PayloadMapping {
            source_pt: 110,
            target_pt: 126,
            source_clock_rate: 48_000,
            target_clock_rate: 8_000,
        })));

        let cancel = CancellationToken::new();
        let stats = LegStats::new();
        let dtmf_sink = Arc::new(parking_lot::RwLock::new(None));

        let task_handle = {
            let ctx = ForwardLoopContextBuilder::new(
                "test-dtmf-mapping".to_string(),
                mock_track.clone(),
                sender_weak.clone(),
                Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
                cancel.clone(),
                ForwardPath::new(LegTransport::Caller, LegTransport::Callee),
                stats,
            )
            .with_transcoder(Some(transcoder), None)
            .with_dtmf_sink(dtmf_sink)
            .with_dtmf_mapping(Some(dtmf_mapping))
            .build();
            crate::utils::media_spawn(async move {
                BridgePeer::run_forward_loop(ctx).await;
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let _ = task_handle.await;

        let captured =
            tokio::time::timeout(std::time::Duration::from_secs(1), output_track.recv()).await;

        match captured {
            Ok(Ok(MediaSample::Audio(frame))) => {
                assert_eq!(frame.payload_type, Some(126));
                assert_eq!(frame.clock_rate, 8_000);
                assert_eq!(frame.sequence_number, Some(10));
                assert_eq!(frame.rtp_timestamp, 8_000);
                assert_eq!(frame.data.as_ref(), &[0x05, 0x0A, 0x03, 0x20]);
            }
            other => panic!("Expected mapped DTMF Audio frame, got {:?}", other),
        }
    }

    #[test]
    fn test_bridge_dtmf_mapping_uses_shared_timing_when_available() {
        use bytes::Bytes;

        let dtmf_mapping = parking_lot::RwLock::new(Some(PayloadMapping {
            source_pt: 110,
            target_pt: 126,
            source_clock_rate: 48_000,
            target_clock_rate: 8_000,
        }));
        let timing = parking_lot::Mutex::new(Some(RtpTiming::default()));

        let mut first = AudioFrame {
            rtp_timestamp: 48_000,
            clock_rate: 48_000,
            data: Bytes::from_static(&[0x05, 0x0A, 0x12, 0xC0]),
            sequence_number: Some(10),
            payload_type: Some(110),
            ..Default::default()
        };
        let mut second = AudioFrame {
            rtp_timestamp: 52_800,
            clock_rate: 48_000,
            data: Bytes::from_static(&[0x05, 0x0A, 0x12, 0xC0]),
            sequence_number: Some(11),
            payload_type: Some(110),
            ..Default::default()
        };

        assert!(BridgePeer::rewrite_dtmf_sample(
            &mut first,
            Some(&dtmf_mapping),
            Some(&timing),
        ));
        assert!(BridgePeer::rewrite_dtmf_sample(
            &mut second,
            Some(&dtmf_mapping),
            Some(&timing),
        ));

        assert_eq!(first.payload_type, Some(126));
        assert_eq!(first.clock_rate, 8_000);
        assert_eq!(first.data.as_ref(), &[0x05, 0x0A, 0x03, 0x20]);
        assert_eq!(second.payload_type, Some(126));
        assert_eq!(second.clock_rate, 8_000);
        assert_eq!(second.data.as_ref(), &[0x05, 0x0A, 0x03, 0x20]);
        assert_eq!(second.rtp_timestamp.wrapping_sub(first.rtp_timestamp), 800);
        assert_eq!(
            second
                .sequence_number
                .expect("second sequence")
                .wrapping_sub(first.sequence_number.expect("first sequence")),
            1
        );
    }

    /// Verify Opus → PCMU transcoding via Transcoder produces correct output.
    #[tokio::test]
    async fn test_transcoder_opus_to_pcmu() {
        use audio_codec::create_encoder;
        // Generate 20ms of 48kHz mono PCM (960 samples)
        let pcm_48k: Vec<i16> = (0..960).map(|i| ((i * 100) % 32767) as i16).collect();

        // Encode to Opus (stereo encoder with mono→stereo upmix)
        let mut opus_enc = create_encoder(CodecType::Opus);
        let opus_data = opus_enc.encode(&pcm_48k);
        assert!(!opus_data.is_empty(), "Opus encoder should produce output");

        // Verify the Opus packet is stereo (TOC byte bit 2 = stereo flag)
        let is_stereo = opus_data[0] & 0x04 != 0;
        assert!(
            is_stereo,
            "Opus encoder should produce stereo packet (TOC bit 2 set)"
        );

        // First decode separately to verify decoder output length
        let mut standalone_dec = audio_codec::create_decoder(CodecType::Opus);
        let decoded_pcm = standalone_dec.decode(&opus_data);
        // After stereo→mono downmix, 20ms at 48kHz should yield 960 samples
        assert_eq!(
            decoded_pcm.len(),
            960,
            "Opus decoder should output 960 mono samples (20ms), got {}",
            decoded_pcm.len()
        );

        // Now transcode: Opus → PCMU
        let mut transcoder = Transcoder::new(CodecType::Opus, CodecType::PCMU, 0);

        let input_frame = AudioFrame {
            rtp_timestamp: 100,
            clock_rate: 48000,
            data: opus_data.clone().into(),
            sequence_number: Some(10),
            payload_type: Some(111),
            ..Default::default()
        };
        let output = transcoder.transcode(&input_frame);
        assert_eq!(
            output.data.len(),
            160,
            "Opus→PCMU should produce 160 bytes (20ms), got {}",
            output.data.len()
        );
        assert_eq!(output.payload_type, Some(0));

        // Second call — should also produce valid PCMU
        let input_frame2 = AudioFrame {
            rtp_timestamp: 100,
            clock_rate: 48000,
            data: opus_data.into(),
            sequence_number: Some(11),
            payload_type: Some(111),
            ..Default::default()
        };
        let output2 = transcoder.transcode(&input_frame2);
        assert_eq!(
            output2.data.len(),
            160,
            "Second call should also produce 160 bytes"
        );
        assert_eq!(output2.payload_type, Some(0));

        // Decode PCMU back to PCM
        let mut pcmu_dec = audio_codec::create_decoder(CodecType::PCMU);
        let decoded = pcmu_dec.decode(&output.data);
        assert_eq!(
            decoded.len(),
            160,
            "PCMU decode should yield 160 samples (20ms at 8kHz), got {}",
            decoded.len()
        );
    }

    /// Verify Opus → G.722 transcoding via Transcoder produces correct output.
    #[tokio::test]
    async fn test_transcoder_opus_to_g722() {
        use audio_codec::create_encoder;

        // Generate 20ms of 48kHz mono PCM (960 samples)
        let pcm_48k: Vec<i16> = (0..960).map(|i| ((i * 100) % 32767) as i16).collect();

        // Encode to Opus
        let mut opus_enc = create_encoder(CodecType::Opus);
        let opus_data = opus_enc.encode(&pcm_48k);
        assert!(!opus_data.is_empty(), "Opus encoder should produce output");

        // Transcode: Opus → G.722 (default 64kbps)
        let mut transcoder = Transcoder::new(CodecType::Opus, CodecType::G722, 9);

        let input_frame = AudioFrame {
            rtp_timestamp: 100,
            clock_rate: 48000,
            data: opus_data.into(),
            sequence_number: Some(10),
            payload_type: Some(111),
            ..Default::default()
        };
        let output = transcoder.transcode(&input_frame);
        // G.722 at 64kbps for 20ms = 160 bytes (1 byte per sample pair at 16kHz)
        assert!(
            !output.data.is_empty(),
            "Opus→G.722 should produce non-empty output"
        );
        assert_eq!(output.payload_type, Some(9));

        // G.722 clock rate is 8000 (RTP convention)
        assert_eq!(output.clock_rate, 8000, "G.722 clock_rate should be 8000");
    }

    /// Run full Opus→PCMU round-trip: encode PCM → Opus → transcode → PCMU → decode → verify PCM correlation.
    #[tokio::test]
    async fn test_transcoder_opus_to_pcmu_roundtrip_quality() {
        use audio_codec::{create_decoder, create_encoder};

        // Generate a known audio signal at 48kHz
        let freq = 440.0; // A4 tone
        let sample_rate = 48000.0;
        let pcm_48k: Vec<i16> = (0..960)
            .map(|i| {
                let t = i as f64 / sample_rate;
                (16384.0 * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16
            })
            .collect();

        // Encode to Opus (first packet)
        let mut opus_enc = create_encoder(CodecType::Opus);
        let opus_data = opus_enc.encode(&pcm_48k);

        // Transcode: Opus → PCMU
        let mut transcoder = Transcoder::new(CodecType::Opus, CodecType::PCMU, 0);

        // Apply transcoder multiple times (simulate real call behavior)
        let mut all_pcmu = Vec::new();
        for i in 0..5 {
            let frame = AudioFrame {
                rtp_timestamp: 100 + i * 960,
                clock_rate: 48000,
                data: opus_data.clone().into(),
                sequence_number: Some(10 + i as u16),
                payload_type: Some(111),
                ..Default::default()
            };
            let output = transcoder.transcode(&frame);
            assert_eq!(
                output.data.len(),
                160,
                "Frame {}: Opus→PCMU should produce 160 bytes",
                i
            );

            // Decode PCMU back to PCM
            let mut pcmu_dec = create_decoder(CodecType::PCMU);
            let decoded = pcmu_dec.decode(&output.data);
            assert_eq!(decoded.len(), 160);
            all_pcmu.push(decoded);
        }

        // Verify all frames have reasonable energy (not silence/garbled)
        for (i, frame) in all_pcmu.iter().enumerate() {
            let energy: f64 =
                frame.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / frame.len() as f64;
            let rms = energy.sqrt();
            assert!(
                rms > 100.0 && rms < 20000.0,
                "Frame {}: RMS {} is outside expected range for 440Hz sine",
                i,
                rms
            );
        }
    }

    /// Verify that the forwarding loop passes audio through unchanged when
    /// NO transcoder is set (passthrough mode).
    #[tokio::test]
    async fn test_bridge_forwarding_passthrough_without_transcoder() {
        use audio_codec::create_encoder;
        use rustrtc::media::track::sample_track;

        let pcm = vec![0i16; 160];
        let mut pcmu_enc = create_encoder(CodecType::PCMU);
        let pcmu_data = pcmu_enc.encode(&pcm);

        let input_frame = AudioFrame {
            rtp_timestamp: 200,
            clock_rate: 8000,
            data: pcmu_data.into(),
            sequence_number: Some(20),
            payload_type: Some(0), // PCMU
            ..Default::default()
        };

        let mock_track: Arc<dyn MediaStreamTrack> = Arc::new(OneShotAudioTrack {
            sample: parking_lot::Mutex::new(Some(MediaSample::Audio(input_frame))),
        });

        let (output_tx, output_track, _) = sample_track(MediaKind::Audio, 10);
        let sender_arc = Arc::new(parking_lot::Mutex::new(Some(output_tx)));
        let sender_weak = Arc::downgrade(&sender_arc);

        let cancel = CancellationToken::new();
        let stats = LegStats::new();
        let dtmf = Arc::new(parking_lot::RwLock::new(None));

        let task_handle = {
            let ctx = ForwardLoopContextBuilder::new(
                "test-passthrough".to_string(),
                mock_track.clone(),
                sender_weak.clone(),
                Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
                cancel.clone(),
                ForwardPath::new(LegTransport::Callee, LegTransport::Caller),
                stats,
            )
            .with_dtmf_sink(dtmf)
            .build();
            crate::utils::media_spawn(async move {
                BridgePeer::run_forward_loop(ctx).await;
            })
        };

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let _ = task_handle.await;

        let captured =
            tokio::time::timeout(std::time::Duration::from_secs(1), output_track.recv()).await;

        match captured {
            Ok(Ok(MediaSample::Audio(frame))) => {
                assert_eq!(
                    frame.payload_type,
                    Some(0),
                    "Passthrough PCMU should keep PT=0"
                );
                assert_eq!(frame.data.len(), 160, "Passthrough should keep 160 bytes");
            }
            other => panic!("Expected passthrough PCMU Audio frame, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_bridge_session_id_propagated_from_builder() {
        let bridge = BridgePeerBuilder::new("sid-bridge".to_string())
            .with_session_id("test-session-abc".to_string())
            .build();

        assert_eq!(
            bridge.session_id.as_deref(),
            Some("test-session-abc"),
            "bridge should have session_id from builder"
        );
    }
}
