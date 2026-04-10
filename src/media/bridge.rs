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

use crate::media::{RecorderOption, Track};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use rustrtc::{
    PeerConnection, PeerConnectionEvent, RtpCodecParameters, RtpSender, TransportMode,
    media::{
        MediaError, MediaKind, MediaSample, MediaStreamTrack, SampleStreamSource,
        SampleStreamTrack, VideoFrame,
    },
    rtp::RtcpPacket,
};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

/// Type alias for the sender channel
pub type MediaSender = SampleStreamSource;

/// Maximum RTP payload size for H264 fragmentation.
/// Standard Ethernet MTU (1500) minus IP (20) + UDP (8) + SRTP overhead (~20) + RTP (12) + FU-A headers (2).
/// Using 1200 bytes gives comfortable headroom for DTLS/SRTP.
const H264_MTU: usize = 1200;

/// Fragment a reassembled H264 NAL unit into FU-A (RFC 6184 §5.8) VideoFrame entries
/// sized to fit within `mtu` bytes, reusing the metadata from `template`.
///
/// This is needed because the H264Depacketizer reassembles FU-A fragments from the
/// RTP side into a single complete NAL unit, but Chrome's WebRTC receiver may not
/// handle oversized SRTP packets (those exceeding the IP/UDP MTU). Re-fragmenting
/// ensures each forwarded RTP packet stays within MTU bounds.
///
/// If the NAL fits within one MTU, returns a single entry (no FU-A wrapping).
fn h264_fragment_nal(nal: &Bytes, mtu: usize, template: &VideoFrame) -> Vec<VideoFrame> {
    // Each FU-A RTP payload = 1 byte FU Indicator + 1 byte FU Header + payload chunk.
    // Max chunk size per packet:
    let chunk_size = mtu.saturating_sub(2);

    if nal.is_empty() || chunk_size == 0 {
        return vec![template.clone()];
    }

    // If the NAL fits entirely in one MTU, send as Single NAL Unit (no wrapping).
    if nal.len() <= mtu {
        let mut frame = template.clone();
        frame.data = nal.clone();
        frame.sequence_number = None;
        frame.payload_type = None;
        return vec![frame];
    }

    // NAL header byte
    let nal_header = nal[0];
    let nal_type = nal_header & 0x1F;
    let nri = nal_header & 0x60; // NRI bits
    // FU Indicator: forbidden_zero=0 | NRI | type=28 (FU-A)
    let fu_indicator: u8 = nri | 28u8;

    // The raw NAL payload (everything after the NAL header byte)
    let nal_payload = nal.slice(1..);
    let mut offset = 0;
    let total = nal_payload.len();
    let mut frames = Vec::new();

    while offset < total {
        let remaining = total - offset;
        let this_chunk = remaining.min(chunk_size);
        let is_start = offset == 0;
        let is_end = offset + this_chunk >= total;

        let mut fu_header: u8 = nal_type;
        if is_start {
            fu_header |= 0x80;
        } // S bit
        if is_end {
            fu_header |= 0x40;
        } // E bit

        let mut fua_payload = Vec::with_capacity(2 + this_chunk);
        fua_payload.push(fu_indicator);
        fua_payload.push(fu_header);
        fua_payload.extend_from_slice(&nal_payload[offset..offset + this_chunk]);

        let mut frame = template.clone();
        frame.data = Bytes::from(fua_payload);
        frame.is_last_packet = is_end;
        frame.sequence_number = None;
        frame.payload_type = None;
        frames.push(frame);

        offset += this_chunk;
    }

    frames
}

