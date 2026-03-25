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
use rustrtc::{
    media::{MediaSample, MediaStreamTrack, MediaKind, SampleStreamSource},
    PeerConnection, PeerConnectionEvent, TransportMode,
    RtpCodecParameters, TransceiverDirection,
};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Type alias for the sender channel
pub type MediaSender = SampleStreamSource;

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
    /// Sender channels for forwarding
    webrtc_send: Arc<AsyncMutex<Option<MediaSender>>>,
    rtp_send: Arc<AsyncMutex<Option<MediaSender>>>,
}

impl BridgePeer {
    /// Create a new bridge peer with given WebRTC and RTP PeerConnections
    pub fn new(
        id: String,
        webrtc_pc: PeerConnection,
        rtp_pc: PeerConnection,
    ) -> Self {
        Self {
            id,
            webrtc_pc,
            rtp_pc,
            bridge_tasks: AsyncMutex::new(Vec::new()),
            cancel_token: CancellationToken::new(),
            recorder_option: None,
            webrtc_send: Arc::new(AsyncMutex::new(None)),
            rtp_send: Arc::new(AsyncMutex::new(None)),
        }
    }

    /// Setup the bridge by adding sample tracks to both sides for forwarding
    pub async fn setup_bridge(&self) -> Result<()> {
        // Setup WebRTC side: create sample track and register with PC
        let (webrtc_tx, webrtc_track, _) = 
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);
        
        // Add track to WebRTC PC as a sender
        let _ssrc = rand::random::<u32>();
        let params = RtpCodecParameters::default();
        let _ = self.webrtc_pc().add_track(webrtc_track, params);
        
        *self.webrtc_send.lock().await = Some(webrtc_tx);
        
        // Setup RTP side: create sample track and register with PC  
        let (rtp_tx, rtp_track, _) = 
            rustrtc::media::track::sample_track(MediaKind::Audio, 100);
        
        // Add track to RTP PC as a sender
        let _ssrc = rand::random::<u32>();
        let params = RtpCodecParameters::default();
        let _ = self.rtp_pc().add_track(rtp_track, params);
        
        *self.rtp_send.lock().await = Some(rtp_tx);
        
