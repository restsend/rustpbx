use crate::media::{
    codecs::CodecType,
    negotiate::prefer_audio_codec,
    stream::MediaStreamBuilder,
    track::{rtp::RtpTrackBuilder, webrtc::WebrtcTrack, TrackConfig},
};
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub mod bridge;
pub mod port_allocator;

pub use bridge::*;
pub use port_allocator::*;

/// MediaProxy configuration
#[derive(Clone, Debug)]
pub struct MediaProxyConfig {
    pub local_ip: IpAddr,
    pub external_ip: Option<IpAddr>,
    pub rtp_port_range: (u16, u16),
    pub rtcp_mux: bool,
    pub stun_server: Option<String>,
}

impl Default for MediaProxyConfig {
    fn default() -> Self {
        Self {
            local_ip: "127.0.0.1".parse().unwrap(),
            external_ip: None,
            rtp_port_range: (20000, 30000),
            rtcp_mux: true,
            stun_server: None,
        }
    }
}

/// Main MediaProxy structure that manages media bridging between WebRTC and RTP
#[derive(Clone)]
pub struct MediaProxy {
    config: MediaProxyConfig,
    port_allocator: Arc<PortAllocator>,
    active_bridges: Arc<RwLock<HashSet<String>>>,
}

impl MediaProxy {
    pub fn new(config: MediaProxyConfig) -> Self {
        let port_allocator = Arc::new(PortAllocator::new(
            config.rtp_port_range.0,
            config.rtp_port_range.1,
        ));

        Self {
            config,
            port_allocator,
            active_bridges: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Create a WebRTC to RTP bridge
    pub async fn create_webrtc_to_rtp_bridge(
        &self,
        session_id: String,
        webrtc_offer: &str,
        cancel_token: CancellationToken,
    ) -> Result<WebRtcToRtpBridge> {
        info!("Creating WebRTC to RTP bridge for session: {}", session_id);

        // Parse WebRTC offer to determine codec preferences
        let mut reader = std::io::Cursor::new(webrtc_offer.as_bytes());
        let offer_sdp = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;
        let preferred_codec = prefer_audio_codec(&offer_sdp).unwrap_or(CodecType::PCMU);

        // Allocate RTP port
        let rtp_port = self.port_allocator.allocate_port().await?;

        // Create RTP track
        let track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: preferred_codec.clock_rate() as u32,
            channels: 1,
            server_side_track_id: format!("{}_rtp", session_id),
        };

        let mut rtp_track_builder =
            RtpTrackBuilder::new(format!("{}_rtp", session_id), track_config.clone())
                .with_local_addr(self.config.local_ip)
                .with_rtp_start_port(rtp_port)
                .with_rtp_end_port(rtp_port)
                .with_cancel_token(cancel_token.clone())
                .with_rtcp_mux(self.config.rtcp_mux);

        if let Some(external_ip) = self.config.external_ip {
            rtp_track_builder = rtp_track_builder.with_external_addr(external_ip);
        }

        if let Some(ref stun_server) = self.config.stun_server {
            rtp_track_builder = rtp_track_builder.with_stun_server(stun_server.clone());
        }

        let rtp_track = rtp_track_builder.build().await?;

        // Create WebRTC track
        let webrtc_track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: 16000, // WebRTC typically uses 16kHz
            channels: 1,
            server_side_track_id: format!("{}_webrtc", session_id),
        };

        let webrtc_track = WebrtcTrack::new(
            cancel_token.clone(),
            session_id.clone(),
            webrtc_track_config,
        );

        // Create media stream
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = MediaStreamBuilder::new(event_sender)
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.clone())
            .build();

        let media_stream = Arc::new(media_stream);

        // Add tracks to stream
        media_stream.update_track(Box::new(webrtc_track)).await;
        media_stream.update_track(Box::new(rtp_track)).await;

        let bridge = WebRtcToRtpBridge::new(
            session_id.clone(),
            media_stream,
            preferred_codec,
            rtp_port,
            self.config.local_ip,
            self.config.external_ip,
        );