/// BridgePeer manages two PeerConnections to bridge media between WebRTC and RTP
pub struct BridgePeer {
    id: String,
    /// WebRTC side PeerConnection (SRTP/DTLS)
    webrtc_pc: PeerConnection,
    /// RTP side PeerConnection (plain RTP)
    rtp_pc: PeerConnection,
    /// Bridge task handles
    bridge_tasks: AsyncMutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Cancellation token
    cancel_token: CancellationToken,
    /// Recorder option (future use)
    #[allow(dead_code)]
    recorder_option: Option<RecorderOption>,
    /// Audio sender channels for forwarding
    webrtc_send: Arc<AsyncMutex<Option<MediaSender>>>,
    rtp_send: Arc<AsyncMutex<Option<MediaSender>>>,
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
            recorder_option: None,
            webrtc_send: Arc::new(AsyncMutex::new(None)),
            rtp_send: Arc::new(AsyncMutex::new(None)),
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

        // Setup video senders if video codecs are configured
        if let Some(ref webrtc_video_params) = self.webrtc_video_codec {
            let (webrtc_video_tx, webrtc_video_track, _) =
                rustrtc::media::track::sample_track(MediaKind::Video, 100);
            if let Ok(sender) = self
                .webrtc_pc()
                .add_track(webrtc_video_track, webrtc_video_params.clone())
            {
                *self.webrtc_video_sender.lock().await = Some(sender);
            }
            *self.webrtc_video_send.lock().await = Some(webrtc_video_tx);
            info!(bridge_id = %self.id, pt = webrtc_video_params.payload_type, clock_rate = webrtc_video_params.clock_rate, "WebRTC video sender setup complete");
        } else {
            info!(bridge_id = %self.id, "WebRTC video sender NOT configured (no video codec)");
        }

        if let Some(ref rtp_video_params) = self.rtp_video_codec {
            let (rtp_video_tx, rtp_video_track, _) =
                rustrtc::media::track::sample_track(MediaKind::Video, 100);
            if let Ok(sender) = self
                .rtp_pc()
                .add_track(rtp_video_track, rtp_video_params.clone())
            {
                *self.rtp_video_sender.lock().await = Some(sender);
            }
            *self.rtp_video_send.lock().await = Some(rtp_video_tx);
            info!(bridge_id = %self.id, pt = rtp_video_params.payload_type, clock_rate = rtp_video_params.clock_rate, "RTP video sender setup complete");
        } else {
            info!(bridge_id = %self.id, "RTP video sender NOT configured (no video codec)");
        }

        info!(bridge_id = %self.id, "Bridge setup complete with sample tracks");
        Ok(())
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
        info!(bridge_id = %self.id, "Starting media bridge");

        // Extract video senders now (before spawning) so PLI forwarder tasks can use them
        let webrtc_video_sender = self.webrtc_video_sender.lock().await.clone();
        let rtp_video_sender = self.rtp_video_sender.lock().await.clone();

        // Spawn a single bidirectional forwarding task instead of 2 separate tasks
        let bidirectional_task =
            self.spawn_bidirectional_forwarder(webrtc_video_sender, rtp_video_sender);

