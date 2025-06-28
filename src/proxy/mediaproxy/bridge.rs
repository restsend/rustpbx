use crate::media::{codecs::CodecType, negotiate::PeerMedia, stream::MediaStream};
use anyhow::Result;
use std::net::IpAddr;
use std::sync::Arc;
use tracing::{error, info};

/// Bridge for WebRTC to RTP media conversion
#[derive(Clone)]
pub struct WebRtcToRtpBridge {
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub codec: CodecType,
    pub rtp_port: u16,
    pub local_ip: IpAddr,
    pub external_ip: Option<IpAddr>,
}

impl WebRtcToRtpBridge {
    pub fn new(
        session_id: String,
        media_stream: Arc<MediaStream>,
        codec: CodecType,
        rtp_port: u16,
        local_ip: IpAddr,
        external_ip: Option<IpAddr>,
    ) -> Self {
        Self {
            session_id,
            media_stream,
            codec,
            rtp_port,
            local_ip,
            external_ip,
        }
    }

    /// Set SIP answer SDP to configure the RTP track's remote address
    /// This method should be called after receiving SIP 200 OK response
    pub async fn set_sip_answer_sdp(&self, sip_answer: &str) -> Result<()> {
        info!(
            "Setting SIP answer SDP for WebRTC to RTP bridge session: {}",
            self.session_id
        );

        // Use MediaStream's method to set remote SDP for all RTP tracks
        self.media_stream
            .set_rtp_tracks_remote_sdp(sip_answer)
            .await?;

        info!(
            "Successfully set remote SDP for WebRTC to RTP bridge session: {}",
            self.session_id
        );

        Ok(())
    }

    /// Generate SDP answer for SIP client based on the allocated RTP track
    pub fn generate_sip_sdp_answer(&self, offer_sdp: &str) -> Result<String> {
        info!("Generating SIP SDP answer for session: {}", self.session_id);

        // Use external IP if available, otherwise use local IP
        let advertised_ip = self.external_ip.unwrap_or(self.local_ip);

        // Parse offer to get session details
        let mut reader = std::io::Cursor::new(offer_sdp.as_bytes());
        let _offer = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;

        // Generate answer SDP
        let answer_sdp = format!(
            "v=0\r\n\
             o=rustpbx 0 0 IN IP4 {}\r\n\
             s=rustpbx\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=audio {} RTP/AVP {}\r\n\
             a=rtpmap:{} {}\r\n\
             a=sendrecv\r\n",
            advertised_ip,
            advertised_ip,
            self.rtp_port,
            self.codec.payload_type(),
            self.codec.payload_type(),
            self.codec.rtpmap(),
        );

        Ok(answer_sdp)
    }

    /// Start the media bridging process
    pub async fn start(&self) -> Result<()> {
        info!(
            "Starting WebRTC to RTP bridge for session: {}",
            self.session_id
        );

        // Start the media stream service
        let stream_clone = self.media_stream.clone();
        let session_id_clone = self.session_id.clone();

        tokio::spawn(async move {
            if let Err(e) = stream_clone.serve().await {
                error!(
                    "WebRTC to RTP bridge error for session {}: {}",
                    session_id_clone, e
                );
            }
        });

        Ok(())
    }

    /// Stop the bridge and cleanup resources
    pub async fn stop(&self) -> Result<()> {
        info!(
            "Stopping WebRTC to RTP bridge for session: {}",
            self.session_id
        );

        self.media_stream
            .stop(Some("bridge_stop".to_string()), Some("system".to_string()));

        self.media_stream.cleanup().await?;
        Ok(())
    }

    /// Get bridge statistics
    pub fn get_stats(&self) -> BridgeStats {
        BridgeStats {
            session_id: self.session_id.clone(),
            bridge_type: BridgeType::WebRtcToRtp,
            codec: self.codec,
            rtp_port: self.rtp_port,
            local_ip: self.local_ip,
            external_ip: self.external_ip,
        }
    }
}

/// Bridge for RTP to WebRTC media conversion
#[derive(Clone)]
pub struct RtpToWebRtcBridge {
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub codec: CodecType,
    pub rtp_port: u16,
    pub local_ip: IpAddr,
    pub external_ip: Option<IpAddr>,
}

