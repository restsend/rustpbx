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
//! let webrtc_pc = bridge.webrtc_pc();
//! let rtp_pc = bridge.rtp_pc();
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

use crate::media::recorder::{Leg as RecLeg, Recorder};
use crate::media::transcoder::{RtpTiming, Transcoder};
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use tokio::sync::Mutex as AsyncMutex;
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
    _output_state: Arc<AsyncMutex<OutputState>>,
}

/// One media endpoint owned by a BridgePeer. Kept for backward compat;
/// new code should use PeerId-based APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeEndpoint {
    WebRtc,
    Rtp,
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
            frame.payload_type = Some(self.payload_type.load(Ordering::Relaxed));
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
    WebRtc,
    Rtp,
}

impl LegTransport {
    const fn as_str(self) -> &'static str {
        match self {
            Self::WebRtc => "WebRTC",
            Self::Rtp => "RTP",
        }
    }

    const fn endpoint(self) -> BridgeEndpoint {
        match self {
            Self::WebRtc => BridgeEndpoint::WebRtc,
            Self::Rtp => BridgeEndpoint::Rtp,
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

    /// Strip WebRTC-only fields when forwarding into a plain RTP pipeline.
    const fn should_strip_webrtc_audio_metadata(self) -> bool {
        matches!(
            (self.from, self.to),
            (LegTransport::WebRtc, LegTransport::Rtp)
        )
    }
}

impl std::fmt::Display for ForwardPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}→{}", self.from.as_str(), self.to.as_str())
    }
}

type DtmfHandler = Arc<dyn Fn(char) + Send + Sync + 'static>;

#[derive(Clone)]
struct BridgeDtmfSink {
    endpoint: BridgeEndpoint,
    payload_types: Vec<u8>,
    handler: DtmfHandler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BridgeDtmfEventKey {
    digit_code: u8,
    rtp_timestamp: u32,
}

#[derive(Debug, Default)]
struct BridgeDtmfDetector {
    last_event: Option<BridgeDtmfEventKey>,
}

impl BridgeDtmfDetector {
    fn observe(&mut self, payload: &[u8], rtp_timestamp: u32) -> Option<char> {
        if payload.len() < 4 {
            return None;
        }

        let digit_code = payload[0];
        let digit = crate::media::telephone_event::dtmf_code_to_char(digit_code)?;

        let event = BridgeDtmfEventKey {
            digit_code,
            rtp_timestamp,
        };

        if self.last_event == Some(event) {
            return None;
        }

        self.last_event = Some(event);
        Some(digit)
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

/// BridgePeer manages PeerConnections to bridge media between endpoints.
///
/// Supports N peers (2+). For the common 2-peer WebRTC↔RTP case,
/// fast-path fields (`webrtc_pc`, `rtp_pc`, etc.) are kept as aliases.
/// Identifies which side of the bridge to operate on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeSide {
    WebRtc,
    Rtp,
}

pub struct BridgePeer {
    id: String,
    /// WebRTC side PeerConnection (SRTP/DTLS) — fast-path alias for peers["webrtc"]
    webrtc_pc: PeerConnection,
    /// RTP side PeerConnection (plain RTP) — fast-path alias for peers["rtp"]
    rtp_pc: PeerConnection,
    /// Bridge task handles
    bridge_tasks: AsyncMutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Cancellation token
    cancel_token: CancellationToken,
    forwarding_started: AtomicBool,
    /// Shared recorder for call recording (written by both bridge directions)
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    dtmf_sink: Arc<parking_lot::RwLock<Option<BridgeDtmfSink>>>,
    /// Audio sender channels for forwarding — fast-path aliases
    webrtc_send: Arc<AsyncMutex<Option<MediaSender>>>,
    rtp_send: Arc<AsyncMutex<Option<MediaSender>>>,
    /// Output state (mode + file source) — both fields protected by a single Mutex.
    webrtc_output_state: Arc<AsyncMutex<OutputState>>,
    rtp_output_state: Arc<AsyncMutex<OutputState>>,
    /// Fast-path mode check for bidirectional forwarder (atomic, no source needed).
    webrtc_output_mode: Arc<AtomicU8>,
    rtp_output_mode: Arc<AtomicU8>,
    /// Video sender channels for forwarding
    webrtc_video_send: Arc<AsyncMutex<Option<MediaSender>>>,
    rtp_video_send: Arc<AsyncMutex<Option<MediaSender>>>,
    /// Stored sample tracks for test-mode direct forwarding
    webrtc_track: AsyncMutex<Option<Arc<SampleStreamTrack>>>,
    rtp_track: AsyncMutex<Option<Arc<SampleStreamTrack>>>,
    /// Sender codec parameters (set by builder, used by setup_bridge)
    webrtc_sender_codec: Option<RtpCodecParameters>,
    rtp_sender_codec: Option<RtpCodecParameters>,
    /// Video codec parameters (set by builder, used by setup_bridge)
    webrtc_video_codec: Option<RtpCodecParameters>,
    rtp_video_codec: Option<RtpCodecParameters>,
    /// RtpSender handles for video — used to subscribe to PLI/FIR RTCP feedback
    webrtc_video_sender: AsyncMutex<Option<Arc<RtpSender>>>,
    rtp_video_sender: AsyncMutex<Option<Arc<RtpSender>>>,
    /// Target payload type stamped by rustpbx onto pass-through video RTP.
    webrtc_video_payload_type: Arc<AtomicU8>,
    rtp_video_payload_type: Arc<AtomicU8>,
    /// Per-direction media forwarding stats (cumulative)
    webrtc_to_rtp_stats: Arc<LegStats>,
    rtp_to_webrtc_stats: Arc<LegStats>,

    /// N-peer: all peers indexed by PeerId (includes "webrtc"/"rtp" for fast path)
    peers: Arc<AsyncMutex<std::collections::HashMap<PeerId, PeerEntry>>>,
    /// N-peer: routing table (source_peer_id → set of destination peer ids)
    routes: Arc<AsyncMutex<std::collections::HashMap<PeerId, std::collections::HashSet<PeerId>>>>,