        let mut tasks = self.bridge_tasks.lock().await;
        tasks.push(bidirectional_task);
    }

    /// Get the WebRTC-side sender (for test injection)
    pub async fn get_webrtc_sender(&self) -> Option<MediaSender> {
        self.webrtc_send.lock().await.clone()
    }

    /// Get the RTP-side sender (for test injection)
    pub async fn get_rtp_sender(&self) -> Option<MediaSender> {
        self.rtp_send.lock().await.clone()
    }

    /// Get the WebRTC-side track (for test consumption)
    pub async fn get_webrtc_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.webrtc_track.lock().await.clone()
    }

    /// Get the RTP-side track (for test consumption)
    pub async fn get_rtp_track(&self) -> Option<Arc<SampleStreamTrack>> {
        self.rtp_track.lock().await.clone()
    }

    /// Start bridge in test mode: directly forwards between stored sample tracks
    /// bypassing PeerConnection event loop. This allows load testing the core
    /// forwarding path without full SDP negotiation.
    pub async fn start_bridge_test_mode(&self) {
        info!(bridge_id = %self.id, "Starting media bridge in test mode");

        let webrtc_track = self.get_webrtc_track().await;
        let rtp_track = self.get_rtp_track().await;
        let rtp_send = Arc::downgrade(&self.rtp_send);
        let webrtc_send = Arc::downgrade(&self.webrtc_send);
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();
        // WebRTC -> RTP direct forwarder (inline loop, no extra sub-task spawn)
        if let Some(track) = webrtc_track {
            let cancel = cancel_token.clone();
            let id = bridge_id.clone();
            let task = tokio::spawn(async move {
                info!(bridge_id = %id, "Test-mode WebRTC -> RTP forwarder started");
                Self::run_forward_loop(id, track, rtp_send, cancel, "WebRTC→RTP", None).await;
            });
            let mut tasks = self.bridge_tasks.lock().await;
            tasks.push(task);
        }

        // RTP -> WebRTC direct forwarder (inline loop)
        if let Some(track) = rtp_track {
            let id = bridge_id.clone();
            let task = tokio::spawn(async move {
                info!(bridge_id = %id, "Test-mode RTP -> WebRTC forwarder started");
                Self::run_forward_loop(id, track, webrtc_send, cancel_token, "RTP→WebRTC", None)
                    .await;
            });
            let mut tasks = self.bridge_tasks.lock().await;
            tasks.push(task);
        }
    }

    /// Spawn a single task that handles bidirectional forwarding
    /// This reduces task context switching and futex contention compared to 2 separate tasks
    fn spawn_bidirectional_forwarder(
        &self,
        webrtc_video_sender: Option<Arc<RtpSender>>,
        rtp_video_sender: Option<Arc<RtpSender>>,
    ) -> tokio::task::JoinHandle<()> {
        let webrtc_pc = self.webrtc_pc.clone();
        let rtp_pc = self.rtp_pc.clone();
        let rtp_send = Arc::downgrade(&self.rtp_send);
        let webrtc_send = Arc::downgrade(&self.webrtc_send);
        let rtp_video_send = Arc::downgrade(&self.rtp_video_send);
        let webrtc_video_send = Arc::downgrade(&self.webrtc_video_send);
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();

        tokio::spawn(async move {
            info!(bridge_id = %bridge_id, "Bidirectional forwarder started");

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
                                    info!(bridge_id = %bridge_id, kind = if is_video { "video" } else { "audio" }, "WebRTC receiver track ready");

                                    let sender = if is_video {
                                        // When JsSIP (WebRTC) sends video, forward PLI from rtp_video_sender back to JsSIP
                                        if let Some(ref rtp_sender) = rtp_video_sender {
                                            Self::spawn_pli_forwarder(
                                                bridge_id.clone(),
                                                rtp_sender.clone(),
                                                track.clone(),
                                                cancel_token.clone(),
                                                "Linphone PLI→JsSIP",
                                            );
                                        }
                                        rtp_video_send.clone()
                                    } else {
                                        rtp_send.clone()
                                    };
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        sender,
                                        cancel_token.clone(),
                                        "WebRTC→RTP",
                                        None, // no audio fallback in WebRTC→RTP direction
                                    ).await;
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
                                    info!(bridge_id = %bridge_id, kind = if is_video { "video" } else { "audio" }, "RTP receiver track ready");

                                    let sender = if is_video {
                                        // When Linphone (RTP) sends video, forward PLI from webrtc_video_sender back to Linphone
                                        if let Some(ref webrtc_sender) = webrtc_video_sender {
                                            Self::spawn_pli_forwarder(
                                                bridge_id.clone(),
                                                webrtc_sender.clone(),
                                                track.clone(),
                                                cancel_token.clone(),
                                                "JsSIP PLI→Linphone",
                                            );
                                        }
                                        webrtc_video_send.clone()
                                    } else {
                                        webrtc_send.clone()
                                    };
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        sender,
                                        cancel_token.clone(),
                                        "RTP→WebRTC",
                                        // For the video RTP track: pass the WebRTC audio sender as a
                                        // fallback so that BUNDLE-mode audio packets arriving on the
                                        // shared RTP socket get properly forwarded instead of dropped.
                                        if is_video { Some(webrtc_send.clone()) } else { None },
                                    ).await;
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

            info!(bridge_id = %bridge_id, "Bidirectional forwarder stopped");
        })
    }

    /// Inline media forwarding loop without spawning a sub-task.
    /// Callers that are already inside a spawned task should use this directly.
    async fn run_forward_loop(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<AsyncMutex<Option<MediaSender>>>,
        cancel_token: CancellationToken,
        direction: &'static str,
        // Optional audio sender used when the track is video but an audio packet
        // arrives (BUNDLE demux: all media on one RTP socket).  Instead of
        // dropping the misrouted audio, forward it here.
        audio_fallback_weak: Option<std::sync::Weak<AsyncMutex<Option<MediaSender>>>>,
    ) {
        info!(bridge_id = %bridge_id, direction = %direction, "Media forwarding started");

        // Get the sender channel from weak pointer
        let sender = if let Some(strong) = sender_weak.upgrade() {
            let guard = strong.lock().await;
            guard.clone()
        } else {
            warn!(bridge_id = %bridge_id, direction = %direction, "Sender channel no longer available");
            return;
        };

        if sender.is_none() {
            warn!(bridge_id = %bridge_id, direction = %direction, "No sender channel available — video/audio sender was not configured");
            return;
        }
        let sender = sender.unwrap();

        // Resolve the audio fallback sender (if provided) for BUNDLE demux.
        let audio_fallback_sender: Option<MediaSender> = if let Some(weak) = audio_fallback_weak {
            if let Some(strong) = weak.upgrade() {
                let guard = strong.lock().await;
                guard.clone()
            } else {
                None
            }
        } else {
            None
        };

        // For video tracks, request an initial keyframe (IDR) from the remote sender.
        // Without this, H264 decoding starts mid-GOP and both sides show black until the
        // next keyframe naturally arrives (which may be tens of seconds away).
        if track.kind() == MediaKind::Video {
            if let Err(e) = track.request_key_frame().await {
                warn!(bridge_id = %bridge_id, direction = %direction, error = %e, "Failed to request initial keyframe");
            } else {
                info!(bridge_id = %bridge_id, direction = %direction, "Requested initial keyframe from remote");
            }
        }

        let is_video = track.kind() == MediaKind::Video;
        let mut packet_count: u64 = 0;
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
                        direction = %direction,
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
                            if packet_count == 1 {
                                info!(bridge_id = %bridge_id, direction = %direction, kind = ?sample.kind(), "First media sample forwarded");
                            }
                            // For video samples: sanitize and re-fragment H264 NAL units.
                            //
                            // 1. Skip misrouted audio packets: Linphone uses BUNDLE (audio+video
                            //    both on one port) and some audio PT=8/9/etc packets land on the
                            //    video track due to imperfect demux.  Forwarding PCMA bytes as H264
                            //    corrupts the stream and triggers endless PLI.
                            //
                            // 2. Strip RTP header extensions: Chrome WebRTC packets carry
                            //    abs-send-time / rtp-stream-id extensions.  Forwarding these to
                            //    Linphone's plain RTP causes Linphone's decoder to see unexpected
                            //    extension bytes, potentially rejecting packets (black screen).
                            //
                            // 3. Re-fragment large NAL units into FU-A chunks ≤1200 bytes so no
                            //    SRTP/UDP packet exceeds MTU.
                            let samples_to_send: Vec<MediaSample> = match sample {
                                MediaSample::Video(mut v) => {
                                    if is_video {
                                        // Route misrouted audio packets (PT < 96 arriving on the
                                        // video track) to the audio sender when available.  This
                                        // happens with BUNDLE: Linphone multiplexes audio+video on
                                        // one RTP port and both land on the video receiver.
                                        if matches!(v.payload_type, Some(pt) if pt < 96) {
                                            if let Some(ref audio_sender) = audio_fallback_sender {
                                                let audio_sample = MediaSample::Audio(
                                                    rustrtc::media::AudioFrame {
                                                        data: v.data,
                                                        payload_type: v.payload_type,
                                                        sequence_number: v.sequence_number,
                                                        rtp_timestamp: v.rtp_timestamp,
                                                        source_addr: v.source_addr,
                                                        ..Default::default()
                                                    },
                                                );
                                                let _ = audio_sender.try_send(audio_sample);
                                            } else {
                                                debug!(
                                                    bridge_id = %bridge_id,
                                                    direction = %direction,
                                                    pt = ?v.payload_type,
                                                    "Dropping misrouted audio packet on video track"
                                                );
                                            }
                                            vec![] // already forwarded or discarded
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

                                            let nal = v.data.clone();
                                            // Log NAL type for first 20 video packets to diagnose H264 stream
                                            if packet_count <= 20 && !nal.is_empty() {
                                                let nal_type = nal[0] & 0x1F;
                                                debug!(
                                                    bridge_id = %bridge_id,
                                                    direction = %direction,
                                                    nal_type = nal_type,
                                                    nal_bytes = nal.len(),
                                                    first_byte = format!("{:02x}", nal[0]),
                                                    "NAL unit"
                                                );
                                            }
                                            let fragments = h264_fragment_nal(&nal, H264_MTU, &v);
                                            fragments.into_iter().map(MediaSample::Video).collect()
                                        }
                                    } else {
                                        v.sequence_number = None;
                                        v.payload_type = None;
                                        vec![MediaSample::Video(v)]
                                    }
                                }
                                other => vec![other],
                            };
                            for sample in samples_to_send {
                                // Forward the sample using try_send first to reduce Tokio scheduling overhead
                                match sender.try_send(sample.clone()) {
                                    Ok(()) => {}
                                    Err(MediaError::WouldBlock) => {
                                        // Fallback to async send only when channel is full
                                        if let Err(e) = sender.send(sample).await {
                                            warn!(bridge_id = %bridge_id, direction = %direction, error = %e, "Failed to forward media sample");
                                            break 'outer;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(bridge_id = %bridge_id, direction = %direction, error = ?e, "Failed to forward media sample (kind mismatch or closed)");
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            debug!(bridge_id = %bridge_id, direction = %direction, error = %e, "Track recv ended");
                            break;
                        }
                    }
                }
            }
        }

        info!(bridge_id = %bridge_id, direction = %direction, "Media forwarding stopped");
    }

    /// Forward media from a track to a sender channel.
    /// Spawns a sub-task; used by the PC-event-driven start paths.
    async fn forward_track_to_sender(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<AsyncMutex<Option<MediaSender>>>,
        cancel_token: CancellationToken,
        direction: &'static str,
        audio_fallback_weak: Option<std::sync::Weak<AsyncMutex<Option<MediaSender>>>>,
    ) {
        tokio::spawn(async move {
            Self::run_forward_loop(
                bridge_id,
                track,
                sender_weak,
                cancel_token,
                direction,
                audio_fallback_weak,
            )
            .await;
        });
    }

    /// Spawn a task that subscribes to PLI/FIR RTCP on `sender` and forwards them as
    /// `request_key_frame()` calls on `source_track`.
    ///
    /// This ensures that when the remote peer (e.g. JsSIP) requests a keyframe,
    /// the request is propagated all the way back to the original video source (e.g. Linphone).
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
                                    debug!(bridge_id = %bridge_id, label = %label, "Forwarded PLI → keyframe request");
                                }
                            }
                            Err(_) => break, // broadcast receiver dropped / channel closed
                            _ => {}
                        }
                    }
                }
            }
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
        info!(bridge_id = %self.id, "Stopping bridge");
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