impl RtpToWebRtcBridge {
    pub fn new(
        session_id: String,
        media_stream: Arc<MediaStream>,
        codec: CodecType,
        rtp_port: u16,
        local_ip: IpAddr,
        external_ip: Option<IpAddr>,
    ) -> Self {
        Self {
            session_id,
            media_stream,
            codec,
            rtp_port,
            local_ip,
            external_ip,
        }
    }

    /// Start the media bridging process
    pub async fn start(&self) -> Result<()> {
        info!(
            "Starting RTP to WebRTC bridge for session: {}",
            self.session_id
        );

        // Start the media stream service
        let stream_clone = self.media_stream.clone();
        let session_id_clone = self.session_id.clone();

        tokio::spawn(async move {
            if let Err(e) = stream_clone.serve().await {
                error!(
                    "RTP to WebRTC bridge error for session {}: {}",
                    session_id_clone, e
                );
            }
        });

        Ok(())
    }

    /// Stop the bridge and cleanup resources
    pub async fn stop(&self) -> Result<()> {
        info!(
            "Stopping RTP to WebRTC bridge for session: {}",
            self.session_id
        );

        self.media_stream
            .stop(Some("bridge_stop".to_string()), Some("system".to_string()));

        self.media_stream.cleanup().await?;
        Ok(())
    }

    /// Get bridge statistics
    pub fn get_stats(&self) -> BridgeStats {
        BridgeStats {
            session_id: self.session_id.clone(),
            bridge_type: BridgeType::RtpToWebRtc,
            codec: self.codec,
            rtp_port: self.rtp_port,
            local_ip: self.local_ip,
            external_ip: self.external_ip,
        }
    }
}

/// Bridge for SIP to SIP NAT traversal
#[derive(Clone)]
pub struct SipNatBridge {
    pub session_id: String,
    pub media_stream: Arc<MediaStream>,
    pub codec: CodecType,
    pub caller_rtp_port: u16,
    pub callee_rtp_port: u16,
    pub local_ip: IpAddr,
    pub external_ip: Option<IpAddr>,
    pub original_peer_media: PeerMedia,
}

impl SipNatBridge {
    pub fn new(
        session_id: String,
        media_stream: Arc<MediaStream>,
        codec: CodecType,
        caller_rtp_port: u16,
        callee_rtp_port: u16,
        local_ip: IpAddr,
        external_ip: Option<IpAddr>,
        original_peer_media: PeerMedia,
    ) -> Self {
        Self {
            session_id,
            media_stream,
            codec,
            caller_rtp_port,
            callee_rtp_port,
            local_ip,
            external_ip,
            original_peer_media,
        }
    }

    /// Set caller's original SDP to configure the caller RTP track's remote address
    /// This should be called with the original INVITE SDP
    pub async fn set_caller_remote_sdp(&self, caller_sdp: &str) -> Result<()> {
        info!(
            "Setting caller remote SDP for SIP NAT bridge session: {}",
            self.session_id
        );

        // Find the caller RTP track by ID and set its remote description
        let caller_track_id = format!("{}_caller_rtp", self.session_id);
        self.media_stream
            .set_track_remote_sdp(&caller_track_id, caller_sdp)
            .await?;

        info!(
            "Successfully set caller remote SDP for SIP NAT bridge session: {}",
            self.session_id
        );

        Ok(())
    }

    /// Set callee's answer SDP to configure the callee RTP track's remote address
    /// This should be called after receiving SIP 200 OK response
    pub async fn set_callee_answer_sdp(&self, callee_answer: &str) -> Result<()> {
        info!(
            "Setting callee answer SDP for SIP NAT bridge session: {}",
            self.session_id
        );

        // Find the callee RTP track by ID and set its remote description
        let callee_track_id = format!("{}_callee_rtp", self.session_id);
        self.media_stream
            .set_track_remote_sdp(&callee_track_id, callee_answer)
            .await?;

        info!(
            "Successfully set callee answer SDP for SIP NAT bridge session: {}",
            self.session_id
        );

        Ok(())
    }

