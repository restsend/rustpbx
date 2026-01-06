use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use rustrtc::{MediaKind, SdpType, SessionDescription};

/// Parsed RTP codec information from SDP
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub payload_type: u8,
    pub codec: CodecType,
    pub clock_rate: u32,
    pub channels: u16,
}

/// Complete negotiation result
#[derive(Debug, Clone)]
pub struct NegotiationResult {
    pub codec: CodecType,
    pub params: rustrtc::RtpCodecParameters,
    pub dtmf_pt: Option<u8>,
}

/// Media negotiator for SDP parsing and codec selection
pub struct MediaNegotiator;

impl MediaNegotiator {
    /// Parse RTP map from SDP media section
    /// Returns: Vec<(payload_type, (codec, clock_rate, channels))>
    pub fn parse_rtp_map_from_section(
        section: &rustrtc::MediaSection,
    ) -> Vec<(u8, (CodecType, u32, u16))> {
        let mut rtp_map = Vec::new();
        for attr in &section.attributes {
            if attr.key == "rtpmap" {
                if let Some(ref value) = attr.value {
                    if let Some((pt_str, codec_str)) = value.split_once(' ') {
                        if let Ok(pt) = pt_str.parse::<u8>() {
                            let parts: Vec<&str> = codec_str.split('/').collect();
                            if parts.len() >= 2 {
                                let codec_name = parts[0];
                                let clock_rate = parts[1].parse::<u32>().unwrap_or(8000);
                                let channels = if parts.len() >= 3 {
                                    parts[2].parse::<u16>().unwrap_or(1)
                                } else {
                                    1
                                };

                                let codec_type = match codec_name.to_lowercase().as_str() {
                                    "pcmu" => CodecType::PCMU,
                                    "pcma" => CodecType::PCMA,
                                    #[cfg(feature = "opus")]
                                    "opus" => CodecType::Opus,
                                    "g722" => CodecType::G722,
                                    "g729" => CodecType::G729,
                                    "telephone-event" => CodecType::TelephoneEvent,
                                    _ => continue,
                                };

                                rtp_map.push((pt, (codec_type, clock_rate, channels)));
                            }
                        }
                    }
                }
            }
        }
        rtp_map
    }

    pub fn parse_codec(name: &str) -> Option<CodecType> {
        match name.to_lowercase().as_str() {
            "pcmu" => Some(CodecType::PCMU),
            "pcma" => Some(CodecType::PCMA),
            #[cfg(feature = "opus")]
            "opus" => Some(CodecType::Opus),
            "g722" => Some(CodecType::G722),
            "g729" => Some(CodecType::G729),
            "telephone-event" => Some(CodecType::TelephoneEvent),
            _ => None,
        }
    }

    /// Extract codec parameters from SDP string
    /// Returns: (RtpCodecParameters, dtmf_payload_type, CodecType)
    pub fn extract_codec_params(
        sdp_str: &str,
    ) -> (rustrtc::RtpCodecParameters, Option<u8>, CodecType) {
        let mut params = rustrtc::RtpCodecParameters::default();
        let mut dtmf_pt = None;
        let mut codec_type = CodecType::PCMU;
        let mut _first_codec_found = false;

        if let Ok(desc) = SessionDescription::parse(SdpType::Answer, sdp_str) {
            if let Some(section) = desc
                .media_sections
                .iter()
                .find(|m| m.kind == MediaKind::Audio)
            {
                let rtp_map = Self::parse_rtp_map_from_section(section);

                // Find the best non-DTMF codec (prefer Opus > G722 > PCMU > PCMA)
                let mut best_priority = -1;

                for (pt, (codec, clock, channels)) in &rtp_map {
                    if *codec == CodecType::TelephoneEvent {
                        dtmf_pt = Some(*pt);
                        continue;
                    }

                    let priority = match codec {
                        CodecType::Opus => 100,
                        CodecType::G722 => 90,
                        CodecType::PCMU => 80,
                        CodecType::PCMA => 70,
                        CodecType::G729 => 60,
                        _ => 0,
                    };

                    if priority > best_priority {
                        best_priority = priority;
                        params.payload_type = *pt;
                        params.clock_rate = *clock;
                        params.channels = if *channels > 255 {
                            255
                        } else {
                            *channels as u8
                        };
                        codec_type = *codec;
                    }
                }
            }
        }

        (params, dtmf_pt, codec_type)
    }

    /// Extract all codec information from SDP
    pub fn extract_all_codecs(sdp_str: &str) -> Vec<CodecInfo> {
        let mut codecs = Vec::new();

        if let Ok(desc) = SessionDescription::parse(SdpType::Offer, sdp_str) {
            if let Some(section) = desc
                .media_sections
                .iter()
                .find(|m| m.kind == MediaKind::Audio)
            {
                let rtp_map = Self::parse_rtp_map_from_section(section);
                for (pt, (codec, clock, channels)) in rtp_map {
                    codecs.push(CodecInfo {
                        payload_type: pt,
                        codec,
                        clock_rate: clock,
                        channels,
                    });
                }
            }
        }

        codecs
    }