        info!(bridge_id = %self.id, "Bridge setup complete with sample tracks");
        Ok(())
    }

    /// Start the bridge - begin forwarding media between sides
    pub async fn start_bridge(&self) {
        info!(bridge_id = %self.id, "Starting media bridge");
        
        // Start WebRTC → RTP forwarding task
        let webrtc_to_rtp = self.spawn_webrtc_to_rtp_forwarder();
        
        // Start RTP → WebRTC forwarding task
        let rtp_to_webrtc = self.spawn_rtp_to_webrtc_forwarder();
        
        let mut tasks = self.bridge_tasks.lock().await;
        tasks.push(webrtc_to_rtp);
        tasks.push(rtp_to_webrtc);
    }

    /// Spawn task to forward media from WebRTC side to RTP side
    fn spawn_webrtc_to_rtp_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let webrtc_pc = self.webrtc_pc.clone();
        let rtp_send = Arc::downgrade(&self.rtp_send);
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();

        tokio::spawn(async move {
            info!(bridge_id = %bridge_id, "WebRTC → RTP forwarder started");
            
            // Wait for track event on WebRTC side (incoming from remote peer)
            let mut pc_recv = Box::pin(webrtc_pc.recv());
            
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!(bridge_id = %bridge_id, "WebRTC → RTP forwarder cancelled");
                        break;
                    }
                    event = &mut pc_recv => {
                        match event {
                            Some(PeerConnectionEvent::Track(transceiver)) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    info!(bridge_id = %bridge_id, "WebRTC receiver track ready, starting forward to RTP");
                                    
                                    // Forward from WebRTC track to RTP sender
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        rtp_send.clone(),
                                        cancel_token.clone(),
                                        "WebRTC→RTP"
                                    ).await;
                                }
                                pc_recv = Box::pin(webrtc_pc.recv());
                            }
                            Some(_) => {
                                // Other event, continue
                                pc_recv = Box::pin(webrtc_pc.recv());
                            }
                            None => {
                                debug!(bridge_id = %bridge_id, "WebRTC PeerConnection closed");
                                break;
                            }
                        }
                    }
                }
            }
            
            info!(bridge_id = %bridge_id, "WebRTC → RTP forwarder stopped");
        })
    }

    /// Spawn task to forward media from RTP side to WebRTC side
    fn spawn_rtp_to_webrtc_forwarder(&self) -> tokio::task::JoinHandle<()> {
        let rtp_pc = self.rtp_pc.clone();
        let webrtc_send = Arc::downgrade(&self.webrtc_send);
        let cancel_token = self.cancel_token.clone();
        let bridge_id = self.id.clone();

        tokio::spawn(async move {
            info!(bridge_id = %bridge_id, "RTP → WebRTC forwarder started");
            
            // Wait for track event on RTP side (incoming from remote peer)
            let mut pc_recv = Box::pin(rtp_pc.recv());
            
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!(bridge_id = %bridge_id, "RTP → WebRTC forwarder cancelled");
                        break;
                    }
                    event = &mut pc_recv => {
                        match event {
                            Some(PeerConnectionEvent::Track(transceiver)) => {
                                if let Some(receiver) = transceiver.receiver() {
                                    let track = receiver.track();
                                    info!(bridge_id = %bridge_id, "RTP receiver track ready, starting forward to WebRTC");
                                    
                                    // Forward from RTP track to WebRTC sender
                                    Self::forward_track_to_sender(
                                        bridge_id.clone(),
                                        track,
                                        webrtc_send.clone(),
                                        cancel_token.clone(),
                                        "RTP→WebRTC"
                                    ).await;
                                }
                                pc_recv = Box::pin(rtp_pc.recv());
                            }
                            Some(_) => {
                                pc_recv = Box::pin(rtp_pc.recv());
                            }
                            None => {
                                debug!(bridge_id = %bridge_id, "RTP PeerConnection closed");
                                break;
                            }
                        }
                    }
                }
            }
            
            info!(bridge_id = %bridge_id, "RTP → WebRTC forwarder stopped");
        })
    }

    /// Forward media from a track to a sender channel
    async fn forward_track_to_sender(
        bridge_id: String,
        track: Arc<dyn MediaStreamTrack>,
        sender_weak: std::sync::Weak<AsyncMutex<Option<MediaSender>>>,
        cancel_token: CancellationToken,
        direction: &'static str,
    ) {
        tokio::spawn(async move {
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
                warn!(bridge_id = %bridge_id, direction = %direction, "No sender channel available");
                return;
            }
            let sender = sender.unwrap();
            
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                    sample_result = track.recv() => {
                        match sample_result {
                            Ok(MediaSample::Audio(frame)) => {
                                // Forward the audio frame
                                let sample = MediaSample::Audio(frame);
                                if let Err(e) = sender.send(sample).await {
                                    warn!(bridge_id = %bridge_id, error = %e, "Failed to forward media, channel closed");
                                    break;
                                }
                            }
                            Ok(_) => {
                                // Other sample types, ignore for audio bridge
                            }
                            Err(e) => {
                                debug!(bridge_id = %bridge_id, error = %e, "Track recv error");
                                break;
                            }
                        }
                    }
                }
            }
            
            info!(bridge_id = %bridge_id, direction = %direction, "Media forwarding stopped");
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

    /// Stop the bridge
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
        Err(anyhow::anyhow!("BridgeTrack handshake not supported - setup PCs externally"))
    }

    async fn local_description(&self) -> Result<String> {
        // Return WebRTC side's local description
        self.bridge.webrtc_pc()
            .local_description()
            .map(|d| Ok(d.to_sdp_string()))
            .unwrap_or_else(|| Err(anyhow::anyhow!("No local description")))
    }

    async fn set_remote_description(&self, remote: &str) -> Result<()> {
        // Set remote on WebRTC side
        use rustrtc::sdp::{SessionDescription, SdpType};
        let desc = SessionDescription::parse(SdpType::Answer, remote)
            .map_err(|e| anyhow::anyhow!("failed to parse sdp: {:?}", e))?;
        self.bridge.webrtc_pc()
            .set_remote_description(desc).await
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
}