    /// Generate modified SDP for caller (replacing private IP with public IP)
    pub fn generate_caller_sdp_answer(&self, offer_sdp: &str) -> Result<String> {
        info!(
            "Generating caller SDP answer for NAT session: {}",
            self.session_id
        );

        // Use external IP if available, otherwise use local IP
        let advertised_ip = self.external_ip.unwrap_or(self.local_ip);

        // Parse offer to get session details
        let mut reader = std::io::Cursor::new(offer_sdp.as_bytes());
        let _offer = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;

        // Generate answer SDP with our allocated port
        let answer_sdp = format!(
            "v=0\r\n\
             o=rustpbx 0 0 IN IP4 {}\r\n\
             s=rustpbx\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=audio {} RTP/AVP {}\r\n\
             a=rtpmap:{} {}\r\n\
             a=sendrecv\r\n",
            advertised_ip,
            advertised_ip,
            self.caller_rtp_port,
            self.codec.payload_type(),
            self.codec.payload_type(),
            self.codec.rtpmap(),
        );

        Ok(answer_sdp)
    }

    /// Generate modified SDP for callee (using our allocated port)
    pub fn generate_callee_invite_sdp(&self, original_offer: &str) -> Result<String> {
        info!(
            "Generating callee INVITE SDP for NAT session: {}",
            self.session_id
        );

        // Use external IP if available, otherwise use local IP
        let advertised_ip = self.external_ip.unwrap_or(self.local_ip);

        // Parse original offer to get session details
        let mut reader = std::io::Cursor::new(original_offer.as_bytes());
        let _offer = webrtc::sdp::SessionDescription::unmarshal(&mut reader)?;

        // Generate modified INVITE SDP with our allocated port
        let invite_sdp = format!(
            "v=0\r\n\
             o=rustpbx 0 0 IN IP4 {}\r\n\
             s=rustpbx\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=audio {} RTP/AVP {}\r\n\
             a=rtpmap:{} {}\r\n\
             a=sendrecv\r\n",
            advertised_ip,
            advertised_ip,
            self.callee_rtp_port,
            self.codec.payload_type(),
            self.codec.payload_type(),
            self.codec.rtpmap(),
        );

        Ok(invite_sdp)
    }

    /// Start the NAT bridging process
    pub async fn start(&self) -> Result<()> {
        info!("Starting SIP NAT bridge for session: {}", self.session_id);

        // Start the media stream service
        let stream_clone = self.media_stream.clone();
        let session_id_clone = self.session_id.clone();

        tokio::spawn(async move {
            if let Err(e) = stream_clone.serve().await {
                error!(
                    "SIP NAT bridge error for session {}: {}",
                    session_id_clone, e
                );
            }
        });

        Ok(())
    }

    /// Stop the bridge and cleanup resources
    pub async fn stop(&self) -> Result<()> {
        info!("Stopping SIP NAT bridge for session: {}", self.session_id);

        self.media_stream
            .stop(Some("bridge_stop".to_string()), Some("system".to_string()));

        self.media_stream.cleanup().await?;
        Ok(())
    }

    /// Get bridge statistics
    pub fn get_stats(&self) -> BridgeStats {
        BridgeStats {
            session_id: self.session_id.clone(),
            bridge_type: BridgeType::SipNat,
            codec: self.codec,
            rtp_port: self.caller_rtp_port, // Use caller port as primary
            local_ip: self.local_ip,
            external_ip: self.external_ip,
        }
    }
}

/// Bridge type enumeration
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeType {
    WebRtcToRtp,
    RtpToWebRtc,
    SipNat,
}

/// Bridge statistics structure
#[derive(Clone, Debug)]
pub struct BridgeStats {
    pub session_id: String,
    pub bridge_type: BridgeType,
    pub codec: CodecType,
    pub rtp_port: u16,
    pub local_ip: IpAddr,
    pub external_ip: Option<IpAddr>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::stream::MediaStreamBuilder;
    use crate::media::track::TrackConfig;
    use std::time::Duration;

    #[tokio::test]
    async fn test_webrtc_to_rtp_bridge_creation() {
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("test_session".to_string())
                .build(),
        );

        let bridge = WebRtcToRtpBridge::new(
            "test_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20000,
            "127.0.0.1".parse().unwrap(),
            None,
        );

