//! WebRTC ↔ RTP Media Bridge
//!
//! This module provides media-layer bridging between WebRTC (SRTP) and plain RTP peers.
//! It creates two PeerConnections:
//! - One in WebRTC mode (with DTLS/SRTP encryption)
//! - One in RTP mode (plain RTP)
//!
//! Audio forwarding is staged onto pull-based egress tracks; this object keeps
//! the WebRTC/RTP PeerConnections and SDP conversion state.
//!
//! # Usage Example
//!
//! ```rust,ignore
//! // Create a bridge
//! let bridge = BridgePeerBuilder::new("bridge-1".to_string())
//!     .with_rtp_port_range(20000, 30000)
//!     .build();
//!
//! // Setup bridge with audio egress tracks
//! bridge.setup_bridge().await.unwrap();
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
//! 2. Create `BridgePeer` to own the WebRTC/RTP PeerConnections
//! 3. The bridge's WebRTC side connects to the WebRTC client
//! 4. The bridge's RTP side connects to the SIP/RTP endpoint

use crate::media::audio_egress_track::{AudioEgressTrack, AudioInputTap};
use crate::media::audio_route::{self, AudioEgressSlot};
use crate::media::negotiate::{NegotiatedCodec, NegotiatedLegProfile};
use crate::media::recorder::{Leg as RecLeg, Recorder};
use anyhow::Result;
use audio_codec::CodecType as AudioCodecType;
use rustrtc::{
    IceServer, PeerConnection, RtpCodecParameters, TransportMode,
    media::MediaStreamTrack,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

fn codec_from_rtp_params(params: &RtpCodecParameters) -> AudioCodecType {
    AudioCodecType::try_from(params.payload_type).unwrap_or_else(|_| {
        #[cfg(feature = "opus")]
        {
            if params.clock_rate == 48_000 {
                return AudioCodecType::Opus;
            }
        }

        AudioCodecType::PCMU
    })
}

fn profile_from_rtp_params(params: &RtpCodecParameters) -> NegotiatedLegProfile {
    let codec = codec_from_rtp_params(params);
    NegotiatedLegProfile {
        audio: Some(NegotiatedCodec {
            codec,
            payload_type: params.payload_type,
            clock_rate: params.clock_rate,
            channels: params.channels as u16,
        }),
        dtmf: None,
    }
}

/// BridgePeer manages two PeerConnections to bridge media between WebRTC and RTP
pub struct BridgePeer {
    id: String,
    /// WebRTC side PeerConnection (SRTP/DTLS)
    webrtc_pc: PeerConnection,
    /// RTP side PeerConnection (plain RTP)
    rtp_pc: PeerConnection,
    /// Cancellation token
    cancel_token: CancellationToken,
    /// Shared recorder for call recording (written by both bridge directions)
    recorder: Option<Arc<parking_lot::RwLock<Option<Recorder>>>>,
    /// Pull-based audio egress tracks. The PeerConnection sender pulls from
    /// these tracks directly.
    webrtc_audio_egress: AudioEgressSlot,
    rtp_audio_egress: AudioEgressSlot,
    /// Sender codec parameters (set by builder, used by setup_bridge)
    webrtc_sender_codec: Option<RtpCodecParameters>,
    rtp_sender_codec: Option<RtpCodecParameters>,
}

impl BridgePeer {
    /// Create a new bridge peer with given WebRTC and RTP PeerConnections
    pub fn new(id: String, webrtc_pc: PeerConnection, rtp_pc: PeerConnection) -> Self {
        Self {
            id,
            webrtc_pc,
            rtp_pc,
            cancel_token: CancellationToken::new(),
            recorder: None,
            webrtc_audio_egress: AudioEgressSlot::new(None),
            rtp_audio_egress: AudioEgressSlot::new(None),
            webrtc_sender_codec: None,
            rtp_sender_codec: None,
        }
    }

    /// Setup the bridge by adding audio egress tracks to both sides.
    ///
    /// `webrtc_params`: Codec parameters for the WebRTC-side track (e.g. Opus 48kHz)
    /// `rtp_params`: Codec parameters for the RTP-side track (e.g. PCMU 8kHz or Opus 48kHz)
    pub async fn setup_bridge_with_codecs(
        &self,
        webrtc_params: RtpCodecParameters,
        rtp_params: RtpCodecParameters,
    ) -> Result<()> {
        let webrtc_egress = Arc::new(AudioEgressTrack::new(
            "webrtc-audio-egress".to_string(),
            profile_from_rtp_params(&webrtc_params),
        ));
        let _ = self.webrtc_pc().add_track(
            webrtc_egress.clone() as Arc<dyn MediaStreamTrack>,
            webrtc_params,
        );
        *self.webrtc_audio_egress.lock().await = Some(webrtc_egress);

        let rtp_egress = Arc::new(AudioEgressTrack::new(
            "rtp-audio-egress".to_string(),
            profile_from_rtp_params(&rtp_params),
        ));
        let _ = self
            .rtp_pc()
            .add_track(rtp_egress.clone() as Arc<dyn MediaStreamTrack>, rtp_params);
        *self.rtp_audio_egress.lock().await = Some(rtp_egress);

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

    pub async fn stage_audio_forwarding(
        &self,
        webrtc_profile: NegotiatedLegProfile,
        rtp_profile: NegotiatedLegProfile,
        webrtc_recorder_leg: RecLeg,
        rtp_recorder_leg: RecLeg,
    ) -> Result<()> {
        let webrtc_tap =
            self.audio_input_tap(webrtc_recorder_leg, webrtc_profile.clone());
        let rtp_tap = self.audio_input_tap(rtp_recorder_leg, rtp_profile.clone());

        self.stage_peer_source_to_rtp(
            &self.webrtc_pc,
            webrtc_profile.clone(),
            rtp_profile.clone(),
            webrtc_tap,
            &self.id,
            "bridge-webrtc→rtp",
        )
        .await?;
        self.stage_peer_source_to_webrtc(
            &self.rtp_pc,
            rtp_profile,
            webrtc_profile,
            rtp_tap,
            &self.id,
            "bridge-rtp→webrtc",
        )
        .await?;

        debug!(bridge_id = %self.id, "Staged bridge audio forwarding on pull-based egress tracks");

        Ok(())
    }

    pub async fn stage_peer_source_to_webrtc(
        &self,
        source_pc: &PeerConnection,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
        session_id: &str,
        direction: &str,
    ) -> Result<()> {
        audio_route::stage_peer_audio_route(
            source_pc,
            &self.webrtc_audio_egress,
            &self.webrtc_pc,
            ingress_profile,
            egress_profile,
            tap,
            session_id,
            direction,
        )
        .await
    }

    pub async fn stage_peer_source_to_rtp(
        &self,
        source_pc: &PeerConnection,
        ingress_profile: NegotiatedLegProfile,
        egress_profile: NegotiatedLegProfile,
        tap: AudioInputTap,
        session_id: &str,
        direction: &str,
    ) -> Result<()> {
        audio_route::stage_peer_audio_route(
            source_pc,
            &self.rtp_audio_egress,
            &self.rtp_pc,
            ingress_profile,
            egress_profile,
            tap,
            session_id,
            direction,
        )
        .await
    }

    fn audio_input_tap(&self, leg: RecLeg, profile: NegotiatedLegProfile) -> AudioInputTap {
        let Some(recorder) = self.recorder.clone() else {
            return AudioInputTap::default();
        };

        AudioInputTap::default().with_recorder(audio_route::recorder_tap(recorder, leg, profile))
    }

    /// Get the WebRTC PeerConnection
    pub fn webrtc_pc(&self) -> &PeerConnection {
        &self.webrtc_pc
    }

    /// Get the RTP PeerConnection
    pub fn rtp_pc(&self) -> &PeerConnection {
        &self.rtp_pc
    }

    /// Stop the conversion PeerConnections.
    pub async fn stop(&self) {
        debug!(bridge_id = %self.id, "Stopping bridge");
        self.cancel_token.cancel();
        self.webrtc_pc.close();
        self.rtp_pc.close();
    }

    /// Close both PeerConnections.
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

    /// Set sender codec parameters for both bridge sides.
    /// These are used by each side's audio egress track.
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
    /// so the answer includes `a=group:BUNDLE` and `a=mid:`.
    pub fn with_rtp_sdp_compatibility(
        mut self,
        mode: rustrtc::config::SdpCompatibilityMode,
    ) -> Self {
        self.rtp_sdp_compatibility = mode;
        self
    }

    /// Attach a shared recorder so that both bridge directions write audio to it.
    /// The recorder is lazily activated: once `start_recording()` puts a
    /// `Recorder` inside the `Arc<RwLock<...>>`, egress input taps write to it.
    pub fn with_recorder(mut self, recorder: Arc<parking_lot::RwLock<Option<Recorder>>>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn build(self) -> Arc<BridgePeer> {
        let webrtc_media_caps =
            self.webrtc_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: Vec::new(),
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
                ice_servers: self.ice_servers,
                ..Default::default()
            });

        let rtp_media_caps =
            self.rtp_audio_capabilities
                .map(|audio| rustrtc::config::MediaCapabilities {
                    audio,
                    video: Vec::new(),
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

    #[tokio::test]
    async fn test_bridge_audio_forwarding_stages_on_egress_tracks() {
        use crate::media::negotiate::MediaNegotiator;

        let bridge = BridgePeerBuilder::new("audio-egress-bridge".to_string())
            .with_rtp_port_range(30500, 30600)
            .build();
        bridge.setup_bridge().await.unwrap();

        let rtp_offer = bridge.rtp_pc().create_offer().await.unwrap();
        bridge.rtp_pc().set_local_description(rtp_offer).unwrap();

        let webrtc_caller = RtpTrackBuilder::new("caller".to_string())
            .with_mode(TransportMode::WebRtc)
            .with_rtp_range(30600, 30700)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();
        let caller_offer = webrtc_caller.local_description().await.unwrap();

        let rtp_callee = RtpTrackBuilder::new("callee".to_string())
            .with_mode(TransportMode::Rtp)
            .with_rtp_range(30700, 30800)
            .with_codec_preference(vec![CodecType::PCMU])
            .build();

        let bridge_rtp_sdp = bridge.rtp_pc().local_description().unwrap().to_sdp_string();
        let callee_answer = rtp_callee.handshake(bridge_rtp_sdp).await.unwrap();
        let callee_desc = SessionDescription::parse(SdpType::Answer, &callee_answer).unwrap();
        bridge
            .rtp_pc()
            .set_remote_description(callee_desc)
            .await
            .unwrap();

        let caller_desc = SessionDescription::parse(SdpType::Offer, &caller_offer).unwrap();
        bridge
            .webrtc_pc()
            .set_remote_description(caller_desc)
            .await
            .unwrap();
        let answer = bridge.webrtc_pc().create_answer().await.unwrap();
        bridge.webrtc_pc().set_local_description(answer).unwrap();
        let caller_answer = bridge
            .webrtc_pc()
            .local_description()
            .unwrap()
            .to_sdp_string();

        bridge
            .stage_audio_forwarding(
                MediaNegotiator::extract_leg_profile(&caller_answer),
                MediaNegotiator::extract_leg_profile(&callee_answer),
                RecLeg::A,
                RecLeg::B,
            )
            .await
            .expect("bridge audio forwarding should stage after negotiation");

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