    /// Optional transcoder for RTP→WebRTC direction (e.g. G.729→PCMU).
    /// Set dynamically via set_transcoder() when caller and callee codecs differ.
    rtp_to_webrtc_transcoder: Arc<parking_lot::RwLock<Option<Transcoder>>>,
    /// Optional RTP timestamp/sequence rewriter for RTP→WebRTC direction.
    rtp_to_webrtc_timing: Arc<parking_lot::RwLock<Option<RtpTiming>>>,
    /// Optional transcoder for WebRTC→RTP direction (e.g. PCMU→G.729).
    webrtc_to_rtp_transcoder: Arc<parking_lot::RwLock<Option<Transcoder>>>,
    /// Optional RTP timestamp/sequence rewriter for WebRTC→RTP direction.
    webrtc_to_rtp_timing: Arc<parking_lot::RwLock<Option<RtpTiming>>>,
}

impl BridgePeer {
    /// Create a new bridge peer with given WebRTC and RTP PeerConnections
    pub fn new(id: String, webrtc_pc: PeerConnection, rtp_pc: PeerConnection) -> Self {
        Self {
            id,
            webrtc_pc,
            rtp_pc,
            bridge_tasks: AsyncMutex::new(Vec::new()),
            cancel_token: CancellationToken::new(),
            forwarding_started: AtomicBool::new(false),
            recorder: None,
            dtmf_sink: Arc::new(parking_lot::RwLock::new(None)),
            webrtc_send: Arc::new(AsyncMutex::new(None)),
            rtp_send: Arc::new(AsyncMutex::new(None)),
            webrtc_output_state: Arc::new(AsyncMutex::new(OutputState {
                mode: BRIDGE_OUTPUT_PEER,
                file_source: None,
                next_rtp_timestamp: None,
                next_sequence_number: None,
                active_rtp_offset: None,
                active_seq_offset: None,
            })),
            rtp_output_state: Arc::new(AsyncMutex::new(OutputState {
                mode: BRIDGE_OUTPUT_PEER,
                file_source: None,
                next_rtp_timestamp: None,
                next_sequence_number: None,
                active_rtp_offset: None,
                active_seq_offset: None,
            })),
            webrtc_output_mode: Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
            rtp_output_mode: Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
            webrtc_video_send: Arc::new(AsyncMutex::new(None)),
            rtp_video_send: Arc::new(AsyncMutex::new(None)),
            webrtc_track: AsyncMutex::new(None),
            rtp_track: AsyncMutex::new(None),
            webrtc_sender_codec: None,
            rtp_sender_codec: None,
            webrtc_video_codec: None,
            rtp_video_codec: None,
            webrtc_video_sender: AsyncMutex::new(None),
            rtp_video_sender: AsyncMutex::new(None),
            webrtc_video_payload_type: Arc::new(AtomicU8::new(96)),
            rtp_video_payload_type: Arc::new(AtomicU8::new(96)),
            webrtc_to_rtp_stats: LegStats::new(),
            rtp_to_webrtc_stats: LegStats::new(),
            peers: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            routes: Arc::new(AsyncMutex::new(std::collections::HashMap::new())),
            rtp_to_webrtc_transcoder: Arc::new(parking_lot::RwLock::new(None)),
            rtp_to_webrtc_timing: Arc::new(parking_lot::RwLock::new(None)),
            webrtc_to_rtp_transcoder: Arc::new(parking_lot::RwLock::new(None)),
            webrtc_to_rtp_timing: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Setup the bridge by adding sample tracks to both sides for forwarding
    ///
    /// `webrtc_params`: Codec parameters for the WebRTC-side track (e.g. Opus 48kHz)
    /// `rtp_params`: Codec parameters for the RTP-side track (e.g. PCMU 8kHz or Opus 48kHz)
    pub async fn setup_bridge_with_codecs(
        &self,
        webrtc_params: RtpCodecParameters,
        rtp_params: RtpCodecParameters,
    ) -> Result<()> {
        // Setup WebRTC side: create sample track and register with PC
        let (webrtc_tx, webrtc_track, _) =
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);

        *self.webrtc_track.lock().await = Some(webrtc_track.clone());
        let _ = self.webrtc_pc().add_track(webrtc_track, webrtc_params);
        *self.webrtc_send.lock().await = Some(webrtc_tx);

        // Setup RTP side: create sample track and register with PC
        let (rtp_tx, rtp_track, _) = rustrtc::media::track::sample_track(MediaKind::Audio, 100);

        *self.rtp_track.lock().await = Some(rtp_track.clone());
        let _ = self.rtp_pc().add_track(rtp_track, rtp_params);
        *self.rtp_send.lock().await = Some(rtp_tx);

        let mut tasks = self.bridge_tasks.lock().await;
        tasks.push(Self::spawn_file_output_clock(
            self.id.clone(),
            BridgeEndpoint::WebRtc,
            self.webrtc_send.lock().await.clone(),
            Arc::clone(&self.webrtc_output_state),
            self.cancel_token.clone(),
        ));
        tasks.push(Self::spawn_file_output_clock(
            self.id.clone(),
            BridgeEndpoint::Rtp,
            self.rtp_send.lock().await.clone(),
            Arc::clone(&self.rtp_output_state),
            self.cancel_token.clone(),
        ));
        drop(tasks);

        // Setup video senders if video codecs are configured
        if let Some(ref webrtc_video_params) = self.webrtc_video_codec {
            self.webrtc_video_payload_type
                .store(webrtc_video_params.payload_type, Ordering::Relaxed);
            let (webrtc_video_tx, webrtc_video_track, _) =
                rustrtc::media::track::sample_track(MediaKind::Video, 100);
            if let Ok(sender) = self
                .webrtc_pc()
                .add_track(webrtc_video_track, webrtc_video_params.clone())
            {
                *self.webrtc_video_sender.lock().await = Some(sender);
            }
            *self.webrtc_video_send.lock().await = Some(webrtc_video_tx);
            debug!(bridge_id = %self.id, pt = webrtc_video_params.payload_type, clock_rate = webrtc_video_params.clock_rate, "WebRTC video sender setup complete");
        } else {
            debug!(bridge_id = %self.id, "WebRTC video sender NOT configured (no video codec)");
        }

        if let Some(ref rtp_video_params) = self.rtp_video_codec {
            self.rtp_video_payload_type
                .store(rtp_video_params.payload_type, Ordering::Relaxed);
            let (rtp_video_tx, rtp_video_track, _) =
                rustrtc::media::track::sample_track(MediaKind::Video, 100);
            if let Ok(sender) = self
                .rtp_pc()
                .add_track(rtp_video_track, rtp_video_params.clone())
            {
                *self.rtp_video_sender.lock().await = Some(sender);
            }
            *self.rtp_video_send.lock().await = Some(rtp_video_tx);
            debug!(bridge_id = %self.id, pt = rtp_video_params.payload_type, clock_rate = rtp_video_params.clock_rate, "RTP video sender setup complete");
        }
        Ok(())
    }

    /// Dynamically add video tracks to BOTH bridge sides mid-call.
    /// Used when a re-INVITE adds video to a previously audio-only call.
    /// Caller side determines the codec PT/clock-rate from its offer;
    /// the same params are used on the opposite side for symmetric video.
    pub async fn add_video_track(&self, pt: u8, clock_rate: u32) -> Result<()> {
        let params = RtpCodecParameters {
            payload_type: pt,
            clock_rate,
            channels: 0,
        };

        self.webrtc_video_payload_type
            .store(params.payload_type, Ordering::Relaxed);
        self.rtp_video_payload_type
            .store(params.payload_type, Ordering::Relaxed);

        // WebRTC side
        if self.webrtc_video_send.lock().await.is_none() {
            let (tx, track, _) = rustrtc::media::track::sample_track(MediaKind::Video, 100);
            let sender = self.webrtc_pc().add_track(track, params.clone())?;
            *self.webrtc_video_sender.lock().await = Some(sender);
            *self.webrtc_video_send.lock().await = Some(tx);
        }

        // RTP side
        if self.rtp_video_send.lock().await.is_none() {
            let (tx, track, _) = rustrtc::media::track::sample_track(MediaKind::Video, 100);
            let sender = self.rtp_pc().add_track(track, params)?;
            *self.rtp_video_sender.lock().await = Some(sender);
            *self.rtp_video_send.lock().await = Some(tx);
        }

        debug!(
            bridge_id = %self.id,
            pt = pt,
            clock_rate = clock_rate,
            "Video tracks added dynamically to both bridge sides"
        );
        Ok(())
    }

    /// Check whether video senders have been configured on this bridge.
    pub async fn has_video(&self) -> bool {
        self.webrtc_video_send.lock().await.is_some() && self.rtp_video_send.lock().await.is_some()
    }

    /// Update target payload types stamped onto pass-through video frames.
    pub fn set_video_payload_types(
        &self,
        webrtc_params: Option<RtpCodecParameters>,
        rtp_params: Option<RtpCodecParameters>,
    ) {
        if let Some(params) = webrtc_params.as_ref() {
            self.webrtc_video_payload_type
                .store(params.payload_type, Ordering::Relaxed);
            debug!(
                bridge_id = %self.id,
                side = "webrtc",
                pt = params.payload_type,
                clock_rate = params.clock_rate,
                "Video pass-through payload type updated"
            );
        }
        if let Some(params) = rtp_params.as_ref() {
            self.rtp_video_payload_type
                .store(params.payload_type, Ordering::Relaxed);
            debug!(
                bridge_id = %self.id,
                side = "rtp",
                pt = params.payload_type,
                clock_rate = params.clock_rate,
                "Video pass-through payload type updated"
            );
        }
    }

    /// Setup the bridge with codec parameters.
    /// Uses sender codecs from builder if set, otherwise falls back to defaults.
    pub async fn setup_bridge(&self) -> Result<()> {
        let webrtc_params = self
            .webrtc_sender_codec
            .clone()
            .unwrap_or(RtpCodecParameters {
                payload_type: 111,
                clock_rate: 48000,
                channels: 2,
            });
        let rtp_params = self.rtp_sender_codec.clone().unwrap_or(RtpCodecParameters {
            payload_type: 0,
            clock_rate: 8000,
            channels: 1,
        });
        self.setup_bridge_with_codecs(webrtc_params, rtp_params)
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
            let w2r = Arc::clone(&self.webrtc_to_rtp_stats);
            let r2w = Arc::clone(&self.rtp_to_webrtc_stats);
            let bridge_id = self.id.clone();
            let cancel = self.cancel_token.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // skip initial immediate tick
                interval.tick().await;
                let (mut prev_w_pkts, mut prev_w_lost, mut prev_w_bytes) = (0u64, 0u64, 0u64);
                let (mut prev_r_pkts, mut prev_r_lost, mut prev_r_bytes) = (0u64, 0u64, 0u64);
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

                            info!(
                                bridge_id = %bridge_id,
                                webrtc_to_rtp_pps   = dw_pkts,
                                webrtc_to_rtp_kbps  = dw_bytes * 8 / 5 / 1000,
                                webrtc_to_rtp_loss  = format!("{:.2}%", w_loss_pct),
                                webrtc_to_rtp_drop  = w_drop,
                                rtp_to_webrtc_pps   = dr_pkts,
                                rtp_to_webrtc_kbps  = dr_bytes * 8 / 5 / 1000,
                                rtp_to_webrtc_loss  = format!("{:.2}%", r_loss_pct),
                                rtp_to_webrtc_drop  = r_drop,
                                "Bridge leg stats [5s]"
                            );

                            (prev_w_pkts, prev_w_lost, prev_w_bytes) = (w_pkts, w_lost, w_bytes);
                            (prev_r_pkts, prev_r_lost, prev_r_bytes) = (r_pkts, r_lost, r_bytes);
                        }
                    }
                }
            })
        };

        let mut tasks = self.bridge_tasks.lock().await;
        tasks.push(bidirectional_task);
        tasks.push(stats_task);
        tasks.push(self.spawn_peer_forward_loops());
    }

    /// Get the WebRTC-side sender (for test injection)
    pub async fn get_webrtc_sender(&self) -> Option<MediaSender> {
        self.webrtc_send.lock().await.clone()
    }

    /// Get the RTP-side sender (for test injection)
    pub async fn get_rtp_sender(&self) -> Option<MediaSender> {
        self.rtp_send.lock().await.clone()
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
        *sink = Some(BridgeDtmfSink {
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

    pub async fn replace_output_with_file(
        &self,
        endpoint: BridgeEndpoint,
        track: &crate::media::FileTrack,
    ) -> Result<()> {
        match endpoint {
            BridgeEndpoint::WebRtc => self.get_webrtc_sender().await,
            BridgeEndpoint::Rtp => self.get_rtp_sender().await,
        }
        .ok_or_else(|| anyhow::anyhow!("bridge {:?} output sender is not ready", endpoint))?;

        let source = track.create_playback_source()?;
        let mut state = self.output_state(endpoint).lock().await;
        let old_source = state.file_source.replace(source);
        state.mode = BRIDGE_OUTPUT_FILE;
        state.active_rtp_offset = None;
        state.active_seq_offset = None;
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_FILE, Ordering::Release);
        drop(state);
        drop(old_source);

        info!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            "Bridge output replaced with file source"
        );
        Ok(())
    }

    pub async fn replace_output_with_silence(
        &self,
        endpoint: BridgeEndpoint,
        codec_info: crate::media::negotiate::CodecInfo,
    ) -> Result<()> {
        match endpoint {
            BridgeEndpoint::WebRtc => self.get_webrtc_sender().await,
            BridgeEndpoint::Rtp => self.get_rtp_sender().await,
        }
        .ok_or_else(|| anyhow::anyhow!("bridge {:?} output sender is not ready", endpoint))?;

        let track = crate::media::FileTrack::new(format!("{}-{:?}-silence", self.id, endpoint))
            .with_loop(true)
            .with_codec_info(codec_info);
        let source = track.create_playback_source()?;
        let mut state = self.output_state(endpoint).lock().await;
        let old_source = state.file_source.replace(source);
        state.mode = BRIDGE_OUTPUT_FILE;
        state.active_rtp_offset = None;
        state.active_seq_offset = None;
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_FILE, Ordering::Release);
        drop(state);
        drop(old_source);

        info!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            "Bridge output replaced with silence source"
        );
        Ok(())
    }

    pub async fn replace_output_with_peer(&self, endpoint: BridgeEndpoint) {
        let mut state = self.output_state(endpoint).lock().await;
        state.mode = BRIDGE_OUTPUT_PEER;
        let old_source = state.file_source.take();
        self.output_mode(endpoint)
            .store(BRIDGE_OUTPUT_PEER, Ordering::Release);
        drop(state);
        drop(old_source);
        info!(
            bridge_id = %self.id,
            endpoint = ?endpoint,
            "Bridge output replaced with peer source"
        );
    }

    pub async fn mute_output(&self, endpoint: BridgeEndpoint) {
        let mut state = self.output_state(endpoint).lock().await;
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

    /// Add a peer to the N-peer bridge.
    pub async fn add_peer(&self, peer_id: PeerId, entry: PeerEntry) -> Result<()> {
        let mut peers = self.peers.lock().await;
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
        let mut peers = self.peers.lock().await;
        let mut routes = self.routes.lock().await;
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
        let peers = self.peers.lock().await;
        for dest in &to_peers {
            if !peers.contains_key(dest) {
                return Err(anyhow::anyhow!("Route destination peer {} not found", dest));
            }
        }
        drop(peers);
        let mut routes = self.routes.lock().await;
        routes.insert(from_peer, to_peers);
        Ok(())
    }

    /// Configure a transcoder for media flowing out of the given endpoint.
    ///
    /// `from_endpoint` is the source side of the direction to transcode:
    /// - `BridgeEndpoint::Rtp` – transcode media received from the RTP side before sending to WebRTC
    /// - `BridgeEndpoint::WebRtc` – transcode media received from the WebRTC side before sending to RTP
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
            BridgeEndpoint::Rtp => (&self.rtp_to_webrtc_transcoder, &self.rtp_to_webrtc_timing),
            BridgeEndpoint::WebRtc => (&self.webrtc_to_rtp_transcoder, &self.webrtc_to_rtp_timing),
        };
        let source_cr = source.clock_rate();
        let target_cr = target.clock_rate();
        *transcoder_slot.write() = Some(Transcoder::new(source, target, target_pt));
        if source_cr != target_cr {
            *timing_slot.write() = Some(RtpTiming::default());
        } else {
            timing_slot.write().take();
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
            BridgeEndpoint::Rtp => (&self.rtp_to_webrtc_transcoder, &self.rtp_to_webrtc_timing),
            BridgeEndpoint::WebRtc => (&self.webrtc_to_rtp_transcoder, &self.webrtc_to_rtp_timing),
        };
        *transcoder_slot.write() = None;
        *timing_slot.write() = None;
    }

    fn output_state(&self, endpoint: BridgeEndpoint) -> &Arc<AsyncMutex<OutputState>> {
        match endpoint {
            BridgeEndpoint::WebRtc => &self.webrtc_output_state,
            BridgeEndpoint::Rtp => &self.rtp_output_state,
        }
    }

    fn output_mode(&self, endpoint: BridgeEndpoint) -> &Arc<AtomicU8> {
        match endpoint {
            BridgeEndpoint::WebRtc => &self.webrtc_output_mode,
            BridgeEndpoint::Rtp => &self.rtp_output_mode,
        }
    }

    fn spawn_file_output_clock(
        bridge_id: String,
        endpoint: BridgeEndpoint,
        sender: Option<MediaSender>,
        output_state: Arc<AsyncMutex<OutputState>>,
        cancel_token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    _ = interval.tick() => {
                            let sample = {
                            let mut guard = output_state.lock().await;
                            if guard.mode != BRIDGE_OUTPUT_FILE {
                                continue;
                            }
                            let Some(source) = guard.file_source.as_mut() else {
                                guard.mode = BRIDGE_OUTPUT_MUTED;
                                continue;
                            };
                            source.next_audio_sample()
                        };

                        let Some(mut sample) = sample else {
                            let mut guard = output_state.lock().await;
                            guard.mode = BRIDGE_OUTPUT_MUTED;
                            guard.file_source.take();
                            guard.active_rtp_offset = None;
                            guard.active_seq_offset = None;
                            debug!(
                                bridge_id = %bridge_id,
                                endpoint = ?endpoint,
                                "Bridge file output completed"
                            );
                            continue;
                        };

                        if let MediaSample::Audio(frame) = &mut sample {
                            let mut guard = output_state.lock().await;

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

                        let Some(sender) = sender.as_ref() else {
                            warn!(
                                bridge_id = %bridge_id,
                                endpoint = ?endpoint,
                                "Bridge file output sender is unavailable"
                            );
                            break;
                        };

                        match sender.try_send(sample.clone()) {
                            Ok(()) => {}
                            Err(MediaError::WouldBlock) => {
                                if let Err(error) = sender.send(sample).await {
                                    warn!(
                                        bridge_id = %bridge_id,
                                        endpoint = ?endpoint,
                                        error = %error,
                                        "Bridge file output send failed"
                                    );
                                    break;
                                }
                            }
                            Err(error) => {
                                warn!(
                                    bridge_id = %bridge_id,
                                    endpoint = ?endpoint,
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
    /// For the 2-peer case, the existing `spawn_bidirectional_forwarder` handles it.
    fn spawn_peer_forward_loops(&self) -> tokio::task::JoinHandle<()> {
        let peers_map = self.peers.clone();
        let routes_map = self.routes.clone();
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();
        let dtmf_sink = Arc::clone(&self.dtmf_sink);
        let recorder = self.recorder.clone();

        tokio::spawn(async move {
            let peers = peers_map.lock().await;
            let extra_peers: Vec<(PeerId, PeerConnection)> = peers
                .iter()
                .filter(|(id, _)| *id != "webrtc" && *id != "rtp")
                .map(|(id, entry)| (id.clone(), entry.pc.clone()))
                .collect();
            drop(peers);

            for (peer_id, pc) in extra_peers {
                let routes = routes_map.clone();
                let _dtmf = Arc::clone(&dtmf_sink);
                let _rec = recorder.clone();
                let cancel = cancel_token.child_token();
                let pid = peer_id.clone();
                let bid = bridge_id.clone();
                let peers_ref = peers_map.clone();
                tokio::spawn(async move {
                    let mut recv = Box::pin(pc.recv());
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            event = &mut recv => {
                                match event {
                                    Some(PeerConnectionEvent::Track(transceiver)) => {
                                        if let Some(receiver) = transceiver.receiver() {
                                            let track = receiver.track();
                                            let _is_video = transceiver.kind() == rustrtc::MediaKind::Video;
                                            let route_dests: Vec<PeerId> = routes.lock().await
                                                .get(&pid)
                                                .cloned()
                                                .unwrap_or_default()
                                                .into_iter().collect();

                                            if !route_dests.is_empty() {
                                                let track_id = track.id().to_string();
                                                let peers_for_forward = peers_ref.clone();
                                                let dests_for_forward = route_dests.clone();
                                                let pid_clone = pid.clone();
                                                let bid_clone = bid.clone();
                                                tokio::spawn(async move {
                                                    loop {
                                                        let sample = match track.recv().await {
                                                            Ok(s) => s,
                                                            Err(_) => break,
                                                        };
                                                        let guard = peers_for_forward.lock().await;
                                                        for dest in &dests_for_forward {
                                                            if let Some(entry) = guard.get(dest) {
                                                                if let Some(ref sender) = entry.audio_sender {
                                                                    let _ = sender.try_send(sample.clone());
                                                                }
                                                            }
                                                        }
                                                        drop(guard);
                                                    }
                                                    debug!(bridge_id = %bid_clone, peer_id = %pid_clone, track = %track_id, "N-peer forward task ended");
                                                });
                                            }
                                        }
                                        recv = Box::pin(pc.recv());
                                    }
                                    Some(_) => {
                                        recv = Box::pin(pc.recv());
                                    }
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

    pub async fn get_webrtc_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.webrtc_track.lock().await.clone()
    }

    /// Get the RTP-side track (for test consumption)
    pub async fn get_rtp_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.rtp_track.lock().await.clone()
    }

    fn spawn_bidirectional_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let webrtc_pc = self.webrtc_pc.clone();
        let rtp_pc = self.rtp_pc.clone();
        let rtp_send = Arc::downgrade(&self.rtp_send);
        let webrtc_send = Arc::downgrade(&self.webrtc_send);
        let rtp_output_mode = Arc::clone(&self.rtp_output_mode);
        let webrtc_output_mode = Arc::clone(&self.webrtc_output_mode);
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();
        let w2r_stats = Arc::clone(&self.webrtc_to_rtp_stats);
        let r2w_stats = Arc::clone(&self.rtp_to_webrtc_stats);
        let recorder = self.recorder.clone();
        let dtmf_sink = Arc::clone(&self.dtmf_sink);
        let webrtc_to_rtp_transcoder = Arc::clone(&self.webrtc_to_rtp_transcoder);
        let webrtc_to_rtp_timing = Arc::clone(&self.webrtc_to_rtp_timing);
        let rtp_to_webrtc_transcoder = Arc::clone(&self.rtp_to_webrtc_transcoder);
        let rtp_to_webrtc_timing = Arc::clone(&self.rtp_to_webrtc_timing);
        let webrtc_video_payload_type = Arc::clone(&self.webrtc_video_payload_type);
        let rtp_video_payload_type = Arc::clone(&self.rtp_video_payload_type);

        tokio::spawn(async move {
            // Create fused receivers for both directions
            let mut webrtc_recv = Box::pin(webrtc_pc.recv());
            let mut rtp_recv = Box::pin(rtp_pc.recv());

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!(bridge_id = %bridge_id, "Bidirectional forwarder cancelled");
                        break;
                    }
                    // Handle WebRTC -> RTP direction
                    event = &mut webrtc_recv => {
                        match event {
                            Some(PeerConnectionEvent::Track(transceiver)) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    let is_video = transceiver.kind() == rustrtc::MediaKind::Video;
                                    let payload_types = transceiver
                                        .get_payload_map()
                                        .keys()
                                        .copied()
                                        .collect::<Vec<_>>();
                                    info!(
                                        bridge_id = %bridge_id,
                                        direction = "WebRTC->RTP",
                                        transceiver_id = transceiver.id(),
                                        kind = ?transceiver.kind(),
                                        mid = ?transceiver.mid(),
                                        receiver_ssrc = receiver.ssrc(),
                                        track_id = %track.id(),
                                        payload_types = ?payload_types,
                                        "Bridge received track event"
                                    );
                                    let sender = if is_video {
                                        let target_transceiver = rtp_pc
                                            .get_transceivers()
                                            .into_iter()
                                            .find(|t| t.kind() == rustrtc::MediaKind::Video);
                                        if let Some(target_transceiver) = target_transceiver {
                                            if let Some(existing_sender) = target_transceiver.sender() {
                                                let forwarding_track: Arc<dyn MediaStreamTrack> =
                                                    Arc::new(VideoForwardingTrack {
                                                        id: format!("{}-webrtc-to-rtp-video", bridge_id),
                                                        inner: track.clone(),
                                                        payload_type: Arc::clone(&rtp_video_payload_type),
                                                    });
                                                let sender = rustrtc::RtpSender::builder(
                                                    forwarding_track,
                                                    existing_sender.ssrc(),
                                                )
                                                .stream_id(existing_sender.stream_id().to_string())
                                                .params(existing_sender.params())
                                                .build();
                                                target_transceiver.set_sender(Some(sender.clone()));
                                                Self::spawn_pli_forwarder(
                                                    bridge_id.clone(),
                                                    sender,
                                                    track.clone(),
                                                    cancel_token.clone(),
                                                    "RTP PLI -> WebRTC source",
                                                );
                                                if let Err(e) = track.request_key_frame().await {
                                                    debug!(
                                                        bridge_id = %bridge_id,
                                                        source_track = %track.id(),
                                                        error = %e,
                                                        "Initial WebRTC video keyframe request failed"
                                                    );
                                                }
                                                debug!(
                                                    bridge_id = %bridge_id,
                                                    source_track = %track.id(),
                                                    target_ssrc = existing_sender.ssrc(),
                                                    "Wired WebRTC->RTP video forwarding track"
                                                );
                                            } else {
                                                warn!(bridge_id = %bridge_id, "RTP video transceiver has no sender for forwarding track");
                                            }
                                        } else {
                                            warn!(bridge_id = %bridge_id, "RTP video transceiver not found for forwarding track");
                                        }
                                        webrtc_recv = Box::pin(webrtc_pc.recv());
                                        continue;
                                    } else {
                                        rtp_send.clone()
                                    };
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        sender,
                                        Arc::clone(&rtp_output_mode),
                                        cancel_token.clone(),
                                        ForwardPath::new(LegTransport::WebRtc, LegTransport::Rtp),
                                        Arc::clone(&w2r_stats),
                                        if !is_video { recorder.clone() } else { None },
                                        if !is_video { Some(RecLeg::A) } else { None },
                                        Arc::clone(&dtmf_sink),
                                        Some(Arc::clone(&webrtc_to_rtp_transcoder)),
                                        Some(Arc::clone(&webrtc_to_rtp_timing)),
                                    ).await;
                                } else {
                                    warn!(
                                        bridge_id = %bridge_id,
                                        direction = "WebRTC->RTP",
                                        transceiver_id = transceiver.id(),
                                        kind = ?transceiver.kind(),
                                        mid = ?transceiver.mid(),
                                        "Bridge received track event without receiver"
                                    );
                                }
                                webrtc_recv = Box::pin(webrtc_pc.recv());
                            }
                            Some(_) => {
                                webrtc_recv = Box::pin(webrtc_pc.recv());
                            }
                            None => {
                                debug!(bridge_id = %bridge_id, "WebRTC PeerConnection closed");
                                break;
                            }
                        }
                    }
                    // Handle RTP -> WebRTC direction
                    event = &mut rtp_recv => {
                        match event {
                            Some(PeerConnectionEvent::Track(transceiver)) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    let is_video = transceiver.kind() == rustrtc::MediaKind::Video;
                                    let payload_types = transceiver
                                        .get_payload_map()
                                        .keys()
                                        .copied()
                                        .collect::<Vec<_>>();
                                    info!(
                                        bridge_id = %bridge_id,
                                        direction = "RTP->WebRTC",
                                        transceiver_id = transceiver.id(),
                                        kind = ?transceiver.kind(),
                                        mid = ?transceiver.mid(),
                                        receiver_ssrc = receiver.ssrc(),
                                        track_id = %track.id(),
                                        payload_types = ?payload_types,
                                        "Bridge received track event"
                                    );
                                    let sender = if is_video {
                                        let target_transceiver = webrtc_pc
                                            .get_transceivers()
                                            .into_iter()
                                            .find(|t| t.kind() == rustrtc::MediaKind::Video);
                                        if let Some(target_transceiver) = target_transceiver {
                                            if let Some(existing_sender) = target_transceiver.sender() {
                                                let forwarding_track: Arc<dyn MediaStreamTrack> =
                                                    Arc::new(VideoForwardingTrack {
                                                        id: format!("{}-rtp-to-webrtc-video", bridge_id),
                                                        inner: track.clone(),
                                                        payload_type: Arc::clone(&webrtc_video_payload_type),
                                                    });
                                                let sender = rustrtc::RtpSender::builder(
                                                    forwarding_track,
                                                    existing_sender.ssrc(),
                                                )
                                                .stream_id(existing_sender.stream_id().to_string())
                                                .params(existing_sender.params())
                                                .build();
                                                target_transceiver.set_sender(Some(sender.clone()));
                                                Self::spawn_pli_forwarder(
                                                    bridge_id.clone(),
                                                    sender,
                                                    track.clone(),
                                                    cancel_token.clone(),
                                                    "WebRTC PLI -> RTP source",
                                                );
                                                if let Err(e) = track.request_key_frame().await {
                                                    debug!(
                                                        bridge_id = %bridge_id,
                                                        source_track = %track.id(),
                                                        error = %e,
                                                        "Initial RTP video keyframe request failed"
                                                    );
                                                }
                                                debug!(
                                                    bridge_id = %bridge_id,
                                                    source_track = %track.id(),
                                                    target_ssrc = existing_sender.ssrc(),
                                                    "Wired RTP->WebRTC video forwarding track"
                                                );
                                            } else {
                                                warn!(bridge_id = %bridge_id, "WebRTC video transceiver has no sender for forwarding track");
                                            }
                                        } else {
                                            warn!(bridge_id = %bridge_id, "WebRTC video transceiver not found for forwarding track");
                                        }
                                        rtp_recv = Box::pin(rtp_pc.recv());
                                        continue;
                                    } else {
                                        webrtc_send.clone()
                                    };
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        sender,
                                        Arc::clone(&webrtc_output_mode),
                                        cancel_token.clone(),
                                        ForwardPath::new(LegTransport::Rtp, LegTransport::WebRtc),
                                        Arc::clone(&r2w_stats),
                                        if !is_video { recorder.clone() } else { None },
                                        if !is_video { Some(RecLeg::B) } else { None },
                                        Arc::clone(&dtmf_sink),
                                        Some(Arc::clone(&rtp_to_webrtc_transcoder)),
                                        Some(Arc::clone(&rtp_to_webrtc_timing)),
                                    ).await;
                                } else {
                                    warn!(
                                        bridge_id = %bridge_id,
                                        direction = "RTP->WebRTC",
                                        transceiver_id = transceiver.id(),
                                        kind = ?transceiver.kind(),
                                        mid = ?transceiver.mid(),
                                        "Bridge received track event without receiver"
                                    );
                                }
                                rtp_recv = Box::pin(rtp_pc.recv());
                            }
                            Some(_) => {
                                rtp_recv = Box::pin(rtp_pc.recv());
                            }
                            None => {
                                debug!(bridge_id = %bridge_id, "RTP PeerConnection closed");
                                break;
                            }
                        }
                    }
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_forward_loop(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<AsyncMutex<Option<MediaSender>>>,
        output_mode: Arc<AtomicU8>,
        cancel_token: CancellationToken,
        path: ForwardPath,
        leg_stats: Arc<LegStats>,
        recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
        recorder_leg: Option<RecLeg>,
        dtmf_sink: Arc<parking_lot::RwLock<Option<BridgeDtmfSink>>>,
        transcoder: Option<Arc<parking_lot::RwLock<Option<Transcoder>>>>,
        transcoder_timing: Option<Arc<parking_lot::RwLock<Option<RtpTiming>>>>,
    ) {
        // Get the sender channel from weak pointer
        let sender = if let Some(strong) = sender_weak.upgrade() {
            let guard = strong.lock().await;
            guard.clone()
        } else {
            warn!(bridge_id = %bridge_id, direction = %path, "Sender channel no longer available");
            return;
        };

        if sender.is_none() {
            warn!(bridge_id = %bridge_id, direction = %path, "No sender channel available — video/audio sender was not configured");
            return;
        }
        let sender = sender.unwrap();

        let is_video = track.kind() == MediaKind::Video;
        let mut dtmf_detector = BridgeDtmfDetector::default();
        let mut packet_count: u64 = 0;
        // Last seen RTP sequence number for loss estimation
        let mut last_seq: Option<u16> = None;
        // Per-second stats for video diagnostics
        let mut stats_packets: u64 = 0;
        let mut stats_bytes: u64 = 0;
        let mut stats_last_pt: Option<u8> = None;
        let mut stats_last_port: Option<u16> = None;
        let mut stats_interval = tokio::time::interval(std::time::Duration::from_secs(1));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        'outer: loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    break;
                }
                _ = stats_interval.tick(), if is_video && packet_count > 0 => {
                    trace!(
                        bridge_id = %bridge_id,
                        direction = %path,
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
                sample_result = track.recv() => {
                    match sample_result {
                        Ok(sample) => {
                            packet_count += 1;
                            if !is_video {
                                Self::observe_dtmf_sample(
                                    &dtmf_sink,
                                    path.source_endpoint(),
                                    &sample,
                                    &mut dtmf_detector,
                                );
                            }
                            let mode = output_mode.load(Ordering::Acquire);
                            if !is_video && mode != BRIDGE_OUTPUT_PEER {
                                if packet_count == 1 {
                                    debug!(
                                        bridge_id = %bridge_id,
                                        direction = %path,
                                        output_source = output_mode_name(mode),
                                        "Peer media suppressed by bridge output source"
                                    );
                                }
                                continue;
                            }
                            if packet_count == 1 {
                                debug!(bridge_id = %bridge_id, direction = %path, kind = ?sample.kind(), "First media sample forwarded");
                            }
                            // Update per-direction stats
                            let (sample_bytes, sample_seq) = match &sample {
                                MediaSample::Audio(a) => (a.data.len() as u64, a.sequence_number),
                                MediaSample::Video(v) => (v.data.len() as u64, v.sequence_number),
                            };
                            leg_stats.packets.fetch_add(1, Ordering::Relaxed);
                            leg_stats.bytes.fetch_add(sample_bytes, Ordering::Relaxed);
                            if let Some(seq) = sample_seq {
                                if let Some(prev) = last_seq {
                                    // wrapping gap: positive implies missing packets
                                    let gap = seq.wrapping_sub(prev.wrapping_add(1));
                                    if gap > 0 && gap < 512 {
                                        leg_stats.lost.fetch_add(gap as u64, Ordering::Relaxed);
                                    }
                                }
                                last_seq = Some(seq);
                            }
                            // For video samples, keep the RTP payload opaque. This bridge
                            // does not transcode video, so VP8/AV1/H264 payload bytes must
                            // not be parsed or repacketized here.
                            //
                            // Strip RTP header extensions: Chrome WebRTC packets carry
                            //    abs-send-time / rtp-stream-id extensions.  Forwarding these to
                            //    Linphone's plain RTP causes Linphone's decoder to see unexpected
                            //    extension bytes, potentially rejecting packets (black screen).
                            let samples_to_send: Vec<MediaSample> = match sample {
                                MediaSample::Audio(mut a) => {
                                    if path.should_strip_webrtc_audio_metadata() {
                                        a.header_extension = None;
                                        a.raw_packet = None;
                                        a.marker = false;
                                    }
                                    // Apply per-packet transcoding if the codec differs
                                    // between the ingress and egress legs.  The Transcoder is
                                    // set dynamically by sip_session when it detects a mismatch
                                    // (e.g. caller uses G.729, agent uses PCMU).
                                    let transcoded = transcoder.as_ref().and_then(|tx_arc| {
                                        let mut guard = tx_arc.write();
                                        let tx = guard.as_mut()?;
                                        // Telephone-event (DTMF) MUST NOT go through
                                        // audio transcoding — it has its own codec format
                                        // that the G.729/PCMU decoder cannot interpret.
                                        let is_dtmf = dtmf_sink
                                            .read()
                                            .as_ref()
                                            .filter(|s| s.endpoint == path.source_endpoint())
                                            .map_or(false, |s| {
                                                a.payload_type
                                                    .is_some_and(|pt| s.payload_types.contains(&pt))
                                            });
                                        if is_dtmf {
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
                                        if let Some(ref timing_arc) = transcoder_timing {
                                            if let Some(ref mut timing) = *timing_arc.write() {
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
                                    match transcoded {
                                        Some(ts) => vec![ts],
                                        None => vec![MediaSample::Audio(a)],
                                    }
                                }
                                MediaSample::Video(mut v) => {
                                    if is_video {
                                        if matches!(v.payload_type, Some(pt) if pt < 96) {
                                            debug!(
                                                bridge_id = %bridge_id,
                                                direction = %path,
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
                                            // Strip WebRTC-specific RTP header extensions
                                            // (abs-send-time, rtp-stream-id, etc.).
                                            // sdes:mid is now auto-injected by rustrtc's RtpSender
                                            // when update_extmap detects the BUNDLE negotiation.
                                            v.header_extension = None;
                                            v.raw_packet = None;
                                            // Clear CSRCs – Chrome conference CSRCs would
                                            // shift the RTP payload offset in Linphone's parser.
                                            v.csrcs.clear();
                                            if matches!(
                                                (path.from, path.to),
                                                (LegTransport::WebRtc, LegTransport::Rtp)
                                            ) {
                                                v.sequence_number = None;
                                            }

                                            vec![MediaSample::Video(v)]
                                        }
                                    } else {
                                        v.sequence_number = None;
                                        v.payload_type = None;
                                        vec![MediaSample::Video(v)]
                                    }
                                }
                            };
                            // Write audio samples to recorder (non-blocking: skip on lock contention)
                            if let (Some(rec), Some(leg)) = (&recorder, recorder_leg) {
                                for s in &samples_to_send {
                                    if matches!(s, MediaSample::Audio(_))
                                        && let Some(mut guard) = rec.try_write()
                                            && let Some(r) = guard.as_mut() {
                                                let _ = r.write_sample(leg, s, None, None, None::<AudioCodecType>);
                                            }
                                }
                            }
                            for sample in samples_to_send {
                                // Forward the sample using try_send first to reduce Tokio scheduling overhead
                                match sender.try_send(sample.clone()) {
                                    Ok(()) => {}
                                    Err(MediaError::WouldBlock) => {
                                        // Fallback to async send only when channel is full
                                        if let Err(e) = sender.send(sample).await {
                                            warn!(bridge_id = %bridge_id, direction = %path, error = %e, "Failed to forward media sample");
                                            break 'outer;
                                        }
                                    }
                                    Err(e) => {
                                        leg_stats.dropped.fetch_add(1, Ordering::Relaxed);
                                        warn!(bridge_id = %bridge_id, direction = %path, error = ?e, "Failed to forward media sample (kind mismatch or closed)");
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            debug!(bridge_id = %bridge_id, direction = %path, error = %e, "Track recv ended");
                            break;
                        }
                    }
                }
            }
        }
    }

    fn observe_dtmf_sample(
        dtmf_sink: &Arc<parking_lot::RwLock<Option<BridgeDtmfSink>>>,
        endpoint: BridgeEndpoint,
        sample: &MediaSample,
        detector: &mut BridgeDtmfDetector,
    ) {
        let Some(sink) = dtmf_sink.read().clone() else {
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

        debug!(
            rtp_ts = frame.rtp_timestamp,
            data_len = frame.data.len(),
            first_byte = frame.data.first().copied().unwrap_or(0),
            "DTMF observe: PT matched, calling detector"
        );

        if let Some(digit) = detector.observe(&frame.data, frame.rtp_timestamp) {
            debug!(digit = %digit, "DTMF observe: digit detected via RFC2833");
            (sink.handler)(digit);
        }
    }

    /// Spawn a task that subscribes to PLI/FIR RTCP on `sender` and forwards them as
    /// `request_key_frame()` calls on `source_track`.
    fn spawn_pli_forwarder(
        bridge_id: String,
        sender: Arc<RtpSender>,
        source_track: Arc<dyn MediaStreamTrack>,
        cancel_token: CancellationToken,
        label: &'static str,
    ) {
        let mut rtcp_rx = sender.subscribe_rtcp();
        tokio::spawn(async move {
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
        });
    }

    /// Forward media from a track to a sender channel.
    /// Spawns a sub-task; used by the PC-event-driven start paths.
    #[allow(clippy::too_many_arguments)]
    async fn forward_track_to_sender(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<AsyncMutex<Option<MediaSender>>>,
        output_mode: Arc<AtomicU8>,
        cancel_token: CancellationToken,
        path: ForwardPath,
        leg_stats: Arc<LegStats>,
        recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
        recorder_leg: Option<RecLeg>,
        dtmf_sink: Arc<parking_lot::RwLock<Option<BridgeDtmfSink>>>,
        transcoder: Option<Arc<parking_lot::RwLock<Option<Transcoder>>>>,
        transcoder_timing: Option<Arc<parking_lot::RwLock<Option<RtpTiming>>>>,
    ) {
        tokio::spawn(async move {
            Self::run_forward_loop(
                bridge_id,
                track,
                sender_weak,
                output_mode,
                cancel_token,
                path,
                leg_stats,
                recorder,
                recorder_leg,
                dtmf_sink,
                transcoder,
                transcoder_timing,
            )
            .await;
        });
    }

    /// Get the WebRTC PeerConnection
    pub fn webrtc_pc(&self) -> &PeerConnection {
        &self.webrtc_pc
    }

    /// Get the RTP PeerConnection
    pub fn rtp_pc(&self) -> &PeerConnection {
        &self.rtp_pc
    }

    /// Stop the bridge (async — waits for forwarding tasks to finish)
    pub async fn stop(&self) {
        debug!(bridge_id = %self.id, "Stopping bridge");
        self.cancel_token.cancel();
        self.webrtc_pc.close();
        self.rtp_pc.close();

        // Wait for tasks to complete
        let mut tasks = self.bridge_tasks.lock().await;
        for task in tasks.drain(..) {
            let _ = task.await;
        }
    }

    /// Close both PeerConnections without waiting for tasks.
    /// Used by Drop — forwarding tasks will exit on their own via cancel_token.
    fn close_sync(&self) {
        self.cancel_token.cancel();
        self.webrtc_pc.close();
        self.rtp_pc.close();
    }
}

impl Drop for BridgePeer {
    fn drop(&mut self) {
        debug!(bridge_id = %self.id, "BridgePeer dropping, closing PeerConnections");
        self.close_sync();
    }
}

/// Builder for creating BridgePeer instances
pub struct BridgePeerBuilder {
    bridge_id: String,
    webrtc_config: Option<rustrtc::RtcConfiguration>,
    rtp_config: Option<rustrtc::RtcConfiguration>,
    rtp_port_range: (u16, u16),
    enable_latching: bool,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    webrtc_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    rtp_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    webrtc_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    rtp_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    webrtc_sender_codec: Option<RtpCodecParameters>,
    rtp_sender_codec: Option<RtpCodecParameters>,
    rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode,
    ice_servers: Vec<IceServer>,
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
}

impl BridgePeerBuilder {
    pub fn new(bridge_id: String) -> Self {
        Self {
            bridge_id,
            webrtc_config: None,
            rtp_config: None,
            rtp_port_range: (20000, 30000),
            enable_latching: false,
            external_ip: None,
            bind_ip: None,
            webrtc_audio_capabilities: None,
            rtp_audio_capabilities: None,
            webrtc_video_capabilities: None,
            rtp_video_capabilities: None,
            webrtc_sender_codec: None,
            rtp_sender_codec: None,
            rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode::LegacySip,
            ice_servers: Vec::new(),
            recorder: None,
        }
    }

    pub fn with_webrtc_config(mut self, config: rustrtc::RtcConfiguration) -> Self {
        self.webrtc_config = Some(config);
        self
    }

    pub fn with_rtp_config(mut self, config: rustrtc::RtcConfiguration) -> Self {
        self.rtp_config = Some(config);
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
    pub fn with_webrtc_audio_capabilities(
        mut self,
        caps: Vec<rustrtc::config::AudioCapability>,
    ) -> Self {
        self.webrtc_audio_capabilities = Some(caps);
        self
    }

    /// Set audio capabilities for the RTP side PeerConnection.
    /// Controls which codecs appear in SDP offers/answers.
    pub fn with_rtp_audio_capabilities(
        mut self,
        caps: Vec<rustrtc::config::AudioCapability>,
    ) -> Self {
        self.rtp_audio_capabilities = Some(caps);
        self
    }

    /// Set video capabilities for the WebRTC side PeerConnection.
    /// Controls which video codecs appear in SDP offers/answers.
    pub fn with_webrtc_video_capabilities(
        mut self,
        caps: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        self.webrtc_video_capabilities = Some(caps);
        self
    }

    /// Set video capabilities for the RTP side PeerConnection.
    /// Controls which video codecs appear in SDP offers/answers.
    pub fn with_rtp_video_capabilities(
        mut self,
        caps: Vec<rustrtc::config::VideoCapability>,
    ) -> Self {
        self.rtp_video_capabilities = Some(caps);
        self
    }

    /// Set sender codec parameters for both bridge sides.
    /// These are used by the sample track (RtpSender) on each side.
    pub fn with_sender_codecs(
        mut self,
        webrtc: RtpCodecParameters,
        rtp: RtpCodecParameters,
    ) -> Self {
        self.webrtc_sender_codec = Some(webrtc);
        self.rtp_sender_codec = Some(rtp);
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
    pub fn with_recorder(mut self, recorder: Arc<parking_lot::RwLock<Option<Recorder>>>) -> Self {
        self.recorder = Some(recorder);
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
        // created lazily by setup_bridge_with_codecs (when webrtc_video_codec
        // is Some) or dynamically by add_video_track().
        let webrtc_video = Self::resolve_video_caps(&self.webrtc_video_capabilities);
        let rtp_video = Self::resolve_video_caps(&self.rtp_video_capabilities);

        let webrtc_media_caps =
            self.webrtc_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: webrtc_video,
                    application: None,
                });

        let webrtc_config = self
            .webrtc_config
            .unwrap_or_else(|| rustrtc::RtcConfiguration {
                transport_mode: TransportMode::WebRtc,
                external_ip: self.external_ip.clone(),
                media_capabilities: webrtc_media_caps,
                ssrc_start: rand::random::<u32>(),
                sdp_compatibility: rustrtc::config::SdpCompatibilityMode::Standard,
                ice_servers: self.ice_servers,
                ..Default::default()
            });

        let rtp_media_caps =
            self.rtp_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: rtp_video,
                    application: None,
                });

        let rtp_config = self
            .rtp_config
            .unwrap_or_else(|| rustrtc::RtcConfiguration {
                transport_mode: TransportMode::Rtp,
                rtp_start_port: Some(self.rtp_port_range.0),
                rtp_end_port: Some(self.rtp_port_range.1),
                enable_latching: self.enable_latching,
                external_ip: self.external_ip,
                bind_ip: self.bind_ip,
                media_capabilities: rtp_media_caps,
                ssrc_start: rand::random::<u32>(),
                sdp_compatibility: self.rtp_sdp_compatibility,
                ..Default::default()
            });

        let webrtc_pc = PeerConnection::new(webrtc_config);
        let rtp_pc = PeerConnection::new(rtp_config);

        let mut bridge = BridgePeer::new(self.bridge_id, webrtc_pc, rtp_pc);
        bridge.webrtc_sender_codec = self.webrtc_sender_codec;
        bridge.rtp_sender_codec = self.rtp_sender_codec;
        bridge.recorder = self.recorder;

        // Store video codec params for setup_bridge to create video senders
        bridge.webrtc_video_codec = self
            .webrtc_video_capabilities
            .as_ref()
            .and_then(|caps| caps.first())
            .map(|cap| RtpCodecParameters {
                payload_type: cap.payload_type,
                clock_rate: cap.clock_rate,
                channels: 0,
            });
        bridge.rtp_video_codec = self
            .rtp_video_capabilities
            .as_ref()
            .and_then(|caps| caps.first())
            .map(|cap| RtpCodecParameters {
                payload_type: cap.payload_type,
                clock_rate: cap.clock_rate,
                channels: 0,
            });

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
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(path, spec).map_err(|e| anyhow::anyhow!("WavWriter: {e}"))?;
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

    #[tokio::test]
    async fn test_bridge_peer_creation() {
        let bridge = BridgePeerBuilder::new("test-bridge".to_string())
            .with_rtp_port_range(25000, 25100)
            .build();

        // setup_bridge() creates the transceivers via add_track()
        bridge.setup_bridge().await.unwrap();

        // Generate offers for both PCs
        let webrtc_offer = bridge.webrtc_pc().create_offer().await;
        let rtp_offer = bridge.rtp_pc().create_offer().await;

        assert!(webrtc_offer.is_ok(), "WebRTC should create offer");
        assert!(rtp_offer.is_ok(), "RTP should create offer");

        // Set local descriptions
        let _ = bridge
            .webrtc_pc()
            .set_local_description(webrtc_offer.unwrap());
        let _ = bridge.rtp_pc().set_local_description(rtp_offer.unwrap());

        // Verify both PCs have local descriptions
        let webrtc_local = bridge.webrtc_pc().local_description();
        let rtp_local = bridge.rtp_pc().local_description();

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
        let bridge_rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge
            .rtp_pc()
            .set_local_description(bridge_rtp_offer)
            .unwrap();

        // Create external RTP endpoint (simulating SIP callee)
        let rtp_callee = RtpTrackBuilder::new("rtp-callee".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(26100, 26200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Get bridge's RTP offer SDP and have callee handshake with it
        let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
        let callee_answer = rtp_callee.handshake(bridge_rtp_sdp).await.unwrap();

        // Bridge RTP side sets remote description from callee
        let rtp_leg_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .rtp_pc()
            .set_remote_description(rtp_leg_desc)
            .await
            .unwrap();

        // Setup WebRTC side for completeness
        let bridge_webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        bridge
            .webrtc_pc()
            .set_local_description(bridge_webrtc_offer)
            .unwrap();

        // Start bridge forwarding
        bridge.start_bridge().await;

        // Verify bridge is properly configured
        let webrtc_pc = bridge.webrtc_pc();
        let rtp_pc = bridge.rtp_pc();

        // Both should have local descriptions
        assert!(
            webrtc_pc.local_description().is_some(),
            "WebRTC should have local description"
        );
        assert!(
            rtp_pc.local_description().is_some(),
            "RTP should have local description"
        );
        assert!(
            rtp_pc.remote_description().is_some(),
            "RTP should have remote description"
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
            .replace_output_with_file(BridgeEndpoint::Rtp, &track)
            .await
            .unwrap();
        drop(track);

        let rtp_track = bridge
            .get_rtp_track()
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

    /// Test that BridgePeer correctly handles SDP format differences
    #[tokio::test]
    async fn test_bridge_sdp_format_differences() {
        let bridge = BridgePeerBuilder::new("test-bridge-sdp".to_string())
            .with_rtp_port_range(27000, 27100)
            .build();

        // setup_bridge() creates transceivers via add_track()
        bridge.setup_bridge().await.unwrap();

        // Generate offers
        let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();

        bridge
            .webrtc_pc()
            .set_local_description(webrtc_offer)
            .unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

        let webrtc_sdp = bridge
            .webrtc_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

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
        let bridge_webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        let bridge_rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge
            .webrtc_pc()
            .set_local_description(bridge_webrtc_offer.clone())
            .unwrap();
        bridge
            .rtp_pc()
            .set_local_description(bridge_rtp_offer.clone())
            .unwrap();

        // Step 5: Bridge WebRTC side SDP (would be sent to WebRTC caller)
        let webrtc_sdp = bridge
            .webrtc_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(webrtc_sdp.contains("SAVPF"), "WebRTC side should use SAVPF");
        assert!(
            webrtc_sdp.contains("fingerprint"),
            "WebRTC side should have DTLS fingerprint"
        );

        // Step 6: Bridge RTP side SDP (would be sent to RTP callee, possibly converted)
        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
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
            .rtp_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 9: Start bridge forwarding
        bridge.start_bridge().await;

        // Verify connections are established
        assert!(
            bridge.webrtc_pc().local_description().is_some(),
            "WebRTC should have local description"
        );
        assert!(
            bridge.rtp_pc().local_description().is_some(),
            "RTP should have local description"
        );
        assert!(
            bridge.rtp_pc().remote_description().is_some(),
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
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

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
            .webrtc_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.webrtc_pc().create_answer().await.unwrap();
        bridge.webrtc_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .webrtc_pc()
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
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();
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
        let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
        assert!(
            bridge_rtp_sdp.contains("RTP/AVP"),
            "Bridge RTP offer should be plain RTP"
        );

        let callee_answer = rtp_callee.handshake(bridge_rtp_sdp).await.unwrap();

        // Step 5: Set callee's answer on bridge's RTP side
        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .rtp_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 6: Set caller's offer on bridge's WebRTC side and create answer
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .webrtc_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.webrtc_pc().create_answer().await.unwrap();
        bridge.webrtc_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .webrtc_pc()
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
            bridge.rtp_pc().remote_description().is_some(),
            "RTP side should have remote"
        );
        assert!(
            bridge.webrtc_pc().local_description().is_some(),
            "WebRTC side should have local"
        );
        assert!(
            bridge.webrtc_pc().remote_description().is_some(),
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
        let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        bridge
            .webrtc_pc()
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
            .webrtc_pc()
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
            .webrtc_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // Step 6: Set caller's RTP offer on bridge's RTP side and create answer
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .rtp_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.rtp_pc().create_answer().await.unwrap();
        bridge.rtp_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

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
            bridge.rtp_pc().remote_description().is_some(),
            "RTP side should have remote"
        );
        assert!(
            bridge.rtp_pc().local_description().is_some(),
            "RTP side should have local"
        );
        assert!(
            bridge.webrtc_pc().remote_description().is_some(),
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
            .with_rtp_audio_capabilities(vec![
                AudioCapability::pcmu(),
                AudioCapability::pcma(),
                AudioCapability::telephone_event(),
            ])
            .build();

        bridge.setup_bridge().await.unwrap();

        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

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
            .with_webrtc_audio_capabilities(vec![
                AudioCapability::opus(),
                AudioCapability::pcmu(),
                AudioCapability::telephone_event(),
            ])
            .build();

        bridge.setup_bridge().await.unwrap();

        let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        bridge
            .webrtc_pc()
            .set_local_description(webrtc_offer)
            .unwrap();

        let webrtc_sdp = bridge
            .webrtc_pc()
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
            .with_rtp_audio_capabilities(vec![
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
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();
        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

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

        // Build bridge codec lists from caller SDP + allow_codecs=[PCMU]
        let codec_lists = MediaNegotiator::build_bridge_codec_lists(
            &caller_offer,
            true,  // caller is WebRTC
            false, // callee is RTP
            &[CodecType::PCMU, CodecType::TelephoneEvent],
        );

        // Build bridge with computed capabilities
        let webrtc_caps: Vec<_> = codec_lists
            .caller_side
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();
        let rtp_caps: Vec<_> = codec_lists
            .callee_side
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();

        let webrtc_sender = codec_lists
            .caller_side
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();
        let rtp_sender = codec_lists
            .callee_side
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();

        let bridge = BridgePeerBuilder::new("e2e-pcmu-only".to_string())
            .with_rtp_port_range(35100, 35200)
            .with_webrtc_audio_capabilities(webrtc_caps)
            .with_rtp_audio_capabilities(rtp_caps)
            .with_sender_codecs(webrtc_sender, rtp_sender)
            .build();

        bridge.setup_bridge().await.unwrap();

        // RTP side offers to callee
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

        let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
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
            .rtp_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // WebRTC side answers caller
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .webrtc_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let answer = bridge.webrtc_pc().create_answer().await.unwrap();
        bridge.webrtc_pc().set_local_description(answer).unwrap();

        let answer_sdp = bridge
            .webrtc_pc()
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

    /// E2E: RTP caller (G729+PCMU) → Bridge → WebRTC callee
    /// G729 is not in WebRTC supported set, so WebRTC callee side should only have PCMU.
    #[tokio::test]
    async fn test_bridge_e2e_rtp_to_webrtc_g729_dropped() {
        use crate::media::negotiate::MediaNegotiator;

        // Create RTP caller offering G729 + PCMU
        let caller = RtpTrackBuilder::new("rtp-caller-g729".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(36000, 36100)
            .with_codec_preference(vec![CodecType::G729, CodecType::PCMU])
            .build();
        let caller_offer = caller.local_description().await.unwrap();

        // Build bridge codec lists
        let codec_lists = MediaNegotiator::build_bridge_codec_lists(
            &caller_offer,
            false, // caller is RTP
            true,  // callee is WebRTC
            &[CodecType::G729, CodecType::PCMU, CodecType::TelephoneEvent],
        );

        // G729 should be on caller side (RTP supports it) but NOT on callee side (WebRTC doesn't)
        assert!(
            codec_lists
                .caller_side
                .iter()
                .any(|c| c.codec == CodecType::G729),
            "G729 on RTP caller side"
        );
        assert!(
            !codec_lists
                .callee_side
                .iter()
                .any(|c| c.codec == CodecType::G729),
            "G729 dropped on WebRTC callee side"
        );

        let webrtc_caps: Vec<_> = codec_lists
            .callee_side
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();
        let rtp_caps: Vec<_> = codec_lists
            .caller_side
            .iter()
            .filter_map(|c| c.to_audio_capability())
            .collect();

        // RTP side sender: first non-DTMF from caller side
        let rtp_sender = codec_lists
            .caller_side
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();
        let webrtc_sender = codec_lists
            .callee_side
            .iter()
            .find(|c| !c.is_dtmf())
            .map(|c| c.to_params())
            .unwrap();

        let bridge = BridgePeerBuilder::new("e2e-g729-drop".to_string())
            .with_rtp_port_range(36100, 36200)
            .with_webrtc_audio_capabilities(webrtc_caps)
            .with_rtp_audio_capabilities(rtp_caps)
            .with_sender_codecs(webrtc_sender, rtp_sender)
            .build();

        bridge.setup_bridge().await.unwrap();

        // WebRTC callee side creates offer
        let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        bridge
            .webrtc_pc()
            .set_local_description(webrtc_offer)
            .unwrap();

        let bridge_webrtc_sdp = bridge
            .webrtc_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();
        assert!(
            bridge_webrtc_sdp.contains("SAVPF"),
            "WebRTC side must use SAVPF"
        );
        assert!(
            !bridge_webrtc_sdp.contains("G729"),
            "WebRTC side must NOT offer G729"
        );

        // WebRTC callee answers
        let callee = RtpTrackBuilder::new("webrtc-callee".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(36200, 36300)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let callee_answer = callee.handshake(bridge_webrtc_sdp).await.unwrap();

        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .webrtc_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        // RTP side answers caller
        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .rtp_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();

        let rtp_answer = bridge.rtp_pc().create_answer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_answer).unwrap();

        let rtp_answer_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
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
        sample: tokio::sync::Mutex<Option<MediaSample>>,
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
            let mut guard = self.sample.lock().await;
            if let Some(s) = guard.take() {
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
        assert!(bridge.rtp_to_webrtc_transcoder.read().is_none());
        assert!(bridge.webrtc_to_rtp_transcoder.read().is_none());

        // Set RTP→WebRTC transcoder (G.729→PCMU)
        bridge.set_transcoder(BridgeEndpoint::Rtp, CodecType::G729, CodecType::PCMU, 0);
        assert!(
            bridge.rtp_to_webrtc_transcoder.read().is_some(),
            "RTP→WebRTC transcoder should be set"
        );

        // Set WebRTC→RTP transcoder (PCMU→G.729)
        bridge.set_transcoder(BridgeEndpoint::WebRtc, CodecType::PCMU, CodecType::G729, 18);
        assert!(
            bridge.webrtc_to_rtp_transcoder.read().is_some(),
            "WebRTC→RTP transcoder should be set"
        );

        // Clear RTP→WebRTC
        bridge.clear_transcoder(BridgeEndpoint::Rtp);
        assert!(
            bridge.rtp_to_webrtc_transcoder.read().is_none(),
            "RTP→WebRTC should be cleared"
        );
        assert!(
            bridge.webrtc_to_rtp_transcoder.read().is_some(),
            "WebRTC→RTP should still be set"
        );

        // Clear all
        bridge.clear_transcoder(BridgeEndpoint::WebRtc);
        assert!(bridge.webrtc_to_rtp_transcoder.read().is_none());
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
            sample: tokio::sync::Mutex::new(Some(MediaSample::Audio(input_frame))),
        });

        // Output channel – run_forward_loop writes transcoded samples here
        // sample_track returns (source, track_a, feedback_rx).
        // track_a receives what source sends.
        let (output_tx, output_track, _) = sample_track(MediaKind::Audio, 10);
        let sender_arc = Arc::new(AsyncMutex::new(Some(output_tx)));
        let sender_weak = Arc::downgrade(&sender_arc);

        // Configure G.729 → PCMU transcoder
        let transcoder = Arc::new(parking_lot::RwLock::new(Some(Transcoder::new(
            CodecType::G729,
            CodecType::PCMU,
            0, // PCMU PT
        ))));
        let timing: Arc<parking_lot::RwLock<Option<RtpTiming>>> =
            Arc::new(parking_lot::RwLock::new(None));

        let cancel = CancellationToken::new();
        let stats: Arc<LegStats> = LegStats::new();
        let dtmf: Arc<parking_lot::RwLock<Option<BridgeDtmfSink>>> =
            Arc::new(parking_lot::RwLock::new(None));

        // Run the forwarding loop in a background task
        let task_handle = {
            let c = cancel.clone();
            let mt = mock_track.clone();
            let sw = sender_weak.clone();
            let tr = Some(transcoder);
            let ti = Some(timing);
            let st = stats;
            let ds = dtmf;
            tokio::spawn(async move {
                BridgePeer::run_forward_loop(
                    "test".to_string(),
                    mt,
                    sw,
                    Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
                    c,
                    ForwardPath::new(LegTransport::Rtp, LegTransport::WebRtc),
                    st,
                    None, // no recorder
                    None, // no recorder leg
                    ds,
                    tr,
                    ti,
                )
                .await;
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

    /// Verify Opus → PCMU transcoding via Transcoder produces correct output.
    #[tokio::test]
    #[cfg(feature = "opus")]
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
        assert!(is_stereo, "Opus encoder should produce stereo packet (TOC bit 2 set)");

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
        assert_eq!(output2.data.len(), 160, "Second call should also produce 160 bytes");
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
    #[cfg(feature = "opus")]
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
    #[cfg(feature = "opus")]
    async fn test_transcoder_opus_to_pcmu_roundtrip_quality() {
        use audio_codec::{create_encoder, create_decoder};

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
            let energy: f64 = frame.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
                / frame.len() as f64;
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
            sample: tokio::sync::Mutex::new(Some(MediaSample::Audio(input_frame))),
        });

        let (output_tx, output_track, _) = sample_track(MediaKind::Audio, 10);
        let sender_arc = Arc::new(AsyncMutex::new(Some(output_tx)));
        let sender_weak = Arc::downgrade(&sender_arc);

        let cancel = CancellationToken::new();
        let stats = LegStats::new();
        let dtmf = Arc::new(parking_lot::RwLock::new(None));

        let task_handle = {
            let c = cancel.clone();
            let mt = mock_track.clone();
            let sw = sender_weak.clone();
            let st = stats;
            let ds = dtmf;
            tokio::spawn(async move {
                BridgePeer::run_forward_loop(
                    "test-passthrough".to_string(),
                    mt,
                    sw,
                    Arc::new(AtomicU8::new(BRIDGE_OUTPUT_PEER)),
                    c,
                    ForwardPath::new(LegTransport::Rtp, LegTransport::WebRtc),
                    st,
                    None,
                    None,
                    ds,
                    None, // no transcoder
                    None, // no timing
                )
                .await;
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
}