        assert_eq!(bridge.session_id, "test_session");
        assert_eq!(bridge.codec, CodecType::PCMU);
        assert_eq!(bridge.rtp_port, 20000);
    }

    #[tokio::test]
    async fn test_sip_sdp_generation() {
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("test_session".to_string())
                .build(),
        );

        let bridge = WebRtcToRtpBridge::new(
            "test_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20000,
            "127.0.0.1".parse().unwrap(),
            Some("203.0.113.1".parse().unwrap()),
        );

        let offer_sdp =
            "v=0\r\no=- 0 0 IN IP4 192.168.1.1\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";
        let answer_sdp = bridge.generate_sip_sdp_answer(offer_sdp).unwrap();

        assert!(answer_sdp.contains("203.0.113.1")); // External IP should be used
        assert!(answer_sdp.contains("20000")); // RTP port
        assert!(answer_sdp.contains("PCMU/8000")); // Codec info
    }

    #[tokio::test]
    async fn test_bridge_stats() {
        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("test_session".to_string())
                .build(),
        );

        let bridge = WebRtcToRtpBridge::new(
            "test_session".to_string(),
            media_stream,
            CodecType::G722,
            20002,
            "127.0.0.1".parse().unwrap(),
            None,
        );

        let stats = bridge.get_stats();
        assert_eq!(stats.session_id, "test_session");
        assert_eq!(stats.bridge_type, BridgeType::WebRtcToRtp);
        assert_eq!(stats.codec, CodecType::G722);
        assert_eq!(stats.rtp_port, 20002);
    }

    #[tokio::test]
    async fn test_sip_nat_bridge_creation() {
        use crate::media::negotiate::PeerMedia;

        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("nat_session".to_string())
                .build(),
        );

        let peer_media = PeerMedia {
            rtp_addr: "192.168.1.100".to_string(),
            rtp_port: 5004,
            rtcp_addr: "192.168.1.100".to_string(),
            rtcp_port: 5005,
            rtcp_mux: false,
            codecs: vec![CodecType::PCMU],
        };

        let bridge = SipNatBridge::new(
            "nat_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20004,
            20006,
            "127.0.0.1".parse().unwrap(),
            Some("203.0.113.1".parse().unwrap()),
            peer_media,
        );

        assert_eq!(bridge.session_id, "nat_session");
        assert_eq!(bridge.codec, CodecType::PCMU);
        assert_eq!(bridge.caller_rtp_port, 20004);
        assert_eq!(bridge.callee_rtp_port, 20006);
    }

    #[tokio::test]
    async fn test_sip_nat_bridge_sdp_generation() {
        use crate::media::negotiate::PeerMedia;

        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("nat_session".to_string())
                .build(),
        );

        let peer_media = PeerMedia {
            rtp_addr: "192.168.1.100".to_string(),
            rtp_port: 5004,
            rtcp_addr: "192.168.1.100".to_string(),
            rtcp_port: 5005,
            rtcp_mux: false,
            codecs: vec![CodecType::PCMU],
        };

        let bridge = SipNatBridge::new(
            "nat_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20004,
            20006,
            "127.0.0.1".parse().unwrap(),
            Some("203.0.113.1".parse().unwrap()),
            peer_media,
        );

        let offer_sdp =
            "v=0\r\no=- 0 0 IN IP4 192.168.1.100\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";

        // Test caller SDP answer generation
        let caller_answer = bridge.generate_caller_sdp_answer(offer_sdp).unwrap();
        assert!(caller_answer.contains("203.0.113.1")); // External IP should be used
        assert!(caller_answer.contains("20004")); // Caller RTP port
        assert!(caller_answer.contains("PCMU/8000")); // Codec info

        // Test callee invite SDP generation
        let callee_invite = bridge.generate_callee_invite_sdp(offer_sdp).unwrap();
        assert!(callee_invite.contains("203.0.113.1")); // External IP should be used
        assert!(callee_invite.contains("20006")); // Callee RTP port
        assert!(callee_invite.contains("PCMU/8000")); // Codec info
    }

    #[tokio::test]
    async fn test_webrtc_to_rtp_bridge_set_sip_answer() {
        use crate::media::track::rtp::RtpTrackBuilder;

        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("test_session".to_string())
                .build(),
        );

        // Create and add an RTP track to the media stream
        let track_config = TrackConfig {
            codec: CodecType::PCMU,
            ptime: Duration::from_millis(20),
            samplerate: 8000,
            channels: 1,
            server_side_track_id: "test_rtp_track".to_string(),
        };

        let rtp_track = RtpTrackBuilder::new("test_rtp_track".to_string(), track_config)
            .with_local_addr("127.0.0.1".parse().unwrap())
            .with_rtp_start_port(20000)
            .with_rtp_end_port(20000)
            .build()
            .await
            .expect("Failed to build RTP track");

        media_stream.update_track(Box::new(rtp_track)).await;

        let bridge = WebRtcToRtpBridge::new(
            "test_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20000,
            "127.0.0.1".parse().unwrap(),
            None,
        );

        // Test SIP answer SDP with valid SDP
        let sip_answer = "v=0\r\n\
                          o=- 0 0 IN IP4 192.168.1.100\r\n\
                          s=-\r\n\
                          c=IN IP4 192.168.1.100\r\n\
                          t=0 0\r\n\
                          m=audio 5004 RTP/AVP 0\r\n\
                          a=rtpmap:0 PCMU/8000\r\n\
                          a=sendrecv\r\n";

        // This should succeed and set the remote address for the RTP track
        let result = bridge.set_sip_answer_sdp(sip_answer).await;
        assert!(result.is_ok(), "Failed to set SIP answer SDP: {:?}", result);
    }

    #[tokio::test]
    async fn test_sip_nat_bridge_remote_sdp_setting() {
        use crate::media::negotiate::PeerMedia;
        use crate::media::track::rtp::RtpTrackBuilder;

        let (event_sender, _) = tokio::sync::broadcast::channel(100);
        let media_stream = Arc::new(
            MediaStreamBuilder::new(event_sender)
                .with_id("nat_session".to_string())
                .build(),
        );

        // Create caller RTP track
        let caller_track_config = TrackConfig {
            codec: CodecType::PCMU,
            ptime: Duration::from_millis(20),
            samplerate: 8000,
            channels: 1,
            server_side_track_id: "nat_session_caller_rtp".to_string(),
        };

        let caller_rtp_track =
            RtpTrackBuilder::new("nat_session_caller_rtp".to_string(), caller_track_config)
                .with_local_addr("127.0.0.1".parse().unwrap())
                .with_rtp_start_port(20004)
                .with_rtp_end_port(20004)
                .build()
                .await
                .expect("Failed to build caller RTP track");

        // Create callee RTP track
        let callee_track_config = TrackConfig {
            codec: CodecType::PCMU,
            ptime: Duration::from_millis(20),
            samplerate: 8000,
            channels: 1,
            server_side_track_id: "nat_session_callee_rtp".to_string(),
        };

        let callee_rtp_track =
            RtpTrackBuilder::new("nat_session_callee_rtp".to_string(), callee_track_config)
                .with_local_addr("127.0.0.1".parse().unwrap())
                .with_rtp_start_port(20006)
                .with_rtp_end_port(20006)
                .build()
                .await
                .expect("Failed to build callee RTP track");

        // Add tracks to media stream
        media_stream.update_track(Box::new(caller_rtp_track)).await;
        media_stream.update_track(Box::new(callee_rtp_track)).await;

        let peer_media = PeerMedia {
            rtp_addr: "192.168.1.100".to_string(),
            rtp_port: 5004,
            rtcp_addr: "192.168.1.100".to_string(),
            rtcp_port: 5005,
            rtcp_mux: false,
            codecs: vec![CodecType::PCMU],
        };

        let bridge = SipNatBridge::new(
            "nat_session".to_string(),
            media_stream,
            CodecType::PCMU,
            20004,
            20006,
            "127.0.0.1".parse().unwrap(),
            Some("203.0.113.1".parse().unwrap()),
            peer_media,
        );

        // Test setting caller remote SDP
        let caller_sdp = "v=0\r\n\
                          o=- 0 0 IN IP4 192.168.1.100\r\n\
                          s=-\r\n\
                          c=IN IP4 192.168.1.100\r\n\
                          t=0 0\r\n\
                          m=audio 5004 RTP/AVP 0\r\n\
                          a=rtpmap:0 PCMU/8000\r\n\
                          a=sendrecv\r\n";

        let result = bridge.set_caller_remote_sdp(caller_sdp).await;
        assert!(
            result.is_ok(),
            "Failed to set caller remote SDP: {:?}",
            result
        );

        // Test setting callee answer SDP
        let callee_answer = "v=0\r\n\
                            o=- 0 0 IN IP4 192.168.1.200\r\n\
                            s=-\r\n\
                            c=IN IP4 192.168.1.200\r\n\
                            t=0 0\r\n\
                            m=audio 6004 RTP/AVP 0\r\n\
                            a=rtpmap:0 PCMU/8000\r\n\
                            a=sendrecv\r\n";

        let result = bridge.set_callee_answer_sdp(callee_answer).await;
        assert!(
            result.is_ok(),
            "Failed to set callee answer SDP: {:?}",
            result
        );
    }
}