/// BridgeTrack implements the Track trait for bridging
pub struct BridgeTrack {
    bridge: Arc<BridgePeer>,
    track_id: String,
}

impl BridgeTrack {
    pub fn new(track_id: String, bridge: Arc<BridgePeer>) -> Self {
        Self { bridge, track_id }
    }

    pub fn bridge(&self) -> &BridgePeer {
        &self.bridge
    }
}

#[async_trait]
impl Track for BridgeTrack {
    fn id(&self) -> &str {
        &self.track_id
    }

    async fn handshake(&self, _remote_offer: String) -> Result<String> {
        // The bridge needs to handle SDP negotiation on both sides
        // For now, this is handled externally by setting up both PCs
        // before creating the bridge
        Err(anyhow::anyhow!(
            "BridgeTrack handshake not supported - setup PCs externally"
        ))
    }

    async fn local_description(&self) -> Result<String> {
        // Return WebRTC side's local description
        self.bridge
            .webrtc_pc()
            .local_description()
            .map(|d| Ok(d.to_sdp_string()))
            .unwrap_or_else(|| Err(anyhow::anyhow!("No local description")))
    }

    async fn set_remote_description(&self, remote: &str) -> Result<()> {
        // Set remote on WebRTC side
        use rustrtc::sdp::{SdpType, SessionDescription};
        let desc = SessionDescription::parse(SdpType::Answer, remote)
            .map_err(|e| anyhow::anyhow!("failed to parse sdp: {:?}", e))?;
        self.bridge
            .webrtc_pc()
            .set_remote_description(desc)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn stop(&self) {
        self.bridge.stop().await;
    }

    async fn get_peer_connection(&self) -> Option<PeerConnection> {
        Some(self.bridge.webrtc_pc.clone())
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
    webrtc_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    rtp_audio_capabilities: Option<Vec<rustrtc::config::AudioCapability>>,
    webrtc_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    rtp_video_capabilities: Option<Vec<rustrtc::config::VideoCapability>>,
    webrtc_sender_codec: Option<RtpCodecParameters>,
    rtp_sender_codec: Option<RtpCodecParameters>,
    rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode,
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
            webrtc_audio_capabilities: None,
            rtp_audio_capabilities: None,
            webrtc_video_capabilities: None,
            rtp_video_capabilities: None,
            webrtc_sender_codec: None,
            rtp_sender_codec: None,
            rtp_sdp_compatibility: rustrtc::config::SdpCompatibilityMode::LegacySip,
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

    pub fn build(self) -> Arc<BridgePeer> {
        let webrtc_media_caps =
            self.webrtc_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: self.webrtc_video_capabilities.clone().unwrap_or_default(),
                    application: None,
                });

        let webrtc_config = self
            .webrtc_config
            .unwrap_or_else(|| rustrtc::RtcConfiguration {
                transport_mode: TransportMode::WebRtc,
                external_ip: self.external_ip.clone(),
                media_capabilities: webrtc_media_caps,
                ssrc_start: rand::random::<u32>(),
                // WebRTC side uses Standard mode (includes rtcp-mux, a=mid)
                sdp_compatibility: rustrtc::config::SdpCompatibilityMode::Standard,
                ..Default::default()
            });

        let rtp_media_caps =
            self.rtp_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: self.rtp_video_capabilities.clone().unwrap_or_default(),
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

        // NOTE: Do NOT add transceivers here. setup_bridge() calls add_track()
        // which creates the single transceiver per PC. Adding transceivers here
        // would cause duplicate m= lines in SDP offers/answers.

        Arc::new(bridge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{CodecType, RtpTrackBuilder};
    use rustrtc::{
        TransportMode,
        sdp::{SdpType, SessionDescription},
    };

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

    /// Test bridge setup with external RTP track
    /// This simulates the scenario where:
    /// - Bridge RTP side connects to an RTP endpoint (SIP leg)
    #[tokio::test]
    async fn test_bridge_setup_with_external_tracks() {
        use crate::media::Track;

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

        info!("P2P WebRTC ↔ RTP bridge test completed successfully");

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
}