    /// Negotiate codec between two SDP offers/answers
    /// Returns the selected codec info
    pub fn negotiate_codec(
        local_codecs: &[CodecType],
        remote_sdp: &str,
    ) -> Result<NegotiationResult> {
        let remote_codecs = Self::extract_all_codecs(remote_sdp);

        // Find first matching codec (prioritize local order)
        for local_codec in local_codecs {
            if let Some(remote) = remote_codecs
                .iter()
                .find(|r| r.codec == *local_codec && r.codec != CodecType::TelephoneEvent)
            {
                let params = rustrtc::RtpCodecParameters {
                    payload_type: remote.payload_type,
                    clock_rate: remote.clock_rate,
                    channels: if remote.channels > 255 {
                        255
                    } else {
                        remote.channels as u8
                    },
                };

                let dtmf_pt = remote_codecs
                    .iter()
                    .find(|r| r.codec == CodecType::TelephoneEvent)
                    .map(|r| r.payload_type);

                return Ok(NegotiationResult {
                    codec: remote.codec,
                    params,
                    dtmf_pt,
                });
            }
        }

        Err(anyhow!("No compatible codec found"))
    }

    /// Build default codec list for RTP endpoints
    pub fn default_rtp_codecs() -> Vec<CodecType> {
        vec![
            #[cfg(feature = "opus")]
            CodecType::Opus,
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::TelephoneEvent,
        ]
    }

    /// Build default codec list for WebRTC endpoints
    pub fn default_webrtc_codecs() -> Vec<CodecType> {
        vec![
            #[cfg(feature = "opus")]
            CodecType::Opus,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
            CodecType::TelephoneEvent,
        ]
    }

    /// Check if transcoding is needed between two codecs
    pub fn needs_transcoding(codec_a: CodecType, codec_b: CodecType) -> bool {
        codec_a != codec_b
    }

    /// Get preferred codec from a list
    pub fn get_preferred_codec(codecs: &[CodecType]) -> Option<CodecType> {
        codecs
            .iter()
            .find(|c| **c != CodecType::TelephoneEvent)
            .copied()
    }

    pub fn extract_ssrc(sdp: &str) -> Option<u32> {
        // Try parsing as Answer first, then Offer if it fails (though usually it's Answer)
        let session = SessionDescription::parse(SdpType::Answer, sdp)
            .or_else(|_| SessionDescription::parse(SdpType::Offer, sdp))
            .ok()?;

        for section in session.media_sections {
            if section.kind == MediaKind::Audio {
                for attr in section.attributes {
                    if attr.key == "ssrc" {
                        if let Some(value) = attr.value {
                            // value format: "12345 cname:..." or just "12345"
                            let ssrc_str = value.split_whitespace().next()?;
                            if let Ok(ssrc) = ssrc_str.parse::<u32>() {
                                return Some(ssrc);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rtp_map() {
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 8 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let desc = SessionDescription::parse(SdpType::Offer, sdp).unwrap();
        let section = desc
            .media_sections
            .iter()
            .find(|m| m.kind == MediaKind::Audio)
            .unwrap();

        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        assert_eq!(rtp_map.len(), 3);
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (c, _, _))| *pt == 0 && *c == CodecType::PCMU)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (c, _, _))| *pt == 8 && *c == CodecType::PCMA)
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (c, _, _))| *pt == 101 && *c == CodecType::TelephoneEvent)
        );
    }

    #[test]
    fn test_extract_codec_params() {
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let (params, dtmf_pt, codec) = MediaNegotiator::extract_codec_params(sdp);

        assert_eq!(codec, CodecType::PCMU);
        assert_eq!(params.payload_type, 0);
        assert_eq!(params.clock_rate, 8000);
        assert_eq!(dtmf_pt, Some(101));
    }

    #[test]
    fn test_negotiate_codec() {
        let local_codecs = vec![CodecType::PCMU, CodecType::PCMA];

        let remote_sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 8 101\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let result = MediaNegotiator::negotiate_codec(&local_codecs, remote_sdp).unwrap();

        assert_eq!(result.codec, CodecType::PCMA);
        assert_eq!(result.params.payload_type, 8);
        assert_eq!(result.dtmf_pt, Some(101));
    }

    #[test]
    fn test_negotiate_codec_no_match() {
        let local_codecs = vec![CodecType::Opus];

        let remote_sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0\r\n\
            a=rtpmap:0 PCMU/8000\r\n";

        let result = MediaNegotiator::negotiate_codec(&local_codecs, remote_sdp);
        assert!(result.is_err());
    }

    #[test]
    fn test_needs_transcoding() {
        assert!(!MediaNegotiator::needs_transcoding(
            CodecType::PCMU,
            CodecType::PCMU
        ));
        assert!(MediaNegotiator::needs_transcoding(
            CodecType::PCMU,
            CodecType::PCMA
        ));
    }

    #[test]
    fn test_default_codecs() {
        let rtp_codecs = MediaNegotiator::default_rtp_codecs();
        assert!(rtp_codecs.contains(&CodecType::PCMU));
        assert!(rtp_codecs.contains(&CodecType::PCMA));

        let webrtc_codecs = MediaNegotiator::default_webrtc_codecs();
        assert!(webrtc_codecs.contains(&CodecType::PCMU));
    }
}