impl BridgePeerBuilder {
    pub fn new(bridge_id: String) -> Self {
        Self {
            bridge_id,
            webrtc_config: None,
            rtp_config: None,
            rtp_port_range: (20000, 30000),
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

    pub fn build(self) -> Arc<BridgePeer> {
        let webrtc_config = self.webrtc_config.unwrap_or_else(|| rustrtc::RtcConfiguration {
            transport_mode: TransportMode::WebRtc,
            ..Default::default()
        });

        let rtp_config = self.rtp_config.unwrap_or_else(|| rustrtc::RtcConfiguration {
            transport_mode: TransportMode::Rtp,
            rtp_start_port: Some(self.rtp_port_range.0),
            rtp_end_port: Some(self.rtp_port_range.1),
            ..Default::default()
        });

        let webrtc_pc = PeerConnection::new(webrtc_config);
        let rtp_pc = PeerConnection::new(rtp_config);

        // Add transceivers for audio on both sides
        webrtc_pc.add_transceiver(rustrtc::MediaKind::Audio, TransceiverDirection::SendRecv);
        rtp_pc.add_transceiver(rustrtc::MediaKind::Audio, TransceiverDirection::SendRecv);

        Arc::new(BridgePeer::new(self.bridge_id, webrtc_pc, rtp_pc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{RtpTrackBuilder, CodecType};
    use rustrtc::{TransportMode, sdp::{SessionDescription, SdpType}};

    #[tokio::test]
    async fn test_bridge_peer_creation() {
        let bridge = BridgePeerBuilder::new("test-bridge".to_string())
            .with_rtp_port_range(25000, 25100)
            .build();

        // Generate offers for both PCs
        let webrtc_offer = bridge.webrtc_pc().create_offer().await;
        let rtp_offer = bridge.rtp_pc().create_offer().await;
        
        assert!(webrtc_offer.is_ok(), "WebRTC should create offer");
        assert!(rtp_offer.is_ok(), "RTP should create offer");
        
        // Set local descriptions
        let _ = bridge.webrtc_pc().set_local_description(webrtc_offer.unwrap());
        let _ = bridge.rtp_pc().set_local_description(rtp_offer.unwrap());

        // Verify both PCs have local descriptions
        let webrtc_local = bridge.webrtc_pc().local_description();
        let rtp_local = bridge.rtp_pc().local_description();

        assert!(webrtc_local.is_some(), "WebRTC PC should have local description");
        assert!(rtp_local.is_some(), "RTP PC should have local description");

        // WebRTC should have DTLS/ICE attributes
        let webrtc_sdp = webrtc_local.unwrap().to_sdp_string();
        assert!(webrtc_sdp.contains("UDP/TLS/RTP/SAVPF"), "WebRTC should use SAVPF");
        assert!(webrtc_sdp.contains("fingerprint"), "WebRTC should have DTLS fingerprint");

        // RTP should be plain
        let rtp_sdp = rtp_local.unwrap().to_sdp_string();
        assert!(rtp_sdp.contains("RTP/AVP"), "RTP should use AVP");
        assert!(!rtp_sdp.contains("fingerprint"), "RTP should not have DTLS fingerprint");
    }

    /// Test bridge setup with external RTP track
    /// This simulates the scenario where:
    /// - Bridge WebRTC side connects to a WebRTC client
    /// - Bridge RTP side connects to an RTP endpoint (SIP leg)
    #[tokio::test]
    async fn test_bridge_setup_with_external_tracks() {
        use rustrtc::sdp::{SessionDescription, SdpType};

        // Create bridge
        let bridge = BridgePeerBuilder::new("test-bridge-integration".to_string())
            .with_rtp_port_range(26000, 26100)
            .build();

        // Create external RTP track (simulating SIP leg)
        let rtp_leg = RtpTrackBuilder::new("rtp-leg".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(26100, 26200)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        // Generate and exchange SDPs between bridge RTP side and external RTP leg
        let bridge_rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(bridge_rtp_offer.clone()).unwrap();
        
        let bridge_rtp_sdp = bridge_rtp_offer.to_sdp_string();
        
        // External RTP leg receives offer and creates answer
        let rtp_leg_answer = rtp_leg.handshake(bridge_rtp_sdp).await.unwrap();
        
        // Bridge RTP side sets remote description
        let rtp_leg_desc = SessionDescription::parse(SdpType::Answer, &rtp_leg_answer).unwrap();
        bridge.rtp_pc().set_remote_description(rtp_leg_desc).await.unwrap();

        // Also setup WebRTC side for completeness
        let bridge_webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        bridge.webrtc_pc().set_local_description(bridge_webrtc_offer).unwrap();

        // Setup bridge sample tracks for forwarding
        bridge.setup_bridge().await.unwrap();

        // Start bridge forwarding
        bridge.start_bridge().await;

        // Verify bridge is properly configured
        let webrtc_pc = bridge.webrtc_pc();
        let rtp_pc = bridge.rtp_pc();

        // Both should have local descriptions
        assert!(webrtc_pc.local_description().is_some(), "WebRTC should have local description");
        assert!(rtp_pc.local_description().is_some(), "RTP should have local description");
        assert!(rtp_pc.remote_description().is_some(), "RTP should have remote description");

        // Clean up
        bridge.stop().await;
    }

    /// Test that BridgePeer correctly handles SDP format differences
    #[tokio::test]
    async fn test_bridge_sdp_format_differences() {
        let bridge = BridgePeerBuilder::new("test-bridge-sdp".to_string())
            .with_rtp_port_range(27000, 27100)
            .build();

        // Generate offers
        let webrtc_offer = bridge.webrtc_pc().create_offer().await.unwrap();
        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();

        bridge.webrtc_pc().set_local_description(webrtc_offer).unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

        let webrtc_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();
        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();

        // Verify format differences
        // WebRTC should have ICE/DTLS attributes
        assert!(webrtc_sdp.contains("ice-ufrag"), "WebRTC should have ICE ufrag");
        assert!(webrtc_sdp.contains("ice-pwd"), "WebRTC should have ICE pwd");
        assert!(webrtc_sdp.contains("setup:"), "WebRTC should have DTLS setup");

        // RTP should NOT have ICE/DTLS attributes
        assert!(!rtp_sdp.contains("ice-ufrag"), "RTP should NOT have ICE ufrag");
        assert!(!rtp_sdp.contains("fingerprint:"), "RTP should NOT have DTLS fingerprint");

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
        bridge.webrtc_pc().set_local_description(bridge_webrtc_offer.clone()).unwrap();
        bridge.rtp_pc().set_local_description(bridge_rtp_offer.clone()).unwrap();
        
        // Step 5: Bridge WebRTC side SDP (would be sent to WebRTC caller)
        let webrtc_sdp = bridge.webrtc_pc().local_description().unwrap().to_sdp_string();
        assert!(webrtc_sdp.contains("SAVPF"), "WebRTC side should use SAVPF");
        assert!(webrtc_sdp.contains("fingerprint"), "WebRTC side should have DTLS fingerprint");
        
        // Step 6: Bridge RTP side SDP (would be sent to RTP callee, possibly converted)
        let rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
        assert!(rtp_sdp.contains("RTP/AVP"), "RTP side should use AVP");
        assert!(!rtp_sdp.contains("fingerprint"), "RTP side should NOT have DTLS fingerprint");
        
        // Step 7: RTP callee receives bridge RTP offer and creates answer
        let rtp_callee_answer = rtp_callee.handshake(rtp_sdp).await.unwrap();
        
        // Step 8: Bridge RTP side sets remote description from callee
        let callee_desc = SessionDescription::parse(SdpType::Answer, &rtp_callee_answer).unwrap();
        bridge.rtp_pc().set_remote_description(callee_desc).await.unwrap();
        
        // Step 9: Start bridge forwarding
        bridge.start_bridge().await;
        
        // Verify connections are established
        assert!(bridge.webrtc_pc().local_description().is_some(), "WebRTC should have local description");
        assert!(bridge.rtp_pc().local_description().is_some(), "RTP should have local description");
        assert!(bridge.rtp_pc().remote_description().is_some(), "RTP should have remote description from callee");
        
        info!("P2P WebRTC ↔ RTP bridge test completed successfully");
        
        // Clean up
        bridge.stop().await;
    }
}
