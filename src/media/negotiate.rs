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

impl CodecInfo {
    pub fn to_params(&self) -> rustrtc::RtpCodecParameters {
        rustrtc::RtpCodecParameters {
            payload_type: self.payload_type,
            clock_rate: self.clock_rate,
            channels: if self.channels > 255 {
                255
            } else {
                self.channels as u8
            },
            ..Default::default()
        }
    }
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
        let mut seen_pts = std::collections::HashSet::new();

        for attr in &section.attributes {
            if attr.key == "rtpmap" {
                if let Some(ref value) = attr.value {
                    if let Some((pt_str, codec_str)) = value.split_once(' ') {
                        if let Ok(pt) = pt_str.parse::<u8>() {
                            let parts: Vec<&str> = codec_str.split('/').collect();
                            if parts.len() >= 2 {
                                let codec_name = parts[0];
                                let mut clock_rate = parts[1].parse::<u32>().unwrap_or(8000);
                                let channels = if parts.len() >= 3 {
                                    parts[2].parse::<u16>().unwrap_or(1)
                                } else {
                                    1
                                };

                                let codec_type = match CodecType::try_from(codec_name) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };

                                if codec_type == CodecType::G722 && clock_rate != 8000 {
                                    clock_rate = 8000;
                                }

                                rtp_map.push((pt, (codec_type, clock_rate, channels)));
                                seen_pts.insert(pt);
                            }
                        }
                    }
                }
            }
        }

        // Handle static payload types
        for format in &section.formats {
            if let Ok(pt) = format.parse::<u8>() {
                if seen_pts.contains(&pt) {
                    continue;
                }

                // Use CodecType::try_from for standard payload type conversion
                let static_codec = if let Ok(codec) = CodecType::try_from(pt) {
                    // Standard payload types: 0=PCMU, 8=PCMA, 9=G722, 18=G729
                    let (rate, chans) = match codec {
                        CodecType::PCMU | CodecType::PCMA | CodecType::G722 | CodecType::G729 => {
                            (8000, 1)
                        }
                        #[cfg(feature = "opus")]
                        CodecType::Opus => (48000, 2),
                        _ => continue, // Ignore telephone-event and other types
                    };
                    Some((codec, rate, chans))
                } else {
                    // Non-standard fallback for common dynamic payload types when rtpmap is missing
                    // Only apply this for Audio logic to implicit infer Opus
                    #[cfg(feature = "opus")]
                    if (pt == 96 || pt == 111) && section.kind == MediaKind::Audio {
                        Some((CodecType::Opus, 48000, 2))
                    } else {
                        None
                    }
                    #[cfg(not(feature = "opus"))]
                    None
                };

                if let Some((codec, rate, chans)) = static_codec {
                    rtp_map.push((pt, (codec, rate, chans)));
                    seen_pts.insert(pt);
                }
            }
        }

        rtp_map
    }

    /// Extract codec parameters from SDP string
    /// Returns: (Vec<CodecInfo>, dtmf_payload_type)
    pub fn extract_codec_params(sdp_str: &str) -> (Vec<CodecInfo>, Option<u8>) {
        let mut codecs = Vec::new();
        let mut dtmf_pt = None;

        let desc = SessionDescription::parse(SdpType::Answer, sdp_str)
            .or_else(|_| SessionDescription::parse(SdpType::Offer, sdp_str));

        if let Ok(desc) = desc {
            if let Some(section) = desc
                .media_sections
                .iter()
                .find(|m| m.kind == MediaKind::Audio)
            {
                let rtp_map = Self::parse_rtp_map_from_section(section);

                for (pt, (codec, clock, channels)) in rtp_map {
                    if codec == CodecType::TelephoneEvent {
                        dtmf_pt = Some(pt);
                        continue;
                    }

                    codecs.push(CodecInfo {
                        payload_type: pt,
                        codec,
                        clock_rate: clock,
                        channels: channels as u16,
                    });
                }
            }
        }

        (codecs, dtmf_pt)
    }

    /// Select the best common codec based on preference
    pub fn select_best_codec(
        remote_codecs: &[CodecInfo],
        allowed_codecs: &[CodecType],
    ) -> Option<CodecInfo> {
        if remote_codecs.is_empty() {
            return None;
        }

        if allowed_codecs.is_empty() {
            // No restriction: pick the first audio codec from remote (skip TelephoneEvent)
            return remote_codecs
                .iter()
                .find(|c| c.codec != CodecType::TelephoneEvent)
                .cloned();
        }

        // RFC 3264: When remote_codecs is from an Answer, respect the answerer's preference
        // Select the first audio codec from remote_codecs that is in our allowed list
        // Skip TelephoneEvent as it's not an audio codec
        for remote in remote_codecs {
            if remote.codec != CodecType::TelephoneEvent && allowed_codecs.contains(&remote.codec) {
                return Some(remote.clone());
            }
        }

        None
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

        let (codecs, dtmf_pt) = MediaNegotiator::extract_codec_params(sdp);
        let first = &codecs[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::PCMU);
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

    #[test]
    fn test_parse_static_payload_types() {
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 0 8 101\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";

        let desc = SessionDescription::parse(SdpType::Offer, sdp).unwrap();
        let section = desc
            .media_sections
            .iter()
            .find(|m| m.kind == MediaKind::Audio)
            .unwrap();
        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        println!("RTP MAP: {:?}", rtp_map);

        // Should find PCMU (0) and PCMA (8) even without rtpmap
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 0 && *codec == CodecType::PCMU),
            "Missing PCMU (0)"
        );
        assert!(
            rtp_map
                .iter()
                .any(|(pt, (codec, _, _))| *pt == 8 && *codec == CodecType::PCMA),
            "Missing PCMA (8)"
        );
    }

    #[test]
    fn test_parse_dynamic_payload_type_fallback() {
        // Test handling of common dynamic payload types when rtpmap is missing (e.g. Opus as 96)
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 96\r\n"; // 96 without rtpmap

        let desc = SessionDescription::parse(SdpType::Offer, sdp).unwrap();
        let section = desc
            .media_sections
            .iter()
            .find(|m| m.kind == MediaKind::Audio)
            .unwrap();
        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        // This expects the permissive behavior we are about to implement
        assert!(
            rtp_map.iter().any(|(pt, (codec, rate, chans))| *pt == 96
                && *codec == CodecType::Opus
                && *rate == 48000
                && *chans == 2),
            "Missing fallback for Opus (96)"
        );
    }

    #[test]
    fn test_parse_dynamic_payload_type_fallback_111() {
        // Test handling of dynamic payload type 111 for Opus fallback
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 111\r\n"; // 111 without rtpmap

        let desc = SessionDescription::parse(SdpType::Offer, sdp).unwrap();
        let section = desc
            .media_sections
            .iter()
            .find(|m| m.kind == MediaKind::Audio)
            .unwrap();
        let rtp_map = MediaNegotiator::parse_rtp_map_from_section(section);

        assert!(
            rtp_map.iter().any(|(pt, (codec, rate, chans))| *pt == 111
                && *codec == CodecType::Opus
                && *rate == 48000
                && *chans == 2),
            "Missing fallback for Opus (111)"
        );
    }

    #[test]
    fn test_extract_codec_params_order_preference() {
        // PCMU(0) is first, G722(9) is later.
        // We should pick PCMU because it's first in the Answer.
        let sdp = "v=0\r\no=- 123456 123456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0 101 8 9\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:9 G722/8000\r\n";
        let (codecs, _) = MediaNegotiator::extract_codec_params(sdp);
        assert_eq!(
            codecs[0].codec,
            CodecType::PCMU,
            "Should have picked PCMU (the first codec)"
        );
    }

    #[test]
    fn test_select_best_codec_with_preference() {
        // Simulating Answer codecs where remote peer chose G722 first, then PCMU
        let codecs = vec![
            CodecInfo {
                payload_type: 9,
                codec: CodecType::G722,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            },
        ];

        // RFC 3264: Respect remote (answerer's) preference
        // Even if our preference is [PCMU, G722], we should respect the Answer order
        let allowed = vec![CodecType::PCMU, CodecType::G722];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        // Should pick G722 because it's first in the Answer (remote preference)
        assert_eq!(best.codec, CodecType::G722);

        // If our allowed list is [G722, PCMU], still pick G722 (first in remote)
        let allowed = vec![CodecType::G722, CodecType::PCMU];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(best.codec, CodecType::G722);

        // Only allow PCMU - should skip G722 and pick PCMU
        let allowed = vec![CodecType::PCMU];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(best.codec, CodecType::PCMU);

        // Empty allowed list - should follow remote order (first codec)
        let allowed = vec![];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(best.codec, CodecType::G722);
    }

    #[test]
    fn test_select_best_codec_skips_telephone_event() {
        // Simulating Answer with TelephoneEvent as first codec (should be skipped)
        let codecs = vec![
            CodecInfo {
                payload_type: 101,
                codec: CodecType::TelephoneEvent,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 8,
                codec: CodecType::PCMA,
                clock_rate: 8000,
                channels: 1,
            },
        ];

        // Should skip TelephoneEvent and pick PCMU (first audio codec)
        let allowed = vec![CodecType::PCMU, CodecType::PCMA];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(best.codec, CodecType::PCMU);
        assert_ne!(best.codec, CodecType::TelephoneEvent);

        // Empty allowed list - should skip TelephoneEvent and pick first audio codec
        let allowed = vec![];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed).unwrap();
        assert_eq!(best.codec, CodecType::PCMU);
        assert_ne!(best.codec, CodecType::TelephoneEvent);
    }

    #[test]
    fn test_g722_clock_rate_correction() {
        // Test that G722/16000 (incorrect) is corrected to G722/8000 (RFC 3551)
        let sdp = "v=0\r\n\
            o=- 1769236545 1769236546 IN IP4 192.168.3.211\r\n\
            s=-\r\n\
            c=IN IP4 192.168.3.211\r\n\
            t=0 0\r\n\
            m=audio 51624 RTP/AVP 0 8 9 18 111\r\n\
            a=mid:0\r\n\
            a=sendrecv\r\n\
            a=rtcp-mux\r\n\
            a=rtpmap:0 PCMU/8000/1\r\n\
            a=rtpmap:8 PCMA/8000/1\r\n\
            a=rtpmap:9 G722/16000/1\r\n\
            a=rtpmap:18 G729/8000/1\r\n\
            a=rtpmap:111 opus/48000/2\r\n";

        let (codecs, _) = MediaNegotiator::extract_codec_params(sdp);

        // Find G722 codec
        let g722_info = codecs.iter().find(|c| c.codec == CodecType::G722);
        assert!(g722_info.is_some(), "G722 should be parsed");

        let g722_info = g722_info.unwrap();
        assert_eq!(
            g722_info.clock_rate, 8000,
            "G722 RTP clock rate should be corrected to 8000 Hz (RFC 3551)"
        );
        assert_eq!(g722_info.payload_type, 9);
        assert_eq!(g722_info.channels, 1);

        // Verify other codecs are not affected
        let g729_info = codecs.iter().find(|c| c.codec == CodecType::G729);
        assert!(g729_info.is_some());
        assert_eq!(g729_info.unwrap().clock_rate, 8000);
    }

    #[test]
    fn test_answer_codec_selection_respects_answerer_preference() {
        // Simulating the scenario from user's log:
        // rustpbx sent INVITE with: 96(G729), 9(G722), 0(PCMU), 8(PCMA), 111(Opus)
        // alice answered with:     0(PCMU), 8(PCMA), 9(G722), 18(G729), 111(Opus)
        // RFC 3264: We MUST use PCMU (alice's first choice), not G729 (our first choice)

        let answer_codecs = vec![
            CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 8,
                codec: CodecType::PCMA,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 9,
                codec: CodecType::G722,
                clock_rate: 8000,
                channels: 1,
            },
            CodecInfo {
                payload_type: 18,
                codec: CodecType::G729,
                clock_rate: 8000,
                channels: 1,
            },
        ];

        // Our preference was G729 first, but we should respect alice's choice (PCMU)
        let our_offer_order = vec![
            CodecType::G729,
            CodecType::G722,
            CodecType::PCMU,
            CodecType::PCMA,
        ];

        let selected = MediaNegotiator::select_best_codec(&answer_codecs, &our_offer_order);
        assert!(selected.is_some(), "Should find a matching codec");

        let selected = selected.unwrap();
        assert_eq!(
            selected.codec,
            CodecType::PCMU,
            "Must use PCMU (answerer's first choice), not G729 (offerer's first choice)"
        );
        assert_eq!(selected.payload_type, 0);
    }
}