        // Track active bridge
        self.active_bridges.write().await.insert(session_id.clone());

        Ok(bridge)
    }

    /// Create a SIP to SIP NAT bridge for handling private IP addresses
    pub async fn create_sip_nat_bridge(
        &self,
        session_id: String,
        offer_sdp: &str,
        cancel_token: CancellationToken,
    ) -> Result<SipNatBridge> {
        info!("Creating SIP NAT bridge for session: {}", session_id);

        // Parse offer SDP to get codec and media information
        let mut reader = std::io::Cursor::new(offer_sdp.as_bytes());
        let offer = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;
        let preferred_codec = prefer_audio_codec(&offer).unwrap_or(CodecType::PCMU);

        // Extract original media information
        let peer_media = crate::media::negotiate::select_peer_media(&offer, "audio")
            .ok_or_else(|| anyhow!("No audio media found in SIP offer"))?;

        // Allocate new RTP port for NAT traversal
        let local_rtp_port = self.port_allocator.allocate_port().await?;

        // Create RTP track for receiving from caller
        let caller_track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: preferred_codec.clock_rate() as u32,
            channels: 1,
            server_side_track_id: format!("{}_caller_rtp", session_id),
        };

        let mut caller_rtp_builder =
            RtpTrackBuilder::new(format!("{}_caller_rtp", session_id), caller_track_config)
                .with_local_addr(self.config.local_ip)
                .with_rtp_start_port(local_rtp_port)
                .with_rtp_end_port(local_rtp_port)
                .with_cancel_token(cancel_token.clone())
                .with_rtcp_mux(self.config.rtcp_mux);

        if let Some(external_ip) = self.config.external_ip {
            caller_rtp_builder = caller_rtp_builder.with_external_addr(external_ip);
        }

        let caller_rtp_track = caller_rtp_builder.build().await?;

        // Allocate port for callee side
        let callee_rtp_port = self.port_allocator.allocate_port().await?;

        // Create RTP track for sending to callee
        let callee_track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: preferred_codec.clock_rate() as u32,
            channels: 1,
            server_side_track_id: format!("{}_callee_rtp", session_id),
        };

        let mut callee_rtp_builder =
            RtpTrackBuilder::new(format!("{}_callee_rtp", session_id), callee_track_config)
                .with_local_addr(self.config.local_ip)
                .with_rtp_start_port(callee_rtp_port)
                .with_rtp_end_port(callee_rtp_port)
                .with_cancel_token(cancel_token.clone())
                .with_rtcp_mux(self.config.rtcp_mux);

        if let Some(external_ip) = self.config.external_ip {
            callee_rtp_builder = callee_rtp_builder.with_external_addr(external_ip);
        }

        let callee_rtp_track = callee_rtp_builder.build().await?;

        // Create media stream
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = MediaStreamBuilder::new(event_sender)
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.clone())
            .build();

        let media_stream = Arc::new(media_stream);

        // Add tracks to stream
        media_stream.update_track(Box::new(caller_rtp_track)).await;
        media_stream.update_track(Box::new(callee_rtp_track)).await;

        let bridge = SipNatBridge::new(
            session_id.clone(),
            media_stream,
            preferred_codec,
            local_rtp_port,
            callee_rtp_port,
            self.config.local_ip,
            self.config.external_ip,
            peer_media,
        );

        // Track active bridge
        self.active_bridges.write().await.insert(session_id.clone());

        Ok(bridge)
    }

    /// Create an RTP to WebRTC bridge
    pub async fn create_rtp_to_webrtc_bridge(
        &self,
        session_id: String,
        rtp_offer: &str,
        cancel_token: CancellationToken,
    ) -> Result<RtpToWebRtcBridge> {
        info!("Creating RTP to WebRTC bridge for session: {}", session_id);

        // Parse RTP offer SDP
        let mut reader = std::io::Cursor::new(rtp_offer.as_bytes());
        let offer_sdp = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;
        let preferred_codec = prefer_audio_codec(&offer_sdp).unwrap_or(CodecType::PCMU);

        // Create WebRTC track
        let webrtc_track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: 16000,
            channels: 1,
            server_side_track_id: format!("{}_webrtc", session_id),
        };

        let webrtc_track = WebrtcTrack::new(
            cancel_token.clone(),
            session_id.clone(),
            webrtc_track_config,
        );

        // Create RTP track that will receive from the remote side
        let rtp_track_config = TrackConfig {
            codec: preferred_codec,
            ptime: Duration::from_millis(20),
            samplerate: preferred_codec.clock_rate() as u32,
            channels: 1,
            server_side_track_id: format!("{}_rtp", session_id),
        };

        let rtp_port = self.port_allocator.allocate_port().await?;
        let mut rtp_track_builder =
            RtpTrackBuilder::new(format!("{}_rtp", session_id), rtp_track_config)
                .with_local_addr(self.config.local_ip)
                .with_rtp_start_port(rtp_port)
                .with_rtp_end_port(rtp_port)
                .with_cancel_token(cancel_token.clone())
                .with_rtcp_mux(self.config.rtcp_mux);

        if let Some(external_ip) = self.config.external_ip {
            rtp_track_builder = rtp_track_builder.with_external_addr(external_ip);
        }

        let rtp_track = rtp_track_builder.build().await?;

        // Create media stream
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = MediaStreamBuilder::new(event_sender)
            .with_id(session_id.clone())
            .with_cancel_token(cancel_token.clone())
            .build();

        let media_stream = Arc::new(media_stream);

        // Add tracks to stream
        media_stream.update_track(Box::new(webrtc_track)).await;
        media_stream.update_track(Box::new(rtp_track)).await;

        let bridge = RtpToWebRtcBridge::new(
            session_id.clone(),
            media_stream,
            preferred_codec,
            rtp_port,
            self.config.local_ip,
            self.config.external_ip,
        );

        // Track active bridge
        self.active_bridges.write().await.insert(session_id.clone());

        Ok(bridge)
    }

    /// Remove a bridge and cleanup resources
    pub async fn remove_bridge(&self, session_id: &str) -> Result<()> {
        info!("Removing bridge for session: {}", session_id);

        // Stop tracking the bridge
        self.active_bridges.write().await.remove(session_id);

        Ok(())
    }

    /// Get list of active bridge session IDs
    pub async fn get_active_bridges(&self) -> Vec<String> {
        self.active_bridges.read().await.iter().cloned().collect()
    }

    /// Get port allocator statistics
    pub async fn get_port_stats(&self) -> PortAllocatorStats {
        self.port_allocator.get_stats().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_media_proxy_creation() {
        let config = MediaProxyConfig::default();
        let media_proxy = MediaProxy::new(config);

        // Test that we can get port stats
        let stats = media_proxy.get_port_stats().await;
        assert_eq!(stats.allocated_ports, 0);
        assert!(stats.available_ports > 0);
    }

    #[tokio::test]
    async fn test_webrtc_to_rtp_bridge_creation() {
        let config = MediaProxyConfig {
            local_ip: "127.0.0.1".parse().unwrap(),
            external_ip: Some("203.0.113.1".parse().unwrap()),
            rtp_port_range: (20000, 20010),
            rtcp_mux: true,
            stun_server: None,
        };

        let media_proxy = MediaProxy::new(config);
        let cancel_token = CancellationToken::new();

        let webrtc_offer = r#"v=0
o=- 0 0 IN IP4 192.168.1.1
s=-
t=0 0
m=audio 5004 UDP/TLS/RTP/SAVPF 0
a=rtpmap:0 PCMU/8000
a=ice-ufrag:4ZcD
a=ice-pwd:2/1mSga9rYFnRU3vBDo7E8VH
a=fingerprint:sha-256 75:74:5A:A6:A4:E5:52:F4:A7:67:4C:01:C7:EE:91:3F:21:3D:A2:E3:53:54:9A:24:C9:58:A1:0C:AE:41:95:7B
a=setup:actpass"#;

        let bridge = media_proxy
            .create_webrtc_to_rtp_bridge("test_session".to_string(), webrtc_offer, cancel_token)
            .await
            .unwrap();

        assert_eq!(bridge.session_id, "test_session");
        assert_eq!(bridge.codec, CodecType::PCMU);

        // Test SDP generation
        let sip_sdp = bridge.generate_sip_sdp_answer(webrtc_offer).unwrap();
        assert!(sip_sdp.contains("203.0.113.1")); // External IP
        assert!(sip_sdp.contains("PCMU/8000")); // Codec

        // Cleanup
        bridge.stop().await.unwrap();
        media_proxy.remove_bridge("test_session").await.unwrap();
    }

    #[tokio::test]
    async fn test_sip_nat_bridge_creation() {
        let config = MediaProxyConfig {
            local_ip: "127.0.0.1".parse().unwrap(),
            external_ip: Some("203.0.113.1".parse().unwrap()),
            rtp_port_range: (20000, 20010),
            rtcp_mux: true,
            stun_server: None,
        };

        let media_proxy = MediaProxy::new(config);
        let cancel_token = CancellationToken::new();

        let sip_offer = r#"v=0
o=- 0 0 IN IP4 192.168.1.100
s=-
t=0 0
m=audio 5004 RTP/AVP 0
a=rtpmap:0 PCMU/8000"#;

        let bridge = media_proxy
            .create_sip_nat_bridge("nat_session".to_string(), sip_offer, cancel_token)
            .await
            .unwrap();

        assert_eq!(bridge.session_id, "nat_session");
        assert_eq!(bridge.codec, CodecType::PCMU);

        // Test SDP generation
        let caller_answer = bridge.generate_caller_sdp_answer(sip_offer).unwrap();
        assert!(caller_answer.contains("203.0.113.1")); // External IP
        assert!(caller_answer.contains("PCMU/8000")); // Codec

        let callee_invite = bridge.generate_callee_invite_sdp(sip_offer).unwrap();
        assert!(callee_invite.contains("203.0.113.1")); // External IP
        assert!(callee_invite.contains("PCMU/8000")); // Codec

        // Cleanup
        bridge.stop().await.unwrap();
        media_proxy.remove_bridge("nat_session").await.unwrap();
    }

    #[tokio::test]
    async fn test_active_bridges_tracking() {
        let config = MediaProxyConfig::default();
        let media_proxy = MediaProxy::new(config);
        let cancel_token = CancellationToken::new();

        let webrtc_offer = r#"v=0
o=- 0 0 IN IP4 192.168.1.1
s=-
t=0 0
m=audio 5004 UDP/TLS/RTP/SAVPF 0
a=rtpmap:0 PCMU/8000
a=ice-ufrag:4ZcD
a=ice-pwd:test"#;

        // Initially no active bridges
        assert_eq!(media_proxy.get_active_bridges().await.len(), 0);

        // Create a bridge
        let bridge1 = media_proxy
            .create_webrtc_to_rtp_bridge("session1".to_string(), webrtc_offer, cancel_token.clone())
            .await
            .unwrap();

        // Should have one active bridge
        let active = media_proxy.get_active_bridges().await;
        assert_eq!(active.len(), 1);
        assert!(active.contains(&"session1".to_string()));

        // Create another bridge
        let bridge2 = media_proxy
            .create_webrtc_to_rtp_bridge("session2".to_string(), webrtc_offer, cancel_token.clone())
            .await
            .unwrap();

        // Should have two active bridges
        let active = media_proxy.get_active_bridges().await;
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"session1".to_string()));
        assert!(active.contains(&"session2".to_string()));

        // Remove one bridge
        media_proxy.remove_bridge("session1").await.unwrap();
        let active = media_proxy.get_active_bridges().await;
        assert_eq!(active.len(), 1);
        assert!(active.contains(&"session2".to_string()));

        // Cleanup
        bridge1.stop().await.unwrap();
        bridge2.stop().await.unwrap();
        media_proxy.remove_bridge("session2").await.unwrap();
    }
}
