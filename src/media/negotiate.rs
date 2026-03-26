use anyhow::{Result, anyhow};
use audio_codec::CodecType;
use rustrtc::{MediaKind, SdpType, SessionDescription};
use std::collections::{BTreeSet, HashMap, HashSet};

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

    pub fn is_dtmf(&self) -> bool {
        self.codec == CodecType::TelephoneEvent
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtractedCodecs {
    pub audio: Vec<CodecInfo>,
    pub dtmf: Vec<CodecInfo>,
}

/// Complete negotiation result
#[derive(Debug, Clone)]
pub struct NegotiationResult {
    pub codec: CodecType,
    pub params: rustrtc::RtpCodecParameters,
    pub dtmf_pt: Option<u8>,
}

/// A single negotiated codec with its RTP parameters from SDP answer.
#[derive(Debug, Clone)]
pub struct NegotiatedCodec {
    pub codec: CodecType,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u16,
}

/// Per-leg negotiated media profile extracted from an SDP answer.
/// Contains the selected audio codec and the selected DTMF entry for that answer.
#[derive(Debug, Clone, Default)]
pub struct NegotiatedLegProfile {
    pub audio: Option<NegotiatedCodec>,
    pub dtmf: Option<NegotiatedCodec>,
}

/// Media negotiator for SDP parsing and codec selection
pub struct MediaNegotiator;

impl MediaNegotiator {
    fn parse_audio_section(sdp_str: &str) -> Option<rustrtc::MediaSection> {
        SessionDescription::parse(SdpType::Answer, sdp_str)
            .or_else(|_| SessionDescription::parse(SdpType::Offer, sdp_str))
            .ok()?
            .media_sections
            .into_iter()
            .find(|m| m.kind == MediaKind::Audio)
    }

    fn parse_rtpmap_attributes(section: &rustrtc::MediaSection) -> HashMap<u8, CodecInfo> {
        let mut codec_by_pt = HashMap::new();

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

                                let codec_type = match CodecType::try_from(codec_name) {
                                    Ok(c) => c,
                                    Err(_) => continue,
                                };

                                codec_by_pt.insert(
                                    pt,
                                    CodecInfo {
                                        payload_type: pt,
                                        codec: codec_type,
                                        clock_rate,
                                        channels,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        codec_by_pt
    }

    fn static_codec_for_payload(section: &rustrtc::MediaSection, pt: u8) -> Option<CodecInfo> {
        let static_codec = if let Ok(codec) = CodecType::try_from(pt) {
            let (rate, chans) = match codec {
                CodecType::PCMU | CodecType::PCMA | CodecType::G722 | CodecType::G729 => (8000, 1),
                #[cfg(feature = "opus")]
                CodecType::Opus => (48000, 2),
                _ => return None,
            };
            Some((codec, rate, chans))
        } else {
            #[cfg(feature = "opus")]
            if (pt == 96 || pt == 111) && section.kind == MediaKind::Audio {
                Some((CodecType::Opus, 48000, 2))
            } else {
                None
            }
            #[cfg(not(feature = "opus"))]
            None
        };

        static_codec.map(|(codec, rate, chans)| CodecInfo {
            payload_type: pt,
            codec,
            clock_rate: rate,
            channels: chans,
        })
    }

    fn extract_ordered_codecs_from_section(section: &rustrtc::MediaSection) -> Vec<CodecInfo> {
        let mut codec_by_pt = Self::parse_rtpmap_attributes(section);
        let mut ordered_codecs = Vec::new();
        let mut seen_pts = HashSet::new();

        for format in &section.formats {
            let Ok(pt) = format.parse::<u8>() else {
                continue;
            };
            if !seen_pts.insert(pt) {
                continue;
            }

            let codec = codec_by_pt
                .remove(&pt)
                .or_else(|| Self::static_codec_for_payload(section, pt));
            if let Some(codec) = codec {
                ordered_codecs.push(codec);
            }
        }

        ordered_codecs
    }

    /// Parse RTP map from SDP media section in `m=` payload order.
    /// Returns: Vec<(payload_type, (codec, clock_rate, channels))>
    pub fn parse_rtp_map_from_section(
        section: &rustrtc::MediaSection,
    ) -> Vec<(u8, (CodecType, u32, u16))> {
        Self::extract_ordered_codecs_from_section(section)
            .into_iter()
            .map(|codec| {
                (
                    codec.payload_type,
                    (codec.codec, codec.clock_rate, codec.channels),
                )
            })
            .collect()
    }

    pub fn extract_codec_params(sdp_str: &str) -> ExtractedCodecs {
        Self::parse_audio_section(sdp_str)
            .map(|section| {
                let mut extracted = ExtractedCodecs::default();
                for codec in Self::extract_ordered_codecs_from_section(&section) {
                    if codec.is_dtmf() {
                        extracted.dtmf.push(codec);
                    } else {
                        extracted.audio.push(codec);
                    }
                }
                extracted
            })
            .unwrap_or_default()
    }

    pub fn extract_dtmf_codecs(sdp_str: &str) -> Vec<CodecInfo> {
        Self::extract_codec_params(sdp_str).dtmf
    }

    /// Select the best common codec based on preference
    pub fn select_best_codec(
        remote_codecs: &[CodecInfo],
        allowed_codecs: &[CodecType],
    ) -> Option<CodecInfo> {
        remote_codecs
            .iter()
            .find(|c| c.codec != CodecType::TelephoneEvent)
            .filter(|c| allowed_codecs.is_empty() || allowed_codecs.contains(&c.codec))
            .cloned()
    }

    /// Extract all codec information from SDP
    pub fn extract_all_codecs(sdp_str: &str) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(sdp_str);
        extracted.audio.into_iter().chain(extracted.dtmf).collect()
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

                let remote_dtmf_codecs: Vec<_> = remote_codecs
                    .iter()
                    .filter(|r| r.codec == CodecType::TelephoneEvent)
                    .cloned()
                    .collect();

                return Ok(NegotiationResult {
                    codec: remote.codec,
                    params,
                    dtmf_pt: remote_dtmf_codecs.first().map(|codec| codec.payload_type),
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

    /// Get preferred codec from a list
    pub fn get_preferred_codec(codecs: &[CodecType]) -> Option<CodecType> {
        codecs
            .iter()
            .find(|c| **c != CodecType::TelephoneEvent)
            .copied()
    }

    /// Extract a negotiated leg profile from an SDP answer.
    /// Takes the first audio codec (the selected one in an answer) and selects
    /// one DTMF entry using the current call assumptions.
    pub fn extract_leg_profile(sdp: &str) -> NegotiatedLegProfile {
        let extracted = Self::extract_codec_params(sdp);
        let audio = extracted.audio.first().map(|c| NegotiatedCodec {
            codec: c.codec,
            payload_type: c.payload_type,
            clock_rate: c.clock_rate,
            channels: c.channels,
        });
        let dtmf = match extracted.dtmf.len() {
            0 => None,
            1 => extracted.dtmf.first().map(|c| NegotiatedCodec {
                codec: c.codec,
                payload_type: c.payload_type,
                clock_rate: c.clock_rate,
                channels: c.channels,
            }),
            _ => {
                let preferred_rate = match audio.as_ref().map(|codec| codec.codec) {
                    #[cfg(feature = "opus")]
                    Some(CodecType::Opus) => 48000,
                    _ => 8000,
                };
                extracted
                    .dtmf
                    .iter()
                    .find(|codec| codec.clock_rate == preferred_rate)
                    .or(extracted.dtmf.first())
                    .map(|c| NegotiatedCodec {
                        codec: c.codec,
                        payload_type: c.payload_type,
                        clock_rate: c.clock_rate,
                        channels: c.channels,
                    })
            }
        };

        NegotiatedLegProfile {
            audio,
            dtmf,
        }
    }

    /// Build codec list for callee offer in anchored media mode.
    ///
    /// Strategy:
    /// 1. Keep caller's codecs that PBX supports (preserving caller's order and PT)
    /// 2. Append PBX-supported codecs not already present (with default PT)
    /// 3. For DTMF: keep caller's telephone-event entries, then append missing
    ///    variants based on which audio codecs are in the offer
    ///    (8000 for narrowband codecs, 48000 for Opus)
    pub fn build_callee_codec_offer(caller_sdp: &str, is_webrtc: bool) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(caller_sdp);
        let supported = if is_webrtc {
            Self::default_webrtc_codecs()
        } else {
            Self::default_rtp_codecs()
        };

        let mut result: Vec<CodecInfo> = Vec::new();
        let mut seen_codecs: BTreeSet<CodecType> = BTreeSet::new();

        // 1. Keep caller's audio codecs that PBX supports (preserve order & PT)
        for codec in &extracted.audio {
            if supported.contains(&codec.codec) {
                result.push(codec.clone());
                seen_codecs.insert(codec.codec);
            }
        }

        // 2. Append PBX-supported audio codecs not already present
        for codec_type in &supported {
            if *codec_type == CodecType::TelephoneEvent {
                continue; // handle DTMF separately
            }
            if !seen_codecs.contains(codec_type) {
                result.push(CodecInfo {
                    payload_type: codec_type.payload_type(),
                    clock_rate: codec_type.clock_rate(),
                    channels: codec_type.channels() as u16,
                    codec: *codec_type,
                });
                seen_codecs.insert(*codec_type);
            }
        }

        // 3. DTMF: keep caller's telephone-event entries, then append missing
        //    variants based on audio codecs present in the offer.
        let mut seen_dtmf_rates: HashSet<u32> = HashSet::new();
        for dtmf in &extracted.dtmf {
            result.push(dtmf.clone());
            seen_dtmf_rates.insert(dtmf.clock_rate);
        }
        if supported.contains(&CodecType::TelephoneEvent) {
            // Determine which DTMF clock rates are needed based on audio codecs
            let has_opus = result.iter().any(|c| c.codec == CodecType::Opus);
            let has_narrowband = result
                .iter()
                .any(|c| c.codec.is_audio() && c.codec != CodecType::Opus);

            let mut needed_rates: Vec<u32> = Vec::new();
            if has_narrowband && !seen_dtmf_rates.contains(&8000) {
                needed_rates.push(8000);
            }
            if has_opus && !seen_dtmf_rates.contains(&48000) {
                needed_rates.push(48000);
            }

            let mut used_pts: HashSet<u8> = result.iter().map(|c| c.payload_type).collect();
            for rate in needed_rates {
                let default_pt = CodecType::TelephoneEvent.payload_type();
                let pt = if !used_pts.contains(&default_pt) {
                    default_pt
                } else {
                    (96..=127)
                        .find(|p| !used_pts.contains(p))
                        .unwrap_or(default_pt)
                };
                used_pts.insert(pt);
                result.push(CodecInfo {
                    payload_type: pt,
                    clock_rate: rate,
                    channels: 1,
                    codec: CodecType::TelephoneEvent,
                });
            }
        }

        result
    }

    /// Build codec list for answering the caller in anchored media mode.
    ///
    /// Unlike the callee offer builder, this must stay a strict subset of the
    /// caller's original offer so the generated answer never advertises a codec
    /// the caller did not offer. PT values are preserved from the caller's SDP.
    pub fn build_caller_answer_codec_list(caller_sdp: &str, is_webrtc: bool) -> Vec<CodecInfo> {
        let extracted = Self::extract_codec_params(caller_sdp);
        let supported = if is_webrtc {
            Self::default_webrtc_codecs()
        } else {
            Self::default_rtp_codecs()
        };

        let mut result = Vec::new();
        for codec in extracted.audio.into_iter().chain(extracted.dtmf) {
            if supported.contains(&codec.codec) {
                result.push(codec);
            }
        }

        result
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

        let codecs = MediaNegotiator::extract_codec_params(sdp);
        let first = &codecs.audio[0];
        let params = first.to_params();

        assert_eq!(first.codec, CodecType::PCMU);
        assert_eq!(params.payload_type, 0);
        assert_eq!(params.clock_rate, 8000);
        assert_eq!(
            codecs
                .dtmf
                .iter()
                .map(|codec| codec.payload_type)
                .collect::<Vec<_>>(),
            vec![101]
        );
    }

    #[test]
    fn test_extract_codec_params_preserves_dtmf_offer_order() {
        let sdp = "v=0\r\n\
            o=- 1234 1234 IN IP4 127.0.0.1\r\n\
            s=-\r\n\
            t=0 0\r\n\
            m=audio 10000 RTP/AVP 96 110 126\r\n\
            a=rtpmap:96 opus/48000/2\r\n\
            a=rtpmap:110 telephone-event/48000\r\n\
            a=rtpmap:126 telephone-event/8000\r\n";

        let codecs = MediaNegotiator::extract_codec_params(sdp);

        assert_eq!(
            codecs
                .dtmf
                .iter()
                .map(|codec| (codec.payload_type, codec.clock_rate))
                .collect::<Vec<_>>(),
            vec![(110, 48000), (126, 8000)]
        );
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
        let codecs = MediaNegotiator::extract_codec_params(sdp);
        assert_eq!(
            codecs.audio[0].codec,
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

        // Only allow PCMU - current behavior does not scan past the first remote audio codec
        let allowed = vec![CodecType::PCMU];
        let best = MediaNegotiator::select_best_codec(&codecs, &allowed);
        assert!(best.is_none());

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
    fn test_g722_clock_rate_preserves_sdp_value() {
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

        let codecs = MediaNegotiator::extract_codec_params(sdp);

        // Find G722 codec
        let g722_info = codecs.audio.iter().find(|c| c.codec == CodecType::G722);
        assert!(g722_info.is_some(), "G722 should be parsed");

        let g722_info = g722_info.unwrap();
        assert_eq!(
            g722_info.clock_rate, 16000,
            "G722 clock rate should now follow the SDP value as offered"
        );
        assert_eq!(g722_info.payload_type, 9);
        assert_eq!(g722_info.channels, 1);

        // Verify other codecs are not affected
        let g729_info = codecs.audio.iter().find(|c| c.codec == CodecType::G729);
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
