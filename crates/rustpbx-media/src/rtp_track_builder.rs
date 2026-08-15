use audio_codec::CodecType;
use rustrtc::{
    IceServer, IceTransportPolicy, RtcConfiguration, TransportMode, config::BufferDropStrategy,
    config::VideoCapability,
};
use tokio_util::sync::CancellationToken;

use crate::RtcTrack;
use crate::negotiate;

pub struct RtpTrackBuilder {
    track_id: String,
    cancel_token: Option<CancellationToken>,
    external_ip: Option<String>,
    bind_ip: Option<String>,
    rtp_start_port: Option<u16>,
    rtp_end_port: Option<u16>,
    mode: TransportMode,
    rtp_map: Vec<negotiate::CodecInfo>,
    video_capabilities: Vec<VideoCapability>,
    enable_latching: bool,
    probation_max_packets: Option<u8>,
    ice_servers: Vec<IceServer>,
    cname: Option<String>,
}

impl RtpTrackBuilder {
    pub fn new(track_id: String) -> Self {
        Self {
            track_id,
            cancel_token: None,
            external_ip: None,
            bind_ip: None,
            rtp_start_port: None,
            rtp_end_port: None,
            mode: TransportMode::Rtp,
            enable_latching: false,
            probation_max_packets: None,
            ice_servers: Vec::new(),
            rtp_map: vec![
                CodecType::Opus,
                CodecType::G729,
                CodecType::G722,
                CodecType::PCMU,
                CodecType::PCMA,
                CodecType::TelephoneEvent,
            ]
            .into_iter()
            .map(negotiate::MediaNegotiator::codec_info_for_type)
            .collect(),
            video_capabilities: Vec::new(),
            cname: None,
        }
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }
    pub fn with_rtp_range(mut self, start: u16, end: u16) -> Self {
        self.rtp_start_port = Some(start);
        self.rtp_end_port = Some(end);
        self
    }

    pub fn with_mode(mut self, mode: TransportMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_external_ip(mut self, addr: String) -> Self {
        self.external_ip = Some(addr);
        self
    }

    pub fn with_bind_ip(mut self, addr: String) -> Self {
        self.bind_ip = Some(addr);
        self
    }

    pub fn with_codec_preference(mut self, codecs: Vec<CodecType>) -> Self {
        self.rtp_map = codecs
            .into_iter()
            .map(negotiate::MediaNegotiator::codec_info_for_type)
            .collect();
        self
    }

    pub fn with_codec_info(mut self, codecs: Vec<negotiate::CodecInfo>) -> Self {
        self.rtp_map = codecs;
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

    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    pub fn with_video_capabilities(mut self, caps: Vec<VideoCapability>) -> Self {
        self.video_capabilities = caps;
        self
    }

    pub fn with_cname(mut self, cname: String) -> Self {
        self.cname = Some(cname);
        self
    }

    pub fn build(self) -> RtcTrack {
        let sdp_compatibility = match self.mode {
            TransportMode::WebRtc => rustrtc::config::SdpCompatibilityMode::Standard,
            TransportMode::Rtp | TransportMode::Srtp => {
                rustrtc::config::SdpCompatibilityMode::LegacySip
            }
        };

        let has_turn_server = self.ice_servers.iter().any(|server| {
            server.urls.iter().any(|url| {
                let u = url.trim_start().to_ascii_lowercase();
                u.starts_with("turn:") || u.starts_with("turns:")
            })
        });
        let audio_capabilities: Vec<_> = if self.rtp_map.is_empty() {
            match self.mode {
                TransportMode::WebRtc => negotiate::MediaNegotiator::default_webrtc_codecs(),
                TransportMode::Rtp | TransportMode::Srtp => {
                    negotiate::MediaNegotiator::default_rtp_codecs()
                }
            }
            .into_iter()
            .filter_map(|codec| {
                negotiate::MediaNegotiator::codec_info_for_type(codec).to_audio_capability()
            })
            .collect()
        } else {
            self.rtp_map
                .iter()
                .filter_map(|codec| codec.to_audio_capability())
                .collect()
        };

        let bind_ip = if matches!(self.mode, TransportMode::Rtp | TransportMode::Srtp) {
            self.bind_ip
        } else {
            None
        };

        let config = RtcConfiguration {
            ice_servers: self.ice_servers,
            ice_transport_policy: if self.mode == TransportMode::WebRtc && has_turn_server {
                IceTransportPolicy::Relay
            } else {
                IceTransportPolicy::All
            },
            transport_mode: self.mode,
            rtp_start_port: self.rtp_start_port,
            rtp_end_port: self.rtp_end_port,
            external_ip: self.external_ip,
            bind_ip,
            enable_latching: self.enable_latching,
            probation_max_packets: self.probation_max_packets,
            media_capabilities: Some(rustrtc::config::MediaCapabilities {
                audio: audio_capabilities,
                video: self.video_capabilities.clone(),
                application: None,
                image: vec![],
            }),
            ssrc_start: rand::random::<u32>(),
            sdp_compatibility,
            cname: self.cname,
            buffer_drop_strategy: BufferDropStrategy::DropOldest,
            // 500 reserved ~95KB per leg that is almost never reached; 100
            // (the rustrtc default) matches build_rtc_config in leg.rs and
            // cuts per-leg reserved memory (~150MB across 1600 legs).
            rtp_buffer_capacity: 100,
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
            ..Default::default()
        };

        if self.video_capabilities.is_empty() {
            RtcTrack::new(self.track_id, config, self.rtp_map)
        } else {
            RtcTrack::new_with_video(self.track_id, config, self.rtp_map, self.video_capabilities)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustrtc::{IceServer, IceTransportPolicy, TransportMode};

    /// CSV 功能清单 L41/L42: the built WebRTC track must switch
    /// `ice_transport_policy` to `Relay` when a `turn:`/`turns:` server is
    /// configured, and stay `All` when only STUN (or nothing) is available.
    /// RTP-mode tracks never apply the relay policy.
    async fn policy_of(mode: TransportMode, urls: &[&str]) -> IceTransportPolicy {
        let track = RtpTrackBuilder::new("policy-probe".into())
            .with_mode(mode)
            .with_ice_servers(vec![IceServer::new(
                urls.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )])
            .build();
        let pc = track.get_peer_connection().await.expect("peer connection");
        pc.config().ice_transport_policy.clone()
    }

    #[tokio::test]
    async fn webrtc_with_turn_switches_policy_to_relay() {
        assert_eq!(
            policy_of(TransportMode::WebRtc, &["turn:turn.example.com:3478"]).await,
            IceTransportPolicy::Relay
        );
    }

    #[tokio::test]
    async fn webrtc_with_turns_switches_policy_to_relay() {
        assert_eq!(
            policy_of(TransportMode::WebRtc, &["turns:turn.example.com:5349"]).await,
            IceTransportPolicy::Relay
        );
    }

    #[tokio::test]
    async fn webrtc_with_stun_only_keeps_policy_all() {
        assert_eq!(
            policy_of(TransportMode::WebRtc, &["stun:stun.example.com:19302"]).await,
            IceTransportPolicy::All
        );
    }

    #[tokio::test]
    async fn rtp_mode_with_turn_never_applies_relay_policy() {
        assert_eq!(
            policy_of(TransportMode::Rtp, &["turn:turn.example.com:3478"]).await,
            IceTransportPolicy::All
        );
    }
}
